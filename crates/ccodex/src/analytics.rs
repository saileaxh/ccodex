//! Official-client analytics events channel (a faithful codex-analytics replica):
//! `codex_thread_initialized` / `codex_turn_event` / `codex_compaction_event` POSTed to
//! `{chatgpt_base}/codex/analytics-events/events` with the exact official JSON shapes,
//! auth headers, transport, and queue semantics (analytics/src/client.rs):
//!
//! - mpsc queue 256, `try_send`; full → warn + drop ("dropping analytics events: queue is full")
//! - one worker per account; each event is POSTed immediately (one POST per event)
//! - fresh `create_client()` per POST (official UA + originator, env proxy), 10s timeout
//! - headers: `auth_provider_from_auth(auth).to_auth_headers()` (Bearer + ChatGPT-Account-ID)
//!   + `Content-Type: application/json`; auth re-read from the AuthManager per send
//! - api-key auth → events dropped (official only lets plugin events through; we emit none);
//!   non-codex-backend auth → dropped
//! - non-2xx / network failure → warn, no retry (official behavior)
//!
//! JSON field order matches the official struct declaration order (serde preserve_order);
//! the three replicated events have no skip_serializing_if, so None serializes as null —
//! exactly like the official client. Every value comes from real relayed traffic or
//! official default-config constants; nothing is fabricated (see docs/AUDIT.md).

use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::mpsc;

const ANALYTICS_EVENTS_QUEUE_SIZE: usize = 256;
const ANALYTICS_EVENTS_TIMEOUT: Duration = Duration::from_secs(10);
/// Official default config.chatgpt_base_url (config/src/lib.rs).
const DEFAULT_CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api";

// ---------------------------------------------------------------------------
// Exact official JSON shapes (analytics/src/events.rs, declaration order).
// ---------------------------------------------------------------------------

#[derive(Clone, Serialize)]
pub struct CodexRuntimeMetadata {
    pub codex_rs_version: String,
    pub runtime_os: String,
    pub runtime_os_version: String,
    pub runtime_arch: String,
}

#[derive(Clone, Serialize)]
pub struct CodexAppServerClientMetadata {
    pub product_client_id: String,
    pub client_name: Option<String>,
    pub client_version: Option<String>,
    /// AppServerRpcTransport snake_case; the TUI drives the app-server in-process.
    pub rpc_transport: &'static str,
    pub experimental_api_enabled: Option<bool>,
}

#[derive(Serialize)]
pub struct ThreadInitializedEventParams {
    pub thread_id: String,
    pub session_id: String,
    pub app_server_client: CodexAppServerClientMetadata,
    pub runtime: CodexRuntimeMetadata,
    pub model: String,
    pub ephemeral: bool,
    /// ThreadSource string enum ("user" for the CLI).
    pub thread_source: Option<&'static str>,
    /// ThreadInitializationMode snake_case: "new" | "resumed" | "forked".
    pub initialization_mode: &'static str,
    pub subagent_source: Option<String>,
    pub parent_thread_id: Option<String>,
    pub forked_from_thread_id: Option<String>,
    pub created_at: u64,
}

#[derive(Serialize)]
pub struct ThreadInitializedEvent {
    pub event_type: &'static str,
    pub event_params: ThreadInitializedEventParams,
}

#[derive(Serialize)]
pub struct CodexCompactionEventParams {
    pub thread_id: String,
    pub session_id: String,
    pub turn_id: String,
    pub app_server_client: CodexAppServerClientMetadata,
    pub runtime: CodexRuntimeMetadata,
    pub thread_source: Option<&'static str>,
    pub subagent_source: Option<String>,
    pub parent_thread_id: Option<String>,
    /// CompactionTrigger: "manual" | "auto".
    pub trigger: String,
    /// CompactionReason: "user_requested" | "context_limit" | "model_downshift" | "comp_hash_changed".
    pub reason: String,
    /// CompactionImplementation: "responses" | "responses_compaction_v2" | "responses_compact".
    pub implementation: String,
    /// CompactionPhase: "standalone_turn" | "pre_turn" | "mid_turn".
    pub phase: String,
    /// CompactionStrategy: "memento".
    pub strategy: String,
    /// CompactionStatus: "completed" | "failed" | "interrupted".
    pub status: &'static str,
    /// CodexErrKind snake_case.
    pub codex_error_kind: Option<&'static str>,
    pub codex_error_http_status_code: Option<u16>,
    pub active_context_tokens_before: i64,
    pub active_context_tokens_after: i64,
    pub retained_image_count: Option<usize>,
    pub compaction_summary_tokens: Option<i64>,
    pub cached_input_tokens: Option<i64>,
    pub cache_write_input_tokens: Option<i64>,
    pub started_at: u64,
    pub completed_at: u64,
    pub duration_ms: Option<u64>,
}

#[derive(Serialize)]
pub struct CodexCompactionEventRequest {
    pub event_type: &'static str,
    pub event_params: CodexCompactionEventParams,
}

