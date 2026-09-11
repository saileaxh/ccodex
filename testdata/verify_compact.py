#!/usr/bin/env python3
"""冒烟验证：legacy /responses/compact 转发 + v2 compaction_trigger 形状（对照官方 0.153.x）。

用法：先启动 mock_upstream.py 与 ccodex（CCODEX_UPSTREAM_BASE_URL_OVERRIDE 指向 mock），
再运行本脚本（自发请求并检查 testdata/mock_upstream.log 最后两条记录）。
"""
import json
import os
import re
import sys
import urllib.request

RELAY = os.environ.get("CCODEX_RELAY", "http://127.0.0.1:8317")
KEY = os.environ.get("CCODEX_KEY", "")
LOG = os.environ.get("MOCK_LOG", "testdata/mock_upstream.log")
MODEL = "gpt-5.6-sol"  # lite 模型（与 verify_smoke 一致）
SESSION = "3fa85f64-5717-4562-b3fc-2c963f66afa6"

V7 = r"[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}"
V4 = r"[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}"

failures = []


def check(name, cond, detail=""):
    if cond:
        print(f"  ok   {name}")
    else:
        failures.append(f"{name} {detail}")
        print(f"  FAIL {name} {detail}")


def post(path, body, extra_headers=None):
    headers = {"Content-Type": "application/json", "session-id": SESSION}
    if KEY:
        headers["Authorization"] = "Bearer " + KEY
    headers.update(extra_headers or {})
    req = urllib.request.Request(
        RELAY + path, data=json.dumps(body).encode(), headers=headers, method="POST"
    )
    with urllib.request.urlopen(req, timeout=120) as resp:
        return resp.status, dict(resp.headers), resp.read()


def last_record(path):
    records = [
        json.loads(line)
        for line in open(LOG, encoding="utf-8")
        if line.strip()
    ]
    records = [r for r in records if r.get("method") == "POST" and r.get("path") == path]
    assert records, f"mock 上游没有收到 {path} 请求"
    return records[-1]


def user_msg(text):
    return {
        "type": "message",
        "role": "user",
        "content": [{"type": "input_text", "text": text}],
    }


def assistant_msg(text):
    return {
        "type": "message",
        "role": "assistant",
        "content": [{"type": "output_text", "text": text}],
    }


COMPACT_META = {
    "trigger": "auto",
    "reason": "context_limit",
    "implementation": "responses_compact",
    "phase": "mid_turn",
    "strategy": "memento",
}
COMPACT_TM_HEADER = json.dumps({"request_kind": "compaction", "compaction": COMPACT_META})

print("== 1) legacy POST /v1/responses/compact ==")
compact_body = {
    "model": MODEL,
    "instructions": "You are a helpful assistant.",
    "input": [user_msg("hello"), assistant_msg("hi there"), user_msg("bye")],
    "parallel_tool_calls": True,
    "reasoning": {"effort": "low", "context": "all_turns"},
    "text": {"verbosity": "low"},
    # 下游专有/无关字段，应被剥离：
    "store": False,
    "stream": True,
    "temperature": 0.9,
    "client_metadata": {"session_id": "should-be-dropped"},
    "id": "should-be-dropped",
}
status, resp_headers, raw = post(
    "/v1/responses/compact",
    compact_body,
    {"x-codex-turn-metadata": COMPACT_TM_HEADER},
)
check("中继 200", status == 200, str(status))
resp = json.loads(raw)
check("响应 output 镜像", isinstance(resp.get("output"), list), str(resp)[:120])
check("turn-state 响应头镜像", resp_headers.get("x-codex-turn-state") == "mock-turn-state-1")

rec = last_record("/responses/compact")
headers = rec["headers"]
body = rec.get("body_json")
check("请求体为未压缩 JSON", body is not None)

