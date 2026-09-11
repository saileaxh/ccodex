//! Turn boundary reconstruction. The official client scopes one `turn_id` to a whole
//! agentic turn (user input → sampling → tool calls → sampling → … → final answer),
//! and its analytics emit one `codex_turn_event` per turn. The relay observes the same
//! lifecycle as separate HTTP requests, so turn boundaries are reconstructed from the
//! wire protocol itself:
//!
//! - a request whose input TAIL is tool outputs (function_call_output /
//!   custom_tool_call_output) referencing call_ids emitted by the previous response in
//!   the same (session, account) is a CONTINUATION of the open turn: same turn_id,
//!   sampling_request_count+1, inter-request gap booked as tool_blocking
//! - a request repeating the exact input of an open turn whose last sampling failed or
//!   never finished is a RETRY: same turn_id, sampling_retry_count+1 (official clients
//!   retry a failed/disconnected sampling within the same turn)
//! - anything else starts a NEW turn (fresh v7 turn_id); the displaced open turn is
//!   finalized as "interrupted" (or "failed" if its last sampling failed)
//!
//! Finalization (one analytics event per turn, official reducer semantics):
//! - response.completed with no pending tool calls → "completed", emitted immediately
//! - response.failed / truncated stream → turn marked failed and held for the retry
//!   grace (a following identical request continues it); emitted as "failed" when
//!   displaced by a different request
//! - open turn displaced by an unrelated request → "interrupted" (official Esc
//!   equivalent: the turn never reached a final answer)
//! - open turn idle past the TTL → dropped silently (official process-kill mid-turn
//!   equivalent: the event never leaves the process)
//!
//! This module also FIXES the data plane: the x-codex-turn-metadata turn_id sent
//! upstream is now the reconstructed per-turn id (reused across samplings), matching
//! official client behavior; previously a fresh id was minted per HTTP request.
//!
//! Compaction requests (v2 trigger item over /responses, or the legacy
//! /responses/compact endpoint) are NOT turns: the official compact task emits only a
//! codex_compaction_event (no turn event), so the relay does the same.

use crate::analytics::{
    CodexAppServerClientMetadata, CodexCompactionEventParams, CodexRuntimeMetadata,
    CodexTurnEventParams, ThreadInitializedEventParams,
};
use crate::model_db::ModelBehavior;
use crate::request_build::SessionCtx;
use crate::sse_tap::SseTap;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Open-turn idle TTL: past this, the turn is dropped silently (process-kill analog).
const OPEN_TURN_TTL: Duration = Duration::from_secs(30 * 60);
/// How long a failed turn waits for the client's retry before being finalized as
/// failed. The official client re-samples inside the same turn within ~3s (4 attempts,
/// 200ms base backoff), so anything later is a give-up, not a retry.
const FAILED_GRACE: Duration = Duration::from_secs(5);
/// Turn-session map entries idle past this are evicted (bounds long-run growth).
const SESSION_TTL: Duration = Duration::from_secs(24 * 3600);

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Errors: official CodexErr → (CodexErrKind snake_case, http status, CodexErrorInfo)
// mapping, replicated from codex-api/src/api_bridge.rs + protocol/src/error.rs.
// ---------------------------------------------------------------------------

/// A turn-ending failure in official terms.
#[derive(Clone)]
pub struct TurnFailure {
    /// CodexErrKind snake_case (codex_error_kind).
    pub kind: &'static str,
    /// CodexErr::http_status_code_value (Some only for retry_limit/unexpected_status/
    /// connection_failed/response_stream_failed).
    pub http_status: Option<u16>,
    /// CodexErrorInfo camelCase JSON (turn_error).
    pub info: Value,
}

fn info_unit(name: &str) -> Value {
    Value::String(name.to_string())
}

fn info_with_status(name: &str, status: Option<u16>) -> Value {
    serde_json::json!({ name: { "httpStatusCode": status } })
}

/// map_api_error for TransportError::Http: classify an upstream HTTP rejection the way
/// the official client would (status + body driven).
pub fn failure_from_http_reject(status: u16, body: &[u8]) -> TurnFailure {
    let body_text = String::from_utf8_lossy(body);
    let parsed: Option<Value> = serde_json::from_str(&body_text).ok();
    let error_code = parsed
        .as_ref()
        .and_then(|v| v.get("error"))
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_str())
        .map(str::to_string);
    let error_type = parsed
        .as_ref()
        .and_then(|v| v.get("error"))
        .and_then(|e| e.get("type"))
        .and_then(|c| c.as_str())
        .map(str::to_string);

    if status == 503 && matches!(error_code.as_deref(), Some("server_is_overloaded" | "slow_down"))
    {
        return TurnFailure {
            kind: "server_overloaded",
            http_status: None,
            info: info_unit("serverOverloaded"),
        };
    }
    match status {
        400 => {
            if error_code.as_deref() == Some("cyber_policy") {
                TurnFailure {
                    kind: "cyber_policy",
                    http_status: None,
                    info: info_unit("cyberPolicy"),
                }
            } else if error_code.as_deref() == Some("misalignment_policy_violation") {
                TurnFailure {
                    kind: "misalignment_policy_violation",
                    http_status: None,
                    info: info_unit("misalignmentPolicyViolation"),
                }
            } else {
                // CodexErrorDetails::InvalidRequest → to_codex_protocol_error: Other.
                TurnFailure {
                    kind: "invalid_request",
                    http_status: None,
                    info: info_unit("other"),
                }
            }
        }
        429 => {
            if error_type.as_deref() == Some("usage_limit_reached") {
                TurnFailure {
                    kind: "usage_limit_reached",
                    http_status: None,
                    info: info_unit("usageLimitExceeded"),
                }
            } else if error_type.as_deref() == Some("usage_not_included") {
                TurnFailure {
                    kind: "usage_not_included",
                    http_status: None,
                    info: info_unit("usageLimitExceeded"),
                }
            } else {
                TurnFailure {
                    kind: "retry_limit",
                    http_status: Some(429),
                    info: info_with_status("responseTooManyFailedAttempts", Some(429)),
                }
            }
        }
        500 => TurnFailure {
            kind: "internal_server_error",
            http_status: None,
            info: info_unit("internalServerError"),
        },
        // All other statuses → CodexErrorDetails::UnexpectedStatus → CodexErrorInfo::Other.
        _ => TurnFailure {
            kind: "unexpected_status",
            http_status: Some(status),
            info: info_unit("other"),
        },
    }
}

