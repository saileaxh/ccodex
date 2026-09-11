//! Official Statsig OTLP metrics channel, reusing the official `codex-otel` crate
//! (MetricsClient + OTLP/JSON exporter to ab.chatgpt.com with the built-in Statsig
//! key, delta temporality, 60s periodic export, meter "codex", resource
//! service.name/service.version/env/os/os_version) — the same code path the official
//! binary uses, driven with the official default CLI configuration:
//!
//! - environment "dev" (DEFAULT_OTEL_ENVIRONMENT), service_name "codex_cli_rs",
//!   service_version = reported codex version
//! - per-emission metadata tags in official order: auth_mode=Chatgpt,
//!   session_source=cli, originator=codex_cli_rs, model=<slug>, app.version=<version>
//!   (SessionTelemetry::tags_with_metadata; service_name tag absent)
//! - only the Statsig-visible instrument set is emitted (STATSIG_DISABLED_METRICS are
//!   skipped by construction): codex.sse_event(+duration), codex.turn.ttft/ttfm/e2e
//!   durations, codex.turn.tool.call, codex.turn.network_proxy, codex.turn.memory,
//!   codex.turn.unified_exec.running_processes, codex.thread.started,
//!   codex.task.compact, codex.process.start
//!
//! Default-config values are used where the official value derives from client config
//! (memory off, network proxy inactive, is_git false, zero background processes).

use codex_otel::OtelExporter;
use codex_otel::{MetricsClient, MetricsConfig};
use std::sync::Arc;
use std::time::Duration;

/// Statsig ingest key shipped in every official binary (otel/src/config.rs); used only
/// for the local-mock endpoint override (the real path uses the crate's built-in).
const STATSIG_API_KEY: &str = "client-MkRuleRQBd6qakfnDYqJVR9JuXcY57Ljly3vi5JVUIO";
const STATSIG_API_KEY_HEADER: &str = "statsig-api-key";

/// Builds one account's official metrics client (exporter + periodic reader + meter
/// "codex") and records `codex.process.start [originator]` on it.
///
/// **Must be called inside [`crate::proxies::with_binding`]**: the OTLP exporter's reqwest
/// client resolves the ambient proxy env at construction, so building it there is what
/// makes this account's `ab.chatgpt.com` exports leave through the account's own proxy —
/// the same exit IP as its conversation traffic, exactly like the official client (one
/// process = one account = one exporter).
///
/// The upstream `record_process_start_once` is gated by a process-wide flag; here each
/// account is its own official-client identity with its own egress, so the counter is
/// recorded once per account client (same instrument, same `originator` tag value
/// bounding as the official helper).
pub fn build_account_client() -> Arc<MetricsClient> {
    let version = crate::identity::codex_version();
    // Dev self-check: CCODEX_STATSIG_ENDPOINT_OVERRIDE points the OTLP exporter at
    // a local mock (bypasses the debug-build disable in resolve_exporter).
    let exporter = match std::env::var("CCODEX_STATSIG_ENDPOINT_OVERRIDE") {
        Ok(endpoint) => OtelExporter::OtlpHttp {
            endpoint,
            headers: std::collections::HashMap::from([(
                STATSIG_API_KEY_HEADER.to_string(),
                STATSIG_API_KEY.to_string(),
            )]),
            protocol: codex_otel::OtelHttpProtocol::Json,
            tls: None,
        },
        Err(_) => OtelExporter::Statsig,
    };
    let mut config = MetricsConfig::otlp("dev", "codex_cli_rs", version, exporter);
    // Dev self-check only: shorten the periodic export interval so an end-to-end run
    // doesn't have to wait out the SDK default. Production leaves it unset — the
    // official client passes None here too, so both share the SDK default cadence.
    if let Ok(ms) = std::env::var("CCODEX_STATSIG_EXPORT_INTERVAL_MS")
        && let Ok(ms) = ms.parse::<u64>()
    {
        config = config.with_export_interval(Duration::from_millis(ms));
    }
    let client = MetricsClient::new(config)
        .expect("Statsig metrics exporter builds with built-in config");
    let _ = client.counter(
        codex_otel::PROCESS_START_METRIC,
        1,
        &[(
            codex_otel::ORIGINATOR_TAG,
            codex_otel::bounded_originator_tag_value(
                codex_login::default_client::originator().value.as_str(),
            ),
        )],
    );
    Arc::new(client)
}

pub struct MetricsHub {
    version: String,
}

impl MetricsHub {
    pub fn new() -> Self {
        Self {
            version: crate::identity::codex_version(),
        }
    }

