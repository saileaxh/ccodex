//! Downstream WebSocket: speaks the official Codex WS protocol (response.create frames in,
//! event frames out). The upstream leg still uses the official HTTP SSE path (the official
//! client falls back to HTTP the same way when WS is unavailable); SSE event data payloads
//! are extracted verbatim and sent back as WS text frames — identical event content to the
//! official WS protocol, with no parsing/re-serialization (the official ResponseEvent only
//! implements Debug, so re-serializing it would inevitably drift).
//!
//! WS-only server features that need upstream response state are answered with the official
//! wrapped previous_response_not_found error (retryable) instead of being silently
//! mis-served: incremental frames (previous_response_id + input delta) and v2 prewarm
//! (generate=false). The official client then retries with a full request, which the HTTP
//! upstream leg can serve exactly.

use crate::forward::{ForwardResult, RelayError, forward_responses};
use crate::gateway::AppState;
use crate::request_build::derive_session;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::Response;
use futures::StreamExt;
use serde_json::{Value, json};
use std::sync::Arc;

pub async fn handle_responses_ws(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let api_key = match crate::gateway::authorize(&state, &headers) {
        Ok(key) => key,
        Err(resp) => return resp,
    };
    ws.on_upgrade(move |socket| connection_loop(state, socket, api_key, headers))
}

async fn send_error(socket: &mut WebSocket, code: &str, message: &str) -> Result<(), axum::Error> {
    socket
        .send(Message::Text(
            json!({ "type": "error", "code": code, "message": message })
                .to_string()
                .into(),
        ))
        .await
}

/// Wrapped WS error frame per the official Responses WS protocol (codex-api
/// WrappedWebsocketErrorEvent). With code "previous_response_not_found" the official client
/// maps it to a retryable error and resends the turn with the FULL input — the sanctioned
/// recovery when a server cannot honor incremental chaining.
async fn send_wrapped_error(
    socket: &mut WebSocket,
    status: u16,
    code: &str,
    message: &str,
) -> Result<(), axum::Error> {
    socket
        .send(Message::Text(
            json!({
                "type": "error",
                "status": status,
                "error": { "code": code, "message": message },
            })
            .to_string()
            .into(),
        ))
        .await
}

const PREVIOUS_RESPONSE_NOT_FOUND: &str = "previous_response_not_found";
const PREVIOUS_RESPONSE_NOT_FOUND_MSG: &str =
    "Previous response was not found. Retrying the full request.";

async fn connection_loop(
    state: Arc<AppState>,
    mut socket: WebSocket,
    api_key: String,
    headers: HeaderMap,
) {
    while let Some(msg) = socket.recv().await {
        let msg = match msg {
            Ok(m) => m,
            Err(_) => return,
        };
        let text = match msg {
            Message::Text(t) => t,
            Message::Ping(p) => {
                if socket.send(Message::Pong(p)).await.is_err() {
                    return;
                }
                continue;
            }
            Message::Pong(_) => continue,
            Message::Close(_) => return,
            Message::Binary(_) => {
                if send_error(
                    &mut socket,
                    "invalid_request",
                    "binary frames are not supported",
                )
                .await
                .is_err()
                {
                    return;
                }
                continue;
            }
        };

        let Ok(frame) = serde_json::from_str::<Value>(&text) else {
            if send_error(&mut socket, "invalid_request", "frame must be JSON")
                .await
                .is_err()
            {
                return;
            }
            continue;
        };
        if frame.get("type").and_then(Value::as_str) != Some("response.create") {
            if send_error(
                &mut socket,
                "invalid_request",
                "expected response.create frame",
            )
            .await
            .is_err()
            {
                return;
            }
            continue;
        }
        let mut body = frame;
        if let Some(obj) = body.as_object_mut() {
            obj.remove("type");
        }
        // Incremental turns (e.g. queued follow-ups on a reused connection) carry
        // previous_response_id + only the new input delta. Our upstream leg is HTTP SSE,
        // whose official request struct has no response chaining at all, so the delta alone
        // would reach the upstream stripped of the whole conversation. Answer with the
        // official previous_response_not_found error: the client retries the turn with the
        // full input, which we then serve normally.
        if body
            .get("previous_response_id")
            .and_then(Value::as_str)
            .is_some()
        {
            if send_wrapped_error(
                &mut socket,
                404,
                PREVIOUS_RESPONSE_NOT_FOUND,
                PREVIOUS_RESPONSE_NOT_FOUND_MSG,
            )
            .await
            .is_err()
            {
                return;
            }
            continue;
        }
        // v2 session prewarm (generate=false) is connection setup that expects a chainable
        // response id back, which we cannot mint without running a real (billed) turn.
        // Failing it the same retryable way resolves startup prewarm as best-effort
        // unavailable; the first real turn then proceeds with a full request.
        if body.get("generate").and_then(Value::as_bool) == Some(false) {
            if send_wrapped_error(
                &mut socket,
                404,
                PREVIOUS_RESPONSE_NOT_FOUND,
                PREVIOUS_RESPONSE_NOT_FOUND_MSG,
            )
            .await
            .is_err()
            {
                return;
            }
            continue;
        }
        if !handle_create(&state, &mut socket, &api_key, &headers, body).await {
            return;
        }
    }
}

