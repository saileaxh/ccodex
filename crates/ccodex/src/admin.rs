//! Admin API: account status, hot reload, device-code login (for the embedded frontend).
//! Everything lives under /admin/api, authenticated by the panel login key (admin.json) —
//! deliberately NOT the downstream api_keys, so a leaked model key can't manage the relay
//! and the login key can't be spent as a model key.

use crate::accounts::Pool;
use crate::gateway::AppState;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde_json::{Value, json};
use std::sync::Arc;

fn admin_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(json!({ "error": { "type": "authentication_error", "message": message } })),
    )
        .into_response()
}

/// Panel auth: Bearer must be the login key from admin.json. Before first setup every
/// admin endpoint (except auth-status / admin-key setup) is refused with 401.
// Err carries a full Response (large) but only on the rare rejection path.
#[allow(clippy::result_large_err)]
pub(crate) fn authorize_admin(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    if !state.admin_auth.is_set() {
        return Err(admin_error(
            StatusCode::UNAUTHORIZED,
            "admin setup required: set a panel login key first",
        ));
    }
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or("");
    if state.admin_auth.verify(bearer) {
        Ok(())
    } else {
        Err(admin_error(StatusCode::UNAUTHORIZED, "invalid login key"))
    }
}

/// GET /admin/api/auth-status: open endpoint so the panel can choose setup vs login
/// screen. Leaks only whether a login key exists.
async fn handle_auth_status(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({
        "setup_required": !state.admin_auth.is_set(),
        "updated_at_unix": state.admin_auth.updated_at_unix(),
    }))
}

/// True when the candidate collides with any downstream model key — the login key and
/// model keys must stay disjoint.
fn collides_with_model_key(state: &AppState, candidate: &str) -> bool {
    state.keys.list().iter().any(|k| k.key == candidate)
}

