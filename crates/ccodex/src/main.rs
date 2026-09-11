mod accounts;
mod admin;
mod admin_auth;
mod analytics;
mod config;
mod forward;
mod gateway;
mod identity;
mod keys;
mod login;
mod metrics;
mod model_db;
mod pricing;
mod proxies;
mod quota;
mod request_build;
mod sse_tap;
mod telemetry;
mod turns;
mod usage;
mod web;
mod ws;

use clap::{Parser, Subcommand};
use codex_api::Provider;
use codex_http_client::{HttpClientFactory, OutboundProxyPolicy};
use codex_login::AuthRouteConfig;
use config::Config;
use gateway::AppState;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "ccodex",
    about = "Codex relay on official crates (byte-identical upstream behavior)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the relay gateway
    Serve {
        /// Path to the config file
        #[arg(short, long, default_value = "config.toml")]
        config: PathBuf,
    },
    /// Import an existing auth.json (run `codex login` on this machine first, then import)
    Import {
        /// Account name (subdirectory name)
        #[arg(long)]
        name: String,
        /// Source auth.json path; defaults to ~/.codex/auth.json
        #[arg(long)]
        from: Option<PathBuf>,
        /// Account root dir (defaults to the config file value; may be set directly)
        #[arg(long)]
        accounts_dir: Option<PathBuf>,
        /// Config file path (used to resolve accounts_dir)
        #[arg(short, long, default_value = "config.toml")]
        config: PathBuf,
    },
    /// Browser OAuth login (same flow as `codex login`): prints the authorize URL, the user
    /// pastes back the final localhost callback URL; credentials land in the account dir
    Login {
        /// Account name (subdirectory name)
        #[arg(long)]
        name: String,
        /// Account root dir (defaults to the config file value; may be set directly)
        #[arg(long)]
        accounts_dir: Option<PathBuf>,
        /// Config file path (used to resolve accounts_dir / proxy)
        #[arg(short, long, default_value = "config.toml")]
        config: PathBuf,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    match Cli::parse().command {
        Command::Import {
            name,
            from,
            accounts_dir,
            config,
        } => import(&name, from, accounts_dir, &config),
        Command::Login {
            name,
            accounts_dir,
            config,
        } => {
            let config = Config::load(&config)?;
            // Proxy env vars must be set before the HTTP client is created (same proxy
            // path as the official client).
            if let Some(proxy) = &config.upstream_proxy {
                apply_proxy_env(proxy);
            }
            let root = accounts_dir.unwrap_or_else(|| config.accounts_dir());
            let codex_home = root.join(&name);
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(async move {
                    let route = AuthRouteConfig::from_http_client_factory(HttpClientFactory::new(
                        OutboundProxyPolicy::ReqwestDefault,
                    ));
                    login::oauth_login_cli(codex_home, &route).await
                })
        }
        Command::Serve { config } => {
            let cfg = Config::load(&config)?;

            // Env vars must be written before the tokio multi-thread runtime starts:
            // 1) identity version (injection point of the patched official UA logic)
            identity::apply(&cfg.identity);
            // 2) outbound proxy (the official HTTP client reads env via reqwest's system
            //    proxy logic at build time)
            if let Some(proxy) = &cfg.upstream_proxy {
                apply_proxy_env(proxy);
                tracing::info!(proxy = %proxy, "upstream proxy configured");
            }

            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(serve(cfg, config))
        }
    }
}

/// Outbound proxy: written into the env vars reqwest's system-proxy logic reads (same
/// proxy path as the official client). Existing env vars win.
fn apply_proxy_env(proxy: &str) {
    for var in ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"] {
        if std::env::var(var).is_err() {
            // SAFETY: called in the single-threaded phase before the runtime starts.
            unsafe { std::env::set_var(var, proxy) };
        }
    }
}

