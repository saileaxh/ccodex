#!/usr/bin/env python3
"""驱动 ccodex 打 mock 上游，断言其发出的遥测（形状 + 语义）。

（配合 testdata/mock_telemetry_upstream.py；通常由 verify_telemetry_e2e.py 调度。）

Checks (each printed PASS/FAIL):
  A. thread_initialized emitted exactly once per session, exact official field order
  B. turn_event field order + 62 fields, official defaults, real usage numbers
  C. turn boundary: tool continuation reuses turn_id, sampling_request_count=2
  D. retry (fail_mid_stream then ok) stays in the same turn, sampling_retry_count
  E. compaction: codex_compaction_event, no turn event, type remote_v2
  F. failures: 503 -> server_overloaded; failure isolation (no turn event for it)
  G. Statsig OTLP: resource attrs, meter "codex", delta temporality, metric set
  H. auth: Bearer token + chatgpt-account-id present on analytics POSTs

Usage: python tools/verify_telemetry.py [--ccodex-port 8317] [--mock-port 8899]
"""
import argparse
import json
import subprocess
import sys
import time
import urllib.request

FAILURES: list[str] = []


def check(name: str, ok: bool, detail: str = "") -> None:
    print(("PASS  " if ok else "FAIL  ") + name + (f"\n      {detail}" if detail and not ok else ""))
    if not ok:
        FAILURES.append(name)


def post(url: str, payload: dict, headers: dict | None = None, timeout: float = 120) -> bytes:
    req = urllib.request.Request(
        url, data=json.dumps(payload).encode(),
        headers={"Content-Type": "application/json", **(headers or {})}, method="POST")
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return resp.read()


def read_capture(path: str) -> list[dict]:
    out = []
    try:
        with open(path, encoding="utf-8") as fh:
            for line in fh:
                line = line.strip()
                if line:
                    out.append(json.loads(line))
    except FileNotFoundError:
        pass
    return out


def analytics_events(records: list[dict]) -> list[dict]:
    events = []
    for rec in records:
        if rec.get("sink") != "analytics":
            continue
        for ev in rec["payload"].get("events", []):
            events.append({"rec": rec, "event": ev})
    return events


def events_of_type(events: list[dict], etype: str) -> list[dict]:
    return [e for e in events if e["event"].get("event_type") == etype]


# Exact declaration order from vendor analytics/src/events.rs.
TURN_ORDER = [
    "thread_id", "session_id", "turn_id", "root_turn_id", "turn_trigger",
    "codex_turn_source", "submission_type", "app_server_client", "runtime",
    "ephemeral", "thread_source", "initialization_mode", "subagent_source",
    "parent_thread_id", "model", "model_provider", "sandbox_policy",
    "reasoning_effort", "reasoning_summary", "service_tier", "approval_policy",
    "approvals_reviewer", "guardian_v2_enabled", "sandbox_network_access",
    "collaboration_mode", "personality", "workspace_kind", "num_input_images",
    "image_preparations", "is_first_turn", "status",
    "explicit_client_interrupt_requested_at_ms", "turn_error", "codex_error_kind",
    "codex_error_http_status_code", "steer_count", "total_tool_call_count",
    "shell_command_count", "file_change_count", "mcp_tool_call_count",
    "dynamic_tool_call_count", "subagent_tool_call_count", "web_search_count",
    "image_generation_count", "input_tokens", "cached_input_tokens",
    "cache_write_input_tokens", "output_tokens", "reasoning_output_tokens",
    "total_tokens", "before_first_sampling_ms", "sampling_ms", "compaction_ms",
    "between_sampling_overhead_ms", "tool_blocking_ms", "after_last_sampling_ms",
    "sampling_request_count", "sampling_retry_count", "duration_ms", "started_at",
    "completed_at",
]