/// map_api_error for in-stream response.failed / error events (sse/responses.rs).
pub fn failure_from_stream_error(error: Option<&Value>) -> TurnFailure {
    let code = error
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    let message = error
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .unwrap_or("");
    let message_l = message.to_lowercase();
    let is_context_window = code == "context_length_exceeded"
        || code == "context_window_exceeded"
        || message_l.contains("context window")
        || message_l.contains("context_length_exceeded");
    let is_quota = code == "insufficient_quota" || message_l.contains("quota");
    let is_usage_not_included = code == "usage_not_included";
    let is_overloaded = matches!(code, "server_is_overloaded" | "slow_down");

    if is_context_window {
        TurnFailure {
            kind: "context_window_exceeded",
            http_status: None,
            info: info_unit("contextWindowExceeded"),
        }
    } else if is_usage_not_included {
        TurnFailure {
            kind: "usage_not_included",
            http_status: None,
            info: info_unit("usageLimitExceeded"),
        }
    } else if is_quota {
        TurnFailure {
            kind: "quota_exceeded",
            http_status: None,
            info: info_unit("usageLimitExceeded"),
        }
    } else if code == "cyber_policy" {
        TurnFailure {
            kind: "cyber_policy",
            http_status: None,
            info: info_unit("cyberPolicy"),
        }
    } else if code == "misalignment_policy_violation" {
        TurnFailure {
            kind: "misalignment_policy_violation",
            http_status: None,
            info: info_unit("misalignmentPolicyViolation"),
        }
    } else if matches!(code, "invalid_prompt" | "bio_policy") {
        TurnFailure {
            kind: "invalid_request",
            http_status: None,
            info: info_unit("other"),
        }
    } else if is_overloaded {
        TurnFailure {
            kind: "server_overloaded",
            http_status: None,
            info: info_unit("serverOverloaded"),
        }
    } else if code == "rate_limit_exceeded" {
        TurnFailure {
            kind: "rate_limit_exceeded",
            http_status: None,
            info: info_unit("rateLimitExceeded"),
        }
    } else {
        // ApiError::Retryable/Stream → CodexErrorDetails::Stream → CodexErrorInfo::Other.
        TurnFailure {
            kind: "stream",
            http_status: None,
            info: info_unit("other"),
        }
    }
}

/// Transport-level failure before/without an HTTP status.
pub fn failure_from_transport(kind: TransportFailureKind) -> TurnFailure {
    match kind {
        TransportFailureKind::Timeout => TurnFailure {
            kind: "request_timeout",
            http_status: None,
            info: info_unit("other"),
        },
        TransportFailureKind::Connection => TurnFailure {
            kind: "connection_failed",
            http_status: None,
            info: info_with_status("httpConnectionFailed", None),
        },
        TransportFailureKind::Other => TurnFailure {
            kind: "stream",
            http_status: None,
            info: info_unit("other"),
        },
    }
}

#[derive(Clone, Copy)]
pub enum TransportFailureKind {
    Timeout,
    Connection,
    Other,
}

// ---------------------------------------------------------------------------
// Token usage (codex-api ResponseCompletedUsage → protocol TokenUsage mapping).
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Default)]
pub struct TokenAccum {
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub cache_write_input_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_output_tokens: i64,
    pub total_tokens: i64,
}

impl TokenAccum {
    pub fn from_wire(usage: &Value) -> Self {
        let get = |v: &Value, key: &str| v.get(key).and_then(Value::as_i64).unwrap_or(0);
        let input_details = usage.get("input_tokens_details");
        let output_details = usage.get("output_tokens_details");
        Self {
            input_tokens: get(usage, "input_tokens"),
            cached_input_tokens: input_details.map(|d| get(d, "cached_tokens")).unwrap_or(0),
            cache_write_input_tokens: input_details
                .map(|d| get(d, "cache_write_tokens"))
                .unwrap_or(0),
            output_tokens: get(usage, "output_tokens"),
            reasoning_output_tokens: output_details
                .map(|d| get(d, "reasoning_tokens"))
                .unwrap_or(0),
            total_tokens: get(usage, "total_tokens"),
        }
    }

    fn add_assign(&mut self, other: &Self) {
        self.input_tokens += other.input_tokens;
        self.cached_input_tokens += other.cached_input_tokens;
        self.cache_write_input_tokens += other.cache_write_input_tokens;
        self.output_tokens += other.output_tokens;
        self.reasoning_output_tokens += other.reasoning_output_tokens;
        self.total_tokens += other.total_tokens;
    }
}

