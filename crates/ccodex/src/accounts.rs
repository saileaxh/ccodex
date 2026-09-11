//! Account pool: one codex_home per account (containing auth.json).
//! Authentication is fully delegated to the official codex-login AuthManager
//! (load / proactive refresh / persistence); request auth headers come from the
//! official model-provider `auth_provider_from_auth_manager`
//! (Authorization + ChatGPT-Account-ID + X-OpenAI-Fedramp, follows refreshes).

use codex_api::SharedAuthProvider;
use codex_http_client::ReqwestTransport;
use codex_login::{AuthCredentialsStoreMode, AuthKeyringBackendKind, AuthManager, AuthRouteConfig};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// Credential health verdict learned from upstream traffic. In-memory only: a restart
/// re-probes once (one request → refresh → classify), which self-heals if auth.json was
/// fixed on disk and re-marks a still-dead credential.
#[derive(Clone, Debug, PartialEq)]
pub enum AuthStatus {
    /// No traffic verdict yet this process.
    Unknown,
    /// Last upstream contact authenticated fine.
    Ok,
    /// Upstream says the credentials are dead (official RefreshTokenError::Permanent, or a
    /// successful refresh still yielding 401). Skipped in selection until a re-login
    /// rebuilds the account (pool reload creates fresh Account objects).
    Invalid { reason: String, since_unix: u64 },
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub struct Account {
    pub name: String,
    pub auth_manager: Arc<AuthManager>,
    pub auth: SharedAuthProvider,
    /// ChatGPT account id (logging / admin UI).
    pub account_id: Option<String>,
    /// ChatGPT account email from the id_token claims (admin UI display name).
    pub account_email: Option<String>,
    /// Subscription plan raw value ("free"/"plus"/…), learned from the id_token claims at
    /// load and refreshed from wham/usage snapshots (header-derived snapshots never carry it).
    plan: RwLock<Option<String>>,
    /// Current upstream primary rate-limit window (start, end) learned from quota
    /// snapshots; the usage store rolls per-account period counters on it.
    period_window: RwLock<Option<(u64, u64)>>,
    /// Official installation_id semantics: persisted per account dir at
    /// <codex_home>/installation_id (read-or-create uuid v4).
    pub installation_id: String,
    /// Bound proxy name ("direct" for explicit direct), None = ambient default.
    pub proxy: Option<String>,
    /// Per-account official HTTP client: one process = one account = one client upstream,
    /// so each account gets its own connection pool and Cloudflare cookie jar and no two
    /// account ids ever share a TLS/H2 connection. Built with the account's proxy binding
    /// injected into the env vars reqwest reads (see proxies::with_binding).
    pub transport: ReqwestTransport,
    /// Per-account official Statsig metrics client (exporter + periodic reader), built in
    /// the same binding swap as `transport` so its OTLP exports leave through the account's
    /// own proxy — the same exit IP as this account's conversation traffic.
    pub metrics: std::sync::Arc<codex_otel::MetricsClient>,
    cooldown_until_ms: AtomicU64,
    consecutive_failures: AtomicU64,
    /// Latest credential health verdict (see AuthStatus).
    auth_status: RwLock<AuthStatus>,
    /// Latest upstream quota snapshot (official rate_limits parse result, as JSON) for the admin UI.
    last_quotas: RwLock<Option<serde_json::Value>>,
}

/// Relay counterpart of the official resolve_installation_id: read
/// <account_dir>/installation_id, or generate + persist a uuid v4.
fn resolve_installation_id(account_dir: &Path) -> String {
    let path = account_dir.join("installation_id");
    if let Ok(text) = std::fs::read_to_string(&path) {
        let trimmed = text.trim();
        if !trimmed.is_empty()
            && let Ok(existing) = uuid::Uuid::parse_str(trimmed)
        {
            return existing.to_string();
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    if let Err(e) = std::fs::write(&path, &id) {
        tracing::warn!(path = %path.display(), error = %e, "installation_id 落盘失败，本次进程内使用临时值");
    }
    id
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Account {
    /// Selectable for upstream traffic: not cooling down and credentials not known-dead.
    pub fn available(&self) -> bool {
        now_millis() >= self.cooldown_until_ms.load(Ordering::Relaxed) && !self.auth_invalid()
    }

    pub fn cooldown_remaining(&self) -> Duration {
        let until = self.cooldown_until_ms.load(Ordering::Relaxed);
        Duration::from_millis(until.saturating_sub(now_millis()))
    }

    pub fn auth_status(&self) -> AuthStatus {
        self.auth_status.read().unwrap().clone()
    }

    pub fn auth_invalid(&self) -> bool {
        matches!(
            &*self.auth_status.read().unwrap(),
            AuthStatus::Invalid { .. }
        )
    }

    pub fn mark_auth_ok(&self) {
        *self.auth_status.write().unwrap() = AuthStatus::Ok;
    }

    pub fn mark_auth_invalid(&self, reason: impl Into<String>) {
        let reason = reason.into();
        tracing::warn!(account = %self.name, reason = %reason, "account credentials invalid, skipping until re-login");
        *self.auth_status.write().unwrap() = AuthStatus::Invalid {
            reason,
            since_unix: now_unix(),
        };
    }

    pub fn cool_down(&self, dur: Duration) {
        let until = now_millis() + dur.as_millis() as u64;
        self.cooldown_until_ms.store(until, Ordering::Relaxed);
        tracing::warn!(account = %self.name, cooldown_secs = dur.as_secs(), "account cooled down");
    }

    /// Exponential backoff on consecutive failures: 10s doubling, capped at 30min.
    pub fn backoff_and_cool_down(&self) {
        let n = self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
        let secs = 10u64.saturating_mul(1u64 << n.min(8)).min(1800);
        self.cool_down(Duration::from_secs(secs));
    }

    pub fn note_success(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
    }

    /// Stores a new quota snapshot. Header-derived snapshots (traffic path) never carry
    /// plan_type, while wham/usage ones do — merge so traffic doesn't erase the plan, and
    /// fold anything learned back into the account-level plan / period window.
    pub fn set_quotas(&self, quotas: serde_json::Value) {
        let mut quotas = quotas;
        let prev = self.last_quotas.read().unwrap().clone();
        merge_quota_plan(&mut quotas, prev.as_ref());
        if let Some(arr) = quotas.as_array() {
            if let Some(plan) = arr
                .iter()
                .find_map(|s| s.get("plan_type").and_then(serde_json::Value::as_str))
            {
                *self.plan.write().unwrap() = Some(plan.to_string());
            }
            let primary = arr
                .iter()
                .find(|s| s.get("limit_id").and_then(serde_json::Value::as_str) == Some("codex"))
                .or_else(|| arr.first())
                .and_then(|s| s.get("primary"));
            if let Some(w) = primary
                && let (Some(end), Some(mins)) = (
                    w.get("resets_at").and_then(serde_json::Value::as_u64),
                    w.get("window_minutes").and_then(serde_json::Value::as_u64),
                )
            {
                *self.period_window.write().unwrap() =
                    Some((end.saturating_sub(mins.saturating_mul(60)), end));
            }
        }
        *self.last_quotas.write().unwrap() = Some(quotas);
    }

    pub fn quotas(&self) -> Option<serde_json::Value> {
        self.last_quotas.read().unwrap().clone()
    }

    pub fn plan(&self) -> Option<String> {
        self.plan.read().unwrap().clone()
    }

    pub fn period_window(&self) -> Option<(u64, u64)> {
        *self.period_window.read().unwrap()
    }
}

/// Fills missing plan_type in the new snapshot array from the previous one (matched by
/// limit_id, falling back to the same index), so header-derived refreshes don't blank it.
fn merge_quota_plan(new: &mut serde_json::Value, prev: Option<&serde_json::Value>) {
    let (Some(new_arr), Some(old_arr)) = (new.as_array_mut(), prev.and_then(|p| p.as_array()))
    else {
        return;
    };
    for (i, snap) in new_arr.iter_mut().enumerate() {
        let has_plan = snap
            .get("plan_type")
            .and_then(serde_json::Value::as_str)
            .is_some();
        if has_plan {
            continue;
        }
        let limit_id = snap.get("limit_id").and_then(serde_json::Value::as_str);
        let old_plan = old_arr
            .iter()
            .find(|o| {
                limit_id.is_some()
                    && o.get("limit_id").and_then(serde_json::Value::as_str) == limit_id
            })
            .or_else(|| old_arr.get(i))
            .and_then(|o| o.get("plan_type"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        if let (Some(obj), Some(plan)) = (snap.as_object_mut(), old_plan) {
            obj.insert("plan_type".to_string(), serde_json::Value::String(plan));
        }
    }
}

struct PoolInner {
    accounts: Vec<Arc<Account>>,
    rr: AtomicUsize,
    sticky: RwLock<HashMap<String, (String, Instant)>>,
    sticky_ttl: Duration,
}

/// Cheap-to-clone pool handle: cloning copies an Arc; cooldown/sticky state stays shared.
#[derive(Clone)]
pub struct Pool {
    inner: Arc<PoolInner>,
}

impl Pool {
    pub async fn load(
        dir: &Path,
        route: &AuthRouteConfig,
        sticky_ttl: Duration,
        proxies: &crate::proxies::ProxyStore,
    ) -> anyhow::Result<Self> {
        let mut accounts = Vec::new();
        if dir.is_dir() {
            let mut entries: Vec<_> = std::fs::read_dir(dir)?
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_dir() && p.join("auth.json").exists())
                .collect();
            entries.sort();
            for path in entries {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| format!("account-{}", accounts.len()));
                let manager = Arc::new(
                    AuthManager::new(
                        path.clone(),
                        /*enable_codex_api_key_env*/ false,
                        AuthCredentialsStoreMode::File,
                        /*forced_chatgpt_workspace_id*/ None,
                        /*chatgpt_base_url*/ None,
                        AuthKeyringBackendKind::default(),
                        route.clone(),
                    )
                    .await,
                );
                let Some(auth) = manager.auth().await else {
                    tracing::warn!(account = %name, "auth.json 无效或无 ChatGPT 凭证，跳过");
                    continue;
                };
                if !auth.uses_codex_backend() {
                    tracing::warn!(account = %name, "不是 ChatGPT OAuth 凭证（API key 账号请直接用官方 API），跳过");
                    continue;
                }
                let account_id = auth.get_account_id();
                let account_email = auth.get_account_email();
                let plan = auth
                    .get_token_data()
                    .ok()
                    .and_then(|t| t.id_token.get_chatgpt_plan_type_raw());
                let installation_id = resolve_installation_id(&path);
                let provider = codex_model_provider::auth_provider_from_auth_manager(
                    Arc::clone(&manager),
                    &auth,
                );
                let binding = proxies.binding(&name);
                let proxy_label = match &binding {
                    crate::proxies::Binding::Default => None,
                    crate::proxies::Binding::Direct => {
                        Some(crate::proxies::DIRECT.to_string())
                    }
                    crate::proxies::Binding::Proxy { name, .. } => Some(name.clone()),
                };
                // Data plane and telemetry are built in one env swap: the account's metrics
                // exporter must egress through the account's own proxy, i.e. the same exit
                // IP as its conversation traffic (official: one process, one account, one
                // proxy for both channels).
                let (transport, metrics) = crate::proxies::with_binding(&binding, || {
                    (
                        ReqwestTransport::from_http_client(
                            codex_login::default_client::create_client(),
                        ),
                        crate::metrics::build_account_client(),
                    )
                })
                .await;
                tracing::info!(account = %name, account_id = ?account_id, email = ?account_email, plan = ?plan, proxy = ?proxy_label, "loaded account");
                accounts.push(Arc::new(Account {
                    name,
                    auth_manager: manager,
                    auth: provider,
                    account_id,
                    account_email,
                    plan: RwLock::new(plan),
                    period_window: RwLock::new(None),
                    installation_id,
                    proxy: proxy_label,
                    transport,
                    metrics,
                    cooldown_until_ms: AtomicU64::new(0),
                    consecutive_failures: AtomicU64::new(0),
                    auth_status: RwLock::new(AuthStatus::Unknown),
                    last_quotas: RwLock::new(None),
                }));
            }
        }
        if accounts.is_empty() {
            // Starting with an empty pool is allowed so the admin panel can bootstrap the
            // first account (device login hot-reloads the pool). Business endpoints will
            // report "all accounts unavailable" until one is added.
            tracing::warn!(
                dir = %dir.display(),
                "no accounts found (one subdirectory per account with auth.json); \
                 add one via `ccodex login` or the admin panel"
            );
        }
        Ok(Self {
            inner: Arc::new(PoolInner {
                accounts,
                rr: AtomicUsize::new(0),
                sticky: RwLock::new(HashMap::new()),
                sticky_ttl,
            }),
        })
    }

    pub fn len(&self) -> usize {
        self.inner.accounts.len()
    }

    pub fn accounts(&self) -> &[Arc<Account>] {
        &self.inner.accounts
    }

    /// Candidate ordering: sticky-session hit first, round-robin for the rest,
    /// cooling-down accounts last.
    pub fn ordered_candidates(&self, session_key: &str) -> Vec<Arc<Account>> {
        let inner = &*self.inner;
        let mut ordered: Vec<Arc<Account>> = Vec::with_capacity(inner.accounts.len());
        if !session_key.is_empty()
            && let Some((name, ts)) = inner.sticky.read().unwrap().get(session_key)
            && ts.elapsed() < inner.sticky_ttl
            && let Some(acc) = inner.accounts.iter().find(|a| &a.name == name)
        {
            ordered.push(Arc::clone(acc));
        }
        let start = inner.rr.fetch_add(1, Ordering::Relaxed);
        for i in 0..inner.accounts.len() {
            let acc = &inner.accounts[(start + i) % inner.accounts.len()];
            if !ordered.iter().any(|a| a.name == acc.name) {
                ordered.push(Arc::clone(acc));
            }
        }
        // Stable partition: available accounts first (a usable sticky hit stays first).
        ordered.sort_by_key(|a| !a.available());
        ordered
    }

    pub fn stick(&self, session_key: &str, account: &str) {
        if session_key.is_empty() {
            return;
        }
        self.inner.sticky.write().unwrap().insert(
            session_key.to_string(),
            (account.to_string(), Instant::now()),
        );
    }

    /// Time until the earliest recovery, when every account is cooling down.
    pub fn earliest_recovery(&self) -> Option<Duration> {
        self.inner
            .accounts
            .iter()
            .map(|a| a.cooldown_remaining())
            .min()
            .filter(|d| !d.is_zero())
    }
}

#[cfg(test)]
mod tests {
    use super::merge_quota_plan;
    use serde_json::json;

    #[test]
    fn plan_survives_header_snapshot() {
        let prev = json!([
            {"limit_id": "codex", "plan_type": "plus", "primary": null},
            {"limit_id": "codex_free", "plan_type": "free", "primary": null}
        ]);
        let mut new = json!([
            {"limit_id": "codex", "plan_type": null, "primary": {"used_percent": 3.0}},
            {"limit_id": "codex_free", "plan_type": null, "primary": null}
        ]);
        merge_quota_plan(&mut new, Some(&prev));
        assert_eq!(new[0]["plan_type"], json!("plus"));
        assert_eq!(new[1]["plan_type"], json!("free"));
        // Already-present plan wins over the old one.
        let mut newer = json!([{"limit_id": "codex", "plan_type": "pro", "primary": null}]);
        merge_quota_plan(&mut newer, Some(&new));
        assert_eq!(newer[0]["plan_type"], json!("pro"));
        // No previous data: untouched.
        let mut solo = json!([{"limit_id": "codex", "plan_type": null}]);
        merge_quota_plan(&mut solo, None);
        assert_eq!(solo[0]["plan_type"], json!(null));
    }
}