/// POST /admin/api/admin-key {current?, new}: setup when unset (open, documented first-run
/// claim); otherwise requires admin auth plus the current key in the body.
async fn handle_admin_key(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, Response> {
    let new = body.get("new").and_then(Value::as_str).unwrap_or("");
    let setup = !state.admin_auth.is_set();
    if !setup {
        authorize_admin(&state, &headers)?;
        let current = body.get("current").and_then(Value::as_str).unwrap_or("");
        if !state.admin_auth.verify(current) {
            return Ok(Json(json!({ "ok": false, "error": "当前登录密钥不正确" })));
        }
    }
    if collides_with_model_key(&state, new.trim()) {
        return Ok(Json(
            json!({ "ok": false, "error": "登录密钥不能与任一访问密钥相同（两者必须不同）" }),
        ));
    }
    match state.admin_auth.set(new) {
        Ok(()) => Ok(Json(json!({ "ok": true, "setup": setup }))),
        Err(e) => Ok(Json(json!({ "ok": false, "error": e }))),
    }
}

/// GET /admin/api/pricing: merged pricing table (defaults + overrides).
async fn handle_pricing(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, Response> {
    authorize_admin(&state, &headers)?;
    let entries: Vec<Value> = state
        .pricing
        .list()
        .into_iter()
        .map(|(model, entry, is_override)| {
            json!({
                "model": model,
                "input_per_m": entry.input_per_m,
                "cached_input_per_m": entry.cached_input_per_m,
                "output_per_m": entry.output_per_m,
                "override": is_override,
            })
        })
        .collect();
    Ok(Json(json!({ "entries": entries })))
}

/// PUT /admin/api/pricing {model, input_per_m, cached_input_per_m, output_per_m}
async fn handle_pricing_set(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, Response> {
    authorize_admin(&state, &headers)?;
    let model = body.get("model").and_then(Value::as_str).unwrap_or("");
    let num = |field: &str| body.get(field).and_then(Value::as_f64);
    let (Some(input), Some(cached), Some(output)) = (
        num("input_per_m"),
        num("cached_input_per_m"),
        num("output_per_m"),
    ) else {
        return Ok(Json(
            json!({ "ok": false, "error": "需要 model + input_per_m/cached_input_per_m/output_per_m 数字" }),
        ));
    };
    let entry = crate::pricing::PriceEntry {
        input_per_m: input,
        cached_input_per_m: cached,
        output_per_m: output,
    };
    match state.pricing.set_override(model, entry) {
        Ok(()) => Ok(Json(json!({ "ok": true }))),
        Err(e) => Ok(Json(json!({ "ok": false, "error": e }))),
    }
}

/// DELETE /admin/api/pricing/{model}: removes an override, restoring the default.
async fn handle_pricing_remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(model): Path<String>,
) -> Result<Json<Value>, Response> {
    authorize_admin(&state, &headers)?;
    match state.pricing.remove_override(&model) {
        Ok(true) => Ok(Json(json!({ "ok": true }))),
        Ok(false) => Ok(Json(
            json!({ "ok": false, "error": format!("{model} 没有自定义覆盖") }),
        )),
        Err(e) => Ok(Json(json!({ "ok": false, "error": e }))),
    }
}

pub fn router(_state: Arc<AppState>) -> Router<Arc<AppState>> {
    // Returned without state; gateway merges it before the outer with_state.
    Router::new()
        .route("/admin/api/auth-status", get(handle_auth_status))
        .route("/admin/api/admin-key", post(handle_admin_key))
        .route("/admin/api/overview", get(handle_overview))
        .route("/admin/api/accounts", get(handle_accounts))
        .route("/admin/api/accounts/reload", post(handle_reload))
        .route(
            "/admin/api/accounts/{name}/proxy",
            put(handle_account_proxy),
        )
        .route(
            "/admin/api/accounts/{name}/quota-refresh",
            post(handle_quota_refresh),
        )
        .route(
            "/admin/api/accounts/device-login",
            post(handle_device_login_start),
        )
        .route(
            "/admin/api/accounts/device-login/{id}",
            get(handle_device_login_status),
        )
        .route(
            "/admin/api/accounts/oauth-login",
            post(handle_oauth_login_start),
        )
        .route(
            "/admin/api/accounts/oauth-login/{id}/complete",
            post(handle_oauth_login_complete),
        )
        .route(
            "/admin/api/proxies",
            get(handle_proxies).post(handle_proxy_add),
        )
        .route("/admin/api/keys", get(handle_keys).post(handle_key_add))
        .route("/admin/api/keys/{name}", delete(handle_key_remove))
        .route(
            "/admin/api/pricing",
            get(handle_pricing).put(handle_pricing_set),
        )
        .route("/admin/api/pricing/{model}", delete(handle_pricing_remove))
        .route("/admin/api/proxies/{name}", delete(handle_proxy_remove))
        .route("/admin/api/proxies/test", post(handle_proxy_test))
        .route("/admin/api/upstream-version", get(handle_upstream_version))
        .route(
            "/admin/api/upstream-version/refresh",
            post(handle_upstream_version_refresh),
        )
}

/// Latest-version cache TTL: the overview page refetches only when visiting past this age.
const UPSTREAM_VERSION_TTL_SECS: u64 = 4 * 3600;

/// Cached result of the "latest official codex version" check (npm registry, same source
/// the sync script resolves the baked version from).
#[derive(Default)]
pub struct UpstreamVersionCache {
    pub latest: Option<String>,
    pub checked_at_unix: u64,
    pub error: Option<String>,
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Queries the npm registry for the latest published @openai/codex version with a plain
/// diagnostic client (env-proxy aware; never touches the codex upstream API).
async fn fetch_latest_codex_version() -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get("https://registry.npmjs.org/@openai/codex/latest")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let body: Value = resp.json().await.map_err(|e| e.to_string())?;
    body.get("version")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "registry 响应缺少 version 字段".to_string())
}

fn upstream_version_json(cache: &UpstreamVersionCache, source: &str) -> Value {
    json!({
        "baked": crate::identity::codex_version(),
        "latest": cache.latest,
        "checked_at_unix": cache.checked_at_unix,
        "error": cache.error,
        "ttl_secs": UPSTREAM_VERSION_TTL_SECS,
        "source": source,
    })
}

/// Fetches and caches the latest version; on failure keeps the previous cached value.
async fn refresh_upstream_version(state: &AppState) -> Value {
    let result = fetch_latest_codex_version().await;
    let mut cache = state.upstream_version.lock().unwrap();
    match result {
        Ok(version) => {
            cache.latest = Some(version);
            cache.checked_at_unix = now_unix();
            cache.error = None;
        }
        Err(e) => {
            cache.error = Some(e);
        }
    }
    upstream_version_json(&cache, "fresh")
}

/// GET: serves the cache while fresh; refetches only when stale (the panel calls this on
/// every overview visit, so "every few hours on visit" falls out naturally).
async fn handle_upstream_version(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, Response> {
    authorize_admin(&state, &headers)?;
    {
        let cache = state.upstream_version.lock().unwrap();
        let fresh = cache.checked_at_unix > 0
            && now_unix().saturating_sub(cache.checked_at_unix) < UPSTREAM_VERSION_TTL_SECS;
        if fresh {
            return Ok(Json(upstream_version_json(&cache, "cache")));
        }
    }
    Ok(Json(refresh_upstream_version(&state).await))
}

/// POST .../refresh: manual force-refresh.
async fn handle_upstream_version_refresh(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, Response> {
    authorize_admin(&state, &headers)?;
    Ok(Json(refresh_upstream_version(&state).await))
}

/// Rebuilds the pool (re-reads account dirs and proxy bindings). Shared by the reload
/// endpoint, the device-login completion hook, and proxy-assignment changes.
async fn reload_pool(state: &AppState) -> anyhow::Result<usize> {
    let pool = Pool::load(
        &state.config.accounts_dir(),
        &state.route,
        state.config.sticky_ttl(),
        &state.proxies,
    )
    .await?;
    let count = pool.len();
    *state.pool.write().unwrap() = pool;
    Ok(count)
}

async fn handle_overview(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, Response> {
    authorize_admin(&state, &headers)?;
    let pool = state.pool.read().unwrap().clone();
    let models = state.models_cache.read().unwrap().clone();
    Ok(Json(json!({
        "accounts": pool.len(),
        "available": pool.accounts().iter().filter(|a| a.available()).count(),
        "identity_version": crate::identity::codex_version(),
        "upstream_commit": env!("CCODEX_UPSTREAM_COMMIT"),
        "listen": state.config.listen(),
        "auth_required": !state.keys.is_empty(),
        "models_cache": models.map(|m| json!({
            "live": m.live,
            "fetched_at_unix": m.fetched_at_unix,
            "bytes": m.body.len(),
        })),
    })))
}

async fn handle_accounts(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, Response> {
    authorize_admin(&state, &headers)?;
    let pool = state.pool.read().unwrap().clone();
    let accounts: Vec<Value> = pool
        .accounts()
        .iter()
        .map(|a| {
            let usage = state
                .usage
                .account_usage(&a.name)
                .map(|u| {
                    json!({
                        "total": u.totals,
                        "period": u.period,
                    })
                })
                .unwrap_or(Value::Null);
            json!({
                "name": a.name,
                "account_id": a.account_id,
                "email": a.account_email,
                "plan": a.plan(),
                "available": a.available(),
                "cooldown_remaining_secs": a.cooldown_remaining().as_secs(),
                "quotas": a.quotas(),
                "proxy": a.proxy,
                "usage": usage,
            })
        })
        .collect();
    Ok(Json(json!({ "accounts": accounts })))
}

async fn handle_reload(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, Response> {
    authorize_admin(&state, &headers)?;
    match reload_pool(&state).await {
        Ok(count) => {
            tracing::info!(accounts = count, "account pool reloaded");
            Ok(Json(json!({ "ok": true, "accounts": count })))
        }
        Err(e) => {
            tracing::warn!(error = %e, "account pool reload failed, keeping current pool");
            Ok(Json(json!({ "ok": false, "error": e.to_string() })))
        }
    }
}

/// GET /admin/api/proxies: pool entries (with last check), the assignment map, and the
/// ambient default from config (shown as a read-only row in the UI).
async fn handle_proxies(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, Response> {
    authorize_admin(&state, &headers)?;
    let (proxies, assignments) = state.proxies.snapshot();
    Ok(Json(json!({
        "config_default": state.config.upstream_proxy,
        "proxies": proxies,
        "assignments": assignments,
    })))
}

/// POST /admin/api/proxies {name, url}
async fn handle_proxy_add(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, Response> {
    authorize_admin(&state, &headers)?;
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let url = body.get("url").and_then(Value::as_str).unwrap_or("").trim();
    match state.proxies.add(name, url) {
        Ok(()) => Ok(Json(json!({ "ok": true }))),
        Err(e) => Ok(Json(json!({ "ok": false, "error": e }))),
    }
}

/// GET /admin/api/keys: managed keys shown in full (the panel is the lost-key recovery
/// path). Each row carries its usage totals (joined by key fingerprint).
async fn handle_keys(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, Response> {
    authorize_admin(&state, &headers)?;
    let keys: Vec<Value> = state
        .keys
        .list()
        .into_iter()
        .map(|k| {
            let usage = state
                .usage
                .key_usage(&crate::usage::key_fingerprint(&k.key))
                .map(|u| serde_json::to_value(u.totals).unwrap_or(Value::Null))
                .unwrap_or(Value::Null);
            json!({
                "name": k.name,
                "key": k.key,
                "created_at_unix": k.created_at_unix,
                "usage": usage,
            })
        })
        .collect();
    Ok(Json(json!({ "keys": keys })))
}

/// POST /admin/api/keys {name, key?}: key omitted = generate `sk-<96 hex>`.
async fn handle_key_add(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, Response> {
    authorize_admin(&state, &headers)?;
    let name = body.get("name").and_then(Value::as_str).unwrap_or("");
    let key = body.get("key").and_then(Value::as_str);
    // Keep the two credential kinds disjoint: the panel login key must not double as a
    // downstream model key.
    if let Some(k) = key
        && state.admin_auth.verify(k)
    {
        return Ok(Json(
            json!({ "ok": false, "error": "该值是面板登录密钥，不能用作访问密钥（两者必须不同）" }),
        ));
    }
    match state.keys.add(name, key) {
        Ok(k) => Ok(Json(json!({ "ok": true, "name": k.name, "key": k.key }))),
        Err(e) => Ok(Json(json!({ "ok": false, "error": e }))),
    }
}

/// DELETE /admin/api/keys/{name}: managed keys only. Panel login uses the separate login
/// key, so deleting any downstream key can never lock the operator out of the panel.
async fn handle_key_remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<Value>, Response> {
    authorize_admin(&state, &headers)?;
    match state.keys.remove(&name) {
        Ok(Some(_)) => Ok(Json(json!({ "ok": true }))),
        Ok(None) => Ok(Json(
            json!({ "ok": false, "error": format!("密钥 {name} 不存在（config.toml 里的 key 请改配置文件）") }),
        )),
        Err(e) => Ok(Json(json!({ "ok": false, "error": e }))),
    }
}

/// DELETE /admin/api/proxies/{name}
async fn handle_proxy_remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<Value>, Response> {
    authorize_admin(&state, &headers)?;
    match state.proxies.remove(&name) {
        Ok(true) => {
            // Bound accounts fall back to the default; rebuild clients so the change
            // takes effect immediately.
            let _ = reload_pool(&state).await;
            Ok(Json(json!({ "ok": true })))
        }
        Ok(false) => Ok(Json(
            json!({ "ok": false, "error": format!("代理 {name} 不存在") }),
        )),
        Err(e) => Ok(Json(json!({ "ok": false, "error": e }))),
    }
}

/// POST /admin/api/proxies/test {name?}: name omitted/null tests the ambient default
/// (config upstream_proxy), "direct" tests a direct connection.
async fn handle_proxy_test(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, Response> {
    authorize_admin(&state, &headers)?;
    let name = body.get("name").and_then(Value::as_str);
    let (url, store_as) = match name {
        None => (state.config.upstream_proxy.clone(), None),
        Some(crate::proxies::DIRECT) => (None, None),
        Some(n) => {
            let (proxies, _) = state.proxies.snapshot();
            let entry = proxies
                .iter()
                .find(|p| p.get("name").and_then(Value::as_str) == Some(n));
            match entry {
                Some(e) => (
                    e.get("url").and_then(Value::as_str).map(str::to_string),
                    Some(n.to_string()),
                ),
                None => {
                    return Ok(Json(
                        json!({ "ok": false, "error": format!("代理 {n} 不存在") }),
                    ));
                }
            }
        }
    };
    let check = crate::proxies::ProxyStore::check_proxy(url.as_deref()).await;
    if let Some(n) = store_as {
        state.proxies.set_check(&n, check.clone());
    }
    Ok(Json(
        serde_json::to_value(check).unwrap_or_else(|_| json!({"ok": false})),
    ))
}

/// PUT /admin/api/accounts/{name}/proxy {proxy: "name" | "direct" | null}
async fn handle_account_proxy(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, Response> {
    authorize_admin(&state, &headers)?;
    if !state
        .pool
        .read()
        .unwrap()
        .accounts()
        .iter()
        .any(|a| a.name == name)
    {
        return Ok(Json(
            json!({ "ok": false, "error": format!("账号 {name} 不存在") }),
        ));
    }
    let target = body.get("proxy").and_then(Value::as_str);
    if let Err(e) = state.proxies.assign(&name, target) {
        return Ok(Json(json!({ "ok": false, "error": e })));
    }
    match reload_pool(&state).await {
        Ok(count) => Ok(Json(json!({ "ok": true, "accounts": count }))),
        Err(e) => Ok(Json(json!({ "ok": false, "error": e.to_string() }))),
    }
}

/// POST /admin/api/accounts/{name}/quota-refresh: actively queries the official usage
/// endpoint (wham/usage) with the account's credentials, following its proxy binding.
/// The passive quota column is otherwise only populated by response traffic.
async fn handle_quota_refresh(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<Value>, Response> {
    authorize_admin(&state, &headers)?;
    let account = state
        .pool
        .read()
        .unwrap()
        .accounts()
        .iter()
        .find(|a| a.name == name)
        .cloned();
    let Some(account) = account else {
        return Ok(Json(
            json!({ "ok": false, "error": format!("账号 {name} 不存在") }),
        ));
    };
    let binding = state.proxies.binding(&name);
    let backend_base = crate::quota::backend_base_from_provider(&state.provider.base_url);
    match crate::quota::fetch_account_quotas(&account, &backend_base, binding).await {
        Ok(quotas) => Ok(Json(json!({ "ok": true, "quotas": quotas }))),
        Err(e) => Ok(Json(json!({ "ok": false, "error": e.to_string() }))),
    }
}

fn valid_account_name(body: &Value) -> Result<String, String> {
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if name.is_empty() || name.contains(['/', '\\', '.']) {
        return Err("账号名不能为空，且不能包含 / \\ . 字符".to_string());
    }
    Ok(name)
}

/// POST /admin/api/accounts/oauth-login {name}: browser OAuth flow (official codex login
/// shape). Returns the authorize URL; completion is the paste-back endpoint below.
async fn handle_oauth_login_start(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, Response> {
    authorize_admin(&state, &headers)?;
    let name = match valid_account_name(&body) {
        Ok(n) => n,
        Err(e) => return Ok(Json(json!({ "error": e }))),
    };
    let codex_home = state.config.accounts_dir().join(&name);
    let (authorize_url, pending) = crate::login::start_oauth_login(codex_home);
    let session_id = uuid::Uuid::new_v4().to_string();
    state
        .oauth_logins
        .lock()
        .unwrap()
        .insert(session_id.clone(), pending);
    Ok(Json(json!({
        "session_id": session_id,
        "authorize_url": authorize_url,
    })))
}

/// POST /admin/api/accounts/oauth-login/{id}/complete {redirect_url}: exchanges the pasted
/// callback URL via official code, persists auth.json, hot-reloads the pool.
async fn handle_oauth_login_complete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, Response> {
    authorize_admin(&state, &headers)?;
    let pasted = body
        .get("redirect_url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let pending = state.oauth_logins.lock().unwrap().remove(&id);
    let Some(pending) = pending else {
        return Ok(Json(
            json!({ "ok": false, "error": "会话不存在或已完成，请重新发起" }),
        ));
    };
    match crate::login::finish_oauth_login(&pending, &state.route, &pasted).await {
        Ok(()) => {
            let _ = reload_pool(&state).await;
            tracing::info!(session = %id, "oauth login finished, pool reloaded");
            Ok(Json(json!({ "ok": true })))
        }
        Err(e) => {
            // Let the user retry pasting (e.g. truncated copy) with the same session.
            state.oauth_logins.lock().unwrap().insert(id, pending);
            Ok(Json(json!({ "ok": false, "error": e.to_string() })))
        }
    }
}

/// Starts a device-code login: returns display info immediately; polling runs in a
/// background task.
async fn handle_device_login_start(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, Response> {
    authorize_admin(&state, &headers)?;
    let name = match valid_account_name(&body) {
        Ok(n) => n,
        Err(e) => return Ok(Json(json!({ "error": e }))),
    };
    let codex_home = state.config.accounts_dir().join(&name);

    let device = match crate::login::start_device_login(codex_home.clone(), &state.route).await {
        Ok(d) => d,
        Err(e) => return Ok(Json(json!({ "error": e.to_string() }))),
    };

    let session_id = uuid::Uuid::new_v4().to_string();
    state.logins.lock().unwrap().insert(
        session_id.clone(),
        crate::login::LoginSession {
            verification_url: device.verification_url.clone(),
            user_code: device.user_code.clone(),
            status: crate::login::LoginStatus::Pending,
        },
    );

    let task_state = Arc::clone(&state);
    let task_session = session_id.clone();
    let route = state.route.clone();
    tokio::spawn(async move {
        let result = crate::login::finish_device_login(codex_home, &route, device).await;
        let status = match result {
            Ok(()) => crate::login::LoginStatus::Done,
            Err(e) => crate::login::LoginStatus::Error(e.to_string()),
        };
        if let Some(session) = task_state.logins.lock().unwrap().get_mut(&task_session) {
            session.status = status;
        }
        // Hot-reload the pool once the login succeeds.
        if matches!(
            task_state
                .logins
                .lock()
                .unwrap()
                .get(&task_session)
                .map(|s| &s.status),
            Some(crate::login::LoginStatus::Done)
        ) && reload_pool(&task_state).await.is_ok()
        {
            tracing::info!(session = %task_session, "device login finished, pool reloaded");
        }
    });

    Ok(Json(json!({
        "session_id": session_id,
        "verification_url": state.logins.lock().unwrap().get(&session_id).map(|s| s.verification_url.clone()),
        "user_code": state.logins.lock().unwrap().get(&session_id).map(|s| s.user_code.clone()),
    })))
}

/// Polls device-login status.
async fn handle_device_login_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, Response> {
    authorize_admin(&state, &headers)?;
    let Some(session) = state.logins.lock().unwrap().get(&id).cloned() else {
        return Ok(Json(json!({ "status": "unknown" })));
    };
    let (status, error) = match &session.status {
        crate::login::LoginStatus::Pending => ("pending", None),
        crate::login::LoginStatus::Done => ("done", None),
        crate::login::LoginStatus::Error(e) => ("error", Some(e.clone())),
    };
    Ok(Json(json!({
        "status": status,
        "error": error,
        "verification_url": session.verification_url,
        "user_code": session.user_code,
    })))
}