    /// SessionTelemetry metadata tags in official into_tags order (service_name None →
    /// omitted). `model` is the thread/turn slug.
    fn metadata_tags<'a>(&'a self, model: &'a str) -> [(&'static str, &'a str); 5] {
        [
            ("auth_mode", "Chatgpt"),
            ("session_source", "cli"),
            ("originator", "codex_cli_rs"),
            ("model", model),
            ("app.version", self.version.as_str()),
        ]
    }

    /// Records into the account's own metrics client (its exporter egresses through the
    /// account's bound proxy, so an account's metrics never ship from another account's IP).
    fn emit(&self, client: &MetricsClient, f: impl FnOnce(&MetricsClient)) {
        f(client);
    }

    /// codex.thread.started [is_git=false] — official session start counter.
    pub fn thread_started(&self, client: &MetricsClient, model: &str) {
        let md = self.metadata_tags(model);
        let mut tags: Vec<(&str, &str)> = vec![("is_git", "false")];
        tags.extend(md);
        self.emit(client, |c| {
            let _ = c.counter("codex.thread.started", 1, &tags);
        });
    }

    /// Per-SSE-event codex.sse_event counter + duration (official log_sse_event).
    pub fn sse_events(&self, client: &MetricsClient, model: &str, events: &[crate::sse_tap::SseMetricEvent]) {
        if events.is_empty() {
            return;
        }
        let md = self.metadata_tags(model);
        self.emit(client, |c| {
            for (kind, success, wait_ms) in events {
                let success = if *success { "true" } else { "false" };
                let mut tags: Vec<(&str, &str)> = vec![("kind", kind.as_str()), ("success", success)];
                tags.extend(md);
                let _ = c.counter("codex.sse_event", 1, &tags);
                let _ = c.record_duration(
                    "codex.sse_event.duration_ms",
                    Duration::from_millis(*wait_ms),
                    &tags,
                );
            }
        });
    }

    /// Turn-end metrics (official tasks/mod.rs turn-final sequence).
    pub fn turn_finished(
        &self,
        client: &MetricsClient,
        model: &str,
        ttft_ms: Option<u64>,
        ttfm_ms: Option<u64>,
        e2e_ms: u64,
        tool_calls: usize,
    ) {
        let md = self.metadata_tags(model);
        self.emit(client, |c| {
            if let Some(ms) = ttft_ms {
                let tags: Vec<(&str, &str)> = md.to_vec();
                let _ = c.record_duration(
                    "codex.turn.ttft.duration_ms",
                    Duration::from_millis(ms),
                    &tags,
                );
            }
            if let Some(ms) = ttfm_ms {
                let tags: Vec<(&str, &str)> = md.to_vec();
                let _ = c.record_duration(
                    "codex.turn.ttfm.duration_ms",
                    Duration::from_millis(ms),
                    &tags,
                );
            }
            let tags: Vec<(&str, &str)> = md.to_vec();
            let _ = c.record_duration(
                "codex.turn.e2e_duration_ms",
                Duration::from_millis(e2e_ms),
                &tags,
            );
            let mut tags: Vec<(&str, &str)> = vec![("tmp_mem_enabled", "false")];
            tags.extend(md);
            let _ = c.histogram(
                "codex.turn.tool.call",
                i64::try_from(tool_calls).unwrap_or(i64::MAX),
                &tags,
            );
            let mut tags: Vec<(&str, &str)> =
                vec![("active", "false"), ("tmp_mem_enabled", "false")];
            tags.extend(md);
            let _ = c.counter("codex.turn.network_proxy", 1, &tags);
            let mut tags: Vec<(&str, &str)> = vec![
                ("read_allowed", "false"),
                ("feature_enabled", "false"),
                ("config_use_memories", "false"),
                ("has_citations", "false"),
            ];
            tags.extend(md);
            let _ = c.counter("codex.turn.memory", 1, &tags);
            let tags: Vec<(&str, &str)> = md.to_vec();
            let _ = c.counter("codex.turn.unified_exec.running_processes", 0, &tags);
        });
    }

    /// codex.task.compact [type, manual] — official compact task counter.
    pub fn task_compact(&self, client: &MetricsClient, model: &str, compact_type: &str, manual: bool) {
        let md = self.metadata_tags(model);
        let manual = if manual { "true" } else { "false" };
        let mut tags: Vec<(&str, &str)> = vec![("type", compact_type), ("manual", manual)];
        tags.extend(md);
        self.emit(client, |c| {
            let _ = c.counter("codex.task.compact", 1, &tags);
        });
    }
}
