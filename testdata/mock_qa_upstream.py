"""Scenario mock upstream: drives the request_user_input_async question flow.

Request 1 (no function_call_output in input): returns a function_call to
request_user_input_async, then response.completed.
Request 2+ (has function_call_output): returns a plain text answer, completed.

Logs every request body (decoded if zstd) to mock_qa.log for shape inspection.
"""
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

LOG = "testdata/mock_qa.log"

QUESTION_ARGS = json.dumps({
    "questions": [{"title": "Pick one option", "options": ["A", "B"]}]
})

FC_SSE = (
    'data: {"type":"response.output_item.added","output_index":0,'
    '"item":{"type":"function_call","id":"fc_qa1","call_id":"call_qa1",'
    '"name":"request_user_input_async","arguments":""}}\n\n'
    'data: {"type":"response.function_call_arguments.delta","item_id":"fc_qa1",'
    '"output_index":0,"delta":' + json.dumps(QUESTION_ARGS) + '}\n\n'
    'data: {"type":"response.output_item.done","output_index":0,'
    '"item":{"type":"function_call","id":"fc_qa1","call_id":"call_qa1",'
    '"name":"request_user_input_async","arguments":' + json.dumps(QUESTION_ARGS) + '}}\n\n'
    'data: {"type":"response.completed","response":{"id":"resp_qa1","status":"completed",'
    '"output":[{"type":"function_call","id":"fc_qa1","call_id":"call_qa1",'
    '"name":"request_user_input_async","arguments":' + json.dumps(QUESTION_ARGS) + '}],'
    '"usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}}\n\n'
)

TEXT_SSE = (
    'data: {"type":"response.output_text.delta","delta":"got it, continuing"}\n\n'
    'data: {"type":"response.completed","response":{"id":"resp_qa2","status":"completed",'
    '"output":[{"type":"message","role":"assistant","content":'
    '[{"type":"output_text","text":"got it, continuing"}]}],'
    '"usage":{"input_tokens":20,"output_tokens":6,"total_tokens":26}}}\n\n'
)


def read_body(handler) -> dict:
    body_len = int(handler.headers.get("content-length", 0) or 0)
    body = handler.rfile.read(body_len) if body_len else b""
    if handler.headers.get("content-encoding") == "zstd":
        import zstandard  # type: ignore

        body = zstandard.ZstdDecompressor().decompress(body, max_output_size=1 << 20)
    return json.loads(body) if body else {}


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        if self.path != "/responses":
            self.send_response(404)
            self.send_header("content-length", "0")
            self.end_headers()
            return
        body = read_body(self)
        has_tool_output = any(
            isinstance(item, dict) and item.get("type") == "function_call_output"
            for item in body.get("input", [])
        )
        with open(LOG, "a", encoding="utf-8") as f:
            f.write(json.dumps({
                "n_items": len(body.get("input", [])),
                "item_types": [i.get("type") for i in body.get("input", []) if isinstance(i, dict)],
                "has_tool_output": has_tool_output,
                "tools": [t.get("name") or t.get("type") for t in body.get("tools", [])][:20],
                "instructions_len": len(body.get("instructions", "") or ""),
            }, ensure_ascii=False) + "\n")
        payload = (TEXT_SSE if has_tool_output else FC_SSE).encode()
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("x-codex-primary-used-percent", "23")
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_GET(self):
        if self.path.startswith("/models"):
            payload = json.dumps(
                {"object": "list", "data": [{"id": "gpt-6-astra", "object": "model"}]}
            ).encode()
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
        else:
            self.send_response(404)
            self.send_header("content-length", "0")
            self.end_headers()

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 9124
    HTTPServer(("127.0.0.1", port), Handler).serve_forever()
