#!/usr/bin/env python3
"""Live verification on server A: legacy /responses/compact + v2 compaction_trigger
through the ccodex relay against the real upstream. Small payloads (quota-conscious)."""
import json
import sys
import urllib.request

BASE = "http://127.0.0.1:8317"
KEY = json.load(open("/var/lib/ccodex/keys.json"))[0]["key"]
SESSION = "7c9e6679-7425-40de-944b-e07fc1f90ae7"
MODEL = "gpt-5.6-terra"

COMPACT_META = {
    "trigger": "manual",
    "reason": "user_requested",
    "implementation": "responses_compact",
    "phase": "standalone_turn",
    "strategy": "memento",
}


def post(path, body, extra=None, timeout=300):
    headers = {
        "Authorization": "Bearer " + KEY,
        "Content-Type": "application/json",
        "session-id": SESSION,
    }
    headers.update(extra or {})
    req = urllib.request.Request(
        BASE + path, data=json.dumps(body).encode(), headers=headers, method="POST"
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.status, dict(resp.headers), resp.read()
    except urllib.error.HTTPError as e:
        return e.code, dict(e.headers), e.read()


def user_msg(t):
    return {"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": t}]}


def assistant_msg(t):
    return {"type": "message", "role": "assistant",
            "content": [{"type": "output_text", "text": t}]}


print("== 1) legacy POST /v1/responses/compact (live upstream) ==")
body = {
    "model": MODEL,
    "instructions": "You are a helpful assistant.",
    "input": [
        user_msg("My favorite color is blue. What is 2+2?"),
        assistant_msg("2+2 equals 4."),
        user_msg("What is my favorite color?"),
    ],
}
status, hdrs, raw = post(
    "/v1/responses/compact", body,
    {"x-codex-turn-metadata": json.dumps({"request_kind": "compaction", "compaction": COMPACT_META})},
)
print(f"   status={status}")
if status == 200:
    resp = json.loads(raw)
    out = resp.get("output", [])
    print(f"   output items: {len(out)}")
    for item in out[:4]:
        t = item.get("type")
        text = ""
        for c in item.get("content") or []:
            text += c.get("text", "")
        print(f"   - {t}: {text[:160]!r}")
    ts = hdrs.get("x-codex-turn-state")
    print(f"   x-codex-turn-state mirrored: {bool(ts)}")
else:
    print("   rejected:", raw.decode("utf-8", "replace")[:400])

print("== 2) v2 POST /v1/responses with compaction_trigger (live upstream) ==")
v2_meta = dict(COMPACT_META, implementation="responses_compaction_v2")
body2 = {
    "model": MODEL,
    "input": [
        user_msg("My favorite color is blue. What is 2+2?"),
        assistant_msg("2+2 equals 4."),
        {"type": "compaction_trigger"},
    ],
    "stream": True,
}
status2, _, raw2 = post(
    "/v1/responses", body2,
    {"x-codex-turn-metadata": json.dumps({"request_kind": "compaction", "compaction": v2_meta})},
)
print(f"   status={status2}")
if status2 == 200:
    types = []
    for block in raw2.decode("utf-8", "replace").split("\n\n"):
        for line in block.splitlines():
            if not line.startswith("data:"):
                continue
            try:
                d = json.loads(line[5:].strip())
            except Exception:
                continue
            if d.get("type") == "response.output_item.done":
                types.append(d.get("item", {}).get("type"))
            elif d.get("type") == "response.completed":
                usage = d.get("response", {}).get("usage", {})
                print(f"   completed, usage={json.dumps(usage)}")
            elif d.get("type") == "response.failed":
                print("   FAILED:", json.dumps(d)[:300])
    print(f"   output item types: {types}")
else:
    print("   rejected:", raw2.decode("utf-8", "replace")[:400])
