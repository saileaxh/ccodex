"""Verify WS protocol handling of incremental chaining frames and prewarm frames.

Expectations after the fix:
1. response.create with previous_response_id -> wrapped error frame, code
   previous_response_not_found (client retries with full input).
2. response.create with generate=false (v2 prewarm) -> same wrapped error.
3. Plain response.create -> NOT the wrapped error (flows into normal handling;
   with zero accounts here it yields the model_cooldown/accounts_invalid frame).
4. Connection stays open after each error (multiple frames on one socket).
"""
import asyncio
import json
import sys

import websockets


async def main() -> int:
    url = "ws://127.0.0.1:8317/v1/responses"
    ok = True

    async def one_frame(ws, frame):
        await ws.send(json.dumps(frame))
        raw = await asyncio.wait_for(ws.recv(), timeout=10)
        return json.loads(raw)

    async with websockets.connect(url) as ws:
        # 1. incremental chaining frame
        r = await one_frame(ws, {
            "type": "response.create",
            "model": "gpt-5.5",
            "previous_response_id": "resp_abc123",
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "follow up"}]}],
        })
        print("incremental ->", json.dumps(r, ensure_ascii=False))
        if not (r.get("type") == "error" and r.get("error", {}).get("code") == "previous_response_not_found"):
            print("FAIL: incremental frame not answered with previous_response_not_found")
            ok = False

        # 2. prewarm frame
        r = await one_frame(ws, {
            "type": "response.create",
            "model": "gpt-5.5",
            "generate": False,
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
        })
        print("prewarm     ->", json.dumps(r, ensure_ascii=False))
        if not (r.get("type") == "error" and r.get("error", {}).get("code") == "previous_response_not_found"):
            print("FAIL: prewarm frame not answered with previous_response_not_found")
            ok = False

        # 3. plain create still flows into normal handling (0 accounts -> cooldown frame)
        r = await one_frame(ws, {
            "type": "response.create",
            "model": "gpt-5.5",
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]}],
        })
        print("plain       ->", json.dumps(r, ensure_ascii=False))
        if r.get("error", {}).get("code") == "previous_response_not_found":
            print("FAIL: plain create misclassified as incremental")
            ok = False

    # 4. HTTP path rejects previous_response_id loudly (open mode: no auth)
    import urllib.request
    import urllib.error
    req = urllib.request.Request(
        "http://127.0.0.1:8317/v1/responses",
        data=json.dumps({"model": "gpt-5.5", "previous_response_id": "resp_x", "input": []}).encode(),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:
            body = resp.read().decode()
            print("http        ->", resp.status, body)
            ok = False
    except urllib.error.HTTPError as e:
        body = e.read().decode()
        print("http        ->", e.code, body)
        if e.code != 400 or "previous_response_id" not in body:
            print("FAIL: HTTP previous_response_id not rejected with 400")
            ok = False

    print("WS INCREMENTAL/PREWARM:", "OK" if ok else "FAIL")
    return 0 if ok else 1


sys.exit(asyncio.run(main()))
