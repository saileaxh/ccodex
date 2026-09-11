#!/usr/bin/env python3
"""服务器实跑验证：把真机上跑的中继的遥测出口临时指向本机接收端，跑一个真实请求，
核对实际发出的 analytics / OTLP 载荷，然后**恢复原状**并复查。

步骤（全部通过 SSH 公钥，不打印任何密钥）：
  1. 上传并在服务器 127.0.0.1:<port> 起一个捕获服务（只记录载荷，不记录鉴权头）
  2. 写 systemd drop-in：三个仅开发用的覆盖变量 + 5s 导出间隔
  3. daemon-reload + restart，等 active
  4. 读下游 sk（shell 变量内，不打印）→ 打一个真实 /v1/responses 请求
  5. 取回捕获，打印去敏摘要（事件类型/关键字段）
  6. 删除 drop-in + restart，恢复生产行为
  7. 再打一个真实请求，检查 journal 无 analytics 失败告警（真的打到官方端点且被接受）

用法:
    python testdata/verify_telemetry_live.py            # 全流程
    python testdata/verify_telemetry_live.py --only-check   # 只做第 7 步
"""
import argparse
import json
import pathlib
import sys
import time

import paramiko

DEFAULT_KEY = r"D:\secure_linux_server\keys\publickey-ai_ed25519"
DEFAULT_PASS = r"D:\secure_linux_server\keys\publickey-ai_ed25519.passphrase.txt"
CAPTURE_SERVER = r'''
import json, sys, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

OUT = sys.argv[2]

class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a): pass
    def do_POST(self):
        n = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(n) if n else b""
        try:
            payload = json.loads(body or b"{}")
        except Exception:
            payload = {"_unparsed": body.decode("utf-8", "replace")[:2000]}
        rec = {
            "sink": "analytics" if self.path.endswith("/analytics-events/events") else "otlp",
            "path": self.path,
            "has_bearer": bool(self.headers.get("Authorization")),
            "account_id_present": bool(self.headers.get("chatgpt-account-id")),
            "content_type": self.headers.get("Content-Type"),
            "statsig_key_prefix": (self.headers.get("statsig-api-key") or "")[:7],
            "received_at": time.time(),
            "payload": payload,
        }
        with open(OUT, "a", encoding="utf-8") as fh:
            fh.write(json.dumps(rec) + "\n")
        b = b"{}"
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)

ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
'''


def connect(args):
    pw = None
    if pathlib.Path(args.key_pass).exists():
        pw = pathlib.Path(args.key_pass).read_text(encoding="utf-8").strip()
    pkey = paramiko.Ed25519Key.from_private_key_file(args.key, password=pw)
    c = paramiko.SSHClient()
    c.set_missing_host_key_policy(paramiko.AutoAddPolicy())
    c.connect(args.host, port=args.ssh_port, username=args.user, pkey=pkey,
              timeout=20, allow_agent=False, look_for_keys=False)
    return c


def run(c, cmd, timeout=180, check=True, quiet=False):
    if not quiet:
        print(f"$ {cmd}", flush=True)
    _, out, err = c.exec_command(cmd, timeout=timeout)
    o = out.read().decode(errors="replace")
    e = err.read().decode(errors="replace")
    code = out.channel.recv_exit_status()
    if o.strip() and not quiet:
        print(o, flush=True)
    if e.strip() and not quiet:
        print(e, file=sys.stderr, flush=True)
    if check and code != 0:
        raise SystemExit(f"命令失败 ({code}): {cmd}")
    return code, o, e


def sftp_write(c, path, text):
    sftp = c.open_sftp()
    with sftp.file(path, "w") as fh:
        fh.write(text)
    sftp.close()