INIT_ORDER = [
    "thread_id", "session_id", "app_server_client", "runtime", "model", "ephemeral",
    "thread_source", "initialization_mode", "subagent_source", "parent_thread_id",
    "forked_from_thread_id", "created_at",
]

COMPACTION_ORDER = [
    "thread_id", "session_id", "turn_id", "app_server_client", "runtime",
    "thread_source", "subagent_source", "parent_thread_id", "trigger", "reason",
    "implementation", "phase", "strategy", "status", "codex_error_kind",
    "codex_error_http_status_code", "active_context_tokens_before",
    "active_context_tokens_after", "retained_image_count", "compaction_summary_tokens",
    "cached_input_tokens", "cache_write_input_tokens", "started_at", "completed_at",
    "duration_ms",
]


def key_order(obj: dict) -> list[str]:
    return list(obj.keys())


def any_value(value: dict):
    """OTLP/JSON AnyValue -> plain Python value."""
    for key in ("stringValue", "intValue", "doubleValue", "boolValue"):
        if key in value:
            return value[key]
    return value


def attrs_of(dp: dict) -> dict:
    return {a["key"]: any_value(a["value"]) for a in dp.get("attributes", [])}


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--ccodex-url", default="http://127.0.0.1:8317")
    ap.add_argument("--mock-url", default="http://127.0.0.1:8899")
    ap.add_argument("--capture", default="testdata/_capture.jsonl")
    ap.add_argument("--key", default="test-key")
    ap.add_argument("--skip-drive", action="store_true",
                    help="only analyze an existing capture file")
    args = ap.parse_args()

    ccodex = args.ccodex_url.rstrip("/")
    mock = args.mock_url.rstrip("/")
    auth = {"Authorization": f"Bearer {args.key}"}
    # Official EndpointSession trio: HTTP header names use dashes, not underscores.
    sessions = {"session-id": "11111111-1111-4111-8111-111111111111"}

    baseline = len(read_capture(args.capture))
    anon_events: list[dict] = []

    if not args.skip_drive:
        def control(**kw):
            post(mock + "/__control", kw)

        def user_message(text: str) -> dict:
            return {"type": "message", "role": "user",
                    "content": [{"type": "input_text", "text": text}]}

        def turn(input_items: list[dict]):
            body = {
                "model": "gpt-5.1-codex",
                "instructions": "test",
                "input": input_items,
                "stream": True,
            }
            return post(ccodex + "/v1/responses", body, {**auth, **sessions}).decode()

        # --- Scenario 1: plain turn -----------------------------------------
        control(sse="ok", tool_call=False, compaction_item=False, reasoning=False)
        turn([user_message("first turn")])

        # --- Scenario 2: tool call + continuation (same turn) ---------------
        control(sse="ok", tool_call=True)
        stream = turn([user_message("use a tool")])
        call_id = None
        for line in stream.splitlines():
            if line.startswith("data:"):
                try:
                    ev = json.loads(line[5:].strip())
                except Exception:  # noqa: BLE001
                    continue
                if ev.get("type") == "response.output_item.done":
                    item = ev.get("item") or {}
                    if item.get("call_id"):
                        call_id = item["call_id"]
        check("tool call captured from stream", bool(call_id), f"stream head: {stream[:400]}")
        if call_id:
            control(sse="ok", tool_call=False)
            # Official continuation shape: the tool output ends the input.
            turn([
                user_message("use a tool"),
                {"type": "function_call", "name": "shell", "arguments": "{}",
                 "call_id": call_id},
                {"type": "function_call_output", "call_id": call_id, "output": "ok"},
            ])

        # --- Scenario 3: retry after mid-stream failure (same turn) ---------
        control(sse="fail_mid_stream", tool_call=False)
        turn([user_message("doomed turn")])
        control(sse="ok")
        turn([user_message("doomed turn")])

        # --- Scenario 4: compaction (standalone, no turn event) -------------
        control(sse="ok", tool_call=False, compaction_item=False)
        try:
            body = post(ccodex + "/v1/responses/compact",
                        {"model": "gpt-5.1-codex",
                         "input": [user_message("summarize")]},
                        {**auth, **sessions})
            print("      compact response:", body.decode()[:200])
        except Exception as e:  # noqa: BLE001
            check("compact request accepted", False, repr(e))

        # --- Scenario 5: anonymous request (no session headers at all) ------
        control(sse="ok", tool_call=False)
        before_anon = len(read_capture(args.capture))
        post(ccodex + "/v1/responses",
             {"model": "gpt-5.1-codex", "instructions": "test",
              "input": [user_message("anon turn")], "stream": True}, auth)
        time.sleep(2)
        anon_events = analytics_events(read_capture(args.capture)[before_anon:])

        # --- Scenario 6: HTTP 503 (last: the account cools down afterwards) -
        control(sse="http_503")
        try:
            turn([user_message("overloaded turn")])
        except Exception:  # noqa: BLE001
            pass
        control(sse="ok")

        # Give the analytics queue a moment, and the metrics reader its interval
        # (dev override: 5s).
        time.sleep(8)

    records = read_capture(args.capture)[baseline:]
    events = analytics_events(records)
    print(f"\n--- capture: {len(records)} telemetry POST(s), {len(events)} event(s) ---\n")

    inits = events_of_type(events, "codex_thread_initialized")
    turns = events_of_type(events, "codex_turn_event")
    comps = events_of_type(events, "codex_compaction_event")

    # --- A. thread_initialized ---------------------------------------------
    check("A1 thread_initialized emitted", len(inits) >= 1, f"got {len(inits)}")
    if inits:
        params = inits[0]["event"]["event_params"]
        check("A2 thread_initialized field order", key_order(params) == INIT_ORDER,
              f"{key_order(params)}")
        check("A3 thread_initialized once per session",
              len({p["event"]["event_params"]["thread_id"] for p in inits}) == len(inits),
              "duplicate thread_id across thread_initialized events")
        check("A4 initialization_mode=new",
              params.get("initialization_mode") == "new", str(params.get("initialization_mode")))
        check("A5 thread_source=user", params.get("thread_source") == "user",
              str(params.get("thread_source")))
        check("A6 app_server_client shape",
              params.get("app_server_client", {}).get("rpc_transport") == "in_process"
              and params["app_server_client"].get("client_name") == "codex-tui",
              json.dumps(params.get("app_server_client")))
        check("A7 runtime metadata",
              params.get("runtime", {}).get("codex_rs_version")
              and params["runtime"].get("runtime_arch"),
              json.dumps(params.get("runtime")))
        # Official events have no skip_serializing_if: every declared key is present.
        missing = [k for k in INIT_ORDER if k not in params]
        check("A8 thread_initialized no missing keys", not missing, str(missing))

    # --- B. turn_event ------------------------------------------------------
    check("B1 turn_event emitted", len(turns) >= 1, f"got {len(turns)}")
    if turns:
        params = turns[0]["event"]["event_params"]
        check("B2 turn_event field order (62 fields)", key_order(params) == TURN_ORDER,
              f"missing={[k for k in TURN_ORDER if k not in params]} "
              f"extra={[k for k in params if k not in TURN_ORDER]}")
        check("B3 official defaults",
              params.get("model_provider") == "openai"
              and params.get("sandbox_policy") == "read_only"
              and params.get("service_tier") == "default"
              and params.get("approval_policy") == "on-request"
              and params.get("approvals_reviewer") == "user"
              and params.get("collaboration_mode") == "default"
              and params.get("reasoning_summary") == "auto"
              and params.get("steer_count") == 0,
              json.dumps({k: params.get(k) for k in
                          ["model_provider", "sandbox_policy", "service_tier",
                           "approval_policy", "approvals_reviewer", "collaboration_mode",
                           "reasoning_summary", "steer_count"]}))
        check("B4 status completed on the plain turn",
              any(t["event"]["event_params"].get("status") == "completed" for t in turns),
              str([t["event"]["event_params"].get("status") for t in turns]))
        completed = [t for t in turns
                     if t["event"]["event_params"].get("status") == "completed"]
        if completed:
            p = completed[0]["event"]["event_params"]
            check("B5 usage numbers from the wire",
                  p.get("input_tokens") == 1000 and p.get("cached_input_tokens") == 1024
                  and p.get("output_tokens") == 50 and p.get("reasoning_output_tokens") == 20
                  and p.get("total_tokens") is not None,
                  json.dumps({k: p.get(k) for k in
                              ["input_tokens", "cached_input_tokens", "output_tokens",
                               "reasoning_output_tokens", "total_tokens"]}))
            check("B6 timing invariants",
                  p.get("sampling_ms") is not None and p["sampling_ms"] >= 0
                  and p.get("duration_ms") is not None and p["duration_ms"] >= 0
                  and p.get("started_at") and p.get("completed_at")
                  and p["completed_at"] >= p["started_at"],
                  json.dumps({k: p.get(k) for k in
                              ["sampling_ms", "duration_ms", "started_at", "completed_at"]}))
            check("B7 tool counts are real zeros for a plain turn",
                  p.get("total_tool_call_count") == 0 and p.get("shell_command_count") == 0,
                  json.dumps({k: p.get(k) for k in
                              ["total_tool_call_count", "shell_command_count"]}))
        # Every turn event must reference a thread that was announced, with the same
        # session id (no cross-thread mixing).
        announced = {
            p["event"]["event_params"]["thread_id"]:
                p["event"]["event_params"]["session_id"] for p in inits}
        check("B8 turn/thread id wiring",
              all(p["event"]["event_params"]["thread_id"] in announced
                  and p["event"]["event_params"]["session_id"]
                  == announced[p["event"]["event_params"]["thread_id"]]
                  for p in turns),
              f"announced={sorted(announced)} "
              f"turns={[(t['event']['event_params']['thread_id'], t['event']['event_params']['session_id']) for t in turns]}")

    # --- C. tool continuation ----------------------------------------------
    tool_turns = [t["event"]["event_params"] for t in turns
                  if (t["event"]["event_params"].get("total_tool_call_count") or 0) >= 1]
    check("C1 tool-call turn counted a shell call",
          any(p.get("shell_command_count", 0) >= 1 for p in tool_turns),
          str([(p.get("total_tool_call_count"), p.get("shell_command_count"))
               for p in tool_turns]))
    cont = [p for p in tool_turns if (p.get("sampling_request_count") or 0) >= 2]
    check("C2 continuation reused the turn (sampling_request_count>=2)", bool(cont),
          str([(p.get("turn_id"), p.get("sampling_request_count")) for p in tool_turns]))
    check("C3 tool_blocking_ms recorded for the continuation gap",
          any((p.get("tool_blocking_ms") or 0) > 0 for p in cont) if cont else False,
          str([p.get("tool_blocking_ms") for p in cont]))

    # --- D. retry -----------------------------------------------------------
    retried = [t["event"]["event_params"] for t in turns
               if (t["event"]["event_params"].get("sampling_retry_count") or 0) >= 1]
    check("D1 retry stayed in the same turn (sampling_retry_count>=1)", bool(retried),
          str([(p.get("turn_id"), p.get("sampling_retry_count"),
                p.get("sampling_request_count")) for p in
               [t["event"]["event_params"] for t in turns]]))

    # --- E. compaction ------------------------------------------------------
    check("E1 compaction event emitted", len(comps) >= 1, f"got {len(comps)}")
    if comps:
        p = comps[0]["event"]["event_params"]
        check("E2 compaction field order", key_order(p) == COMPACTION_ORDER,
              f"missing={[k for k in COMPACTION_ORDER if k not in p]}")
        check("E3 implementation responses_compact",
              p.get("implementation") == "responses_compact", str(p.get("implementation")))
        check("E4 trigger/reason/phase/strategy/status",
              p.get("trigger") == "manual" and p.get("reason") == "user_requested"
              and p.get("phase") == "standalone_turn" and p.get("strategy") == "memento"
              and p.get("status") == "completed",
              json.dumps({k: p.get(k) for k in
                          ["trigger", "reason", "phase", "strategy", "status"]}))
        check("E5 tokens before/after are integers",
              isinstance(p.get("active_context_tokens_before"), int)
              and isinstance(p.get("active_context_tokens_after"), int)
              and p["active_context_tokens_after"] >= p["active_context_tokens_before"],
              json.dumps({k: p.get(k) for k in
                          ["active_context_tokens_before", "active_context_tokens_after",
                           "compaction_summary_tokens", "cached_input_tokens"]}))
        check("E6 no turn_event for the standalone compaction",
              all(c["event"]["event_params"]["turn_id"]
                  not in {t["event"]["event_params"]["turn_id"] for t in turns}
                  for c in comps),
              "compaction turn_id appeared as a turn event")

    # --- F. failures --------------------------------------------------------
    overloaded = [t["event"]["event_params"] for t in turns
                  if t["event"]["event_params"].get("codex_error_kind") == "server_overloaded"]
    check("F1 503 maps to server_overloaded", bool(overloaded),
          str([(t["event"]["event_params"].get("status"),
                t["event"]["event_params"].get("codex_error_kind"),
                t["event"]["event_params"].get("codex_error_http_status_code"))
               for t in turns]))
    if overloaded:
        p = overloaded[0]
        check("F2 turn_error carries CodexErrorInfo",
              p.get("turn_error") == "serverOverloaded", json.dumps(p.get("turn_error")))
        check("F3 failed turn has no http status (in-stream/official shape)",
              p.get("codex_error_http_status_code") is None
              or isinstance(p.get("codex_error_http_status_code"), int),
              str(p.get("codex_error_http_status_code")))

    # --- I. anonymous (keyless) request -------------------------------------
    anon_inits = events_of_type(anon_events, "codex_thread_initialized")
    anon_turns = events_of_type(anon_events, "codex_turn_event")
    check("I1 keyless request is a complete one-shot thread",
          len(anon_inits) == 1 and len(anon_turns) == 1,
          f"inits={len(anon_inits)} turns={len(anon_turns)}")
    if anon_inits and anon_turns:
        thread = anon_inits[0]["event"]["event_params"]["thread_id"]
        turn = anon_turns[0]["event"]["event_params"]
        keyed_thread = inits[0]["event"]["event_params"]["thread_id"] if inits else None
        check("I2 one-shot turn carries its own thread",
              turn["thread_id"] == thread and thread != keyed_thread,
              f"turn_thread={turn['thread_id']} init_thread={thread} keyed={keyed_thread}")
        check("I3 one-shot turn completed", turn.get("status") == "completed",
              str(turn.get("status")))

    # --- G. Statsig OTLP ----------------------------------------------------
    otlp = [r for r in records if r.get("sink") == "otlp"]
    check("G1 OTLP export received", bool(otlp), f"{len(otlp)} export(s)")
    if otlp:
        rec = otlp[-1]
        check("G2 statsig-api-key header present",
              (rec.get("statsig_key") or "").startswith("client-"), str(rec.get("statsig_key")))
        # Delta temporality: each export carries only the deltas since the previous one,
        # so the metric set is the union over all exports.
        scopes = [r["payload"]["resourceMetrics"][0]["scopeMetrics"][0] for r in otlp]
        payload = rec["payload"]
        resource = payload.get("resourceMetrics", [{}])[0].get("resource", {})
        attrs = {a["key"]: any_value(a["value"]) for a in resource.get("attributes", [])}
        check("G3 resource service.name/service.version",
              attrs.get("service.name") == "codex_cli_rs"
              and bool(attrs.get("service.version")),
              json.dumps(attrs))
        check("G4 meter name is codex",
              all(s.get("scope", {}).get("name") == "codex" for s in scopes),
              json.dumps([s.get("scope") for s in scopes]))
        metrics = [m for s in scopes for m in s.get("metrics", [])]
        names = {m["name"] for m in metrics}
        check("G5 process.start + thread.started emitted",
              "codex.process.start" in names and "codex.thread.started" in names,
              str(sorted(names)))
        check("G6 turn metrics emitted",
              {"codex.turn.ttft.duration_ms", "codex.turn.e2e_duration_ms",
               "codex.turn.tool.call", "codex.turn.memory",
               "codex.turn.unified_exec.running_processes"} <= names,
              str(sorted(names)))
        # Delta temporality + official instrument kinds.
        temporalities = {m["name"]: m.get("sum", {}).get("aggregationTemporality")
                         or m.get("histogram", {}).get("aggregationTemporality")
                         for m in metrics}
        check("G7 delta temporality on counters/histograms",
              all(t in (1, None) for t in temporalities.values()), json.dumps(temporalities))
        check("G8 task.compact metric for the compaction",
              "codex.task.compact" in names, str(sorted(names)))
        # Metadata tags on a turn metric.
        probe = next((m for m in metrics
                      if m["name"] == "codex.turn.e2e_duration_ms"), None)
        if probe:
            tags = attrs_of(probe["histogram"]["dataPoints"][0])
            check("G9 turn metric metadata tags",
                  tags.get("auth_mode") == "Chatgpt" and tags.get("session_source") == "cli"
                  and tags.get("originator") == "codex_cli_rs" and tags.get("model")
                  and tags.get("app.version"),
                  json.dumps(tags))

    # --- P. 出口一致性：遥测必须走账号绑定的代理 ------------------------------
    # e2e 把账号绑定到 mock 充当的 HTTP 代理：数据面/analytics/OTLP 都应以绝对形式
    # （proxy form）到达。若遥测走了直连或另一条默认代理，这里立刻失败。
    upstream_recs = [r for r in records if r.get("sink") == "upstream"]
    check("P1 data plane egresses through the account proxy",
          bool(upstream_recs) and all(r.get("proxy_form") for r in upstream_recs),
          f"{sum(1 for r in upstream_recs if r.get('proxy_form'))}/{len(upstream_recs)}")
    analytics_all = [r for r in records if r.get("sink") == "analytics"]
    check("P2 analytics events egress through the account proxy",
          bool(analytics_all) and all(r.get("proxy_form") for r in analytics_all),
          f"{sum(1 for r in analytics_all if r.get('proxy_form'))}/{len(analytics_all)}")
    otlp_all = [r for r in records if r.get("sink") == "otlp"]
    check("P3 Statsig OTLP exports egress through the account proxy",
          bool(otlp_all) and all(r.get("proxy_form") for r in otlp_all),
          f"{sum(1 for r in otlp_all if r.get('proxy_form'))}/{len(otlp_all)}")

    # --- H. auth on the analytics POST --------------------------------------
    analytics_recs = [r for r in records if r.get("sink") == "analytics"]
    if analytics_recs:
        rec = analytics_recs[0]
        check("H1 analytics POST carries the account Bearer token",
              (rec.get("authorization") or "").startswith("Bearer "), str(rec.get("authorization"))[:24])
        check("H2 chatgpt-account-id header present",
              bool(rec.get("account_id")), str(rec.get("account_id")))
        check("H3 Content-Type json",
              (rec.get("content_type") or "").startswith("application/json"),
              str(rec.get("content_type")))

    print()
    if FAILURES:
        print(f"{len(FAILURES)} check(s) FAILED: {FAILURES}")
        return 1
    print("all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
