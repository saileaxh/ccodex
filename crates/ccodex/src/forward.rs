//! Upstream forwarding over the official request path only:
//! Provider::build_request (URL + version header)
//!   → session headers (session-id / thread-id / x-client-request-id, same as ResponsesClient)
//!   → officially constructed body + feature headers (request_build; zero downstream
//!     field/header passthrough)
//!   → Accept: text/event-stream (same as stream_encoded)
//!   → RequestCompression::Zstd (official enable_request_compression default)
//!   → AuthProvider::apply_auth (official headers: Bearer + ChatGPT-Account-ID)
//!   → per-account ReqwestTransport (official HttpClient: originator + UA + Cloudflare
//!     cookies + proxy; one client per account, so accounts never share a connection)
//! Returns the raw SSE byte stream for byte-identical downstream forwarding.
//!
//! Telemetry: each attempt is classified by the turn tracker (turn boundary
//! reconstruction — the wire turn_id is the per-turn id, reused across samplings of a
//! turn, matching official client behavior); upstream-visible failures are reported to
//! the tracker per account; compaction requests carry a CompactionStart guard through
//! to the response end.

use crate::accounts::{Account, Pool};
use crate::model_db::ModelDb;
use crate::request_build::{SessionCtx, SessionStore, build_upstream_request};
use crate::telemetry::Telemetry;
use crate::turns::{
    AttemptStart, CompactionOutcome, CompactionStart, TransportFailureKind, TurnFailure,
    failure_from_http_reject, failure_from_transport, has_compaction_trigger,
};
use bytes::Bytes;
use codex_api::Provider;
use codex_http_client::{
    ByteStream, HttpTransport, Request, RequestBody, RequestCompression, ReqwestTransport,
};
use futures::StreamExt;
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use std::time::Duration;

/// Per-attempt telemetry handle, produced by prepare and carried to the response end.
pub enum AttemptTelem {
    /// Not a tracked endpoint (alpha/search).
    None,
    /// A regular /responses sampling of a tracked turn. `session_key` is the tracker
    /// key for this attempt: the downstream-derived key, or a per-request key the
    /// tracker minted for a keyless request.
    Turn { turn_id: String, session_key: String },
    /// A compaction request (v2 trigger over /responses or legacy /responses/compact).
    Compaction(CompactionStart),
}

pub struct UpstreamStream {
    pub account_name: String,
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub bytes: ByteStream,
    /// Per-account session the attempt ran under (telemetry event ids).
    pub session: SessionCtx,
    pub telem: AttemptTelem,
}

/// Buffered non-SSE upstream response (e.g. the alpha/search endpoint, plain JSON).
pub struct UpstreamJson {
    pub account_name: String,
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub session: SessionCtx,
    pub telem: AttemptTelem,
}

pub enum ForwardResult {
    Stream(Box<UpstreamStream>),
    /// Upstream rejected the request itself (4xx); mirrored downstream as-is.
    Rejected {
        status: StatusCode,
        headers: HeaderMap,
        body: Bytes,
    },
}

pub enum ForwardJsonResult {
    Json(Box<UpstreamJson>),
    /// Upstream rejected the request itself (4xx); mirrored downstream as-is.
    Rejected {
        status: StatusCode,
        headers: HeaderMap,
        body: Bytes,
    },
}

#[derive(Debug)]
pub enum RelayError {
    /// All accounts unavailable (cooldown / failures).
    AllAccountsUnavailable { retry_after: Option<Duration> },
    /// Every account's credentials are known-dead (upstream revoked them); retrying is
    /// pointless until a re-login happens in the admin panel.
    AllAccountsInvalid,
    /// Downstream request lacks required semantic fields (400 to downstream; not upstream-visible).
    BadRequest(String),
}

enum Attempt {
    Stream(UpstreamStream),
    Json(UpstreamJson),
    HttpReject(StatusCode, HeaderMap, Bytes),
    Transport(TransportFailureKind, String),
}

