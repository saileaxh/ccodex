//! Downstream gateway: OpenAI Responses-compatible endpoints, Codex CLI direct aliases,
//! WebSocket, and the admin panel. The upstream path is entirely official crates;
//! downstream only authenticates, forwards bytes, and taps accounting.

use crate::accounts::Pool;
use crate::config::Config;
use crate::forward::{
    ForwardJsonResult, ForwardResult, RelayError, forward_compact, forward_responses,
    forward_search,
};
use crate::login::LoginSession;
use crate::model_db::ModelDb;
use crate::request_build::derive_session;
use crate::sse_tap::SseTap;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use codex_api::Provider;
use codex_http_client::HttpTransport;
use codex_login::AuthRouteConfig;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

pub struct AppState {
    pub config: Config,
    /// For hot reload: an RwLock around a cheap clone handle; readers clone a snapshot.
    pub pool: RwLock<Pool>,
    pub provider: Provider,
    pub model_db: ModelDb,
    /// HTTP route config for the official AuthManager (shared by refresh + device login).
    pub route: AuthRouteConfig,
    /// Upstream proxy pool (admin-managed, persisted next to the accounts dir). Shared
    /// with the telemetry channels so per-account sends can resolve the live binding.
    pub proxies: std::sync::Arc<crate::proxies::ProxyStore>,
    /// Managed downstream API keys (admin-managed, persisted next to the accounts dir).
    /// The only downstream credential source — config.toml no longer carries api_keys
    /// (legacy entries are migrated into the store at startup).
    pub keys: crate::keys::KeyStore,
    /// Panel login key, stored hashed in admin.json — separate from the downstream keys
    /// on purpose: neither credential kind authorizes the other's surface.
    pub admin_auth: crate::admin_auth::AdminAuth,
    /// Per-key / per-account usage + cost accounting (usage.json).
    pub usage: crate::usage::UsageStore,
    /// Model pricing table (defaults + admin overrides in pricing.json).
    pub pricing: crate::pricing::PricingStore,
    /// Latest official codex version check (TTL-cached; refreshed on overview visits).
    pub upstream_version: Mutex<crate::admin::UpstreamVersionCache>,
    /// In-flight device-login sessions (admin API).
    pub logins: Mutex<HashMap<String, LoginSession>>,
    /// In-flight OAuth paste-back login sessions (admin API).
    pub oauth_logins: Mutex<HashMap<String, crate::login::OAuthPending>>,
    /// Remembered mapping from (downstream session, account) to official v7
    /// session/thread ids.
    pub sessions: crate::request_build::SessionStore,
    /// Official-client telemetry: turn tracker + analytics events + Statsig metrics.
    pub telemetry: Arc<crate::telemetry::Telemetry>,
    /// Cached upstream model list (seeded from the embedded official models.json, refreshed
    /// in the background — the upstream payload is ~260KB and the official 5s interactive
    /// timeout is unworkable over high-latency proxy chains).
    pub models_cache: RwLock<Option<CachedModels>>,
}

#[derive(Clone)]
pub struct CachedModels {
    pub body: bytes::Bytes,
    pub fetched_at_unix: u64,
    /// true = live upstream bytes; false = embedded official models.json seed.
    pub live: bool,
}

/// Serves the cached model list instantly; the background task keeps it current.
/// Codex shape (`{"models":[…]}`, byte-identical upstream bytes) for official clients.
async fn handle_models_cached(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(resp) = authorize(&state, &headers) {
        return resp;
    }
    let cached = state.models_cache.read().unwrap().clone();
    match cached {
        Some(c) => {
            let mut resp = Response::new(Body::from(c.body));
            resp.headers_mut().insert(
                http::header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            resp
        }
        None => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "upstream_error",
            "models cache not initialized",
        ),
    }
}

