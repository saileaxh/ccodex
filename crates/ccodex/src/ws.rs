//! Downstream WebSocket: speaks the official Codex WS protocol (response.create frames in,
//! event frames out). The upstream leg still uses the official HTTP SSE path (the official
//! client falls back to HTTP the same way when WS is unavailable); SSE event data payloads
//! are extracted verbatim and sent back as WS text frames — identical event content to the
//! official WS protocol, with no parsing/re-serialization (the official ResponseEvent only
//! implements Debug, so re-serializing it would inevitably drift).

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
    )
    .await;

    match result {
        Ok(ForwardResult::Stream(stream)) => {
            let upstream = *stream;
            let account = upstream.account_name.clone();
            let mut tap = crate::sse_tap::SseTap::new();
            let mut bytes = upstream.bytes;
            let mut buf: Vec<u8> = Vec::new();
            while let Some(chunk) = bytes.next().await {
                let Ok(b) = chunk else {
                    let _ = send_error(socket, "stream_error", "upstream stream interrupted").await;
                    return true;
                };
                tap.feed(&b);
                buf.extend_from_slice(&b);
                // SSE events are blank-line separated; extract each data payload as a WS frame.
                while let Some(pos) = buf.windows(2).position(|w| w == b"\n\n") {
                    let block: Vec<u8> = buf.drain(..pos).collect();
                    let _ = buf.drain(..2.min(buf.len()));
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
            tracing::info!(
                account = %account,
                completed = tap.completed,
                usage = ?tap.usage,
                failed = ?tap.failed,
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
        Err(RelayError::BadRequest(message)) => send_error(socket, "invalid_request", &message)
            .await
            .is_ok(),
    }
}