def one_request(c, args):
    """真实请求：下游 sk 读进 shell 变量，绝不打印。"""
    prompt = "reply with the single word: ok"
    body = json.dumps({
        "model": args.model,
        "instructions": "you are a test",
        "input": [{"type": "message", "role": "user",
                   "content": [{"type": "input_text", "text": prompt}]}],
        "stream": True,
    })
    cmd = (
        'KEY=$(python3 -c "import json;print(json.load(open(\'%s\'))[0][\'key\'])"); '
        'curl -sS --max-time 120 -o /tmp/live_resp.txt -w "HTTP %%{http_code}\\n" '
        '-H "Authorization: Bearer $KEY" -H "session-id: 0192f8c0-0000-7000-8000-0000000000ab" '
        '-H "Content-Type: application/json" -X POST http://127.0.0.1:%d/v1/responses -d %s'
        % (args.keys_file, args.relay_port, shell_quote(body))
    )
    return run(c, cmd, timeout=180)


def shell_quote(s):
    return "'" + s.replace("'", "'\\''") + "'"


def summarize(records):
    ana = [r for r in records if r["sink"] == "analytics"]
    otlp = [r for r in records if r["sink"] == "otlp"]
    print(f"\n捕获：analytics POST {len(ana)} 条，OTLP 导出 {len(otlp)} 次")
    events = [e for r in ana for e in r["payload"].get("events", [])]
    from collections import Counter
    print("事件类型：", dict(Counter(e.get("event_type") for e in events)))
    for r in ana[:1]:
        print(f"  鉴权：bearer={r['has_bearer']} account-id={r['account_id_present']} "
              f"content-type={r['content_type']}")
    for e in events:
        p = e.get("event_params", {})
        t = e.get("event_type")
        if t == "codex_turn_event":
            print("  turn:", json.dumps({k: p.get(k) for k in
                  ["turn_id", "status", "model", "sampling_request_count",
                   "input_tokens", "cached_input_tokens", "output_tokens",
                   "total_tokens", "sampling_ms", "duration_ms", "codex_error_kind"]},
                  ensure_ascii=False))
        elif t == "codex_thread_initialized":
            print("  thread:", json.dumps({k: p.get(k) for k in
                  ["thread_id", "session_id", "model", "initialization_mode",
                   "thread_source"]}, ensure_ascii=False))
        elif t == "codex_compaction_event":
            print("  compaction:", json.dumps({k: p.get(k) for k in
                  ["turn_id", "implementation", "status", "phase",
                   "active_context_tokens_before", "active_context_tokens_after"]},
                  ensure_ascii=False))
    if otlp:
        last = otlp[-1]
        scopes = [r["payload"]["resourceMetrics"][0]["scopeMetrics"][0] for r in otlp]
        names = sorted({m["name"] for s in scopes for m in s.get("metrics", [])})
        res = {a["key"]: list(a["value"].values())[0]
               for a in last["payload"]["resourceMetrics"][0]["resource"].get("attributes", [])}
        print(f"  OTLP key 前缀={last['statsig_key_prefix']} resource={res}")
        print(f"  OTLP 指标集合={names}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="109.244.63.120")
    ap.add_argument("--user", default="root")
    ap.add_argument("--ssh-port", type=int, default=22)
    ap.add_argument("--key", default=DEFAULT_KEY)
    ap.add_argument("--key-pass", default=DEFAULT_PASS)
    ap.add_argument("--service", default="ccodex")
    ap.add_argument("--relay-port", type=int, default=8317)
    ap.add_argument("--capture-port", type=int, default=8899)
    ap.add_argument("--keys-file", default="/var/lib/ccodex/keys.json")
    ap.add_argument("--model", default="gpt-5.1-codex")
    ap.add_argument("--wait", type=float, default=20.0,
                    help="等待 OTLP 导出周期落盘后再查 journal（生产默认 60s 周期）")
    ap.add_argument("--only-check", action="store_true",
                    help="跳过捕获阶段，只做恢复后的官方端点复查")
    ap.add_argument("--skip-restore", action="store_true",
                    help="调试用：不删除 drop-in")
    args = ap.parse_args()

    c = connect(args)
    try:
        if not args.only_check:
            print("===== 1. 起捕获服务 =====")
            sftp_write(c, "/tmp/ccodex_capture.py", CAPTURE_SERVER)
            run(c, "pkill -f ccodex_capture.py 2>/dev/null; rm -f /tmp/ccodex_capture.jsonl; "
                   f"nohup python3 /tmp/ccodex_capture.py {args.capture_port} "
                   "/tmp/ccodex_capture.jsonl > /tmp/ccodex_capture.log 2>&1 & sleep 1; "
                   "ss -ltnp | grep %d || true" % args.capture_port, check=False)

            print("===== 2. 临时 drop-in（仅开发用覆盖变量） =====")
            run(c, "mkdir -p /etc/systemd/system/%s.service.d" % args.service)
            # config.toml 的 upstream_proxy 会把 HTTPS_PROXY/ALL_PROXY 写进进程环境
            # （官方客户端同一条代理路径），因此捕获端点是回环地址、必须挂 NO_PROXY，
            # 否则遥测请求会被送去代理而打不到捕获服务。
            sftp_write(c, f"/etc/systemd/system/{args.service}.service.d/zz-telemetry-capture.conf",
                       "[Service]\n"
                       f"Environment=CCODEX_ANALYTICS_BASE_URL_OVERRIDE=http://127.0.0.1:{args.capture_port}/backend-api\n"
                       f"Environment=CCODEX_STATSIG_ENDPOINT_OVERRIDE=http://127.0.0.1:{args.capture_port}/otlp/v1/metrics\n"
                       "Environment=CCODEX_STATSIG_EXPORT_INTERVAL_MS=5000\n"
                       "Environment=NO_PROXY=127.0.0.1,localhost\n"
                       "Environment=no_proxy=127.0.0.1,localhost\n")
            run(c, f"systemctl daemon-reload && systemctl restart {args.service} && sleep 3 && "
                   f"systemctl is-active {args.service}")

            print("===== 3. 真实请求 =====")
            one_request(c, args)
            time.sleep(9)

            print("===== 4. 捕获摘要 =====")
            sftp = c.open_sftp()
            text = ""
            try:
                with sftp.file("/tmp/ccodex_capture.jsonl", "r") as fh:
                    text = fh.read().decode("utf-8", "replace")
            except OSError as e:
                print("  无捕获文件:", e)
            sftp.close()
            records = [json.loads(line) for line in text.splitlines() if line.strip()]
            summarize(records)

        if not args.skip_restore:
            print("\n===== 5. 恢复生产行为 =====")
            run(c, f"rm -f /etc/systemd/system/{args.service}.service.d/zz-telemetry-capture.conf && "
                   f"pkill -f ccodex_capture.py 2>/dev/null; "
                   f"systemctl daemon-reload && systemctl restart {args.service} && sleep 3 && "
                   f"systemctl is-active {args.service}", check=False)
            run(c, f"systemctl show {args.service} -p Environment | tr ' ' '\\n' | "
                   "grep -i CCODEX || echo '(无 CCODEX 覆盖变量，已恢复)'")

        print("\n===== 6. 官方端点复查 =====")
        run(c, f"journalctl -u {args.service} --since '-2min' --no-pager | "
               "grep -iE 'analytics|events failed|otlp|statsig' || "
               "echo '(无分析通道告警)'", check=False)
        one_request(c, args)
        time.sleep(args.wait)
        run(c, f"journalctl -u {args.service} --since '-3min' --no-pager | "
               "grep -iE 'events failed|failed to send events|failed to export|otlp|statsig|WARN' || "
               "echo '(无告警：事件已被官方端点接受)'", check=False)
        run(c, f"journalctl -u {args.service} -n 8 --no-pager", check=False)
    finally:
        c.close()


if __name__ == "__main__":
    sys.exit(main())