// ---------------------------------------------------------------------------
// Tool counts (analytics reducer TurnToolCounts over raw wire items).
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Default)]
pub struct ToolCounts {
    pub total: usize,
    pub shell_command: usize,
    pub file_change: usize,
    pub mcp_tool_call: usize,
    pub dynamic_tool_call: usize,
    pub subagent_tool_call: usize,
    pub web_search: usize,
    pub image_generation: usize,
}

const SHELL_TOOL_NAMES: &[&str] = &[
    "shell",
    "shell_command",
    "exec_command",
    "write_stdin",
    "local_shell",
];

impl ToolCounts {
    /// Classify one wire tool-call item. `declared` = tool names the client declared
    /// for this turn; a call to an undeclared name would be a client-side tool error
    /// (no thread item, not counted officially), so it is excluded.
    pub fn record(&mut self, item_type: &str, name: &str, declared: &HashSet<String>) {
        match item_type {
            "web_search_call" => {
                self.web_search += 1;
                self.total += 1;
            }
            "image_generation_call" => {
                self.image_generation += 1;
                self.total += 1;
            }
            "function_call" | "custom_tool_call" | "local_shell_call" => {
                if !declared.is_empty() && !declared.contains(name) {
                    return;
                }
                if name == "apply_patch" {
                    self.file_change += 1;
                } else if SHELL_TOOL_NAMES.contains(&name) {
                    self.shell_command += 1;
                } else if name.starts_with("mcp__") {
                    self.mcp_tool_call += 1;
                } else {
                    // Remaining declared tools are dynamic/namespace tools.
                    self.dynamic_tool_call += 1;
                }
                self.total += 1;
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Tracker state.
// ---------------------------------------------------------------------------

/// Events the tracker produces, ready for the analytics channel. Turn/Compaction carry
/// the extra measured values the Statsig metrics channel needs (ttft/ttfm are not
/// turn-event fields; the compaction event has no model field).
pub enum Emission {
    ThreadInitialized(Box<ThreadInitializedEventParams>),
    Turn(Box<TurnEmission>),
    Compaction(Box<CompactionEmission>),
}

pub struct TurnEmission {
    pub params: CodexTurnEventParams,
    pub ttft_ms: Option<u64>,
    pub ttfm_ms: Option<u64>,
}

pub struct CompactionEmission {
    pub params: CodexCompactionEventParams,
    pub model: String,
    /// Official codex.task.compact type tag: "remote_v2" | "remote" | "local".
    pub compact_type: &'static str,
    pub manual: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Outcome {
    /// A sampling request is in flight (or the stream broke without a terminal event).
    InFlight,
    /// Last response completed with tool calls awaiting client outputs.
    PendingCalls,
    /// Last sampling failed; held for the retry grace period.
    Failed,
}

struct OpenTurn {
    turn_id: String,
    thread_id: String,
    session_id: String,
    model: String,
    reasoning_effort: Option<String>,
    reasoning_summary: Option<String>,
    num_input_images: usize,
    is_first_turn: bool,
    declared_tools: HashSet<String>,
    started: Instant,
    started_at_unix: u64,
    last_activity: Instant,
    last_activity_unix: u64,
    before_first_sampling_ms: u64,
    sampling_ms: u64,
    compaction_ms: u64,
    tool_blocking_ms: u64,
    current_sampling_start: Option<Instant>,
    last_response_end: Option<Instant>,
    sampling_count: u32,
    retry_count: u32,
    pending_calls: HashSet<String>,
    counts: ToolCounts,
    usage: TokenAccum,
    saw_any_usage: bool,
    ttft_ms: Option<u64>,
    ttfm_ms: Option<u64>,
    input_fingerprint: u64,
    outcome: Outcome,
    failure: Option<TurnFailure>,
    /// When the turn was marked failed (retry-grace clock; see FAILED_GRACE).
    failed_since: Option<Instant>,
}

struct TurnSession {
    /// Account this session belongs to (emissions from the TTL/grace sweep need it to
    /// resolve the analytics queue).
    account: String,
    /// One-shot session for a keyless request: evicted as soon as it has no open turn.
    ephemeral: bool,
    open: Option<Box<OpenTurn>>,
    turns_seen: u64,
    cumulative_total_tokens: i64,
    thread_announced: bool,
    last_touch: Instant,
}

impl TurnSession {
    fn new(account: &str) -> Self {
        Self::with_ephemeral(account, false)
    }

    fn new_ephemeral(account: &str) -> Self {
        Self::with_ephemeral(account, true)
    }

    fn with_ephemeral(account: &str, ephemeral: bool) -> Self {
        Self {
            account: account.to_string(),
            ephemeral,
            open: None,
            turns_seen: 0,
            cumulative_total_tokens: 0,
            thread_announced: false,
            last_touch: Instant::now(),
        }
    }
}

/// Per-process static metadata shared by all emitted events.
#[derive(Clone)]
pub struct TurnEnv {
    pub runtime: CodexRuntimeMetadata,
    pub app_client: CodexAppServerClientMetadata,
}

impl TurnEnv {
    pub fn new() -> Self {
        Self {
            runtime: crate::analytics::current_runtime_metadata(),
            app_client: crate::analytics::tui_app_server_client_metadata(),
        }
    }
}

pub struct TurnTracker {
    sessions: Mutex<HashMap<String, TurnSession>>,
    env: TurnEnv,
}

/// Result of classifying an attempt: the turn_id to put on the wire plus events to emit.
pub struct AttemptStart {
    pub turn_id: String,
    pub emissions: Vec<Emission>,
    /// Keyless (anonymous) attempt: the per-request session key the tracker minted, so
    /// the stream tail can find the one-shot turn and finalize it. None for keyed
    /// sessions (the caller already has the key).
    pub anon_key: Option<String>,
}

/// Compaction-turn handle: captured at request start, completed at response end.
pub struct CompactionStart {
    pub turn_id: String,
    before_tokens: i64,
    started_at_unix: u64,
    started: Instant,
    pub emissions: Vec<Emission>,
}

/// Outcome of a compaction request.
pub struct CompactionOutcome {
    pub status: &'static str,
    pub failure: Option<TurnFailure>,
    /// Wire usage of the compaction response (response.completed / unary body usage).
    pub usage: Option<TokenAccum>,
}

fn tracker_key(account: &str, session_key: &str) -> String {
    format!("{account}\u{1f}{session_key}")
}

/// Fingerprint of the request's semantic input (retry detection). Only the input items
/// participate: instructions/tools/reasoning are stable within a session.
fn input_fingerprint(body: &Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    if let Some(input) = body.get("input") {
        serde_json::to_string(input).ok().hash(&mut hasher);
    }
    hasher.finish()
}

/// call_ids of tool outputs at the input TAIL (continuation detection). Stops at the
/// first non-tool-output item; an empty result means "not a continuation-shaped tail".
fn tail_tool_output_call_ids(body: &Value) -> Vec<String> {
    let Some(items) = body.get("input").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut ids = Vec::new();
    for item in items.iter().rev() {
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
        if item_type == "function_call_output" || item_type == "custom_tool_call_output" {
            if let Some(call_id) = item.get("call_id").and_then(Value::as_str) {
                ids.push(call_id.to_string());
            }
        } else {
            break;
        }
    }
    ids
}

/// True when the input tail carries a v2 compaction trigger item.
pub fn has_compaction_trigger(body: &Value) -> bool {
    body.get("input")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.get("type").and_then(Value::as_str) == Some("compaction_trigger")
            })
        })
}

/// Images in the turn's new user input: trailing user message items (the content the
/// client appended for this turn), counting input_image parts (official num_input_images).
fn count_turn_input_images(body: &Value) -> usize {
    let Some(items) = body.get("input").and_then(Value::as_array) else {
        return 0;
    };
    let mut count = 0;
    for item in items.iter().rev() {
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
        let is_user_message = item_type == "message"
            && item.get("role").and_then(Value::as_str) == Some("user");
        if !is_user_message {
            break;
        }
        if let Some(parts) = item.get("content").and_then(Value::as_array) {
            count += parts
                .iter()
                .filter(|p| {
                    p.get("type").and_then(Value::as_str) == Some("input_image")
                })
                .count();
        }
    }
    count
}

/// Tool names the client declared (for tool-call classification): function/custom tool
/// names at the top level and inside namespace declarations.
fn declared_tool_names(body: &Value) -> HashSet<String> {
    let mut names = HashSet::new();
    let Some(tools) = body.get("tools").and_then(Value::as_array) else {
        return names;
    };
    for tool in tools {
        let tool_type = tool.get("type").and_then(Value::as_str).unwrap_or("");
        if tool_type == "namespace" {
            if let Some(inner) = tool.get("tools").and_then(Value::as_array) {
                for t in inner {
                    if let Some(name) = t.get("name").and_then(Value::as_str) {
                        names.insert(name.to_string());
                    }
                }
            }
        } else if let Some(name) = tool.get("name").and_then(Value::as_str) {
            names.insert(name.to_string());
        }
    }
    names
}

/// The event's reasoning_effort is the user's CONFIG value (official:
/// turn_context.reasoning_effort), which is invisible on the wire. Reconstruction: if
/// the request's explicit effort differs from what the model would get by default
/// (suffix → models.json default), the user must have configured it — report the raw
/// value; otherwise report null (default config), exactly like an unconfigured client.
fn reported_reasoning_effort(
    body: &Value,
    model: &str,
    behavior: &ModelBehavior,
    model_db: &crate::model_db::ModelDb,
) -> Option<String> {
    let explicit = body
        .get("reasoning")
        .and_then(|r| r.get("effort"))
        .and_then(Value::as_str)?;
    let default_raw = model_db
        .effort_suffix(model)
        .unwrap_or_else(|| behavior.default_reasoning_level.clone());
    let resolved_explicit = crate::request_build::resolve_reasoning_effort(explicit, behavior);
    let resolved_default = crate::request_build::resolve_reasoning_effort(&default_raw, behavior);
    if resolved_explicit != resolved_default {
        Some(explicit.to_string())
    } else {
        None
    }
}

/// reasoning_summary: config value or "auto" (official default). Same reconstruction:
/// an explicit summary that differs from the model default is a user configuration.
fn reported_reasoning_summary(body: &Value, behavior: &ModelBehavior) -> Option<String> {
    let Some(explicit) = body
        .get("reasoning")
        .and_then(|r| r.get("summary"))
        .and_then(Value::as_str)
    else {
        return Some("auto".to_string());
    };
    if explicit == behavior.default_reasoning_summary {
        // Indistinguishable from default config → official reports "auto".
        Some("auto".to_string())
    } else if explicit == "none" {
        // reasoning_summary_mode: Some(ReasoningSummary::None) → null.
        None
    } else {
        Some(explicit.to_string())
    }
}

impl TurnTracker {
    pub fn new(env: TurnEnv) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            env,
        }
    }

    /// Classify an upstream attempt of a regular /responses request: continue / retry
    /// the open turn for this (session, account), or finalize the displaced one and
    /// start a new turn. Returns the turn_id to put on the wire.
    ///
    /// Anonymous contexts (empty session key) get an ephemeral session: every request
    /// is its own one-shot thread (official exec), nothing is remembered.
    pub fn begin_attempt(
        &self,
        attempt_session: &SessionCtx,
        account: &str,
        body: &Value,
        model: &str,
        behavior: &ModelBehavior,
        model_db: &crate::model_db::ModelDb,
    ) -> AttemptStart {
        let mut emissions = Vec::new();
        if attempt_session.key.is_empty() {
            // Keyless request: one-shot session, minted per request and dropped as soon
            // as its turn is finalized (official exec: a thread that lives one turn).
            let anon_key = uuid::Uuid::now_v7().to_string();
            let mut map = self.sessions.lock().unwrap();
            let session = map
                .entry(tracker_key(account, &anon_key))
                .or_insert_with(|| TurnSession::new_ephemeral(account));
            let turn = self.start_new_turn(
                session,
                attempt_session,
                body,
                model,
                behavior,
                model_db,
                &mut emissions,
            );
            let turn_id = turn.turn_id.clone();
            session.open = Some(turn);
            return AttemptStart {
                turn_id,
                emissions,
                anon_key: Some(anon_key),
            };
        }

        let key = tracker_key(account, &attempt_session.key);
        let mut map = self.sessions.lock().unwrap();
        let session = map.entry(key).or_insert_with(|| TurnSession::new(account));
        session.last_touch = Instant::now();

        // A failed turn this session gave up on (grace expired) ends officially before
        // anything new starts for the same session.
        if let Some(emission) = self.take_grace_expired(session) {
            emissions.push(emission);
        }

        let fingerprint = input_fingerprint(body);
        let tail_ids = tail_tool_output_call_ids(body);

        if let Some(open) = session.open.take() {
            let mut open = open;
            match open.outcome {
                Outcome::Failed | Outcome::InFlight
                    if fingerprint == open.input_fingerprint =>
                {
                    // Same input again after a failed/unfinished sampling: official
                    // retry within the same turn.
                    open.retry_count += 1;
                    open.outcome = Outcome::InFlight;
                    open.failure = None;
                    open.failed_since = None;
                    open.current_sampling_start = Some(Instant::now());
                    open.last_activity = Instant::now();
                    let turn_id = open.turn_id.clone();
                    session.open = Some(open);
                    return AttemptStart {
                        turn_id,
                        emissions,
                        anon_key: None,
                    };
                }
                Outcome::PendingCalls
                    if !tail_ids.is_empty()
                        && tail_ids.iter().all(|id| open.pending_calls.contains(id)) =>
                {
                    // Tool outputs answering the open turn's calls: next sampling.
                    open.sampling_count += 1;
                    if let Some(end) = open.last_response_end {
                        open.tool_blocking_ms = open
                            .tool_blocking_ms
                            .saturating_add(end.elapsed().as_millis() as u64);
                    }
                    open.pending_calls.clear();
                    open.outcome = Outcome::InFlight;
                    open.current_sampling_start = Some(Instant::now());
                    open.last_activity = Instant::now();
                    open.input_fingerprint = fingerprint;
                    let turn_id = open.turn_id.clone();
                    session.open = Some(open);
                    return AttemptStart {
                        turn_id,
                        emissions,
                        anon_key: None,
                    };
                }
                _ => {
                    // Displaced: the open turn never reached a final answer.
                    let final_kind = DisplacedFinal::from_open_turn(&open);
                    emissions.push(self.finalize_turn(session, open, final_kind));
                }
            }
        }

        let turn = self.start_new_turn(
            session,
            attempt_session,
            body,
            model,
            behavior,
            model_db,
            &mut emissions,
        );
        let turn_id = turn.turn_id.clone();
        session.open = Some(turn);
        AttemptStart {
            turn_id,
            emissions,
            anon_key: None,
        }
    }

    fn start_new_turn(
        &self,
        session: &mut TurnSession,
        attempt_session: &SessionCtx,
        body: &Value,
        model: &str,
        behavior: &ModelBehavior,
        model_db: &crate::model_db::ModelDb,
        emissions: &mut Vec<Emission>,
    ) -> Box<OpenTurn> {
        if !session.thread_announced {
            session.thread_announced = true;
            emissions.push(Emission::ThreadInitialized(Box::new(
                self.thread_initialized_params(attempt_session, model),
            )));
        }
        let is_first_turn = session.turns_seen == 0;
        session.turns_seen += 1;
        let now = Instant::now();
        Box::new(OpenTurn {
            turn_id: uuid::Uuid::now_v7().to_string(),
            thread_id: attempt_session.thread_id.to_string(),
            session_id: attempt_session.session_id.to_string(),
            model: model.to_string(),
            reasoning_effort: reported_reasoning_effort(body, model, behavior, model_db),
            reasoning_summary: reported_reasoning_summary(body, behavior),
            num_input_images: count_turn_input_images(body),
            is_first_turn,
            declared_tools: declared_tool_names(body),
            started: now,
            started_at_unix: now_unix(),
            last_activity: now,
            last_activity_unix: now_unix(),
            before_first_sampling_ms: 0,
            sampling_ms: 0,
            compaction_ms: 0,
            tool_blocking_ms: 0,
            current_sampling_start: Some(now),
            last_response_end: None,
            sampling_count: 1,
            retry_count: 0,
            pending_calls: HashSet::new(),
            counts: ToolCounts::default(),
            usage: TokenAccum::default(),
            saw_any_usage: false,
            ttft_ms: None,
            ttfm_ms: None,
            input_fingerprint: input_fingerprint(body),
            outcome: Outcome::InFlight,
            failure: None,
            failed_since: None,
        })
    }

    fn thread_initialized_params(
        &self,
        attempt_session: &SessionCtx,
        model: &str,
    ) -> ThreadInitializedEventParams {
        ThreadInitializedEventParams {
            thread_id: attempt_session.thread_id.to_string(),
            session_id: attempt_session.session_id.to_string(),
            app_server_client: self.env.app_client.clone(),
            runtime: self.env.runtime.clone(),
            model: model.to_string(),
            ephemeral: false,
            thread_source: Some("user"),
            initialization_mode: "new",
            subagent_source: None,
            parent_thread_id: None,
            forked_from_thread_id: None,
            created_at: now_unix(),
        }
    }

    /// The request body failed to build (downstream semantic error → relay 400,
    /// upstream never contacted). Remove a just-started turn with no traffic.
    pub fn note_attempt_aborted(&self, session_key: &str, account: &str, turn_id: &str) {
        if session_key.is_empty() {
            return;
        }
        let key = tracker_key(account, session_key);
        let mut map = self.sessions.lock().unwrap();
        if let Some(session) = map.get_mut(&key)
            && let Some(open) = session.open.as_ref()
            && open.turn_id == turn_id
            && !open.saw_any_usage
            && open.counts.total == 0
        {
            session.open = None;
        }
    }

    /// The attempt ended with an upstream-visible failure (HTTP reject after the
    /// official 401-refresh retry, or a transport error). The turn is marked failed
    /// and held for the retry grace: an identical following request continues it.
    pub fn note_attempt_failed(
        &self,
        session_key: &str,
        account: &str,
        turn_id: &str,
        failure: TurnFailure,
    ) {
        if session_key.is_empty() {
            return;
        }
        let key = tracker_key(account, session_key);
        let mut map = self.sessions.lock().unwrap();
        if let Some(session) = map.get_mut(&key) {
            session.last_touch = Instant::now();
            if let Some(open) = session.open.as_mut()
                && open.turn_id == turn_id
            {
                if let Some(start) = open.current_sampling_start.take() {
                    open.sampling_ms = open
                        .sampling_ms
                        .saturating_add(start.elapsed().as_millis() as u64);
                }
                open.outcome = Outcome::Failed;
                open.failure = Some(failure);
                open.failed_since = Some(Instant::now());
                open.last_activity = Instant::now();
                open.last_activity_unix = now_unix();
            }
        }
    }

    /// The upstream response stream reached EOF (called from the gateway stream tail).
    /// Updates sampling/usage/tool accounting and finalizes the turn if this response
    /// was the turn's final answer.
    pub fn note_response_end(
        &self,
        session_key: &str,
        account: &str,
        tap: &SseTap,
    ) -> Vec<Emission> {
        let mut emissions = Vec::new();
        if session_key.is_empty() {
            return emissions;
        }
        let key = tracker_key(account, session_key);
        let mut map = self.sessions.lock().unwrap();
        let Some(session) = map.get_mut(&key) else {
            return emissions;
        };
        session.last_touch = Instant::now();
        let Some(mut open) = session.open.take() else {
            return emissions;
        };

        let now = Instant::now();
        let stream_end = tap.last_chunk_at.unwrap_or(now);
        if let Some(start) = open.current_sampling_start.take() {
            open.sampling_ms = open.sampling_ms.saturating_add(
                stream_end
                    .saturating_duration_since(start)
                    .as_millis() as u64,
            );
        }
        if let Some(usage) = tap.usage.as_ref() {
            let parsed = TokenAccum::from_wire(usage);
            session.cumulative_total_tokens += parsed.total_tokens;
            open.usage.add_assign(&parsed);
            open.saw_any_usage = true;
        }
        let mut declared = std::mem::take(&mut open.declared_tools);
        for (call_id, name, item_type) in &tap.tool_calls {
            let _ = call_id;
            open.counts.record(item_type, name, &declared);
        }
        declared.clear();
        open.declared_tools = declared;
        open.pending_calls = tap
            .tool_calls
            .iter()
            .filter(|(_, _, item_type)| item_type != "web_search_call" && item_type != "image_generation_call")
            .map(|(call_id, _, _)| call_id.clone())
            .filter(|call_id| !call_id.is_empty())
            .collect();
        if open.ttft_ms.is_none() {
            open.ttft_ms = tap.first_contentful_ms;
        }
        if open.ttfm_ms.is_none() {
            open.ttfm_ms = tap.first_agent_message_ms;
        }
        open.last_response_end = Some(stream_end);
        open.last_activity = now;
        open.last_activity_unix = now_unix();

        if tap.completed && open.pending_calls.is_empty() {
            // Final answer: the turn is complete.
            emissions.push(self.finalize_turn(session, open, DisplacedFinal::completed()));
        } else if tap.completed {
            open.outcome = Outcome::PendingCalls;
            session.open = Some(open);
        } else if let Some(error) = tap.failed.as_ref() {
            open.outcome = Outcome::Failed;
            open.failure = Some(failure_from_stream_error(Some(error)));
            open.failed_since = Some(now);
            session.open = Some(open);
        } else {
            // Stream truncated without a terminal event: officially a retryable
            // Stream error — hold for the retry grace like any failed sampling.
            open.outcome = Outcome::Failed;
            open.failure = Some(failure_from_transport(TransportFailureKind::Other));
            open.failed_since = Some(now);
            session.open = Some(open);
        }
        emissions
    }

    /// Start of a compaction request (v2 trigger over /responses or legacy
    /// /responses/compact): not a turn — captures the context-token baseline and a
    /// fresh v7 turn_id for the wire (the official compact task's turn id).
    pub fn begin_compaction(
        &self,
        attempt_session: &SessionCtx,
        account: &str,
        model: &str,
    ) -> CompactionStart {
        let mut emissions = Vec::new();
        let before = if attempt_session.key.is_empty() {
            0
        } else {
            let key = tracker_key(account, &attempt_session.key);
            let mut map = self.sessions.lock().unwrap();
            let session = map.entry(key).or_insert_with(|| TurnSession::new(account));
            session.last_touch = Instant::now();
            if !session.thread_announced {
                session.thread_announced = true;
                emissions.push(Emission::ThreadInitialized(Box::new(
                    self.thread_initialized_params(attempt_session, model),
                )));
            }
            session.cumulative_total_tokens
        };
        CompactionStart {
            turn_id: uuid::Uuid::now_v7().to_string(),
            before_tokens: before,
            started_at_unix: now_unix(),
            started: Instant::now(),
            emissions,
        }
    }

    /// Compaction request finished (success or upstream-visible failure): emit the
    /// codex_compaction_event with official v2/legacy field semantics. A compaction
    /// running INSIDE an open turn (phase pre_turn/mid_turn) books its duration into
    /// that turn's compaction_ms (official TurnProfile compaction bucket).
    pub fn note_compaction_end(
        &self,
        attempt_session: &SessionCtx,
        account: &str,
        start: &CompactionStart,
        meta: &Value,
        implementation: &str,
        model: &str,
        outcome: CompactionOutcome,
    ) -> Vec<Emission> {
        let mut cumulative_after = start.before_tokens;
        let phase = meta
            .get("phase")
            .and_then(Value::as_str)
            .unwrap_or("standalone_turn")
            .to_string();
        let duration_ms = u64::try_from(start.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if !attempt_session.key.is_empty() {
            let key = tracker_key(account, &attempt_session.key);
            let mut map = self.sessions.lock().unwrap();
            let session = map.entry(key).or_insert_with(|| TurnSession::new(account));
            session.last_touch = Instant::now();
            if let Some(usage) = outcome.usage.as_ref() {
                session.cumulative_total_tokens += usage.total_tokens;
            }
            cumulative_after = session.cumulative_total_tokens;
            if phase != "standalone_turn"
                && let Some(open) = session.open.as_mut()
            {
                open.compaction_ms = open.compaction_ms.saturating_add(duration_ms);
            }
        } else if let Some(usage) = outcome.usage.as_ref() {
            cumulative_after = start.before_tokens + usage.total_tokens;
        }

        let usage = outcome.usage.unwrap_or_default();
        let is_v2 = implementation == "responses_compaction_v2";
        // Official v2 override (compact_remote_v2.rs): before = response input tokens,
        // summary/cached/cache_write from the response usage. Legacy remote compact
        // leaves them null (compact_remote.rs default details).
        let (before, summary_tokens, cached, cache_write) = if is_v2 {
            (
                usage.input_tokens,
                Some(usage.output_tokens),
                Some(usage.cached_input_tokens),
                Some(usage.cache_write_input_tokens),
            )
        } else {
            (start.before_tokens, None, None, None)
        };
        let after = cumulative_after;

        let meta_str = |key: &str, default: &str| {
            meta.get(key)
                .and_then(Value::as_str)
                .unwrap_or(default)
                .to_string()
        };
        let status = outcome.status;
        let (kind, http_status) = match &outcome.failure {
            Some(f) => (Some(f.kind), f.http_status),
            None => (None, None),
        };
        let completed_at = now_unix();
        let compact_type = match implementation {
            "responses_compaction_v2" => "remote_v2",
            "responses_compact" => "remote",
            _ => "local",
        };
        let trigger = meta_str("trigger", "manual");

        vec![Emission::Compaction(Box::new(CompactionEmission {
            manual: trigger == "manual",
            compact_type,
            model: model.to_string(),
            params: CodexCompactionEventParams {
                thread_id: attempt_session.thread_id.to_string(),
                session_id: attempt_session.session_id.to_string(),
                turn_id: start.turn_id.clone(),
                app_server_client: self.env.app_client.clone(),
                runtime: self.env.runtime.clone(),
                thread_source: Some("user"),
                subagent_source: None,
                parent_thread_id: None,
                trigger,
                reason: meta_str("reason", "user_requested"),
                implementation: implementation.to_string(),
                phase,
                strategy: meta_str("strategy", "memento"),
                status,
                codex_error_kind: kind,
                codex_error_http_status_code: http_status,
                active_context_tokens_before: before,
                active_context_tokens_after: after,
                retained_image_count: None,
                compaction_summary_tokens: summary_tokens,
                cached_input_tokens: cached,
                cache_write_input_tokens: cache_write,
                started_at: start.started_at_unix,
                completed_at,
                duration_ms: Some(duration_ms),
            },
        }))]
    }

    /// Build the turn event for a finished turn and remove it from the session.
    fn finalize_turn(
        &self,
        session: &mut TurnSession,
        turn: Box<OpenTurn>,
        final_kind: DisplacedFinal,
    ) -> Emission {
        let (status, failure, completed_at, duration_ms) = match final_kind {
            DisplacedFinal::Completed => (
                "completed",
                None,
                now_unix(),
                turn.started.elapsed().as_millis() as u64,
            ),
            DisplacedFinal::Interrupted => (
                "interrupted",
                None,
                turn.last_activity_unix,
                turn.last_activity.saturating_duration_since(turn.started).as_millis() as u64,
            ),
            DisplacedFinal::Failed => (
                "failed",
                turn.failure.clone(),
                turn.last_activity_unix,
                turn.last_activity.saturating_duration_since(turn.started).as_millis() as u64,
            ),
        };
        let (kind, http_status, info) = match &failure {
            Some(f) => (Some(f.kind), f.http_status, Some(f.info.clone())),
            None => (None, None, None),
        };
        let usage = turn.saw_any_usage.then_some(turn.usage);
        let _ = session;
        Emission::Turn(Box::new(TurnEmission {
            ttft_ms: turn.ttft_ms,
            ttfm_ms: turn.ttfm_ms,
            params: CodexTurnEventParams {
            thread_id: turn.thread_id.clone(),
            session_id: turn.session_id.clone(),
            turn_id: turn.turn_id.clone(),
            root_turn_id: None,
            turn_trigger: None,
            codex_turn_source: None,
            submission_type: None,
            app_server_client: self.env.app_client.clone(),
            runtime: self.env.runtime.clone(),
            ephemeral: false,
            thread_source: Some("user"),
            initialization_mode: "new",
            subagent_source: None,
            parent_thread_id: None,
            model: Some(turn.model.clone()),
            model_provider: "openai".to_string(),
            sandbox_policy: Some("read_only"),
            reasoning_effort: turn.reasoning_effort.clone(),
            reasoning_summary: turn.reasoning_summary.clone(),
            service_tier: "default".to_string(),
            approval_policy: "on-request".to_string(),
            approvals_reviewer: "user".to_string(),
            guardian_v2_enabled: false,
            sandbox_network_access: false,
            collaboration_mode: Some("default"),
            personality: None,
            workspace_kind: None,
            num_input_images: turn.num_input_images,
            image_preparations: Vec::new(),
            is_first_turn: turn.is_first_turn,
            status: Some(status),
            explicit_client_interrupt_requested_at_ms: None,
            turn_error: info,
            codex_error_kind: kind,
            codex_error_http_status_code: http_status,
            steer_count: Some(0),
            total_tool_call_count: Some(turn.counts.total),
            shell_command_count: Some(turn.counts.shell_command),
            file_change_count: Some(turn.counts.file_change),
            mcp_tool_call_count: Some(turn.counts.mcp_tool_call),
            dynamic_tool_call_count: Some(turn.counts.dynamic_tool_call),
            subagent_tool_call_count: Some(turn.counts.subagent_tool_call),
            web_search_count: Some(turn.counts.web_search),
            image_generation_count: Some(turn.counts.image_generation),
            input_tokens: usage.map(|u| u.input_tokens),
            cached_input_tokens: usage.map(|u| u.cached_input_tokens),
            cache_write_input_tokens: usage.map(|u| u.cache_write_input_tokens),
            output_tokens: usage.map(|u| u.output_tokens),
            reasoning_output_tokens: usage.map(|u| u.reasoning_output_tokens),
            total_tokens: usage.map(|u| u.total_tokens),
            before_first_sampling_ms: turn.before_first_sampling_ms,
            sampling_ms: turn.sampling_ms,
            compaction_ms: turn.compaction_ms,
            between_sampling_overhead_ms: 0,
            tool_blocking_ms: turn.tool_blocking_ms,
            after_last_sampling_ms: 0,
            sampling_request_count: turn.sampling_count,
            sampling_retry_count: turn.retry_count,
            duration_ms: Some(duration_ms),
            started_at: Some(turn.started_at_unix),
            completed_at: Some(completed_at),
            },
        }))
    }

    /// TTL sweep: silently drop turns idle past the TTL (official process-kill
    /// mid-turn analog — no event leaves the process) and evict dead sessions.
    /// Emits the turn event of a failed turn nobody retried: officially the turn ends
    /// the moment the client sees the error, and the client's own retry lands inside
    /// the same turn. The relay only learns "retry or give up" from the *next* request,
    /// so a failed turn is held for FAILED_GRACE and then finalized as failed.
    fn take_grace_expired(&self, session: &mut TurnSession) -> Option<Emission> {
        let expired = session.open.as_ref().is_some_and(|open| {
            matches!(open.outcome, Outcome::Failed)
                && open
                    .failed_since
                    .is_some_and(|t| t.elapsed() >= FAILED_GRACE)
        });
        if !expired {
            return None;
        }
        let open = session.open.take()?;
        Some(self.finalize_turn(session, open, DisplacedFinal::Failed))
    }

    /// Periodic maintenance: finalize grace-expired failures, drop turns idle past the
    /// open-turn TTL (silent, the process-kill analog) and sessions past SESSION_TTL.
    /// Returns emissions for the caller to fan out per account.
    pub fn sweep(&self) -> Vec<(String, Emission)> {
        let mut out = Vec::new();
        let mut map = self.sessions.lock().unwrap();
        let mut expired_keys = Vec::new();
        for (key, session) in map.iter_mut() {
            if let Some(emission) = self.take_grace_expired(session) {
                out.push((session.account.clone(), emission));
            }
            if let Some(open) = session.open.as_ref()
                && open.last_activity.elapsed() > OPEN_TURN_TTL
            {
                session.open = None;
            }
            if session.last_touch.elapsed() >= SESSION_TTL
                || (session.ephemeral && session.open.is_none())
            {
                expired_keys.push(key.clone());
            }
        }
        for key in expired_keys {
            map.remove(&key);
        }
        out
    }
}

#[derive(Default)]
enum DisplacedFinal {
    Completed,
    #[default]
    Interrupted,
    Failed,
}

impl DisplacedFinal {
    fn completed() -> Self {
        Self::Completed
    }
}

impl DisplacedFinal {
    fn from_open_turn(turn: &OpenTurn) -> Self {
        match turn.outcome {
            Outcome::Failed => Self::Failed,
            _ => Self::Interrupted,
        }
    }
}
