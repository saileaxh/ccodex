#!/usr/bin/env python3
"""端到端遥测自检：假账号 + mock 上游 + 本地 ccodex + 校验器。

在 --workdir 下生成一次性环境：
    config.toml            listen 127.0.0.1:<ccodex-port>, accounts_dir ./accounts
    accounts/e2e/auth.json 假 ChatGPT OAuth 凭证（未签名 JWT，exp 远期）
    keys.json              不生成 -> 中继空密钥库模式（任意 Bearer 放行）

随后启动 mock_telemetry_upstream.py 与 ccodex（debug 构建），三个仅开发用的覆盖变量：
    CCODEX_UPSTREAM_BASE_URL_OVERRIDE   -> mock
    CCODEX_ANALYTICS_BASE_URL_OVERRIDE  -> mock（analytics 接收端）
    CCODEX_STATSIG_ENDPOINT_OVERRIDE    -> mock（OTLP 接收端）
最后运行 testdata/verify_telemetry.py 做形状/语义断言。

用法: python testdata/verify_telemetry_e2e.py [--bin target/debug/ccodex.exe]
"""
import argparse
import base64
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request
from datetime import datetime, timedelta, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def b64url(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).decode().rstrip("=")


def fake_jwt(claims: dict) -> str:
    header = b64url(json.dumps({"alg": "RS256", "typ": "JWT"}).encode())
    payload = b64url(json.dumps(claims).encode())
    return f"{header}.{payload}.{b64url(b'e2e-signature')}"


def wait_port(port: int, timeout: float = 60.0) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        with socket.socket() as s:
            s.settimeout(0.5)
            if s.connect_ex(("127.0.0.1", port)) == 0:
                return True
        time.sleep(0.2)
    return False


def write_proxies(workdir: Path, mock_port: int) -> None:
    """Binds the e2e account to the mock acting as its HTTP proxy.

    Every egress of that account — data plane, analytics events, Statsig OTLP — must
    then arrive at the mock in absolute (proxy) form. Telemetry that ignored the account
    binding would arrive in origin form, which verify_telemetry.py asserts against.
    """
    (workdir / "proxies.json").write_text(json.dumps({
        "proxies": [{"name": "e2e-proxy", "url": f"http://127.0.0.1:{mock_port}"}],
        "assignments": {"e2e": "e2e-proxy"},
    }, indent=2), encoding="utf-8")


def write_env(workdir: Path, ccodex_port: int) -> Path:
    accounts = workdir / "accounts"
    acc = accounts / "e2e"
    acc.mkdir(parents=True, exist_ok=True)
    exp = int((datetime.now(timezone.utc) + timedelta(days=30)).timestamp())
    access = fake_jwt({"exp": exp, "sub": "user-e2e"})
    id_token = fake_jwt({
        "exp": exp,
        "email": "e2e@example.com",
        "https://api.openai.com/auth": {
            "chatgpt_plan_type": "pro",
            "chatgpt_user_id": "user-e2e",
            "chatgpt_account_id": "acct-e2e",
        },
    })
    (acc / "auth.json").write_text(json.dumps({
        "auth_mode": "chatgpt",
        "OPENAI_API_KEY": None,
        "tokens": {
            "id_token": id_token,
            "access_token": access,
            "refresh_token": "rt-e2e",
            "account_id": "acct-e2e",
        },
        "last_refresh": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
    }, indent=2), encoding="utf-8")

    config = workdir / "config.toml"
    config.write_text(
        f'listen = "127.0.0.1:{ccodex_port}"\n'
        f'accounts_dir = {json.dumps(str(accounts))}\n',
        encoding="utf-8")
    return config


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default=str(ROOT / "target" / "debug" / "ccodex.exe"))
    ap.add_argument("--ccodex-port", type=int, default=8317)
    ap.add_argument("--mock-port", type=int, default=8899)
    ap.add_argument("--workdir", default=None)
    ap.add_argument("--keep", action="store_true", help="keep the workdir and logs")
    args = ap.parse_args()

    binary = Path(args.bin)
    if not binary.exists():
        print(f"binary not found: {binary} (run: cargo build -p ccodex)")
        return 2

    workdir = Path(args.workdir) if args.workdir else Path(tempfile.mkdtemp(prefix="ccodex-e2e-"))
    workdir.mkdir(parents=True, exist_ok=True)
    capture = workdir / "capture.jsonl"
    config = write_env(workdir, args.ccodex_port)
    write_proxies(workdir, args.mock_port)

    mock_log = open(workdir / "mock.log", "wb")
    relay_log = open(workdir / "ccodex.log", "wb")
    procs: list[subprocess.Popen] = []
    try:
        procs.append(subprocess.Popen(
            [sys.executable, str(ROOT / "testdata" / "mock_telemetry_upstream.py"),
             "--port", str(args.mock_port), "--capture", str(capture)],
            stdout=mock_log, stderr=subprocess.STDOUT))
        if not wait_port(args.mock_port, 20):
            print("mock upstream failed to start")
            return 2

        env = {**os.environ,
               "RUST_LOG": "info",
               "CCODEX_UPSTREAM_BASE_URL_OVERRIDE":
                   f"http://127.0.0.1:{args.mock_port}/backend-api/codex",
               "CCODEX_ANALYTICS_BASE_URL_OVERRIDE":
                   f"http://127.0.0.1:{args.mock_port}/backend-api",
               "CCODEX_STATSIG_ENDPOINT_OVERRIDE":
                   f"http://127.0.0.1:{args.mock_port}/otlp/v1/metrics",
               # Dev-only: 5s export cadence instead of the SDK default.
               "CCODEX_STATSIG_EXPORT_INTERVAL_MS": "5000"}
        procs.append(subprocess.Popen(
            [str(binary), "serve", "--config", str(config)],
            env=env, stdout=relay_log, stderr=subprocess.STDOUT))
        if not wait_port(args.ccodex_port, 60):
            print("ccodex failed to start; log tail:")
            print((workdir / "ccodex.log").read_text(encoding="utf-8", errors="replace")[-3000:])
            return 2

        print(f"workdir {workdir}")
        proc = subprocess.run(
            [sys.executable, str(ROOT / "testdata" / "verify_telemetry.py"),
             "--ccodex-url", f"http://127.0.0.1:{args.ccodex_port}",
             "--mock-url", f"http://127.0.0.1:{args.mock_port}",
             "--capture", str(capture), "--key", "test-key"],
            cwd=str(ROOT))
        if proc.returncode != 0:
            print("\n--- ccodex log tail ---")
            print((workdir / "ccodex.log").read_text(encoding="utf-8", errors="replace")[-4000:])
        return proc.returncode
    finally:
        for p in procs:
            p.terminate()
        for p in procs:
            try:
                p.wait(timeout=5)
            except subprocess.TimeoutExpired:
                p.kill()
        mock_log.close()
        relay_log.close()
        if not args.keep and args.workdir is None:
            shutil.rmtree(workdir, ignore_errors=True)
        else:
            print(f"artifacts kept in {workdir}")


if __name__ == "__main__":
    sys.exit(main())
