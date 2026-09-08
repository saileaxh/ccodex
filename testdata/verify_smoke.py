#!/usr/bin/env python3
"""冒烟验证：mock 上游日志对照官方 0.153.x 请求形状（lite 模型 gpt-5.6-sol）。

用法：先启动 mock_upstream.py 与 ccodex（CCODEX_UPSTREAM_BASE_URL_OVERRIDE 指向 mock），
发一次 POST /v1/responses（testdata/smoke_request.json），再运行本脚本。
"""
import json
import re
import sys

records = [
    json.loads(line)
    for line in open("testdata/mock_upstream.log", encoding="utf-8")
    if line.strip()
]
records = [r for r in records if r.get("method") == "POST" and r.get("path") == "/responses"]
assert records, "mock 上游没有收到 /responses 请求"
rec = records[-1]
headers = rec["headers"]
body = rec.get("body_json")
assert body, "请求体未能 zstd 解码（需要 pip install zstandard）"

failures = []


def check(name, cond, detail=""):
    if cond:
        print(f"  ok   {name}")
    else:
        failures.append(f"{name} {detail}")
        print(f"  FAIL {name} {detail}")


print("== headers ==")
check("authorization", headers.get("authorization", "").startswith("Bearer "))
check("chatgpt-account-id", headers.get("chatgpt-account-id") == "account-123")
check("originator", headers.get("originator") == "codex_cli_rs")
ua = headers.get("user-agent", "")
check(
    "user-agent 官方格式",
    re.fullmatch(r"codex_cli_rs/\d+\.\d+\.\d+ \(Windows 10\.0\.26200; x86_64\) WindowsTerminal/1\.21", ua) is not None,
    ua,
)
check("version 头与 UA 同版本", headers.get("version") and headers["version"] in ua)
check("session-id 为 uuid v7", re.fullmatch(r"[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}", headers.get("session-id", "")) is not None, headers.get("session-id", ""))
check("thread-id 为 uuid v7 且 == x-client-request-id",
      re.fullmatch(r"[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}", headers.get("thread-id", "")) is not None
      and headers.get("thread-id") == headers.get("x-client-request-id"))
check("accept", headers.get("accept") == "text/event-stream")
check("content-encoding zstd", headers.get("content-encoding") == "zstd")
check("x-codex-window-id = thread:0", headers.get("x-codex-window-id") == f"{headers.get('thread-id')}:0")
check("responses-lite 头（lite 模型）", headers.get("x-openai-internal-codex-responses-lite") == "true")
# 官方默认不发送的头，一律不应出现
for absent in ("x-codex-beta-features", "x-codex-turn-state", "traceparent", "tracestate", "conversation_id"):
    check(f"无 {absent}", absent not in headers, headers.get(absent, ""))
turn_meta_header = headers.get("x-codex-turn-metadata")
check("x-codex-turn-metadata 头存在", turn_meta_header is not None)

print("== body 顶层（官方 ResponsesApiRequest 字段序）==")
check(
    "字段集合与顺序",
    list(body.keys())
    == [
        "model", "input", "tool_choice", "parallel_tool_calls", "reasoning",
        "store", "stream", "include", "prompt_cache_key", "text", "client_metadata",
    ],
    str(list(body.keys())),
)
check("store=false", body["store"] is False)
check("stream=true", body["stream"] is True)
check("tool_choice=auto", body["tool_choice"] == "auto")
check("parallel_tool_calls=false（lite）", body["parallel_tool_calls"] is False)
check("include", body["include"] == ["reasoning.encrypted_content"])
check("reasoning lite 形状", body["reasoning"] == {"effort": "low", "context": "all_turns"}, str(body["reasoning"]))
check("text.verbosity 模型默认", body["text"] == {"verbosity": "low"}, str(body["text"]))
check("prompt_cache_key == session-id", body["prompt_cache_key"] == headers.get("session-id"))
# 下游专有字段一律剥离
for stripped in ("temperature", "max_output_tokens", "previous_response_id", "service_tier", "stream_options", "access_programs", "tools", "instructions"):
    check(f"无 {stripped}", stripped not in body)

print("== client_metadata（官方六键）==")
cm = body["client_metadata"]
check("键集", set(cm.keys()) == {
    "x-codex-installation-id", "session_id", "thread_id",
    "x-codex-window-id", "turn_id", "x-codex-turn-metadata",
}, str(set(cm.keys())))
check("installation_id 为 uuid v4", re.fullmatch(r"[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}", cm["x-codex-installation-id"]) is not None, cm["x-codex-installation-id"])
check("turn_id 为 uuid v7", re.fullmatch(r"[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}", cm["turn_id"]) is not None, cm["turn_id"])
check("turn_metadata 头体一致", cm["x-codex-turn-metadata"] == turn_meta_header)
tm = json.loads(cm["x-codex-turn-metadata"])
check("tm.request_kind", tm.get("request_kind") == "turn")
check("tm.agent_name", tm.get("agent_name") == "root")
check("tm.thread_source", tm.get("thread_source") == "user")
check("tm.window", tm.get("window_id") == headers.get("x-codex-window-id") and tm.get("window_number") == 0)
check("tm.sandbox 默认", tm.get("sandbox") == "none" and tm.get("sandbox_mode") == "read-only")
check("tm.auto_review", tm.get("auto_review_enabled") is False)
check("tm.turn_started_at_unix_ms", isinstance(tm.get("turn_started_at_unix_ms"), int))

print("== input（lite 前缀项 + 用户消息）==")
items = body["input"]
check("前缀 additional_tools", items[0].get("type") == "additional_tools" and items[0].get("id", "").startswith("at_") and items[0].get("role") == "developer")
check("前缀 base instructions message",
      items[1].get("type") == "message" and items[1].get("id", "").startswith("msg_")
      and items[1].get("role") == "developer"
      and items[1].get("internal_chat_message_metadata_passthrough", {}).get("content_item_kinds") == ["model.base_instructions"]
      and items[1]["content"][0]["text"].startswith("You are Codex"))
check("用户消息原样", items[2].get("role") == "user" and items[2]["content"][0]["text"] == "hi")

if failures:
    print(f"\nSMOKE FAIL ({len(failures)} 项)")
    sys.exit(1)
print("\nSMOKE PASS")
