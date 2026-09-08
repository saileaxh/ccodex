#!/usr/bin/env python3
"""WS 冒烟：response.create 帧进、事件帧出、连接复用、错误帧。"""
import asyncio
import json
import sys

import websockets

REQ = json.load(open("testdata/smoke_request.json", encoding="utf-8"))


async def one_turn(ws, tag):
    frame = {"type": "response.create", **REQ}
    await ws.send(json.dumps(frame))
    events = []
    while True:
        raw = await asyncio.wait_for(ws.recv(), timeout=10)
        evt = json.loads(raw)
        events.append(evt.get("type"))
        if evt.get("type") in ("response.completed", "response.failed", "error"):
            break
    print(f"[{tag}] events: {events}")
    assert events == ["response.output_text.delta", "response.completed"], events
    return events


async def main():
    async with websockets.connect(
        "ws://127.0.0.1:8317/v1/responses",
        additional_headers={"Authorization": "Bearer sk-smoke"},
    ) as ws:
        await one_turn(ws, "turn1")
        await one_turn(ws, "turn2")  # 连接复用

        # 非法帧 → error 帧但不断连
        await ws.send("not json")
        raw = await asyncio.wait_for(ws.recv(), timeout=5)
        assert json.loads(raw)["type"] == "error", raw
        print("[bad-frame] error frame ok, connection alive")

        # 非 response.create 类型
        await ws.send(json.dumps({"type": "whatever"}))
        raw = await asyncio.wait_for(ws.recv(), timeout=5)
        assert json.loads(raw)["type"] == "error", raw
        print("[wrong-type] error frame ok")

    # 无鉴权应被拒（upgrade 前 401）
    try:
        async with websockets.connect("ws://127.0.0.1:8317/v1/responses") as ws:
            await ws.recv()
        print("FAIL: unauthenticated ws accepted")
        sys.exit(1)
    except websockets.exceptions.InvalidStatus as e:
        assert e.response.status_code == 401, e.response.status_code
        print("[auth] unauthenticated upgrade rejected with 401")

    print("WS SMOKE OK")


asyncio.run(main())
