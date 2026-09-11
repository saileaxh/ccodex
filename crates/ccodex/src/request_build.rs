//! Upstream request construction: instead of sanitizing/forwarding the downstream JSON,
//! the body is assembled from scratch following official 0.153.x `build_responses_request`
//! (core/src/client.rs).
//!
//! Downstream supplies only semantic content (model / instructions / input / tools /
//! reasoning / text); every other field and header is synthesized here per official
//! default configuration (CLI first-turn user turn):
//!
//! - Field set and order = codex-api ResponsesApiRequest struct order
//! - Lite models (models.json use_responses_lite): instructions/tools become input prefix
//!   items (additional_tools + developer message, deterministic v5 ids),
//!   parallel_tool_calls=false, reasoning.context="all_turns", plus the
//!   x-openai-internal-codex-responses-lite header
//! - reasoning / text.verbosity / node_repl flags follow per-model defaults from models.json
//! - client_metadata and x-codex-turn-metadata replicate the official shape
//!   (installation_id persisted per account dir, window_id="{thread_id}:0", turn_id=v7)

use crate::model_db::{ModelBehavior, ModelDb};
use http::{HeaderMap, HeaderValue};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use uuid::Uuid;

// Turn-metadata constants synthesized from official CLI defaults (Windows: sandbox
// disabled / read-only policy, auto_review off; session_source=Cli → agent "root";
// thread_source=user).
const AGENT_NAME: &str = "root";
const THREAD_SOURCE: &str = "user";
const SANDBOX_TAG: &str = "none";
const SANDBOX_MODE_TAG: &str = "read-only";
const REQUEST_KIND_TURN: &str = "turn";
const REQUEST_KIND_COMPACTION: &str = "compaction";

/// Official CompactionTurnMetadata fallback when the downstream client did not send its own
/// (official clients always attach one to compaction requests; we copy theirs verbatim).
/// `implementation` is "responses_compaction_v2" (trigger item over /responses) or
/// "responses_compact" (legacy /responses/compact endpoint).
pub(crate) fn default_compaction_metadata(implementation: &str) -> Value {
    json!({
        "trigger": "manual",
        "reason": "user_requested",
        "implementation": implementation,
        "phase": "standalone_turn",
        "strategy": "memento",
    })
}

#[derive(Clone)]
pub struct SessionCtx {
    /// Session stickiness key (api-key isolated, hashed); empty for anonymous requests.
    pub key: String,
    pub session_id: Uuid,
    pub thread_id: Uuid,
}

impl SessionCtx {
    /// Official window_id format: "{thread_id}:{window_number}"; the main window is always 0.
    pub fn window_id(&self) -> String {
        format!("{}:0", self.thread_id)
    }

    /// Namespace for lite prefix item ids: official prefix_namespace = v5(OID, thread_id).
    fn prefix_namespace(&self) -> Uuid {
        Uuid::new_v5(&Uuid::NAMESPACE_OID, self.thread_id.to_string().as_bytes())
    }

    /// Re-derives this context for a specific account: the same downstream session on a
    /// different account is a different upstream session. An official client's session
    /// id never appears under two accounts, so reusing ids across account switches would
    /// let the upstream correlate the accounts. Ids are remembered per (session, account)
    /// pair — stable within one account (prompt cache preserved), fresh per account.
    /// Anonymous contexts (empty key) always get fresh, unstored ids.
    pub fn for_account(&self, store: &SessionStore, account: &str) -> SessionCtx {
        if self.key.is_empty() {
            return SessionCtx {
                key: String::new(),
                session_id: Uuid::now_v7(),
                thread_id: Uuid::now_v7(),
            };
        }
        let (session_id, thread_id) = store.ids_for(&self.key, account);
        SessionCtx {
            key: self.key.clone(),
            session_id,
            thread_id,
        }
    }
}

/// Maps one downstream session to a stable session/thread id pair **per account**
/// (see SessionCtx::for_account). Official session_id/thread_id are UUID v7
/// (SessionId/ThreadId::new); to keep the upstream-visible id version identical, the
/// relay likewise generates v7 pairs. Entries idle past the TTL are lazily evicted.
pub struct SessionStore {
    by_key: std::sync::RwLock<HashMap<String, (Uuid, Uuid, std::time::Instant)>>,
    ttl: std::time::Duration,
}

impl SessionStore {
    pub fn new(ttl: std::time::Duration) -> Self {
        Self {
            by_key: std::sync::RwLock::new(HashMap::new()),
            ttl,
        }
    }

    fn ids_for(&self, session_key: &str, account: &str) -> (Uuid, Uuid) {
        let key = format!("{account}\u{1f}{session_key}");
        {
            let map = self.by_key.read().unwrap();
            if let Some((s, t, ts)) = map.get(&key)
                && ts.elapsed() < self.ttl
            {
                return (*s, *t);
            }
        }
        let mut map = self.by_key.write().unwrap();
        // Evict expired entries on write to bound long-run growth.
        let ttl = self.ttl;
        map.retain(|_, (_, _, ts)| ts.elapsed() < ttl);
        let entry = map
            .entry(key)
            .or_insert_with(|| (Uuid::now_v7(), Uuid::now_v7(), std::time::Instant::now()));
        (entry.0, entry.1)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.by_key.read().unwrap().len()
    }
}