/// Handles one response.create; returns false when the connection should close.
async fn handle_create(
    state: &Arc<AppState>,
    socket: &mut WebSocket,
    api_key: &str,
    headers: &HeaderMap,
    body: Value,
) -> bool {
    let pool = state.pool.read().unwrap().clone();
    let session = derive_session(
        api_key,
        &body,
        headers,
        state.config.session_isolation(),
        &state.sessions,
    );
    // Same as the HTTP path: the body is rebuilt inside forward per official construction
    // logic; no downstream fields are passed through.

    let result = forward_responses(
        &pool,
        &state.provider,
        &body,
        &session,
        &state.model_db,
        &state.sessions,
        state.config.max_account_switches(),
        state.config.request_compression(),
        // WS frames carry no per-turn x-codex-turn-metadata header; a compaction_trigger
        // item still flips request_kind via the built-in default compaction block.
        None,
        &state.telemetry,
    )
    .await;

    match result {
        Ok(ForwardResult::Stream(stream)) => {
            let upstream = *stream;
            let account = upstream.account_name.clone();
            let telem_session = upstream.session.clone();
            let telem = upstream.telem;
            let model = body
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let mut tap = crate::sse_tap::SseTap::new();
            // The account's own metrics client (same-account egress as analytics).
            let tap_metrics = {
                let pool = state.pool.read().unwrap();
                state.telemetry.metrics_for_name(&pool, &account)
            };
            let mut bytes = upstream.bytes;
            let mut buf: Vec<u8> = Vec::new();
            while let Some(chunk) = bytes.next().await {
                let Ok(b) = chunk else {
                    tap.note_stream_error();
                    crate::gateway::drain_sse_metrics(state, &tap_metrics, &model, &mut tap);
                    let _ = send_error(socket, "stream_error", "upstream stream interrupted").await;
                    return true;
                };
                tap.feed(&b);
                if let Some(usage) = tap.take_usage_for_accounting() {
                    crate::gateway::record_stream_usage(
                        state, crate::gateway::usage_key(api_key), &account, &model, usage,
                    );
                }
                crate::gateway::drain_sse_metrics(state, &tap_metrics, &model, &mut tap);
                buf.extend_from_slice(&b);
                // SSE events are blank-line separated; extract each data payload as a WS frame.
                while let Some(block) = crate::sse_tap::take_event(&mut buf) {
                    let Ok(text) = std::str::from_utf8(&block) else {
                        continue;
                    };
                    for line in text.lines() {
                        if let Some(data) = line.strip_prefix("data:")
                            && socket
                                .send(Message::Text(data.trim().to_string().into()))
                                .await
                                .is_err()
                        {
                            return false;
                        }
                    }
                }
            }
            // Stream tail: same turn-tracker finalization + usage/cost accounting as the
            // HTTP path (WS turns are billed too).
            crate::gateway::finalize_stream_telemetry(
                state,
                telem,
                &telem_session,
                &account,
                &model,
                &crate::request_build::default_compaction_metadata("responses_compaction_v2"),
                &tap,
            );
            let usage = tap.usage.take();
            tracing::info!(
                account = %account,
                completed = tap.completed,
                usage = ?usage,
                failed = ?tap.failed_code(),
                "ws turn finished"
            );
            true
        }
        Ok(ForwardResult::Rejected { status, body, .. }) => {
            // Convert the upstream error body into a WS error event.
            let text = String::from_utf8_lossy(&body);
            let frame = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("error").cloned())
                .and_then(|mut e| {
                    let obj = e.as_object_mut()?;
                    obj.insert("type".to_string(), Value::String("error".to_string()));
                    Some(Value::Object(obj.clone()))
                })
                .unwrap_or_else(
                    || json!({ "type": "error", "code": status.as_str(), "message": text }),
                );
            socket
                .send(Message::Text(frame.to_string().into()))
                .await
                .is_ok()
        }
        Err(RelayError::AllAccountsUnavailable { .. }) => send_error(
            socket,
            "model_cooldown",
            "all upstream accounts are cooling down",
        )
        .await
        .is_ok(),
        Err(RelayError::AllAccountsInvalid) => send_error(
            socket,
            "accounts_invalid",
            "all upstream account credentials were revoked; re-login or remove them in the admin panel",
        )
        .await
        .is_ok(),
        Err(RelayError::BadRequest(message)) => send_error(socket, "invalid_request", &message)
            .await
            .is_ok(),
    }
}