/// GET /v1/models for generic OpenAI-compatible tooling (cc-switch etc.): the same cached
/// catalog reshaped to the OpenAI list format (`data[].id` = slug), `visibility:"hide"`
/// entries filtered out. Official Codex clients use the codex-shape routes above.
async fn handle_models_openai(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(resp) = authorize(&state, &headers) {
        return resp;
    }
    let cached = state.models_cache.read().unwrap().clone();
    let Some(c) = cached else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "upstream_error",
            "models cache not initialized",
        );
    };
    let parsed: Value = match serde_json::from_slice(&c.body) {
        Ok(v) => v,
        Err(e) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "upstream_error",
                &format!("cached model list is not valid JSON: {e}"),
            );
        }
    };
    let data: Vec<Value> = parsed
        .get("models")
        .and_then(Value::as_array)
        .map(|models| {
            models
                .iter()
                .filter(|m| m.get("visibility").and_then(Value::as_str) != Some("hide"))
                .filter_map(|m| m.get("slug").and_then(Value::as_str))
                .map(|slug| {
                    json!({
                        "id": slug,
                        "object": "model",
                        "created": 0,
                        "owned_by": "openai",
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let mut resp = Response::new(Body::from(
        serde_json::to_vec(&json!({ "object": "list", "data": data }))
            .unwrap_or_else(|_| b"{\"object\":\"list\",\"data\":[]}".to_vec()),
    ));
    resp.headers_mut().insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    resp
}

/// One models fetch with the official request shape.
async fn models_fetch_once(
    account: &crate::accounts::Account,
    provider: &codex_api::Provider,
    path: &str,
) -> anyhow::Result<codex_http_client::Response> {
    let req = provider.build_request(http::Method::GET, path);
    let req = account.auth.apply_auth(req).await?;
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        account.transport.execute(req),
    )
    .await
    .map_err(|_| anyhow::anyhow!("models fetch timed out (60s)"))?
    .map_err(|e| anyhow::anyhow!("models fetch failed: {e}"))
}

/// One background refresh: official ModelsClient request shape (`/models?client_version=…`,
/// account auth + transport), but with a bulk-transfer timeout instead of the official 5s
/// interactive one. Returns the model count on success.
///
/// This loop doubles as the account health probe after a restart (auth_status starts
/// Unknown): like drive_attempts it does one official 401→refresh→retry and marks the
/// credential ok/invalid accordingly, so the panel converges to the truth within seconds
/// of boot instead of waiting for the first /responses turn. (Proactive refresh does NOT
/// cover tokens invalidated server-side ahead of their exp claim — observed in prod.)
async fn refresh_models_once(state: &AppState) -> anyhow::Result<usize> {
    let account = state
        .pool
        .read()
        .unwrap()
        .ordered_candidates("")
        .into_iter()
        .find(|a| a.available())
        .ok_or_else(|| anyhow::anyhow!("no account available"))?;
    let path = format!(
        "/models?client_version={}",
        crate::identity::codex_version()
    );
    let mut resp = models_fetch_once(&account, &state.provider, &path).await?;
    if resp.status == http::StatusCode::UNAUTHORIZED {
        tracing::warn!(account = %account.name, "models refresh got 401, refreshing token and retrying once");
        match account.auth_manager.refresh_token().await {
            Ok(_) => {
                resp = models_fetch_once(&account, &state.provider, &path).await?;
                if resp.status == http::StatusCode::UNAUTHORIZED {
                    account.mark_auth_invalid(
                        "token 刷新成功但上游仍返回 401，凭证已失效".to_string(),
                    );
                }
            }
            Err(e) => {
                if matches!(e, codex_login::auth::RefreshTokenError::Permanent(_)) {
                    account.mark_auth_invalid(format!("凭证刷新被上游永久拒绝: {e}"));
                } else {
                    tracing::warn!(account = %account.name, error = %e, "models-path token refresh failed (transient)");
                }
            }
        }
    }
    if !resp.status.is_success() {
        anyhow::bail!("models fetch rejected: upstream status {}", resp.status);
    }
    account.mark_auth_ok();
    let count = serde_json::from_slice::<Value>(&resp.body)
        .ok()
        .and_then(|v| v.get("models").and_then(Value::as_array).map(|a| a.len()))
        .unwrap_or(0);
    let fetched_at_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    *state.models_cache.write().unwrap() = Some(CachedModels {
        body: resp.body,
        fetched_at_unix,
        live: true,
    });
    Ok(count)
}

/// Hourly refresh loop (5min retry after failure). Runs only when accounts exist.
pub fn spawn_models_refresh(state: Arc<AppState>) {
    tokio::spawn(async move {
        loop {
            let wait = match refresh_models_once(&state).await {
                Ok(n) => {
                    tracing::info!(models = n, "model list refreshed from upstream");
                    std::time::Duration::from_secs(3600)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "model list refresh failed, keeping cache");
                    std::time::Duration::from_secs(300)
                }
            };
            tokio::time::sleep(wait).await;
        }
    });
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        // HTTP (POST) and WS (GET upgrade) share the same paths, matching the official WS endpoint.
        .route(
            "/v1/responses",
            post(handle_responses).get(crate::ws::handle_responses_ws),
        )
        .route("/responses", post(handle_responses))
        .route(
            "/backend-api/codex/responses",
            post(handle_responses).get(crate::ws::handle_responses_ws),
        )
        .route("/alpha/search", post(handle_search))
        .route("/v1/alpha/search", post(handle_search))
        .route("/backend-api/codex/alpha/search", post(handle_search))
        .route("/responses/compact", post(handle_compact))
        .route("/v1/responses/compact", post(handle_compact))
        .route("/backend-api/codex/responses/compact", post(handle_compact))
        .route("/models", get(handle_models_cached))
        .route("/v1/models", get(handle_models_openai))
        .route("/backend-api/codex/models", get(handle_models_cached))
        .route("/health", get(handle_health))
        .merge(crate::admin::router(Arc::clone(&state)))
        .fallback(crate::web::static_handler)
        .with_state(state)
}

async fn handle_health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(json!({
        "status": "ok",
        "accounts": state.pool.read().unwrap().len(),
        "upstream_commit": env!("CCODEX_UPSTREAM_COMMIT"),
        "identity_version": crate::identity::codex_version(),
    }))
}

/// Downstream auth: `Authorization: Bearer <key>`. Valid = managed keys (keys.json);
/// open mode only when the store is completely empty (local mode). The panel login key
/// is deliberately not valid here.
// Err carries a full Response (large) but only on the rare rejection path.
#[allow(clippy::result_large_err)]
pub(crate) fn authorize(state: &AppState, headers: &HeaderMap) -> Result<String, Response> {
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or("")
        .to_string();
    if state.keys.is_empty() {
        return Ok(bearer);
    }
    if state.keys.contains(&bearer) {
        Ok(bearer)
    } else {
        Err(error_response(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "invalid api key",
        ))
    }
}

fn error_response(status: StatusCode, kind: &str, message: &str) -> Response {
    (
        status,
        Json(json!({
            "error": { "type": kind, "message": message }
        })),
    )
        .into_response()
}

/// Response headers forwarded downstream: keep SSE type + upstream quota/trace info,
/// drop hop-by-hop headers.
fn downstream_headers(upstream: &HeaderMap) -> HeaderMap {
    const HOP_BY_HOP: &[&str] = &[
        "content-length",
        "transfer-encoding",
        "connection",
        "keep-alive",
        "upgrade",
        "content-encoding",
    ];
    let mut out = HeaderMap::new();
    for (name, value) in upstream.iter() {
        let lower = name.as_str().to_ascii_lowercase();
        if HOP_BY_HOP.contains(&lower.as_str()) {
            continue;
        }
        out.insert(name.clone(), value.clone());
    }
    out
}

/// The downstream client's CompactionTurnMetadata block, lifted out of its
/// x-codex-turn-metadata header. Official clients attach it to every compaction request
/// (v2 trigger over /responses and legacy /responses/compact alike); it is operation
/// telemetry (trigger/reason/implementation/phase/strategy), not identity, so it is
/// copied verbatim into the rebuilt per-account turn metadata.
fn downstream_compaction_meta(headers: &HeaderMap) -> Option<Value> {
    let raw = headers.get("x-codex-turn-metadata")?.to_str().ok()?;
    let meta: Value = serde_json::from_str(raw).ok()?;
    meta.get("compaction").filter(|c| c.is_object()).cloned()
}

async fn handle_responses(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let api_key = match authorize(&state, &headers) {
        Ok(key) => key,
        Err(resp) => return resp,
    };

    let payload: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("invalid JSON body: {e}"),
            );
        }
    };

    let session = derive_session(
        &api_key,
        &payload,
        &headers,
        state.config.session_isolation(),
        &state.sessions,
    );
    // Captured for usage/cost attribution once the stream completes.
    let requested_model = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // The body is rebuilt inside forward per the official construction logic (with
    // per-account installation_id on switch); downstream fields are only semantically
    // extracted, never passed through.

    let pool = state.pool.read().unwrap().clone();
    let result = forward_responses(
        &pool,
        &state.provider,
        &payload,
        &session,
        &state.model_db,
        &state.sessions,
        state.config.max_account_switches(),
        state.config.request_compression(),
        downstream_compaction_meta(&headers).as_ref(),
        &state.telemetry,
    )
    .await;

    match result {
        Ok(ForwardResult::Stream(stream)) => {
            let upstream = *stream;
            let account = upstream.account_name.clone();
            tracing::info!(account = %account, status = %upstream.status, "streaming from upstream");

            let mut tap = SseTap::new();
            let tap_account = account.clone();
            // 该账号自己的 metrics client：SSE 指标必须记进它自己的出口（与 analytics 同账号）
            let tap_metrics = {
                let pool = state.pool.read().unwrap();
                state.telemetry.metrics_for_name(&pool, &tap_account)
            };
            let tap_state = Arc::clone(&state);
            let tap_model = requested_model.clone();
            // Open mode (no downstream keys configured) has no bearer: skip per-key rows,
            // still keep per-account accounting.
            let tap_key = if api_key.is_empty() {
                None
            } else {
                Some((
                    crate::usage::key_fingerprint(&api_key),
                    crate::keys::KeyStore::mask(&api_key),
                ))
            };
            let telem_session = upstream.session.clone();
            let telem = upstream.telem;
            // The downstream's own compaction block (real trigger/reason/phase) wins;
            // only absent metadata falls back to the official default block.
            let compaction_block = downstream_compaction_meta(&headers).unwrap_or_else(|| {
                crate::request_build::default_compaction_metadata("responses_compaction_v2")
            });
            let byte_stream = async_stream::stream! {
                let mut bytes = upstream.bytes;
                use futures::StreamExt;
                while let Some(chunk) = bytes.next().await {
                    match chunk {
                        Ok(b) => {
                            tap.feed(&b);
                            // Official codex.sse_event samples, emitted live per event.
                            let events = std::mem::take(&mut tap.metric_events);
                            if let Some(metrics) = &tap_metrics {
                                tap_state.telemetry.metrics.sse_events(metrics, &tap_model, &events);
                            }
                            yield Ok::<_, codex_http_client::TransportError>(b);
                        }
                        Err(e) => {
                            tracing::warn!(account = %tap_account, error = %e, "upstream stream error");
                            tap.note_stream_error();
                            let events = std::mem::take(&mut tap.metric_events);
                            if let Some(metrics) = &tap_metrics {
                                tap_state.telemetry.metrics.sse_events(metrics, &tap_model, &events);
                            }
                            yield Err(e);
                        }
                    }
                }
                // Stream tail: turn-tracker finalization first (borrows tap.usage),
                // then usage/cost accounting (takes it).
                let pool_snapshot = tap_state.pool.read().unwrap().clone();
                match telem {
                    crate::forward::AttemptTelem::Turn { session_key, .. } => {
                        let emissions = tap_state.telemetry.tracker.note_response_end(
                            &session_key,
                            &tap_account,
                            &tap,
                        );
                        tap_state.telemetry.emit_for_account_name(
                            &pool_snapshot,
                            &tap_account,
                            emissions,
                        );
                    }
                    crate::forward::AttemptTelem::Compaction(start) => {
                        // v2 compaction over /responses: close out with the response usage.
                        let (status, failure) = if tap.completed {
                            ("completed", None)
                        } else {
                            (
                                "failed",
                                Some(crate::turns::failure_from_stream_error(tap.failed.as_ref())),
                            )
                        };
                        let usage = tap
                            .usage
                            .as_ref()
                            .map(crate::turns::TokenAccum::from_wire);
                        let emissions = tap_state.telemetry.tracker.note_compaction_end(
                            &telem_session,
                            &tap_account,
                            &start,
                            &compaction_block,
                            "responses_compaction_v2",
                            &tap_model,
                            crate::turns::CompactionOutcome {
                                status,
                                failure,
                                usage,
                            },
                        );
                        tap_state.telemetry.emit_for_account_name(
                            &pool_snapshot,
                            &tap_account,
                            emissions,
                        );
                    }
                    crate::forward::AttemptTelem::None => {}
                }
                if let Some(usage) = tap.usage.take() {
                    let tokens = crate::usage::parse_usage_tokens(&usage);
                    let cost = tap_state.pricing.cost_usd(
                        &tap_model,
                        tokens.0.saturating_sub(tokens.1),
                        tokens.1,
                        tokens.2,
                    );
                    let window = tap_state
                        .pool
                        .read()
                        .unwrap()
                        .accounts()
                        .iter()
                        .find(|a| a.name == tap_account)
                        .and_then(|a| a.period_window());
                    tap_state
                        .usage
                        .record(tap_key, &tap_account, window, tokens, cost);
                }
                tracing::info!(
                    account = %tap_account,
                    completed = tap.completed,
                    usage = ?tap.usage,
                    failed = ?tap.failed,
                    "turn finished"
                );
            };

            let mut response = Response::new(Body::from_stream(byte_stream));
            *response.status_mut() = upstream.status;
            *response.headers_mut() = downstream_headers(&upstream.headers);
            response
        }
        Ok(ForwardResult::Rejected {
            status,
            headers,
            body,
        }) => {
            tracing::warn!(status = %status, "upstream rejected request");
            let mut response = Response::new(Body::from(body));
            *response.status_mut() = status;
            *response.headers_mut() = downstream_headers(&headers);
            response
        }
        Err(err) => relay_error_response(err),
    }
}