async fn serve(config: Config, config_path: PathBuf) -> anyhow::Result<()> {
    // Official Provider: URL joining + version header (aligned with
    // ModelProviderInfo::create_openai_provider).
    let mut provider_headers = http::HeaderMap::new();
    if let Ok(v) = http::HeaderValue::from_str(&identity::codex_version()) {
        provider_headers.insert("version", v);
    }
    let provider = Provider {
        name: "OpenAI".to_string(),
        // Dev self-check: CCODEX_UPSTREAM_BASE_URL_OVERRIDE points at a local mock upstream.
        base_url: std::env::var("CCODEX_UPSTREAM_BASE_URL_OVERRIDE")
            .unwrap_or_else(|_| codex_model_provider_info::CHATGPT_CODEX_BASE_URL.to_string()),
        query_params: None,
        headers: provider_headers,
        retry: codex_api::RetryConfig {
            max_attempts: 4,
            base_delay: Duration::from_millis(200),
            retry_429: true,
            retry_5xx: true,
            retry_transport: true,
        },
        stream_idle_timeout: Duration::from_secs(300),
    };

    // Account pool (official AuthManager: load / proactive refresh / 401 recovery).
    let route = AuthRouteConfig::from_http_client_factory(HttpClientFactory::new(
        OutboundProxyPolicy::ReqwestDefault,
    ));
    // Shared with the telemetry channels, which resolve each account's binding per send.
    let proxy_store = std::sync::Arc::new(proxies::ProxyStore::load(
        &proxies::ProxyStore::path_for(&config.accounts_dir()),
    ));
    let key_store = keys::KeyStore::load(&keys::KeyStore::path_for(&config.accounts_dir()));
    // One-time migration: legacy config.toml api_keys move into the managed store
    // (fingerprint-named, idempotent). Config is no longer an auth source afterwards.
    let legacy = config::legacy_api_keys(&config_path);
    if !legacy.is_empty() {
        let mut migrated = 0;
        for k in &legacy {
            if key_store.contains(k) {
                continue;
            }
            let name = format!("migrated-{}", &usage::key_fingerprint(k)[..8]);
            match key_store.add(&name, Some(k)) {
                Ok(_) => migrated += 1,
                Err(e) => tracing::warn!(error = %e, "config api_keys 迁移失败"),
            }
        }
        tracing::warn!(
            migrated,
            total = legacy.len(),
            "config.toml 的 api_keys 已迁移进 keys.json；api_keys 配置项不再生效，请从配置文件中删除该行"
        );
    }
    let admin_auth =
        admin_auth::AdminAuth::load(&admin_auth::AdminAuth::path_for(&config.accounts_dir()));
    let usage_store = usage::UsageStore::load(&usage::UsageStore::path_for(&config.accounts_dir()));
    let pricing_store =
        pricing::PricingStore::load(&pricing::PricingStore::path_for(&config.accounts_dir()));
    let pool = accounts::Pool::load(
        &config.accounts_dir(),
        &route,
        config.sticky_ttl(),
        &proxy_store,
    )
    .await?;
    tracing::info!(accounts = pool.len(), "account pool ready");

    let state = Arc::new(AppState {
        model_db: model_db::ModelDb::load(),
        config,
        pool: std::sync::RwLock::new(pool),
        provider,
        route,
        proxies: proxy_store.clone(),
        keys: key_store,
        admin_auth,
        usage: usage_store,
        pricing: pricing_store,
        upstream_version: std::sync::Mutex::new(admin::UpstreamVersionCache::default()),
        logins: std::sync::Mutex::new(std::collections::HashMap::new()),
        oauth_logins: std::sync::Mutex::new(std::collections::HashMap::new()),
        sessions: request_build::SessionStore::new(Duration::from_secs(24 * 3600)),
        telemetry: std::sync::Arc::new(telemetry::Telemetry::new(proxy_store.clone())),
        // Seeded with the embedded official models.json so /v1/models answers instantly
        // from the first request; the background task upgrades it to live upstream bytes.
        models_cache: std::sync::RwLock::new(Some(gateway::CachedModels {
            body: bytes::Bytes::from_static(model_db::MODELS_JSON.as_bytes()),
            fetched_at_unix: 0,
            live: false,
        })),
    });
    gateway::spawn_models_refresh(Arc::clone(&state));
    telemetry::Telemetry::spawn_sweep(&state.telemetry, {
        let state = Arc::clone(&state);
        move || state.pool.read().unwrap().clone()
    });

    let listen = state.config.listen().to_string();
    let addr: SocketAddr = listen
        .parse()
        .map_err(|e| anyhow::anyhow!("listen 地址无效 {listen}: {e}"))?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(
        listen = %addr,
        upstream_commit = env!("CCODEX_UPSTREAM_COMMIT"),
        "ccodex serving"
    );
    axum::serve(listener, gateway::router(state)).await?;
    Ok(())
}

fn import(
    name: &str,
    from: Option<PathBuf>,
    accounts_dir: Option<PathBuf>,
    config_path: &std::path::Path,
) -> anyhow::Result<()> {
    let from = from.unwrap_or_else(|| dirs_home().join(".codex").join("auth.json"));
    if !from.exists() {
        anyhow::bail!(
            "找不到 {}（先运行 codex login，或用 --from 指定）",
            from.display()
        );
    }
    let text = std::fs::read_to_string(&from)?;
    let parsed: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("{} 不是合法 JSON: {e}", from.display()))?;
    if parsed.pointer("/tokens/access_token").is_none() {
        anyhow::bail!(
            "{} 不含 tokens.access_token，不是 ChatGPT OAuth 凭证",
            from.display()
        );
    }

    let root = match accounts_dir {
        Some(dir) => dir,
        None => Config::load(config_path)?.accounts_dir(),
    };
    let dest_dir = root.join(name);
    std::fs::create_dir_all(&dest_dir)?;
    let dest = dest_dir.join("auth.json");
    std::fs::write(&dest, &text)?;
    tracing::info!(account = %name, dest = %dest.display(), "account imported");
    Ok(())
}

fn dirs_home() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
}
