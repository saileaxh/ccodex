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

use crate::accounts::{Account, Pool};
use crate::model_db::ModelDb;
use crate::request_build::{SessionCtx, SessionStore, build_upstream_request};
use bytes::Bytes;
use codex_api::Provider;
use codex_http_client::{
    ByteStream, HttpTransport, Request, RequestBody, RequestCompression, ReqwestTransport,
};
use futures::StreamExt;
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use std::time::Duration;

pub struct UpstreamStream {
    pub account_name: String,
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub bytes: ByteStream,
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

#[derive(Debug)]
pub enum RelayError {
    /// All accounts unavailable (cooldown / failures).
    AllAccountsUnavailable { retry_after: Option<Duration> },
    /// Downstream request lacks required semantic fields (400 to downstream; not upstream-visible).
    BadRequest(String),
}

enum Attempt {
    Stream(UpstreamStream),
    HttpReject(StatusCode, HeaderMap, Bytes),
    Transport(String),
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

async fn try_once(
    account: &Account,
    provider: &Provider,
    transport: &ReqwestTransport,
    body: &serde_json::Value,
    extra_headers: &HeaderMap,
    compression: bool,
) -> Attempt {
    let mut req: Request = provider.build_request(Method::POST, "/responses");
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
            return Attempt::Transport(format!("auth: {e}"));
        }
    };

    match transport.stream(req).await {
        Ok(resp) if resp.status.is_success() => Attempt::Stream(UpstreamStream {
            account_name: account.name.clone(),
            status: resp.status,
            headers: resp.headers,
            bytes: resp.bytes,
        }),
        Ok(resp) => {
            let status = resp.status;
            let headers = resp.headers.clone();
            let body = collect_body(resp.bytes).await;
            Attempt::HttpReject(status, headers, body)
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
        }) => Attempt::HttpReject(
            status,
            headers.unwrap_or_default(),
            Bytes::from(body.unwrap_or_default()),
        ),
        Err(e) => Attempt::Transport(e.to_string()),
    }
}

fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get(http::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
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
) -> Result<ForwardResult, RelayError> {
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
        let turn_id = uuid::Uuid::now_v7().to_string();
        let turn_started_at_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        // The body is rebuilt with each account's installation_id (officially it belongs to
        // codex_home, and switching accounts means switching codex_home). The body contains
        // only officially constructed fields; nothing downstream is passed through.
        let built = build_upstream_request(
            downstream_body,
            &attempt_session,
            model_db,
            &account.installation_id,
            &turn_id,
            turn_started_at_unix_ms,
        )
        .map_err(|e| RelayError::BadRequest(e.to_string()))?;
        let extra_headers = attempt_headers(&attempt_session, &built.extra_headers);

        let mut attempt = try_once(
            account,
            provider,
            &account.transport,
            &built.body,
            &extra_headers,
            compression,
        )
        .await;

        // 401: refresh credentials via the official path, retry once on the same account
        // (matches official UnauthorizedRecovery).
        if matches!(&attempt, Attempt::HttpReject(status, _, _) if *status == StatusCode::UNAUTHORIZED)
        {
            tracing::warn!(account = %account.name, "401 from upstream, refreshing token and retrying once");
            match account.auth_manager.refresh_token().await {
                Ok(()) => {
                    attempt = try_once(
                        account,
                        provider,
                        &account.transport,
                        &built.body,
                        &extra_headers,
                        compression,
                    )
                    .await;
                }
                Err(e) => {
                    tracing::warn!(account = %account.name, error = %e, "token refresh failed");
                }
            }
        }

        match attempt {
            Attempt::Stream(stream) => {
                account.note_success();
                pool.stick(&session.key, &account.name);
                // Official rate-limit header parsing (exported by the export-rate-limits-module patch).
                let quotas = codex_api::rate_limits::parse_all_rate_limits(&stream.headers);
                if !quotas.is_empty() {
                    tracing::info!(account = %account.name, quotas = ?quotas, "upstream rate-limit snapshot");
                    if let Ok(value) = serde_json::to_value(&quotas) {
                        account.set_quotas(value);
                    }
                }
                return Ok(ForwardResult::Stream(Box::new(stream)));
            }
            Attempt::HttpReject(status, headers, body_bytes) => match status.as_u16() {
                401 => {
                    tracing::warn!(account = %account.name, "still 401 after refresh, cooling down 5m");
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
                    return Ok(ForwardResult::Rejected {
                        status,
                        headers,
                        body: body_bytes,
                    });
                }
            },
            Attempt::Transport(err) => {
                tracing::warn!(account = %account.name, error = %err, "transport error, backing off");
                account.backoff_and_cool_down();
            }
        }
    }

    if candidates.iter().all(|a| !a.available()) {
        return Err(RelayError::AllAccountsUnavailable {
            retry_after: pool.earliest_recovery(),
        });
    }
    if let Some((status, headers, body)) = last_reject {
        // Switch budget exhausted: mirror the last upstream rejection downstream,
        // preserving its status semantics.
        return Ok(ForwardResult::Rejected {
            status,
            headers,
            body,
        });
    }
    Err(RelayError::AllAccountsUnavailable {
        retry_after: pool.earliest_recovery(),
    })
}
