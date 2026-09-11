//! Passive SSE stream analysis: scans events while bytes pass through, extracting
//! usage accounting from response.completed and detecting response.failed / error
//! terminal states. Read-only — never rewrites a single byte.
//!
//! Telemetry extension: mirrors what the official client's SessionTelemetry sees on
//! its SSE stream (codex.sse_event per event with kind/success + wait duration, turn
//! TTFT/TTFM contentful-event timing, tool-call items for turn reconstruction). All
//! values are measured from real bytes on the wire; nothing is synthesized.

use serde_json::Value;
use std::time::Instant;

/// One official `codex.sse_event` sample: (kind, success, wait_ms since previous event).
pub type SseMetricEvent = (String, bool, u64);

#[derive(Default)]
pub struct SseTap {
    buf: Vec<u8>,
    pub usage: Option<Value>,
    pub failed: Option<Value>,
    pub completed: bool,
    /// Stream start (first feed) — baseline for TTFT/TTFM measurements.
    started: Option<Instant>,
    last_event_at: Option<Instant>,
    /// Official codex.sse_event samples in arrival order (drained by the caller).
    pub metric_events: Vec<SseMetricEvent>,
    /// TTFT: stream start → first contentful event (official turn_ttft predicate).
    pub first_contentful_ms: Option<u64>,
    /// TTFM: stream start → first assistant message item.
    pub first_agent_message_ms: Option<u64>,
    /// Tool calls emitted by this response: (call_id, name, wire item type).
    pub tool_calls: Vec<(String, String, String)>,
    /// A `compaction` output item was seen (v2 remote compaction confirmation).
    pub saw_compaction_item: bool,
    /// Instant of the most recent chunk (stream-end timing for sampling duration).
    pub last_chunk_at: Option<Instant>,
}