/// Shared RelayError → downstream response mapping (mirrors official client-visible semantics).
fn relay_error_response(err: RelayError) -> Response {
    match err {
        RelayError::AllAccountsUnavailable { retry_after } => {
            let mut resp = error_response(
                StatusCode::TOO_MANY_REQUESTS,
                "model_cooldown",
                "all upstream accounts are cooling down",
            );
            if let Some(wait) = retry_after {
                let secs = wait.as_secs().max(1).to_string();
                if let Ok(v) = HeaderValue::from_str(&secs) {
                    resp.headers_mut()
                        .insert(axum::http::header::RETRY_AFTER, v);
                }
            }
            resp
        }
        RelayError::AllAccountsInvalid => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "accounts_invalid",
            "all upstream account credentials were revoked; re-login or remove them in the admin panel",
        ),
        RelayError::BadRequest(message) => {
            error_response(StatusCode::BAD_REQUEST, "invalid_request_error", &message)
        }
    }
}

/// POST {,/v1,/backend-api/codex}/alpha/search: the official standalone-web-search
/// executor channel. A downstream official client executes the model's web.run calls
/// through here (SearchClient → {provider}/alpha/search). Same auth, session stickiness
/// (seed = body `id`) and per-account identity rules as /responses; plain JSON mirrored.
async fn handle_search(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let api_key = match authorize(&state, &headers) {
        Ok(key) => key,
        Err(resp) => return resp,
    };

    let payload: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("invalid JSON body: {e}"),
            );
        }
    };
    if !payload.is_object() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "search request body must be a JSON object",
        );
    }

    let session = derive_session(
        &api_key,
        &payload,
        &headers,
        state.config.session_isolation(),
        &state.sessions,
    );

    let pool = state.pool.read().unwrap().clone();
    let result = forward_search(
        &pool,
        &state.provider,
        &payload,
        &session,
        &state.model_db,
        &state.sessions,
        state.config.max_account_switches(),
    )
    .await;

    match result {
        Ok(ForwardJsonResult::Json(json)) => {
            let upstream = *json;
            tracing::info!(account = %upstream.account_name, status = %upstream.status, "search upstream response");
            let mut response = Response::new(Body::from(upstream.body));
            *response.status_mut() = upstream.status;
            *response.headers_mut() = downstream_headers(&upstream.headers);
            response
        }
        Ok(ForwardJsonResult::Rejected {
            status,
            headers,
            body,
        }) => {
            tracing::warn!(status = %status, "search upstream rejected request");
            let mut response = Response::new(Body::from(body));
            *response.status_mut() = status;
            *response.headers_mut() = downstream_headers(&headers);
            response
        }
        Err(err) => relay_error_response(err),
    }
}