#[derive(Serialize)]
pub struct CodexTurnEventParams {
    pub thread_id: String,
    pub session_id: String,
    pub turn_id: String,
    pub root_turn_id: Option<String>,
    pub turn_trigger: Option<String>,
    pub codex_turn_source: Option<String>,
    pub submission_type: Option<String>,
    pub app_server_client: CodexAppServerClientMetadata,
    pub runtime: CodexRuntimeMetadata,
    pub ephemeral: bool,
    pub thread_source: Option<&'static str>,
    pub initialization_mode: &'static str,
    pub subagent_source: Option<String>,
    pub parent_thread_id: Option<String>,
    pub model: Option<String>,
    pub model_provider: String,
    /// sandbox_policy_mode: "read_only" | "workspace_write" | "external_sandbox" | "full_access".
    pub sandbox_policy: Option<&'static str>,
    pub reasoning_effort: Option<String>,
    pub reasoning_summary: Option<String>,
    pub service_tier: String,
    pub approval_policy: String,
    pub approvals_reviewer: String,
    pub guardian_v2_enabled: bool,
    pub sandbox_network_access: bool,
    pub collaboration_mode: Option<&'static str>,
    pub personality: Option<String>,
    pub workspace_kind: Option<String>,
    pub num_input_images: usize,
    /// ImagePreparationMetadata list; empty unless the client prepared images.
    pub image_preparations: Vec<serde_json::Value>,
    pub is_first_turn: bool,
    /// TurnStatus snake_case: "completed" | "failed" | "interrupted".
    pub status: Option<&'static str>,
    pub explicit_client_interrupt_requested_at_ms: Option<u64>,
    /// CodexErrorInfo (app-server protocol camelCase JSON).
    pub turn_error: Option<serde_json::Value>,
    pub codex_error_kind: Option<&'static str>,
    pub codex_error_http_status_code: Option<u16>,
    pub steer_count: Option<usize>,
    pub total_tool_call_count: Option<usize>,
    pub shell_command_count: Option<usize>,
    pub file_change_count: Option<usize>,
    pub mcp_tool_call_count: Option<usize>,
    pub dynamic_tool_call_count: Option<usize>,
    pub subagent_tool_call_count: Option<usize>,
    pub web_search_count: Option<usize>,
    pub image_generation_count: Option<usize>,
    pub input_tokens: Option<i64>,
    pub cached_input_tokens: Option<i64>,
    pub cache_write_input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub reasoning_output_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    pub before_first_sampling_ms: u64,
    pub sampling_ms: u64,
    pub compaction_ms: u64,
    pub between_sampling_overhead_ms: u64,
    pub tool_blocking_ms: u64,
    pub after_last_sampling_ms: u64,
    pub sampling_request_count: u32,
    pub sampling_retry_count: u32,
    pub duration_ms: Option<u64>,
    pub started_at: Option<u64>,
    pub completed_at: Option<u64>,
}

#[derive(Serialize)]
pub struct CodexTurnEventRequest {
    pub event_type: &'static str,
    pub event_params: CodexTurnEventParams,
}

/// Untagged exactly like the official enum; only the three replicated variants exist.
#[derive(Serialize)]
#[serde(untagged)]
pub enum TrackEventRequest {
    ThreadInitialized(ThreadInitializedEvent),
    Compaction(Box<CodexCompactionEventRequest>),
    TurnEvent(Box<CodexTurnEventRequest>),
}

#[derive(Serialize)]
struct TrackEventsRequest {
    events: Vec<TrackEventRequest>,
}

// ---------------------------------------------------------------------------
// Per-account queue + sender hub.
// ---------------------------------------------------------------------------

struct AnalyticsQueue {
    sender: mpsc::Sender<TrackEventRequest>,
    auth_manager: Arc<codex_login::AuthManager>,
}

/// Cloneable handle passed to the turn tracker / gateway for event submission.
#[derive(Clone)]
pub struct AnalyticsSender {
    sender: mpsc::Sender<TrackEventRequest>,
}

impl AnalyticsSender {
    /// Official try_send semantics: full queue → warn + drop.
    pub fn track(&self, event: TrackEventRequest) {
        if self.sender.try_send(event).is_err() {
            tracing::warn!("dropping analytics events: queue is full");
        }
    }
}

pub struct AnalyticsHub {
    url: String,
    /// Proxy bindings, resolved per send so an account's events always leave through the
    /// same exit IP as that account's conversation traffic.
    proxies: Arc<crate::proxies::ProxyStore>,
    queues: RwLock<HashMap<String, AnalyticsQueue>>,
}

