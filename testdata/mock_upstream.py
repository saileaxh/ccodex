#!/usr/bin/env python3
"""ccodex 冒烟测试 mock 上游：记录请求头/体，返回固定 SSE。"""
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

LOG = sys.argv[2] if len(sys.argv) > 2 else "mock_upstream.log"

SSE_BODY = (
    'data: {"type":"response.output_text.delta","delta":"hello"}\n\n'
    'data: {"type":"response.completed","response":{"id":"resp_smoke","status":"completed",'
    '"usage":{"input_tokens":42,"output_tokens":7,"total_tokens":49}}}\n\n'
)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _log(self, extra=""):
        body_len = int(self.headers.get("content-length", 0) or 0)
        body = self.rfile.read(body_len) if body_len else b""
        record = {
            "method": self.command,
            "path": self.path,
            "headers": {k.lower(): v for k, v in self.headers.items()},
            "body_hex_prefix": body[:32].hex(),
            "body_len": len(body),
            "extra": extra,
        }
        # zstd 压缩体尝试解码（装了 zstandard 才验证内容）
        try:
            import zstandard  # type: ignore

            if record["headers"].get("content-encoding") == "zstd":
                record["body_json"] = json.loads(
                    zstandard.ZstdDecompressor().decompress(body, max_output_size=1 << 20)
                )
        except Exception as e:  # noqa: BLE001
            record["body_decode_error"] = str(e)
        with open(LOG, "a", encoding="utf-8") as f:
            f.write(json.dumps(record, ensure_ascii=False) + "\n")

    def do_POST(self):
        self._log()
        if self.path == "/responses":
            payload = SSE_BODY.encode()
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("x-codex-primary-used-percent", "23")
            self.send_header("content-length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
        else:
            self.send_response(404)
            self.send_header("content-length", "0")
            self.end_headers()

    def do_GET(self):
        self._log()
        if self.path.startswith("/models"):
            payload = json.dumps(
                {"object": "list", "data": [{"id": "gpt-5.6-sol", "object": "model"}]}
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
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 9123
    HTTPServer(("127.0.0.1", port), Handler).serve_forever()