/// POST {,/v1,/backend-api/codex}/responses/compact: the legacy remote-compaction channel
/// (codex-api CompactClient). Official 0.153.4 defaults to compaction v2 (a
/// compaction_trigger item over plain /responses — already handled by handle_responses),
/// but clients with `features.remote_compaction_v2 = false` still POST here. Unary JSON
/// in/out; same auth, session stickiness and per-account identity rules as /responses.
async fn handle_compact(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let api_key = match authorize(&state, &headers) {
        Ok(key) => key,
        Err(resp) => return resp,
    };

    let payload: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("invalid JSON body: {e}"),
            );
        }
    };
    if !payload.is_object() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "compact request body must be a JSON object",
        );
    }

    let session = derive_session(
        &api_key,
        &payload,
        &headers,
        state.config.session_isolation(),
        &state.sessions,
    );

    let pool = state.pool.read().unwrap().clone();
    let result = forward_compact(
        &pool,
        &state.provider,
        &payload,
        &session,
        &state.model_db,
        &state.sessions,
        state.config.max_account_switches(),
        downstream_compaction_meta(&headers).as_ref(),
        &state.telemetry,
    )
    .await;

    match result {
        Ok(ForwardJsonResult::Json(json)) => {
            let upstream = *json;
            tracing::info!(account = %upstream.account_name, status = %upstream.status, "compact upstream response");
            let mut response = Response::new(Body::from(upstream.body));
            *response.status_mut() = upstream.status;
            *response.headers_mut() = downstream_headers(&upstream.headers);
            response
        }
        Ok(ForwardJsonResult::Rejected {
            status,
            headers,
            body,
        }) => {
            tracing::warn!(status = %status, "compact upstream rejected request");
            let mut response = Response::new(Body::from(body));
            *response.status_mut() = status;
            *response.headers_mut() = downstream_headers(&headers);
            response
        }
        Err(err) => relay_error_response(err),
    }
}
