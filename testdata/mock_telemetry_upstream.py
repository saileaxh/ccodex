#!/usr/bin/env python3
"""本地 mock 上游 + 遥测接收端（遥测自检用）。

（区别于 testdata/mock_upstream.py：那个只做请求录制与固定 SSE 的冒烟 mock，
本脚本额外提供 analytics/OTLP 接收端与场景开关。）

Two servers:

  1. mock upstream  (--port, default 8899)
       - POST /backend-api/codex/responses        -> canned SSE stream
       - POST /backend-api/codex/responses/compact-> canned unary JSON
       - POST /backend-api/codex/alpha/search     -> canned unary JSON
       - POST /<anything>/analytics-events/events -> ANALYTICS SINK
       - POST /<anything>/otlp/v1/metrics         -> OTLP SINK
     Requests to the analytics/OTLP sinks are recorded to --capture (JSONL).
     Scenario control (SSE failures) via POST /__control.

  2. ccodex is started pointed at server 1 with:
       CCODEX_UPSTREAM_BASE_URL_OVERRIDE=http://127.0.0.1:<port>/backend-api/codex
       CCODEX_ANALYTICS_BASE_URL_OVERRIDE=http://127.0.0.1:<port>/backend-api
       CCODEX_STATSIG_ENDPOINT_OVERRIDE=http://127.0.0.1:<port>/otlp/v1/metrics

Usage:
    python testdata/mock_telemetry_upstream.py --port 8899 --capture testdata/_capture.jsonl
"""

import argparse
import json
import threading
import time
import urllib.parse
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

CAPTURE_PATH = "testdata/_capture.jsonl"
CAPTURE_LOCK = threading.Lock()

# Scenario knobs (set via POST /__control).
CONTROL = {
    # "ok" | "fail_mid_stream" | "truncate" | "http_429" | "http_500" | "http_503" | "http_400"
    "sse": "ok",
    "tool_call": False,       # emit a function_call + tool output continuation prompt
    "compaction_item": False,  # emit a compaction item in the stream
    "reasoning": False,        # emit reasoning summary events (TTFM covers reasoning too)
    "usage_cached": 1024,      # cached_input_tokens reported
    "usage_output": 50,
    "usage_reasoning": 20,
}


def capture(record: dict) -> None:
    with CAPTURE_LOCK:
        with open(CAPTURE_PATH, "a", encoding="utf-8") as fh:
            fh.write(json.dumps(record, ensure_ascii=False) + "\n")


def sse(obj: dict) -> bytes:
    return ("data: " + json.dumps(obj, ensure_ascii=False) + "\n\n").encode()