/// What a 2xx upstream response means for the caller: SSE passthrough vs buffered JSON.
#[derive(Clone, Copy, PartialEq)]
enum SuccessMode {
    Stream,
    Json,
}

/// Session headers (the official endpoint::responses trio) + officially built feature headers.
fn attempt_headers(session: &SessionCtx, built_headers: &HeaderMap) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let session_id = session.session_id.to_string();
    let thread_id = session.thread_id.to_string();
    if let Ok(v) = HeaderValue::from_str(&session_id) {
        headers.insert("session-id", v);
    }
    if let Ok(v) = HeaderValue::from_str(&thread_id) {
        headers.insert("thread-id", v.clone());
        headers.insert("x-client-request-id", v);
    }
    headers.extend(built_headers.clone());
    // Same as official ResponsesClient::stream_encoded.
    headers.insert(
        http::header::ACCEPT,
        HeaderValue::from_static("text/event-stream"),
    );
    headers
}

async fn collect_body(mut stream: ByteStream) -> Bytes {
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => buf.extend_from_slice(&bytes),
            Err(_) => break,
        }
    }
    Bytes::from(buf)
}

fn classify_transport(err: &codex_http_client::TransportError) -> TransportFailureKind {
    match err {
        codex_http_client::TransportError::Timeout => TransportFailureKind::Timeout,
        codex_http_client::TransportError::Connection(_) => TransportFailureKind::Connection,
        _ => TransportFailureKind::Other,
    }
}

#[allow(clippy::too_many_arguments)]
async fn try_once(
    account: &Account,
    provider: &Provider,
    transport: &ReqwestTransport,
    path: &str,
    body: &serde_json::Value,
    extra_headers: &HeaderMap,
    compression: bool,
    mode: SuccessMode,
    session: &SessionCtx,
    telem: AttemptTelem,
) -> (Attempt, Option<AttemptTelem>) {
    let mut req: Request = provider.build_request(Method::POST, path);
    req.headers.extend(extra_headers.clone());
    req.body = Some(RequestBody::Json(body.clone()));
    req.compression = if compression {
        RequestCompression::Zstd
    } else {
        RequestCompression::None
    };

    let req = match account.auth.apply_auth(req).await {
        Ok(req) => req,
        Err(e) => {
            tracing::warn!(account = %account.name, error = %e, "auth header resolution failed");
            return (
                Attempt::Transport(TransportFailureKind::Other, format!("auth: {e}")),
                Some(telem),
            );
        }
    };

    match transport.stream(req).await {
        Ok(resp) if resp.status.is_success() => match mode {
            SuccessMode::Stream => (
                Attempt::Stream(UpstreamStream {
                    account_name: account.name.clone(),
                    status: resp.status,
                    headers: resp.headers,
                    bytes: resp.bytes,
                    session: session.clone(),
                    telem,
                }),
                None,
            ),
            SuccessMode::Json => {
                let status = resp.status;
                let headers = resp.headers.clone();
                let body = collect_body(resp.bytes).await;
                (
                    Attempt::Json(UpstreamJson {
                        account_name: account.name.clone(),
                        status,
                        headers,
                        body,
                        session: session.clone(),
                        telem,
                    }),
                    None,
                )
            }
        },
        Ok(resp) => {
            let status = resp.status;
            let headers = resp.headers.clone();
            let body = collect_body(resp.bytes).await;
            (
                Attempt::HttpReject(status, headers, body),
                Some(telem),
            )
        }
        // The official stream() maps every non-2xx to TransportError::Http carrying
        // status/headers/body — recover it as a real upstream rejection so 401/429/4xx
        // handling below actually runs (stringifying it would misclassify everything
        // as a transport failure and cool down healthy accounts).
        Err(codex_http_client::TransportError::Http {
            status,
            headers,
            body,
            ..
        }) => (
            Attempt::HttpReject(
                status,
                headers.unwrap_or_default(),
                Bytes::from(body.unwrap_or_default()),
            ),
            Some(telem),
        ),
        Err(e) => (
            Attempt::Transport(classify_transport(&e), e.to_string()),
            Some(telem),
        ),
    }
}

fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get(http::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// The shared account loop: sticky/round-robin ordering, per-account session ids, one
/// official 401→refresh→retry (UnauthorizedRecovery), credential-death marking, cooldown
/// and backoff classification, quota snapshots on success. Endpoint specifics live in the
/// prepare closure (officially-shaped body + headers per attempt) and the SuccessMode.
/// on_failure reports each attempt's upstream-visible failure to the turn tracker.
#[allow(clippy::too_many_arguments)]
async fn drive_attempts<F, G>(
    pool: &Pool,
    provider: &Provider,
    session: &SessionCtx,
    store: &SessionStore,
    max_account_switches: usize,
    compression: bool,
    mode: SuccessMode,
    path: &str,
    prepare: F,
    on_failure: G,
) -> Result<Attempt, RelayError>
where
    F: Fn(&Account, &SessionCtx) -> Result<(serde_json::Value, HeaderMap, AttemptTelem), RelayError>,
    G: Fn(&Account, &SessionCtx, &AttemptTelem, TurnFailure),
{
    let candidates = pool.ordered_candidates(&session.key);

    let mut switches = 0usize;
    let mut last_reject: Option<(StatusCode, HeaderMap, Bytes)> = None;

    for account in &candidates {
        if switches > max_account_switches {
            break;
        }
        if !account.available() {
            continue;
        }
        switches += 1;

        // Per-account session and turn identifiers: a downstream session appears as an
        // independent upstream session under each account (ids remembered per pair), and
        // every account attempt is a fresh turn. An official session/turn id never shows
        // up under two accounts — reusing them would correlate the accounts upstream.
        // (The same-account 401 refresh retry below reuses this attempt's ids, matching
        // official UnauthorizedRecovery.)
        let attempt_session = session.for_account(store, &account.name);
        let (body, extra_headers, telem) = prepare(account, &attempt_session)?;

        let (mut attempt, mut telem) = try_once(
            account,
            provider,
            &account.transport,
            path,
            &body,
            &extra_headers,
            compression,
            mode,
            &attempt_session,
            telem,
        )
        .await;

        // 401: refresh credentials via the official path, retry once on the same account
        // (matches official UnauthorizedRecovery). The turn continues across the
        // refresh+retry; only a permanent refresh failure changes the failure mapping
        // (official: CodexErr::RefreshTokenFailed, not the raw 401).
        let mut refreshed = false;
        let mut failure_override: Option<TurnFailure> = None;
        if matches!(&attempt, Attempt::HttpReject(status, _, _) if *status == StatusCode::UNAUTHORIZED)
        {
            tracing::warn!(account = %account.name, "401 from upstream, refreshing token and retrying once");
            match account.auth_manager.refresh_token().await {
                Ok(()) => {
                    refreshed = true;
                    let prev = telem
                        .take()
                        .expect("telem is returned for the retried attempt");
                    let (next_attempt, next_telem) = try_once(
                        account,
                        provider,
                        &account.transport,
                        path,
                        &body,
                        &extra_headers,
                        compression,
                        mode,
                        &attempt_session,
                        prev,
                    )
                    .await;
                    attempt = next_attempt;
                    telem = next_telem;
                }
                Err(e) => {
                    // Official classification: Permanent = the credential itself is dead
                    // (revoked/expired refresh token); Transient = network etc.
                    if matches!(e, codex_login::auth::RefreshTokenError::Permanent(_)) {
                        account.mark_auth_invalid(format!("凭证刷新被上游永久拒绝: {e}"));
                        failure_override = Some(TurnFailure {
                            kind: "refresh_token_failed",
                            http_status: None,
                            info: serde_json::Value::String("unauthorized".to_string()),
                        });
                    } else {
                        tracing::warn!(account = %account.name, error = %e, "token refresh failed (transient)");
                    }
                }
            }
        }

        match attempt {
            success @ (Attempt::Stream(_) | Attempt::Json(..)) => {
                account.note_success();
                account.mark_auth_ok();
                pool.stick(&session.key, &account.name);
                // Official rate-limit header parsing (exported by the export-rate-limits-module patch).
                let headers = match &success {
                    Attempt::Stream(s) => &s.headers,
                    Attempt::Json(j) => &j.headers,
                    _ => unreachable!(),
                };
                let quotas = codex_api::rate_limits::parse_all_rate_limits(headers);
                if !quotas.is_empty() {
                    tracing::info!(account = %account.name, quotas = ?quotas, "upstream rate-limit snapshot");
                    if let Ok(value) = serde_json::to_value(&quotas) {
                        account.set_quotas(value);
                    }
                }
                return Ok(success);
            }
            Attempt::HttpReject(status, headers, body_bytes) => {
                if let Some(telem) = telem.as_ref() {
                    let failure = failure_override.unwrap_or_else(|| {
                        failure_from_http_reject(status.as_u16(), &body_bytes)
                    });
                    on_failure(account, &attempt_session, telem, failure);
                }
                match status.as_u16() {
                    401 => {
                        // Only a *successful* refresh followed by another 401 proves the
                        // credentials dead; a transient refresh failure keeps the original
                        // 401 here and must not condemn the account.
                        if refreshed {
                            tracing::warn!(account = %account.name, "still 401 after refresh, credentials invalid");
                            account.mark_auth_invalid(
                                "token 刷新成功但上游仍返回 401，凭证已失效".to_string(),
                            );
                        } else {
                            tracing::warn!(account = %account.name, "still 401 after refresh, cooling down 5m");
                        }
                        account.cool_down(Duration::from_secs(300));
                        last_reject = Some((status, headers, body_bytes));
                    }
                    429 => {
                        let wait = parse_retry_after(&headers);
                        account.cool_down(wait.unwrap_or(Duration::from_secs(60)));
                        last_reject = Some((status, headers, body_bytes));
                    }
                    s if (500..600).contains(&s) => {
                        tracing::warn!(account = %account.name, status = s, "upstream 5xx, backing off");
                        account.backoff_and_cool_down();
                        last_reject = Some((status, headers, body_bytes));
                    }
                    _ => {
                        // Any other 4xx is the caller's problem: mirror downstream without
                        // penalizing the account.
                        return Ok(Attempt::HttpReject(status, headers, body_bytes));
                    }
                }
            }
            Attempt::Transport(kind, err) => {
                if let Some(telem) = telem.as_ref() {
                    on_failure(
                        account,
                        &attempt_session,
                        telem,
                        failure_from_transport(kind),
                    );
                }
                tracing::warn!(account = %account.name, error = %err, "transport error, backing off");
                account.backoff_and_cool_down();
            }
        }
    }

    if !candidates.is_empty() && candidates.iter().all(|a| a.auth_invalid()) {
        return Err(RelayError::AllAccountsInvalid);
    }
    if candidates.iter().all(|a| !a.available()) {
        return Err(RelayError::AllAccountsUnavailable {
            retry_after: pool.earliest_recovery(),
        });
    }
    if let Some((status, headers, body)) = last_reject {
        // Switch budget exhausted: mirror the last upstream rejection downstream,
        // preserving its status semantics.
        return Ok(Attempt::HttpReject(status, headers, body));
    }
    Err(RelayError::AllAccountsUnavailable {
        retry_after: pool.earliest_recovery(),
    })
}

#[allow(clippy::too_many_arguments)]
pub async fn forward_responses(
    pool: &Pool,
    provider: &Provider,
    downstream_body: &serde_json::Value,
    session: &SessionCtx,
    model_db: &ModelDb,
    store: &SessionStore,
    max_account_switches: usize,
    compression: bool,
    compaction_meta: Option<&serde_json::Value>,
    telemetry: &Telemetry,
) -> Result<ForwardResult, RelayError> {
    let model = downstream_body
        .get("model")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();
    let is_compaction =
        compaction_meta.is_some() || has_compaction_trigger(downstream_body);
    let implementation = "responses_compaction_v2";
    let compaction_block = compaction_meta.cloned().unwrap_or_else(|| {
        crate::request_build::default_compaction_metadata(implementation)
    });

    let prepare_model = model.clone();
    let prepare = move |account: &Account, attempt_session: &SessionCtx| {
        let behavior = model_db.for_model(&prepare_model);
        let (turn_id, telem) = if is_compaction {
            let mut start = telemetry.tracker.begin_compaction(
                attempt_session,
                &account.name,
                &prepare_model,
            );
            let emissions = std::mem::take(&mut start.emissions);
            let turn_id = start.turn_id.clone();
            telemetry.emit(account, emissions);
            (turn_id, AttemptTelem::Compaction(start))
        } else {
            let AttemptStart {
                turn_id,
                emissions,
                anon_key,
            } = telemetry.tracker.begin_attempt(
                attempt_session,
                &account.name,
                downstream_body,
                &prepare_model,
                &behavior,
                model_db,
            );
            telemetry.emit(account, emissions);
            let telem = AttemptTelem::Turn {
                turn_id: turn_id.clone(),
                // Keyless requests get the tracker's per-request session key; keyed ones
                // keep their downstream-derived key.
                session_key: anon_key.unwrap_or_else(|| attempt_session.key.clone()),
            };
            (turn_id, telem)
        };
        let turn_started_at_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        // The body is rebuilt with each account's installation_id (officially it belongs to
        // codex_home, and switching accounts means switching codex_home). The body contains
        // only officially constructed fields; nothing downstream is passed through.
        let built = match build_upstream_request(
            downstream_body,
            attempt_session,
            model_db,
            &account.installation_id,
            &turn_id,
            turn_started_at_unix_ms,
            compaction_meta,
        ) {
            Ok(built) => built,
            Err(e) => {
                // The upstream never saw this turn: drop a just-started empty turn.
                if let AttemptTelem::Turn {
                    turn_id,
                    session_key,
                } = &telem
                {
                    telemetry
                        .tracker
                        .note_attempt_aborted(session_key, &account.name, turn_id);
                }
                return Err(RelayError::BadRequest(e.to_string()));
            }
        };
        let headers = attempt_headers(attempt_session, &built.extra_headers);
        Ok((built.body, headers, telem))
    };

    let on_failure = |account: &Account,
                      attempt_session: &SessionCtx,
                      telem: &AttemptTelem,
                      failure: TurnFailure| {
        match telem {
            AttemptTelem::Turn {
                turn_id,
                session_key,
            } => {
                // Mark the turn failed and hold it for the retry grace: an identical
                // following request continues the same turn (official client retry).
                telemetry
                    .tracker
                    .note_attempt_failed(session_key, &account.name, turn_id, failure);
            }
            AttemptTelem::Compaction(start) => {
                let emissions = telemetry.tracker.note_compaction_end(
                    attempt_session,
                    &account.name,
                    start,
                    &compaction_block,
                    implementation,
                    &model,
                    CompactionOutcome {
                        status: "failed",
                        failure: Some(failure),
                        usage: None,
                    },
                );
                telemetry.emit(account, emissions);
            }
            AttemptTelem::None => {}
        }
    };

    match drive_attempts(
        pool,
        provider,
        session,
        store,
        max_account_switches,
        compression,
        SuccessMode::Stream,
        "/responses",
        prepare,
        on_failure,
    )
    .await?
    {
        Attempt::Stream(stream) => Ok(ForwardResult::Stream(Box::new(stream))),
        Attempt::HttpReject(status, headers, body) => Ok(ForwardResult::Rejected {
            status,
            headers,
            body,
        }),
        Attempt::Json(..) => unreachable!("responses uses SuccessMode::Stream"),
        Attempt::Transport(..) => unreachable!("transport errors are retried in the loop"),
    }
}

/// Official standalone-web-search executor channel (codex-api SearchClient): the model's
/// web.run call is executed via POST {provider}/alpha/search with a SearchRequest body
/// carrying the conversation session id in `id`. The relay mirrors that: body passthrough
/// except `id` (per-account session id, same anti-correlation rule as /responses), plain
/// JSON response mirrored back. Session headers are NOT sent (official SearchClient has
/// no EndpointSession trio); x-codex-turn-metadata is synthesized officially per account.
/// Uncompressed, matching the official Request::new default.
/// Not a tracked telemetry endpoint (no turn/compaction semantics upstream).
#[allow(clippy::too_many_arguments)]
pub async fn forward_search(
    pool: &Pool,
    provider: &Provider,
    downstream_body: &serde_json::Value,
    session: &SessionCtx,
    model_db: &ModelDb,
    store: &SessionStore,
    max_account_switches: usize,
) -> Result<ForwardJsonResult, RelayError> {
    let prepare = move |account: &Account, attempt_session: &SessionCtx| {
        let mut body = downstream_body.clone();
        let model = body
            .get("model")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let behavior = model_db.for_model(model);
        if let Some(obj) = body.as_object_mut() {
            obj.insert(
                "id".to_string(),
                serde_json::Value::String(attempt_session.session_id.to_string()),
            );
        }
        let turn_id = uuid::Uuid::now_v7().to_string();
        let turn_started_at_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let turn_metadata = crate::request_build::build_turn_metadata(
            &behavior,
            &account.installation_id,
            attempt_session,
            &attempt_session.window_id(),
            &turn_id,
            turn_started_at_unix_ms,
            "turn",
            None,
        );
        let mut headers = HeaderMap::new();
        if let Ok(v) = HeaderValue::from_str(&turn_metadata.to_string()) {
            headers.insert("x-codex-turn-metadata", v);
        }
        Ok((body, headers, AttemptTelem::None))
    };
    let on_failure = |_: &Account, _: &SessionCtx, _: &AttemptTelem, _: TurnFailure| {};
    match drive_attempts(
        pool,
        provider,
        session,
        store,
        max_account_switches,
        /*compression*/ false,
        SuccessMode::Json,
        "/alpha/search",
        prepare,
        on_failure,
    )
    .await?
    {
        Attempt::Json(json) => Ok(ForwardJsonResult::Json(Box::new(json))),
        Attempt::HttpReject(status, headers, body) => Ok(ForwardJsonResult::Rejected {
            status,
            headers,
            body,
        }),
        Attempt::Stream(_) => unreachable!("search uses SuccessMode::Json"),
        Attempt::Transport(..) => unreachable!("transport errors are retried in the loop"),
    }
}

/// Legacy remote-compaction channel (codex-api CompactClient): POST {provider}/responses/compact,
/// unary plain JSON, response {"output": [...ResponseItem]}. Official 0.153.4 defaults to
/// compaction v2 (trigger item over /responses), but a client with
/// `features.remote_compaction_v2 = false` still calls this endpoint. Body is rebuilt into
/// the official CompactionInput shape per account (see build_compact_request); headers follow
/// the official compact_conversation_history set: session-id + thread-id (NOT the /responses
/// trio — no x-client-request-id), installation-id as a real header, window-id, turn-metadata
/// (request_kind=compaction), routing hint, lite header; no Accept override (unary).
/// Uncompressed, matching the official EndpointSession::execute_with default.
#[allow(clippy::too_many_arguments)]
pub async fn forward_compact(
    pool: &Pool,
    provider: &Provider,
    downstream_body: &serde_json::Value,
    session: &SessionCtx,
    model_db: &ModelDb,
    store: &SessionStore,
    max_account_switches: usize,
    compaction_meta: Option<&serde_json::Value>,
    telemetry: &Telemetry,
) -> Result<ForwardJsonResult, RelayError> {
    let model = downstream_body
        .get("model")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();
    let implementation = "responses_compact";
    let compaction_block = compaction_meta
        .cloned()
        .unwrap_or_else(|| crate::request_build::default_compaction_metadata(implementation));

    let prepare_model = model.clone();
    let prepare = move |account: &Account, attempt_session: &SessionCtx| {
        let mut start =
            telemetry
                .tracker
                .begin_compaction(attempt_session, &account.name, &prepare_model);
        let emissions = std::mem::take(&mut start.emissions);
        let turn_id = start.turn_id.clone();
        telemetry.emit(account, emissions);
        let turn_started_at_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let built = crate::request_build::build_compact_request(
            downstream_body,
            attempt_session,
            model_db,
            &account.installation_id,
            &turn_id,
            turn_started_at_unix_ms,
            compaction_meta,
        )
        .map_err(|e| RelayError::BadRequest(e.to_string()))?;
        // Official compact session headers: build_session_headers(session_id, thread_id) only.
        let mut headers = HeaderMap::new();
        if let Ok(v) = HeaderValue::from_str(&attempt_session.session_id.to_string()) {
            headers.insert("session-id", v);
        }
        if let Ok(v) = HeaderValue::from_str(&attempt_session.thread_id.to_string()) {
            headers.insert("thread-id", v);
        }
        headers.extend(built.extra_headers.clone());
        Ok((built.body, headers, AttemptTelem::Compaction(start)))
    };

    let on_failure = |account: &Account,
                      attempt_session: &SessionCtx,
                      telem: &AttemptTelem,
                      failure: TurnFailure| {
        if let AttemptTelem::Compaction(start) = telem {
            let emissions = telemetry.tracker.note_compaction_end(
                attempt_session,
                &account.name,
                start,
                &compaction_block,
                implementation,
                &model,
                CompactionOutcome {
                    status: "failed",
                    failure: Some(failure),
                    usage: None,
                },
            );
            telemetry.emit(account, emissions);
        }
    };

    match drive_attempts(
        pool,
        provider,
        session,
        store,
        max_account_switches,
        /*compression*/ false,
        SuccessMode::Json,
        "/responses/compact",
        prepare,
        on_failure,
    )
    .await?
    {
        Attempt::Json(json) => {
            // Unary success: close out the compaction with the response's usage.
            let upstream = json;
            if let AttemptTelem::Compaction(start) = &upstream.telem {
                let usage = serde_json::from_slice::<serde_json::Value>(&upstream.body)
                    .ok()
                    .and_then(|v| v.get("usage").cloned())
                    .map(|u| crate::turns::TokenAccum::from_wire(&u));
                let emissions = telemetry.tracker.note_compaction_end(
                    &upstream.session,
                    &upstream.account_name,
                    start,
                    &compaction_block,
                    implementation,
                    &model,
                    CompactionOutcome {
                        status: "completed",
                        failure: None,
                        usage,
                    },
                );
                if let Some(account) = pool
                    .accounts()
                    .iter()
                    .find(|a| a.name == upstream.account_name)
                {
                    telemetry.emit(account, emissions);
                }
            }
            Ok(ForwardJsonResult::Json(Box::new(upstream)))
        }
        Attempt::HttpReject(status, headers, body) => Ok(ForwardJsonResult::Rejected {
            status,
            headers,
            body,
        }),
        Attempt::Stream(_) => unreachable!("compact uses SuccessMode::Json"),
        Attempt::Transport(..) => unreachable!("transport errors are retried in the loop"),
    }
}