impl SseTap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        let now = Instant::now();
        if self.started.is_none() {
            self.started = Some(now);
            self.last_event_at = Some(now);
        }
        self.last_chunk_at = Some(now);
        self.buf.extend_from_slice(chunk);
        // Events are separated by blank lines.
        while let Some(pos) = find_double_newline(&self.buf) {
            let event: Vec<u8> = self.buf.drain(..pos).collect();
            self.buf.drain(..2.min(self.buf.len()));
            self.handle_event(&event, now);
        }
        // Defensive: drop abnormally long partial lines.
        if self.buf.len() > 1 << 20 {
            self.buf.clear();
        }
    }

    /// Marks the stream as broken (transport error / truncated): the official client
    /// logs a failed sse_event with unknown kind in this case.
    pub fn note_stream_error(&mut self) {
        let now = Instant::now();
        let gap = self.gap_ms(now);
        self.metric_events
            .push(("unknown".to_string(), false, gap));
    }

    fn gap_ms(&mut self, now: Instant) -> u64 {
        let gap = self
            .last_event_at
            .map(|t| now.saturating_duration_since(t).as_millis() as u64)
            .unwrap_or(0);
        self.last_event_at = Some(now);
        gap
    }

    fn handle_event(&mut self, event: &[u8], now: Instant) {
        let Ok(text) = std::str::from_utf8(event) else {
            return;
        };
        // Official kind = the SSE `event:` line verbatim; the codex backend always
        // sends one matching the JSON "type". Fall back to the JSON type.
        let mut kind: Option<&str> = None;
        let mut data_json: Option<Value> = None;
        for line in text.lines() {
            if let Some(name) = line.strip_prefix("event:") {
                kind = Some(name.trim());
            } else if let Some(data) = line.strip_prefix("data:") {
                let data = data.trim();
                if data == "[DONE]" {
                    continue;
                }
                if data_json.is_none() {
                    data_json = serde_json::from_str::<Value>(data).ok();
                }
            }
        }
        let json_type = data_json
            .as_ref()
            .and_then(|v| v.get("type"))
            .and_then(|t| t.as_str());
        let kind = kind.or(json_type).unwrap_or("unknown").to_string();

        // Official success semantics: response.failed → failed; unparseable data →
        // failed; everything else → success.
        let is_failed_event = kind == "response.failed" || kind == "error";
        let success = !is_failed_event && data_json.is_some();
        let gap = self.gap_ms(now);
        self.metric_events.push((kind.clone(), success, gap));

        let Some(value) = data_json else {
            return;
        };
        match kind.as_str() {
            "response.completed" => {
                self.completed = true;
                if let Some(usage) = value.get("response").and_then(|r| r.get("usage")).cloned() {
                    self.usage = Some(usage);
                }
            }
            "response.failed" => {
                self.failed = value.get("response").and_then(|r| r.get("error")).cloned();
            }
            "error" => {
                self.failed = Some(value);
            }
            "response.output_item.done" => {
                if let Some(item) = value.get("item") {
                    self.handle_item(item, now);
                }
            }
            // Contentful streaming deltas (official TTFT predicate).
            "response.output_text.delta"
            | "response.reasoning_summary_text.delta"
            | "response.reasoning_text.delta" => {
                self.mark_contentful(now);
            }
            _ => {}
        }
    }

    fn handle_item(&mut self, item: &Value, now: Instant) {
        let Some(item_type) = item.get("type").and_then(|t| t.as_str()) else {
            return;
        };
        match item_type {
            "function_call" | "custom_tool_call" | "local_shell_call" => {
                self.mark_contentful(now);
                let call_id = item
                    .get("call_id")
                    .or_else(|| item.get("id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = item
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                self.tool_calls.push((call_id, name, item_type.to_string()));
            }
            "web_search_call" | "image_generation_call" => {
                self.mark_contentful(now);
                let id = item
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                self.tool_calls.push((id, String::new(), item_type.to_string()));
            }
            "message" => {
                // TTFT counts messages only when they carry non-empty text; TTFM is
                // the first assistant message item regardless.
                let has_text = item
                    .get("content")
                    .and_then(|c| c.as_array())
                    .is_some_and(|parts| {
                        parts.iter().any(|p| {
                            p.get("type").and_then(|t| t.as_str()) == Some("output_text")
                                && p.get("text")
                                    .and_then(|t| t.as_str())
                                    .is_some_and(|t| !t.is_empty())
                        })
                    });
                if has_text {
                    self.mark_contentful(now);
                }
                if self.first_agent_message_ms.is_none() {
                    self.first_agent_message_ms = Some(self.elapsed_ms(now));
                }
            }
            "reasoning" => {
                let has_summary = item
                    .get("summary")
                    .and_then(|s| s.as_array())
                    .is_some_and(|parts| {
                        parts.iter().any(|p| {
                            p.get("text")
                                .and_then(|t| t.as_str())
                                .is_some_and(|t| !t.is_empty())
                        })
                    });
                if has_summary {
                    self.mark_contentful(now);
                }
            }
            "compaction" => {
                self.saw_compaction_item = true;
            }
            _ => {}
        }
    }

    fn mark_contentful(&mut self, now: Instant) {
        if self.first_contentful_ms.is_none() {
            self.first_contentful_ms = Some(self.elapsed_ms(now));
        }
    }

    fn elapsed_ms(&self, now: Instant) -> u64 {
        self.started
            .map(|t| now.saturating_duration_since(t).as_millis() as u64)
            .unwrap_or(0)
    }
}

fn find_double_newline(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_usage_from_completed_event() {
        let mut tap = SseTap::new();
        tap.feed(b"data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\n");
        assert!(tap.completed);
        assert_eq!(
            tap.usage.as_ref().unwrap()["input_tokens"],
            serde_json::json!(10)
        );
    }

    #[test]
    fn records_metric_events_with_kind_and_success() {
        let mut tap = SseTap::new();
        tap.feed(b"event: response.created\ndata: {\"type\":\"response.created\",\"response\":{}}\n\n");
        tap.feed(b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n");
        tap.feed(b"event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"rate_limit_exceeded\"}}}\n\n");
        assert_eq!(tap.metric_events.len(), 3);
        assert_eq!(tap.metric_events[0].0, "response.created");
        assert!(tap.metric_events[0].1);
        assert_eq!(tap.metric_events[2].0, "response.failed");
        assert!(!tap.metric_events[2].1);
        assert!(tap.first_contentful_ms.is_some());
        assert!(tap.failed.is_some());
    }

    #[test]
    fn extracts_tool_calls_and_ttfm() {
        let mut tap = SseTap::new();
        tap.feed(b"event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"exec_command\",\"arguments\":\"{}\"}}\n\n");
        tap.feed(b"event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"done\"}]}}\n\n");
        assert_eq!(tap.tool_calls.len(), 1);
        assert_eq!(tap.tool_calls[0].0, "call_1");
        assert_eq!(tap.tool_calls[0].1, "exec_command");
        assert!(tap.first_agent_message_ms.is_some());
        assert!(tap.first_contentful_ms.is_some());
    }
}
