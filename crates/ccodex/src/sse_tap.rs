//! Passive SSE stream analysis: scans events while bytes pass through, extracting
//! usage accounting from response.completed and detecting response.failed / error
//! terminal states. Read-only — never rewrites a single byte.

#[derive(Default)]
pub struct SseTap {
    buf: Vec<u8>,
    pub usage: Option<serde_json::Value>,
    pub failed: Option<serde_json::Value>,
    pub completed: bool,
}

impl SseTap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
        // Events are separated by blank lines.
        while let Some(pos) = find_double_newline(&self.buf) {
            let event: Vec<u8> = self.buf.drain(..pos).collect();
            self.buf.drain(..2.min(self.buf.len()));
            self.handle_event(&event);
        }
        // Defensive: drop abnormally long partial lines.
        if self.buf.len() > 1 << 20 {
            self.buf.clear();
        }
    }

    fn handle_event(&mut self, event: &[u8]) {
        let Ok(text) = std::str::from_utf8(event) else {
            return;
        };
        let quick = |needle: &str| text.contains(needle);
        if !quick("response.completed") && !quick("response.failed") && !quick("\"error\"") {
            return;
        }
        for line in text.lines() {
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(data.trim()) else {
                continue;
            };
            let kind = value.get("type").and_then(|t| t.as_str()).unwrap_or("");
            match kind {
                "response.completed" => {
                    self.completed = true;
                    if let Some(usage) = value.get("response").and_then(|r| r.get("usage")).cloned()
                    {
                        self.usage = Some(usage);
                    }
                }
                "response.failed" => {
                    self.failed = value.get("response").and_then(|r| r.get("error")).cloned();
                }
                "error" => {
                    self.failed = Some(value);
                }
                _ => {}
            }
        }
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
}