def build_stream(control: dict, response_id: str) -> list[bytes]:
    """The SSE events the official client would receive for a simple text turn."""
    events: list[bytes] = []

    def ev(**kw):
        events.append(sse(kw))

    ev(type="response.created", response={"id": response_id, "status": "in_progress"})

    if control.get("reasoning"):
        ev(type="response.reasoning_summary_text.delta", delta="thinking about it")
        ev(type="response.output_item.done", item={
            "type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "thought"}]})

    if control.get("compaction_item"):
        ev(type="response.output_item.done", item={
            "type": "compaction", "id": "cmp_1", "encrypted_content": "xxx"})

    if control.get("tool_call"):
        call_id = "call_" + uuid.uuid4().hex[:12]
        ev(type="response.output_item.done", item={
            "type": "function_call", "id": "fc_1", "name": "shell",
            "arguments": json.dumps({"command": ["ls"]}), "call_id": call_id})
        ev(type="response.completed", response={
            "id": response_id, "status": "completed",
            "usage": {
                "input_tokens": 1000,
                "input_tokens_details": {"cached_tokens": control["usage_cached"]},
                "output_tokens": control["usage_output"],
                "output_tokens_details": {"reasoning_tokens": control["usage_reasoning"]},
                "total_tokens": 1000 + control["usage_output"],
            },
        })
        return events

    ev(type="response.output_text.delta", delta="Hello")
    ev(type="response.output_text.delta", delta=" world")
    ev(type="response.output_item.done", item={
        "type": "message", "id": "msg_1", "role": "assistant",
        "content": [{"type": "output_text", "text": "Hello world"}]})
    ev(type="response.completed", response={
        "id": response_id, "status": "completed",
        "usage": {
            "input_tokens": 1000,
            "input_tokens_details": {"cached_tokens": control["usage_cached"]},
            "output_tokens": control["usage_output"],
            "output_tokens_details": {"reasoning_tokens": control["usage_reasoning"]},
            "total_tokens": 1000 + control["usage_output"],
        },
    })
    return events


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):  # silence
        pass

    def _read_body(self) -> bytes:
        length = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(length) if length else b""

    def _json(self, code: int, obj: dict, extra_headers: dict | None = None) -> None:
        body = json.dumps(obj, ensure_ascii=False).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        for k, v in (extra_headers or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(body)

    def _absorb_proxy_form(self) -> bool:
        """Absolute-form request line = the client sent this through an HTTP proxy.

        Used by the proxy-egress assertion: the e2e binds the account to this server as
        its proxy, so telemetry that honours the binding arrives in absolute form.
        """
        if not self.path.startswith("http://"):
            return False
        parsed = urllib.parse.urlsplit(self.path)
        self.path = parsed.path + (f"?{parsed.query}" if parsed.query else "")
        self.proxy_form = True
        return True

    def do_POST(self):  # noqa: N802
        self.proxy_form = False
        self._absorb_proxy_form()
        path = self.path.split("?")[0]
        body = self._read_body()

        if path == "/__control":
            try:
                CONTROL.update(json.loads(body or b"{}"))
            except Exception as e:  # noqa: BLE001
                return self._json(400, {"error": str(e)})
            return self._json(200, {"control": CONTROL})

        # ---- telemetry sinks -------------------------------------------------
        if path.endswith("/analytics-events/events"):
            try:
                payload = json.loads(body or b"{}")
            except Exception:  # noqa: BLE001
                payload = {"_unparsed": body.decode("utf-8", "replace")}
            capture({
                "sink": "analytics",
                "path": path,
                "proxy_form": self.proxy_form,
                "authorization": self.headers.get("Authorization"),
                "account_id": self.headers.get("chatgpt-account-id"),
                "content_type": self.headers.get("Content-Type"),
                "user_agent": self.headers.get("User-Agent"),
                "received_at": time.time(),
                "payload": payload,
            })
            return self._json(200, {})

        if path.endswith("/otlp/v1/metrics"):
            try:
                payload = json.loads(body or b"{}")
            except Exception:  # noqa: BLE001
                payload = {"_unparsed": body.decode("utf-8", "replace")}
            capture({
                "sink": "otlp",
                "path": path,
                "proxy_form": self.proxy_form,
                "statsig_key": self.headers.get("statsig-api-key"),
                "content_type": self.headers.get("Content-Type"),
                "received_at": time.time(),
                "payload": payload,
            })
            return self._json(200, {})

        # ---- upstream data plane --------------------------------------------
        # Data-plane requests are recorded too (shape only): the proxy-egress check
        # compares their form against the telemetry channels'.
        if path.endswith(("/responses", "/responses/compact", "/alpha/search")):
            capture({"sink": "upstream", "path": path, "proxy_form": self.proxy_form})
        if path.endswith("/responses/compact"):
            return self._json(200, {
                "id": "resp_compact_1",
                "object": "response.compaction",
                "created_at": int(time.time()),
                "output": [{"type": "compaction", "id": "cmp_1", "encrypted_content": "yyy"}],
                "usage": {
                    "input_tokens": 5000,
                    "input_tokens_details": {"cached_tokens": 2048},
                    "output_tokens": 300,
                    "output_tokens_details": {"reasoning_tokens": 0},
                    "total_tokens": 5300,
                },
            })

        if path.endswith("/alpha/search"):
            return self._json(200, {"results": [], "id": "search_1"})

        if path.endswith("/responses"):
            scenario = CONTROL.get("sse", "ok")
            if scenario.startswith("http_"):
                code = int(scenario.split("_", 1)[1])
                # Official api_bridge predicates: 503 keys on error.code, 429 on
                # error.type, 400 on error.code.
                err = {
                    429: {"error": {"type": "usage_limit_reached", "message": "limit"}},
                    500: {"error": {"code": "server_error", "message": "boom"}},
                    503: {"error": {"code": "server_is_overloaded", "message": "busy"}},
                    400: {"error": {"code": "invalid_request_error", "message": "bad"}},
                }.get(code, {"error": {"message": "err"}})
                return self._json(code, err)

            response_id = "resp_" + uuid.uuid4().hex[:12]
            events = build_stream(CONTROL, response_id)

            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Cache-Control", "no-cache")
            self.send_header("Transfer-Encoding", "chunked")
            self.end_headers()

            def chunk(payload: bytes):
                self.wfile.write(b"%x\r\n" % len(payload) + payload + b"\r\n")

            try:
                if scenario == "fail_mid_stream":
                    for ev in events[: max(1, len(events) // 2)]:
                        chunk(ev)
                        self.wfile.flush()
                    chunk(sse({"type": "response.failed", "response": {
                        "id": response_id, "status": "failed",
                        "error": {"code": "rate_limit_exceeded", "message": "slow down"}}}))
                    chunk(b"")
                    self.wfile.write(b"0\r\n\r\n")
                    self.wfile.flush()
                    return
                for ev in events:
                    chunk(ev)
                    self.wfile.flush()
                    time.sleep(0.01)
                if scenario == "truncate":
                    # Cut the connection without a terminal event.
                    self.close_connection = True
                    return
                chunk(b"")
                self.wfile.write(b"0\r\n\r\n")
                self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                pass
            return

        return self._json(404, {"error": {"message": f"mock: no route for {path}"}})

    def do_GET(self):  # noqa: N802
        path = self.path.split("?")[0]
        if path == "/__health":
            return self._json(200, {"ok": True, "control": CONTROL})
        # /models intentionally 404s: the relay keeps its embedded models.json snapshot.
        return self._json(404, {"error": {"message": f"mock: no route for {path}"}})


def main():
    global CAPTURE_PATH
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8899)
    ap.add_argument("--capture", default=CAPTURE_PATH)
    args = ap.parse_args()
    CAPTURE_PATH = args.capture
    open(CAPTURE_PATH, "w", encoding="utf-8").close()

    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print(f"mock upstream on http://127.0.0.1:{args.port}  capture -> {CAPTURE_PATH}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