impl AnalyticsHub {
    pub fn new(proxies: Arc<crate::proxies::ProxyStore>) -> Self {
        // Dev self-check: point the analytics channel at a local mock upstream.
        let base = std::env::var("CCODEX_ANALYTICS_BASE_URL_OVERRIDE")
            .unwrap_or_else(|_| DEFAULT_CHATGPT_BASE_URL.to_string());
        let base = base.trim_end_matches('/');
        Self {
            url: format!("{base}/codex/analytics-events/events"),
            proxies,
            queues: RwLock::new(HashMap::new()),
        }
    }

    /// Queue for the account, spawning the worker on first use. A re-loaded account
    /// (new AuthManager after pool reload / re-login) gets a fresh worker so sends
    /// always authenticate with live credentials (official: one client per process).
    pub fn sender_for(&self, account: &crate::accounts::Account) -> AnalyticsSender {
        if let Some(queue) = self.queues.read().unwrap().get(&account.name)
            && Arc::ptr_eq(&queue.auth_manager, &account.auth_manager)
        {
            return AnalyticsSender {
                sender: queue.sender.clone(),
            };
        }
        let mut map = self.queues.write().unwrap();
        let queue = map.entry(account.name.clone()).or_insert_with(|| {
            spawn_queue(
                account.auth_manager.clone(),
                account.name.clone(),
                self.url.clone(),
                self.proxies.clone(),
            )
        });
        if !Arc::ptr_eq(&queue.auth_manager, &account.auth_manager) {
            *queue = spawn_queue(
                account.auth_manager.clone(),
                account.name.clone(),
                self.url.clone(),
                self.proxies.clone(),
            );
        }
        AnalyticsSender {
            sender: queue.sender.clone(),
        }
    }
}

fn spawn_queue(
    auth_manager: Arc<codex_login::AuthManager>,
    account: String,
    url: String,
    proxies: Arc<crate::proxies::ProxyStore>,
) -> AnalyticsQueue {
    let (sender, mut receiver) = mpsc::channel::<TrackEventRequest>(ANALYTICS_EVENTS_QUEUE_SIZE);
    let worker_auth_manager = auth_manager.clone();
    tokio::spawn(async move {
        while let Some(event) = receiver.recv().await {
            send_track_events(&worker_auth_manager, &account, &url, &proxies, event).await;
        }
    });
    AnalyticsQueue {
        sender,
        auth_manager,
    }
}

/// Official send_track_events + send_track_events_request: auth re-read per send,
/// api-key / non-codex-backend auth drops, fresh client per POST, 10s timeout,
/// warn-and-move-on on failure (no retry).
async fn send_track_events(
    auth_manager: &codex_login::AuthManager,
    account: &str,
    url: &str,
    proxies: &crate::proxies::ProxyStore,
    event: TrackEventRequest,
) {
    let Some(auth) = auth_manager.auth().await else {
        return;
    };
    if auth.is_api_key_auth() || !auth.uses_codex_backend() {
        return;
    }

    let payload = TrackEventsRequest {
        events: vec![event],
    };
    // The official client posts telemetry from the same process — and therefore the same
    // proxy — as its API traffic. reqwest resolves the system proxy when the client is
    // built, so the client is built inside the account's binding swap; the send itself
    // runs outside it (the proxy is already baked into the client), so one slow POST
    // cannot hold up another account's telemetry.
    let client = crate::proxies::with_binding(
        &proxies.binding(account),
        codex_login::default_client::create_client,
    )
    .await;
    let response = client
        .post(url)
        .timeout(ANALYTICS_EVENTS_TIMEOUT)
        .headers(codex_model_provider::auth_provider_from_auth(&auth).to_auth_headers())
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await;

    match response {
        Ok(response) if response.status().is_success() => {}
        Ok(response) => {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            tracing::warn!("events failed with status {status}: {body}");
        }
        Err(err) => {
            tracing::warn!("failed to send events request: {err}");
        }
    }
}

// ---------------------------------------------------------------------------
// Static per-process metadata shared by all events.
// ---------------------------------------------------------------------------

/// Official CodexRuntimeMetadata (events.rs current_runtime_metadata), with the
/// reported codex version following the relay's baked identity (same value the
/// official release binary reports via CARGO_PKG_VERSION).
pub fn current_runtime_metadata() -> CodexRuntimeMetadata {
    let os_info = os_info::get();
    CodexRuntimeMetadata {
        codex_rs_version: crate::identity::codex_version(),
        runtime_os: std::env::consts::OS.to_string(),
        runtime_os_version: os_info.version().to_string(),
        runtime_arch: std::env::consts::ARCH.to_string(),
    }
}

/// Official TUI-embedded app-server client metadata: product_client_id = originator,
/// client_name "codex-tui", in-process transport, experimental api on.
pub fn tui_app_server_client_metadata() -> CodexAppServerClientMetadata {
    CodexAppServerClientMetadata {
        product_client_id: codex_login::default_client::originator().value,
        client_name: Some("codex-tui".to_string()),
        client_version: Some(crate::identity::codex_version()),
        rpc_transport: "in_process",
        experimental_api_enabled: Some(true),
    }
}