/// Derives the session from the downstream request: client-supplied session identifiers
/// win, otherwise random. With session_isolation the downstream api key is mixed in so
/// same-named sessions from different users can't be correlated upstream. The id pair is
/// remembered in the SessionStore: later requests of the same session reuse the v7 ids
/// (preserves prompt cache and stickiness).
pub fn derive_session(
    api_key: &str,
    body: &Value,
    headers: &http::HeaderMap,
    session_isolation: bool,
    store: &SessionStore,
) -> SessionCtx {
    let header_str = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let body_str = |key: &str| {
        body.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let seed = header_str("session-id")
        .or_else(|| header_str("conversation_id"))
        .or_else(|| header_str("thread-id"))
        .or_else(|| body_str("prompt_cache_key"))
        .or_else(|| body_str("previous_response_id"))
        // alpha/search SearchRequest: the official executor puts the conversation session
        // id in the body's `id` field (no session headers on that endpoint).
        .or_else(|| body_str("id"));

    let key_material = |seed: &str| {
        if session_isolation {
            format!("{api_key}:{seed}")
        } else {
            seed.to_string()
        }
    };

    match seed {
        Some(seed) => {
            let digest = Sha256::digest(key_material(&seed).as_bytes());
            let key = hex_short(&digest);
            // Base ids; forward re-derives per account via for_account before any
            // upstream use.
            let (session_id, thread_id) = store.ids_for(&key, "");
            SessionCtx {
                key,
                session_id,
                thread_id,
            }
        }
        // Seedless anonymous request: empty key (never sticks, never stored); every
        // for_account call mints fresh v7 ids, like a one-shot official exec.
        None => SessionCtx {
            key: String::new(),
            session_id: Uuid::now_v7(),
            thread_id: Uuid::now_v7(),
        },
    }
}

fn hex_short(digest: &[u8]) -> String {
    digest[..12].iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Debug)]
pub struct BuiltRequest {
    /// Body in official field order.
    pub body: Value,
    /// Non-auth headers the official client would add (window-id / turn-metadata / responses-lite).
    pub extra_headers: HeaderMap,
}

#[derive(Debug)]
pub enum BuildError {
    MissingModel,
    InputNotArray,
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuildError::MissingModel => write!(f, "missing required field: model"),
            BuildError::InputNotArray => write!(f, "input must be an array or a string"),
        }
    }
}
impl std::error::Error for BuildError {}