print("-- compact 头 --")
check("session-id 为 uuid v7", re.fullmatch(V7, headers.get("session-id", "")) is not None)
check("thread-id 为 uuid v7", re.fullmatch(V7, headers.get("thread-id", "")) is not None)
check("无 x-client-request-id（官方 compact 只发双会话头）", "x-client-request-id" not in headers)
check("无 accept: text/event-stream（unary）", headers.get("accept") != "text/event-stream")
check("installation-id 头为 uuid v4", re.fullmatch(V4, headers.get("x-codex-installation-id", "")) is not None)
check("x-codex-window-id = thread:0", headers.get("x-codex-window-id") == f"{headers.get('thread-id')}:0")
check("routing-hint", headers.get("x-codex-routing-hint") == f"model={MODEL}", headers.get("x-codex-routing-hint", ""))
check("responses-lite 头（lite 模型）", headers.get("x-openai-internal-codex-responses-lite") == "true")
for absent in ("x-codex-beta-features", "x-codex-turn-state", "conversation_id"):
    check(f"无 {absent}", absent not in headers, headers.get(absent, ""))
tm = json.loads(headers.get("x-codex-turn-metadata", "{}"))
check("tm.request_kind=compaction", tm.get("request_kind") == "compaction")
check("tm.compaction 块复制下游", tm.get("compaction") == COMPACT_META, str(tm.get("compaction")))
check("tm.session/thread 与头一致", tm.get("session_id") == headers.get("session-id") and tm.get("thread_id") == headers.get("thread-id"))

print("-- compact 体（官方 CompactionInput 字段序）--")
check(
    "字段集合与顺序",
    list(body.keys())
    == ["model", "input", "instructions", "parallel_tool_calls", "reasoning", "prompt_cache_key", "text"],
    str(list(body.keys())),
)
check("model", body["model"] == MODEL)
check("instructions 原样", body["instructions"] == "You are a helpful assistant.")
check("input 三项原样", len(body["input"]) == 3 and body["input"][0]["role"] == "user")
check("parallel_tool_calls", body["parallel_tool_calls"] is True)
check("reasoning 复制下游", body["reasoning"] == {"effort": "low", "context": "all_turns"}, str(body["reasoning"]))
check("text 复制下游", body["text"] == {"verbosity": "low"})
check("prompt_cache_key == session-id", body["prompt_cache_key"] == headers.get("session-id"))
for stripped in ("store", "stream", "temperature", "client_metadata", "id", "include", "tool_choice", "service_tier"):
    check(f"无 {stripped}", stripped not in body)

print("== 2) v2：POST /v1/responses 带 compaction_trigger 项 ==")
V2_META = dict(COMPACT_META, implementation="responses_compaction_v2", trigger="auto", phase="pre_turn")
v2_body = {
    "model": MODEL,
    "input": [user_msg("hello"), assistant_msg("hi"), {"type": "compaction_trigger"}],
    "stream": True,
}
status, _, raw = post(
    "/v1/responses",
    v2_body,
    {"x-codex-turn-metadata": json.dumps({"request_kind": "compaction", "compaction": V2_META})},
)
check("中继 200", status == 200, f"{status}: {raw[:200]}")

rec = last_record("/responses")
headers = rec["headers"]
body = rec.get("body_json")
check("routing-hint 头存在", headers.get("x-codex-routing-hint") == f"model={MODEL}", headers.get("x-codex-routing-hint", ""))
tm = json.loads(headers.get("x-codex-turn-metadata", "{}"))
check("tm.request_kind=compaction", tm.get("request_kind") == "compaction", tm.get("request_kind", ""))
check("tm.compaction 块复制下游（v2）", tm.get("compaction") == V2_META, str(tm.get("compaction")))
items = body["input"]
check("compaction_trigger 项保留在末尾", items[-1].get("type") == "compaction_trigger", str([i.get("type") for i in items]))

print("== 3) 普通 /v1/responses 仍是 request_kind=turn ==")
status, _, _ = post("/v1/responses", {"model": MODEL, "input": [user_msg("hi")], "stream": True})
check("中继 200", status == 200, str(status))
rec = last_record("/responses")
tm = json.loads(rec["headers"].get("x-codex-turn-metadata", "{}"))
check("tm.request_kind=turn", tm.get("request_kind") == "turn")
check("无 compaction 块", "compaction" not in tm)
check("routing-hint 头存在", rec["headers"].get("x-codex-routing-hint") == f"model={MODEL}")

if failures:
    print(f"\nCOMPACT SMOKE FAIL ({len(failures)} 项)")
    sys.exit(1)
print("\nCOMPACT SMOKE PASS")
