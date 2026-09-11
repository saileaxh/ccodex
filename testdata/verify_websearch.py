#!/usr/bin/env python3
"""端到端验证官方 standalone web search 流程（经 ccodex 中继）：
1) /v1/responses 声明 web.run 命名空间工具（lite → additional_tools 透传）
2) 模型发起 web.run 调用
3) /v1/alpha/search 执行搜索（中继转发到上游 alpha/search）
4) /v1/responses 提交 function_call_output，拿到引用真实数据的最终回答
"""
import json
import sys
import urllib.request

BASE = "http://127.0.0.1:8317"
KEY = json.load(open("/var/lib/ccodex/keys.json"))[0]["key"]
SESSION = "0ecc2c6a-6b60-4c7a-9f10-websearchtest01"[:36].ljust(36, "0")
MODEL = "gpt-5.6-terra"
PROMPT = "What is the current Bitcoin price in USD right now? Use the web.run tool to look it up."

WEB_RUN_DESC = (
    "Tool for accessing the internet. Commands: "
    'search_query: {"search_query": [{"q": "..."}]} searches the internet; '
    'open: {"open": [{"ref_id": "..."}]} opens a page.'
)

SCHEMA_FULL = {
    "type": "object",
    "properties": {
        "search_query": {
            "type": "array",
            "description": "Query the internet search engine for a given list of queries.",
            "items": {
                "type": "object",
                "properties": {
                    "q": {"type": "string", "description": "Search query."},
                    "recency": {"type": "integer"},
                    "domains": {"type": "array", "items": {"type": "string"}},
                },
                "required": ["q"],
                "additionalProperties": False,
            },
        }
    },
}
SCHEMA_EMPTY = {"type": "object", "properties": {}}


def post(path, body, timeout=240):
    req = urllib.request.Request(
        BASE + path,
        data=json.dumps(body).encode(),
        headers={
            "Authorization": "Bearer " + KEY,
            "Content-Type": "application/json",
            "session-id": SESSION,
        },
        method="POST",
    )
    chunks = []
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        while True:
            b = resp.read(1 << 16)
            if not b:
                break
            chunks.append(b)
    return b"".join(chunks)


def responses_call(input_items, tools):
    body = {
        "model": MODEL,
        "instructions": "You are a helpful assistant with web search tools.",
        "input": input_items,
        "stream": True,
        "store": False,
    }
    if tools:
        body["tools"] = tools
    raw = post("/v1/responses", body)
    items, completed = [], None
    for block in raw.decode("utf-8", "replace").split("\n\n"):
        for line in block.splitlines():
            if not line.startswith("data:"):
                continue
            try:
                d = json.loads(line[5:].strip())
            except Exception:
                continue
            t = d.get("type")
            if t == "response.output_item.done":
                items.append(d.get("item", {}))
            elif t == "response.completed":
                completed = d.get("response", {})
            elif t == "response.failed":
                print("FAILED:", json.dumps(d)[:400])
                sys.exit(1)
    return items, completed


def user_msg(text):
    return {
        "type": "message",
        "role": "user",
        "content": [{"type": "input_text", "text": text}],
    }


def make_tools(schema):
    return [
        {
            "type": "namespace",
            "name": "web",
            "description": "Tools in the web namespace.",
            "tools": [
                {
                    "type": "function",
                    "name": "run",
                    "description": WEB_RUN_DESC,
                    "strict": False,
                    "parameters": schema,
                }
            ],
        }
    ]


print("== step 1: declare web.run, ask for BTC price ==")
try:
    items, _ = responses_call([user_msg(PROMPT)], make_tools(SCHEMA_FULL))
    print("   full-schema declaration accepted")
except urllib.error.HTTPError as e:
    detail = e.read().decode()[:200]
    print(f"   full schema rejected ({e.code}): {detail}")
    print("   falling back to empty-properties schema + rich description")
    items, _ = responses_call([user_msg(PROMPT)], make_tools(SCHEMA_EMPTY))

call = next(
    (i for i in items if i.get("type") == "function_call" and i.get("namespace") == "web"),
    None,
)
if not call:
    print("NO web.run call. Items:", json.dumps([i.get("type") for i in items]))
    for i in items:
        if i.get("type") == "message":
            print(json.dumps(i.get("content"), ensure_ascii=False)[:300])
    sys.exit(2)

call_id = call["call_id"]
args = json.loads(call.get("arguments") or "{}")
print(f"   model called web.run call_id={call_id} args={json.dumps(args, ensure_ascii=False)}")

print("== step 2: execute via relay /v1/alpha/search ==")
search_body = {
    "id": SESSION,
    "model": MODEL,
    "commands": args,
    "settings": {"external_web_access": True},
    "max_output_tokens": 2048,
}
try:
    search_raw = post("/v1/alpha/search", search_body)
except urllib.error.HTTPError as e:
    print(f"   search rejected ({e.code}):", e.read().decode()[:300])
    sys.exit(3)
search = json.loads(search_raw)
output_text = search.get("output", "")
results = search.get("results") or []
print(f"   output {len(output_text)} chars, results: {len(results)}")
for r in results[:3]:
    print("   -", r.get("title"), "|", r.get("url"))
print("   output head:", output_text[:240].replace("\n", " "))

print("== step 3: submit function_call_output ==")
fc_item = {k: v for k, v in call.items() if k in ("type", "id", "call_id", "name", "namespace", "arguments", "status")}
next_input = [
    user_msg(PROMPT),
    fc_item,
    {"type": "function_call_output", "call_id": call_id, "output": output_text},
]
items2, completed2 = responses_call(next_input, make_tools(SCHEMA_EMPTY))
final = ""
for i in items2:
    if i.get("type") == "message":
        for c in i.get("content") or []:
            if c.get("type") in ("output_text", "text"):
                final += c.get("text", "")
usage = (completed2 or {}).get("usage", {})
print("== FINAL ANSWER ==")
print(final[:600])
print("== usage:", json.dumps(usage))
has_citation = "http" in final or "turn" in final or "[" in final
print("== mentions sources/citations:", has_citation)