/// Builds the upstream request from scratch per official logic. turn_id /
/// turn_started_at_unix_ms are fixed by the caller for one upstream turn (a same-account
/// 401 refresh retry reuses them; an account switch starts a fresh turn on a fresh
/// session); installation_id varies per account. `compaction_meta` is the downstream
/// client's CompactionTurnMetadata block (from its x-codex-turn-metadata header), used
/// when the input carries a compaction_trigger item (official remote compaction v2).
pub fn build_upstream_request(
    downstream: &Value,
    session: &SessionCtx,
    model_db: &ModelDb,
    installation_id: &str,
    turn_id: &str,
    turn_started_at_unix_ms: i64,
    compaction_meta: Option<&Value>,
) -> Result<BuiltRequest, BuildError> {
    let obj = downstream.as_object();
    let model = obj
        .and_then(|o| o.get("model"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or(BuildError::MissingModel)?
        .to_string();
    let behavior = model_db.for_model(&model);

    // ---- Semantic field extraction (all other downstream fields are ignored) ----
    let user_instructions = obj
        .and_then(|o| o.get("instructions"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let instructions = user_instructions.unwrap_or_else(|| behavior.base_instructions.clone());

    let mut input = extract_input(obj)?;
    normalize_input_items(&mut input, behavior.use_responses_lite);

    let user_tools: Vec<Value> = obj
        .and_then(|o| o.get("tools"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    // ---- reasoning: explicit user value → model-name suffix (gpt-x-high style) → model default ----
    let user_reasoning = obj.and_then(|o| o.get("reasoning"));
    let user_effort = user_reasoning
        .and_then(|r| r.get("effort"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let suffix_effort = model_db.effort_suffix(&model);
    let effort = user_effort
        .or(suffix_effort)
        .unwrap_or_else(|| behavior.default_reasoning_level.clone());
    let effort = resolve_reasoning_effort(&effort, &behavior);
    let mut reasoning = Map::new();
    reasoning.insert("effort".into(), Value::String(effort));
    let summary = user_reasoning
        .and_then(|r| r.get("summary"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| behavior.default_reasoning_summary.clone());
    if behavior.supports_reasoning_summary_parameter && summary != "none" {
        reasoning.insert("summary".into(), Value::String(summary));
    }
    if behavior.use_responses_lite {
        reasoning.insert("context".into(), json!("all_turns"));
    }

    // ---- text: verbosity defaults per model; user output schema → official TextFormat shape ----
    let user_text = obj.and_then(|o| o.get("text"));
    let verbosity = user_text
        .and_then(|t| t.get("verbosity"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            behavior
                .support_verbosity
                .then(|| behavior.default_verbosity.clone())
                .flatten()
        });
    let format = user_text
        .and_then(|t| t.get("format"))
        .and_then(|f| f.get("schema").map(|schema| (f, schema)))
        .map(|(f, schema)| {
            json!({
                "type": "json_schema",
                "strict": f.get("strict").and_then(Value::as_bool).unwrap_or(true),
                "schema": schema,
                "name": "codex_output_schema",
            })
        });
    let text = if verbosity.is_none() && format.is_none() {
        None
    } else {
        let mut t = Map::new();
        if let Some(v) = verbosity {
            t.insert("verbosity".into(), Value::String(v));
        }
        if let Some(f) = format {
            t.insert("format".into(), f);
        }
        Some(Value::Object(t))
    };

    // ---- turn metadata (one JSON shared by body client_metadata and the x-codex-turn-metadata header) ----
    // Official remote compaction v2 appends a {"type":"compaction_trigger"} item to a normal
    // /responses call and labels the turn metadata request_kind=compaction (+ its
    // CompactionTurnMetadata block, copied from the downstream header when present).
    let is_compaction = input
        .iter()
        .any(|item| item.get("type").and_then(Value::as_str) == Some("compaction_trigger"));
    let default_compaction;
    let (request_kind, compaction_block) = if is_compaction {
        default_compaction = default_compaction_metadata("responses_compaction_v2");
        (
            REQUEST_KIND_COMPACTION,
            Some(compaction_meta.unwrap_or(&default_compaction)),
        )
    } else {
        (REQUEST_KIND_TURN, None)
    };
    let window_id = session.window_id();
    let turn_metadata = build_turn_metadata(
        &behavior,
        installation_id,
        session,
        &window_id,
        turn_id,
        turn_started_at_unix_ms,
        request_kind,
        compaction_block,
    );
    let turn_metadata_json =
        serde_json::to_string(&turn_metadata).expect("turn metadata serializes");

    let client_metadata = json!({
        "x-codex-installation-id": installation_id,
        "session_id": session.session_id.to_string(),
        "thread_id": session.thread_id.to_string(),
        "x-codex-window-id": window_id,
        "turn_id": turn_id,
        "x-codex-turn-metadata": turn_metadata_json,
    });

    // ---- lite prefix items vs. non-lite top-level fields ----
    let mut extra_headers = HeaderMap::new();
    insert_header(&mut extra_headers, "x-codex-window-id", &window_id);
    insert_header(
        &mut extra_headers,
        "x-codex-turn-metadata",
        &turn_metadata_json,
    );
    // Official build_routing_hint_header: codex backend (ChatGPT auth) always sends
    // model=<slug>, with ;tier=<service_tier> appended when a tier is configured
    // (ours never is — service_tier is omitted from the body for the same reason).
    insert_header(
        &mut extra_headers,
        "x-codex-routing-hint",
        &format!("model={model}"),
    );

    let (final_instructions, final_input, final_tools) = if behavior.use_responses_lite {
        insert_header(
            &mut extra_headers,
            "x-openai-internal-codex-responses-lite",
            "true",
        );
        let lite_tools = lite_tools_json(&user_tools);
        let prefix_ns = session.prefix_namespace();
        let at_id = format!(
            "at_{}",
            Uuid::new_v5(
                &prefix_ns,
                &serde_json::to_vec(&lite_tools).unwrap_or_default()
            )
        );
        let mut prefix = vec![json!({
            "type": "additional_tools",
            "id": at_id,
            "role": "developer",
            "tools": lite_tools,
        })];
        if !instructions.is_empty() {
            let msg_id = format!("msg_{}", Uuid::new_v5(&prefix_ns, instructions.as_bytes()));
            prefix.push(json!({
                "type": "message",
                "id": msg_id,
                "role": "developer",
                "content": [{ "type": "input_text", "text": instructions }],
                "internal_chat_message_metadata_passthrough": {
                    "content_item_kinds": ["model.base_instructions"]
                },
            }));
        }
        prefix.extend(input);
        (None, prefix, None)
    } else {
        (Some(instructions), input, Some(Value::Array(user_tools)))
    };

    // ---- Assemble in official ResponsesApiRequest struct order ----
    let mut body = Map::new();
    body.insert("model".into(), Value::String(model));
    if let Some(instr) = final_instructions.filter(|s| !s.is_empty()) {
        body.insert("instructions".into(), Value::String(instr));
    }
    body.insert("input".into(), Value::Array(final_input));
    if let Some(tools) = final_tools {
        body.insert("tools".into(), tools);
    }
    body.insert("tool_choice".into(), json!("auto"));
    body.insert(
        "parallel_tool_calls".into(),
        Value::Bool(!behavior.use_responses_lite),
    );
    body.insert("reasoning".into(), Value::Object(reasoning));
    body.insert("store".into(), Value::Bool(false));
    body.insert("stream".into(), Value::Bool(true));
    // stream_options: officially None by default (concurrent_reasoning_summaries off) → omitted.
    body.insert("include".into(), json!(["reasoning.encrypted_content"]));
    // service_tier: officially None unless configured → omitted.
    body.insert(
        "prompt_cache_key".into(),
        Value::String(session.session_id.to_string()),
    );
    if let Some(text) = text {
        body.insert("text".into(), text);
    }
    body.insert("client_metadata".into(), client_metadata);
    // access_programs: None → omitted.

    Ok(BuiltRequest {
        body: Value::Object(body),
        extra_headers,
    })
}

/// Builds the legacy `/responses/compact` request (official CompactionInput shape) from a
/// downstream compact call. Same rules as build_upstream_request: the body is assembled in
/// official struct field order from semantically extracted fields only; identity fields
/// (prompt_cache_key, installation/session ids) are rebuilt per account. Unary plain JSON
/// (no store/stream/include/tool_choice/client_metadata — CompactionInput has none).
///
/// Official shape (core client.compact_conversation_history → codex-api CompactionInput):
/// instructions/tools stay top-level even for lite models (no lite prefix-item conversion);
/// reasoning/text resolve exactly like a normal turn; item prep strips unprefixed ids only.
#[allow(clippy::too_many_arguments)]
pub fn build_compact_request(
    downstream: &Value,
    session: &SessionCtx,
    model_db: &ModelDb,
    installation_id: &str,
    turn_id: &str,
    turn_started_at_unix_ms: i64,
    compaction_meta: Option<&Value>,
) -> Result<BuiltRequest, BuildError> {
    let obj = downstream.as_object();
    let model = obj
        .and_then(|o| o.get("model"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or(BuildError::MissingModel)?
        .to_string();
    let behavior = model_db.for_model(&model);

    let instructions = obj
        .and_then(|o| o.get("instructions"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| behavior.base_instructions.clone());

    let mut input = extract_input(obj)?;
    // Official prepare_response_items_for_request for compact: unprefixed id strip only.
    normalize_input_items(&mut input, false);

    let tools = obj.and_then(|o| o.get("tools")).and_then(Value::as_array);
    let parallel_tool_calls = obj
        .and_then(|o| o.get("parallel_tool_calls"))
        .and_then(Value::as_bool)
        .unwrap_or(true);

    // reasoning: copy the downstream's (officially built) value, else resolve like a turn.
    let reasoning = obj
        .and_then(|o| o.get("reasoning"))
        .filter(|r| r.is_object())
        .cloned()
        .unwrap_or_else(|| {
            let effort = resolve_reasoning_effort(&behavior.default_reasoning_level, &behavior);
            let mut r = Map::new();
            r.insert("effort".into(), Value::String(effort));
            if behavior.supports_reasoning_summary_parameter
                && behavior.default_reasoning_summary != "none"
            {
                r.insert(
                    "summary".into(),
                    Value::String(behavior.default_reasoning_summary.clone()),
                );
            }
            if behavior.use_responses_lite {
                r.insert("context".into(), json!("all_turns"));
            }
            Value::Object(r)
        });

    let service_tier = obj
        .and_then(|o| o.get("service_tier"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    // text: copy downstream's, else the model-default verbosity (official compact has no
    // output schema, so no format key is ever synthesized here).
    let text = obj
        .and_then(|o| o.get("text"))
        .filter(|t| t.is_object())
        .cloned()
        .or_else(|| {
            behavior
                .support_verbosity
                .then(|| behavior.default_verbosity.clone())
                .flatten()
                .map(|v| json!({ "verbosity": v }))
        });

    let access_programs = obj.and_then(|o| o.get("access_programs")).cloned();

    // ---- turn metadata: request_kind=compaction + CompactionTurnMetadata block ----
    let default_compaction = default_compaction_metadata("responses_compact");
    let compaction_block = compaction_meta.unwrap_or(&default_compaction);
    let window_id = session.window_id();
    let turn_metadata = build_turn_metadata(
        &behavior,
        installation_id,
        session,
        &window_id,
        turn_id,
        turn_started_at_unix_ms,
        REQUEST_KIND_COMPACTION,
        Some(compaction_block),
    );
    let turn_metadata_json =
        serde_json::to_string(&turn_metadata).expect("turn metadata serializes");

    // ---- headers: the official compact_conversation_history set ----
    let mut extra_headers = HeaderMap::new();
    // installation id is a real HTTP header here (CompactionInput has no client_metadata).
    insert_header(
        &mut extra_headers,
        "x-codex-installation-id",
        installation_id,
    );
    insert_header(&mut extra_headers, "x-codex-window-id", &window_id);
    insert_header(
        &mut extra_headers,
        "x-codex-turn-metadata",
        &turn_metadata_json,
    );
    let routing_hint = match &service_tier {
        Some(tier) => format!("model={model};tier={tier}"),
        None => format!("model={model}"),
    };
    insert_header(&mut extra_headers, "x-codex-routing-hint", &routing_hint);
    if behavior.use_responses_lite {
        insert_header(
            &mut extra_headers,
            "x-openai-internal-codex-responses-lite",
            "true",
        );
    }
    // beta-features / turn-state / attestation: absent under default config (turn-state is
    // stripped by relay policy, same as /responses).

    // ---- Assemble in official CompactionInput struct order ----
    let mut body = Map::new();
    body.insert("model".into(), Value::String(model));
    body.insert("input".into(), Value::Array(input));
    if !instructions.is_empty() {
        body.insert("instructions".into(), Value::String(instructions));
    }
    if let Some(tools) = tools {
        body.insert("tools".into(), Value::Array(tools.clone()));
    }
    body.insert(
        "parallel_tool_calls".into(),
        Value::Bool(parallel_tool_calls),
    );
    body.insert("reasoning".into(), reasoning);
    if let Some(tier) = &service_tier {
        body.insert("service_tier".into(), Value::String(tier.clone()));
    }
    body.insert(
        "prompt_cache_key".into(),
        Value::String(session.session_id.to_string()),
    );
    if let Some(text) = text {
        body.insert("text".into(), text);
    }
    if let Some(ap) = access_programs {
        body.insert("access_programs".into(), ap);
    }

    Ok(BuiltRequest {
        body: Value::Object(body),
        extra_headers,
    })
}

/// Official CodexTurnMetadataPayload field order (default first-turn shape with
/// request_kind=turn). Also used by the alpha/search forwarder (official SearchClient
/// sends the same metadata shape as the originating turn). Compaction requests
/// (v2 trigger item over /responses, or legacy /responses/compact) carry
/// request_kind=compaction plus the CompactionTurnMetadata block, appended last per
/// the official payload struct order.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_turn_metadata(
    behavior: &ModelBehavior,
    installation_id: &str,
    session: &SessionCtx,
    window_id: &str,
    turn_id: &str,
    turn_started_at_unix_ms: i64,
    request_kind: &str,
    compaction: Option<&Value>,
) -> Value {
    let mut meta = json!({
        "installation_id": installation_id,
        "session_id": session.session_id.to_string(),
        "thread_id": session.thread_id.to_string(),
        "agent_name": AGENT_NAME,
        "turn_id": turn_id,
        "window_id": window_id,
        "window_number": 0,
        "request_kind": request_kind,
        "thread_source": THREAD_SOURCE,
        "sandbox": SANDBOX_TAG,
        "sandbox_mode": SANDBOX_MODE_TAG,
        "auto_review_enabled": false,
        "node_repl_auto_review_required": behavior.node_repl_auto_review_required,
        "node_repl_disabled": behavior.node_repl_disabled,
        "turn_started_at_unix_ms": turn_started_at_unix_ms,
    });
    if let Some(compaction) = compaction {
        meta["compaction"] = compaction.clone();
    }
    meta
}

/// Downstream input accepts an array (standard) or a string (Responses API shorthand,
/// converted to an official message item).
fn extract_input(obj: Option<&Map<String, Value>>) -> Result<Vec<Value>, BuildError> {
    match obj.and_then(|o| o.get("input")) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => Ok(items.clone()),
        Some(Value::String(text)) => Ok(vec![json!({
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": text }],
        })]),
        _ => Err(BuildError::InputNotArray),
    }
}

/// Official prepare_response_items_for_request: unprefixed item ids are dropped;
/// lite mode additionally strips input_image detail (strip_image_details).
fn normalize_input_items(input: &mut [Value], strip_image_details: bool) {
    for item in input.iter_mut() {
        let Some(item_obj) = item.as_object_mut() else {
            continue;
        };
        if let Some(id) = item_obj.get("id").and_then(Value::as_str)
            && !is_prefixed_id(id)
        {
            item_obj.remove("id");
        }
        if strip_image_details {
            strip_detail_in_content(item_obj.get_mut("content"));
            // function_call_output / custom_tool_call_output carry output.content.
            if let Some(output) = item_obj.get_mut("output") {
                strip_detail_in_content(output.get_mut("content"));
            }
        }
    }
}

fn strip_detail_in_content(content: Option<&mut Value>) {
    if let Some(Value::Array(items)) = content {
        for entry in items.iter_mut() {
            if entry.get("type").and_then(Value::as_str) == Some("input_image")
                && let Some(entry_obj) = entry.as_object_mut()
            {
                entry_obj.remove("detail");
            }
        }
    }
}

/// Official ResponseItemId::is_prefixed: "<prefix>_<suffix>" with both parts non-empty.
fn is_prefixed_id(id: &str) -> bool {
    id.split_once('_')
        .is_some_and(|(prefix, suffix)| !prefix.is_empty() && !suffix.is_empty())
}

/// Official ModelInfo::resolve_reasoning_effort:
/// ultra → multi_agent_reasoning_effort (if supported) → max → last non-ultra → medium;
/// persistent → "disabled"; anything else passes through.
pub(crate) fn resolve_reasoning_effort(effort: &str, behavior: &ModelBehavior) -> String {
    match effort {
        "ultra" => {
            if let Some(ma) = &behavior.multi_agent_reasoning_effort
                && ma != "ultra"
                && behavior.supported_reasoning_levels.iter().any(|l| l == ma)
            {
                return ma.clone();
            }
            if behavior
                .supported_reasoning_levels
                .iter()
                .any(|l| l == "max")
            {
                return "max".to_string();
            }
            behavior
                .supported_reasoning_levels
                .iter()
                .rev()
                .find(|l| *l != "ultra")
                .cloned()
                .unwrap_or_else(|| "medium".to_string())
        }
        "persistent" => "disabled".to_string(),
        other => other.to_string(),
    }
}

/// Official create_tools_json_for_responses_lite (namespace_tools=true):
/// function/custom tools are coalesced into the "functions" namespace; other tool
/// types stay top-level in their original relative order.
fn lite_tools_json(user_tools: &[Value]) -> Vec<Value> {
    let mut namespaced: Vec<Value> = Vec::new();
    let mut top_level: Vec<(usize, Value)> = Vec::new();

    for (idx, tool) in user_tools.iter().enumerate() {
        match tool.get("type").and_then(Value::as_str) {
            Some("function") => namespaced.push(reshape_function_tool(tool)),
            // Freeform tools (official Custom) enter the namespace unchanged.
            Some("custom") => namespaced.push(tool.clone()),
            _ => top_level.push((idx, tool.clone())),
        }
    }

    if namespaced.is_empty() {
        return top_level.into_iter().map(|(_, t)| t).collect();
    }
    // The namespace takes the position of the first function/custom tool
    // (official functions_index semantics).
    let first_ns_idx = user_tools
        .iter()
        .position(|t| {
            matches!(
                t.get("type").and_then(Value::as_str),
                Some("function") | Some("custom")
            )
        })
        .unwrap_or(0);
    let namespace = json!({
        "type": "namespace",
        "name": "functions",
        "description": "",
        "tools": namespaced,
    });
    let mut out: Vec<Value> = top_level.into_iter().map(|(_, t)| t).collect();
    let insert_at = top_level_insert_position(user_tools, first_ns_idx);
    out.insert(insert_at.min(out.len()), namespace);
    out
}

/// Insert index of the namespace within the function/custom-stripped list:
/// how many top-level tools precede the first function/custom in the original list.
fn top_level_insert_position(user_tools: &[Value], first_ns_idx: usize) -> usize {
    user_tools[..first_ns_idx]
        .iter()
        .filter(|t| {
            !matches!(
                t.get("type").and_then(Value::as_str),
                Some("function") | Some("custom")
            )
        })
        .count()
}

/// Official ResponsesApiTool serialization shape: name/description/strict/defer_loading?/parameters.
fn reshape_function_tool(tool: &Value) -> Value {
    let mut out = Map::new();
    out.insert("type".into(), json!("function"));
    out.insert(
        "name".into(),
        tool.get("name").cloned().unwrap_or(Value::Null),
    );
    out.insert(
        "description".into(),
        tool.get("description").cloned().unwrap_or(json!("")),
    );
    out.insert(
        "strict".into(),
        Value::Bool(tool.get("strict").and_then(Value::as_bool).unwrap_or(false)),
    );
    if let Some(defer) = tool.get("defer_loading") {
        out.insert("defer_loading".into(), defer.clone());
    }
    out.insert(
        "parameters".into(),
        tool.get("parameters").cloned().unwrap_or(json!({})),
    );
    Value::Object(out)
}

fn insert_header(headers: &mut HeaderMap, name: &str, value: &str) {
    if let (Ok(n), Ok(v)) = (
        name.parse::<http::HeaderName>(),
        HeaderValue::from_str(value),
    ) {
        headers.insert(n, v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> ModelDb {
        ModelDb::load()
    }

    fn session() -> SessionCtx {
        SessionCtx {
            key: "k".to_string(),
            session_id: Uuid::new_v5(&Uuid::NAMESPACE_OID, b"test-session"),
            thread_id: Uuid::new_v5(&Uuid::NAMESPACE_OID, b"test-thread"),
        }
    }

    fn body_keys(body: &Value) -> Vec<String> {
        body.as_object().unwrap().keys().cloned().collect()
    }

    /// Lite model (gpt-5.6-sol): official field order + lite prefix items +
    /// reasoning.context + verbosity.
    #[test]
    fn lite_model_shape_matches_official() {
        let db = test_db();
        let downstream = json!({
            "model": "gpt-5.6-sol",
            "input": [{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}],
            "tools": [
                {"type":"function","name":"shell","description":"run","strict":true,"parameters":{"type":"object"}},
                {"type":"web_search"}
            ],
            // Everything below must be ignored (no passthrough).
            "temperature": 0.9,
            "max_output_tokens": 100,
            "previous_response_id": "resp_x",
            "store": true,
            "stream": false,
            "service_tier": "priority",
            "stream_options": {"include_obfuscation": false},
            "access_programs": ["x"],
        });
        let built = build_upstream_request(
            &downstream,
            &session(),
            &db,
            "11111111-1111-4111-8111-111111111111",
            "0192f8c0-0000-7000-8000-000000000000",
            1_700_000_000_123,
            None,
        )
        .unwrap();
        let body = &built.body;

        assert_eq!(
            body_keys(body),
            vec![
                "model",
                "input",
                "tool_choice",
                "parallel_tool_calls",
                "reasoning",
                "store",
                "stream",
                "include",
                "prompt_cache_key",
                "text",
                "client_metadata"
            ],
            "lite body field set/order drifted from official"
        );
        assert_eq!(body["parallel_tool_calls"], json!(false));
        assert_eq!(
            body["reasoning"],
            json!({"effort":"low","context":"all_turns"})
        );
        assert_eq!(body["text"], json!({"verbosity":"low"}));
        assert!(
            body.get("instructions").is_none(),
            "lite must not carry top-level instructions"
        );
        assert!(
            body.get("tools").is_none(),
            "lite must not carry top-level tools"
        );

        // Prefix items: additional_tools (function coalesced into the namespace,
        // web_search stays top-level after it).
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], json!("additional_tools"));
        assert!(input[0]["id"].as_str().unwrap().starts_with("at_"));
        assert_eq!(input[0]["role"], json!("developer"));
        let tools = input[0]["tools"].as_array().unwrap();
        assert_eq!(tools[0]["type"], json!("namespace"));
        assert_eq!(tools[0]["name"], json!("functions"));
        assert_eq!(tools[0]["description"], json!(""));
        assert_eq!(tools[0]["tools"][0]["name"], json!("shell"));
        assert_eq!(tools[1]["type"], json!("web_search"));

        // Base-instructions prefix message.
        assert_eq!(input[1]["type"], json!("message"));
        assert!(input[1]["id"].as_str().unwrap().starts_with("msg_"));
        assert_eq!(input[1]["role"], json!("developer"));
        assert_eq!(
            input[1]["internal_chat_message_metadata_passthrough"]["content_item_kinds"][0],
            json!("model.base_instructions")
        );
        assert!(
            input[1]["content"][0]["text"]
                .as_str()
                .unwrap()
                .starts_with("You are Codex")
        );

        // User message third, content untouched.
        assert_eq!(input[2]["role"], json!("user"));

        // client_metadata official key set.
        let cm = &body["client_metadata"];
        for key in [
            "x-codex-installation-id",
            "session_id",
            "thread_id",
            "x-codex-window-id",
            "turn_id",
            "x-codex-turn-metadata",
        ] {
            assert!(cm.get(key).is_some(), "client_metadata missing {key}");
        }
        let s = session();
        assert_eq!(cm["session_id"], json!(s.session_id.to_string()));
        assert_eq!(cm["x-codex-window-id"], json!(s.window_id()));
        assert_eq!(body["prompt_cache_key"], json!(s.session_id.to_string()));

        // The turn-metadata header matches the embedded body string and carries the
        // official default-turn fields.
        let tm: Value =
            serde_json::from_str(cm["x-codex-turn-metadata"].as_str().unwrap()).unwrap();
        assert_eq!(tm["request_kind"], json!("turn"));
        assert_eq!(tm["agent_name"], json!("root"));
        assert_eq!(tm["thread_source"], json!("user"));
        assert_eq!(tm["window_number"], json!(0));
        assert_eq!(tm["sandbox"], json!("none"));
        assert_eq!(tm["sandbox_mode"], json!("read-only"));
        assert_eq!(tm["auto_review_enabled"], json!(false));
        assert_eq!(tm["node_repl_auto_review_required"], json!(false));
        assert_eq!(tm["node_repl_disabled"], json!(false));
        assert_eq!(tm["turn_started_at_unix_ms"], json!(1_700_000_000_123i64));
        assert!(tm.get("workspaces").is_none());

        let hv = built
            .extra_headers
            .get("x-codex-turn-metadata")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(hv, cm["x-codex-turn-metadata"].as_str().unwrap());
        assert_eq!(
            built.extra_headers.get("x-codex-window-id").unwrap(),
            &s.window_id().parse::<HeaderValue>().unwrap()
        );
        assert!(
            built
                .extra_headers
                .contains_key("x-openai-internal-codex-responses-lite")
        );
    }

    /// Non-lite model (gpt-5.5): top-level instructions/tools + parallel true + effort medium.
    #[test]
    fn non_lite_model_shape_matches_official() {
        let db = test_db();
        let downstream = json!({
            "model": "gpt-5.5",
            "input": "hello",
            "tools": [{"type":"function","name":"shell","parameters":{"type":"object"}}],
        });
        let built =
            build_upstream_request(&downstream, &session(), &db, "inst", "turn", 1, None).unwrap();
        let body = &built.body;
        assert_eq!(
            body_keys(body),
            vec![
                "model",
                "instructions",
                "input",
                "tools",
                "tool_choice",
                "parallel_tool_calls",
                "reasoning",
                "store",
                "stream",
                "include",
                "prompt_cache_key",
                "text",
                "client_metadata"
            ]
        );
        assert_eq!(body["parallel_tool_calls"], json!(true));
        assert_eq!(body["reasoning"], json!({"effort":"medium"}));
        assert!(
            body["instructions"]
                .as_str()
                .unwrap()
                .starts_with("You are Codex")
        );
        assert_eq!(body["tools"][0]["name"], json!("shell"));
        // String input shorthand → message item.
        assert_eq!(body["input"][0]["type"], json!("message"));
        assert_eq!(body["input"][0]["role"], json!("user"));
        assert!(
            !built
                .extra_headers
                .contains_key("x-openai-internal-codex-responses-lite")
        );
    }

    /// gpt-5.2: default_reasoning_summary=auto → reasoning.summary is emitted.
    #[test]
    fn summary_default_from_model() {
        let db = test_db();
        let downstream = json!({"model":"gpt-5.2","input":[]});
        let built = build_upstream_request(&downstream, &session(), &db, "i", "t", 1, None).unwrap();
        assert_eq!(
            built.body["reasoning"],
            json!({"effort":"medium","summary":"auto"})
        );
    }

    /// Explicit user reasoning / verbosity / output schema pass through semantically,
    /// landing in the official shape.
    #[test]
    fn user_semantic_overrides() {
        let db = test_db();
        let downstream = json!({
            "model": "gpt-5.5",
            "input": [],
            "reasoning": {"effort":"ultra","summary":"detailed"},
            "text": {"verbosity":"high","format":{"type":"json_schema","schema":{"type":"object"},"strict":false}},
        });
        let built = build_upstream_request(&downstream, &session(), &db, "i", "t", 1, None).unwrap();
        // gpt-5.5 has no max/ultra → resolves to the last supported non-ultra = xhigh.
        assert_eq!(
            built.body["reasoning"],
            json!({"effort":"xhigh","summary":"detailed"})
        );
        assert_eq!(
            built.body["text"],
            json!({
                "verbosity":"high",
                "format":{"type":"json_schema","strict":false,"schema":{"type":"object"},"name":"codex_output_schema"}
            })
        );
    }

    /// Item id rule: unprefixed dropped, prefixed kept; lite strips input_image detail.
    #[test]
    fn item_normalization() {
        let db = test_db();
        let downstream = json!({
            "model": "gpt-5.6-sol",
            "input": [
                {"type":"message","id":"plain-id","role":"user","content":[
                    {"type":"input_image","image_url":"data:x","detail":"high"}
                ]},
                {"type":"message","id":"msg_keep","role":"assistant","content":[
                    {"type":"output_text","text":"ok"}
                ]},
            ],
        });
        let built = build_upstream_request(&downstream, &session(), &db, "i", "t", 1, None).unwrap();
        let input = built.body["input"].as_array().unwrap();
        let user_msg = &input[2];
        assert!(
            user_msg.get("id").is_none(),
            "unprefixed id must be dropped"
        );
        assert!(
            user_msg["content"][0].get("detail").is_none(),
            "lite must strip image detail"
        );
        assert_eq!(
            input[3]["id"],
            json!("msg_keep"),
            "prefixed id must be kept"
        );
    }

    /// Missing model → error.
    #[test]
    fn missing_model_rejected() {
        let db = test_db();
        let err =
            build_upstream_request(&json!({"input":[]}), &session(), &db, "i", "t", 1, None).unwrap_err();
        assert!(matches!(err, BuildError::MissingModel));
    }

    /// Session ids: stable per (seed, account), v7 version, isolated per api key,
    /// fresh per account switch, ephemeral for anonymous requests.
    #[test]
    fn session_ids_stable_v7_and_isolated() {
        let store = SessionStore::new(std::time::Duration::from_secs(3600));
        let body = json!({"model":"gpt-5.6-sol","input":[],"prompt_cache_key":"conv-1"});
        let headers = http::HeaderMap::new();

        // Same seed + same account → stable ids.
        let a1 = derive_session("sk-a", &body, &headers, true, &store);
        let a2 = a1.for_account(&store, "acc1");
        let a3 = a1.for_account(&store, "acc1");
        assert_eq!(
            a2.session_id, a3.session_id,
            "same seed + account must reuse ids"
        );
        assert_eq!(a2.thread_id, a3.thread_id);
        assert_eq!(
            a2.session_id.get_version_num(),
            7,
            "official session_id is v7"
        );
        assert_eq!(
            a2.thread_id.get_version_num(),
            7,
            "official thread_id is v7"
        );

        // Same seed + different account → a fresh, uncorrelated session.
        let b = a1.for_account(&store, "acc2");
        assert_ne!(
            a2.session_id, b.session_id,
            "account switch must mint fresh ids"
        );
        // ...but stable for that account on later requests.
        assert_eq!(b.session_id, a1.for_account(&store, "acc2").session_id);

        // Different api keys are isolated.
        let other = derive_session("sk-b", &body, &headers, true, &store);
        assert_ne!(
            a1.session_id, other.session_id,
            "different api keys must be isolated"
        );

        // Anonymous: empty key, fresh ids on every for_account, nothing stored.
        let stored = store.len();
        let anon = derive_session(
            "sk-a",
            &json!({"model":"m","input":[]}),
            &headers,
            true,
            &store,
        );
        assert!(anon.key.is_empty());
        let anon1 = anon.for_account(&store, "acc1");
        let anon2 = anon.for_account(&store, "acc1");
        assert_ne!(anon1.session_id, anon2.session_id);
        assert_eq!(store.len(), stored, "anonymous sessions must not be stored");
    }
}
