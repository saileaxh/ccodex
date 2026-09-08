#!/usr/bin/env python3
"""Deploy ccodex to a Linux server (RHEL-ish: OpenCloudOS/CentOS/Rocky).

Usage:
    set DEPLOY_PASS=<root password>            # required
    set DEPLOY_HOST=8ssdzz76.saileaxh.com      # optional (default shown below)
    python tools/deploy_server.py all          # or a single stage

Stages: package → upload → deps → build → install → verify
The server builds natively (OpenSSL TLS stack); ua_platform = "native" in the
generated config keeps UA consistent with that stack.
"""
import os
import sys
import tarfile
import time
import secrets
import paramiko

HOST = os.environ.get("DEPLOY_HOST", "8ssdzz76.saileaxh.com")
USER = os.environ.get("DEPLOY_USER", "root")
PASSWORD = os.environ.get("DEPLOY_PASS")
LOCAL = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TGZ = os.path.join(os.path.dirname(LOCAL), "ccodex-deploy.tgz")
REMOTE_SRC = "/opt/ccodex/src"
REMOTE_HOME = "/opt/ccodex"
API_KEY_FILE = os.path.join(REMOTE_HOME, "api_key.txt")


def connect():
    if not PASSWORD:
        sys.exit("DEPLOY_PASS env var is required")
    c = paramiko.SSHClient()
    c.set_missing_host_key_policy(paramiko.AutoAddPolicy())
    c.connect(HOST, username=USER, password=PASSWORD, timeout=20,
              allow_agent=False, look_for_keys=False)
    return c


def run(c, cmd, timeout=900, check=True, quiet=False):
    if not quiet:
        print(f"$ {cmd}", flush=True)
    _, stdout, stderr = c.exec_command(cmd, timeout=timeout)
    out = stdout.read().decode(errors="replace")
    err = stderr.read().decode(errors="replace")
    code = stdout.channel.recv_exit_status()
    if out.strip() and not quiet:
        print(out, flush=True)
    if err.strip() and not quiet:
        print(err, file=sys.stderr, flush=True)
    if check and code != 0:
        raise SystemExit(f"command failed ({code}): {cmd}")
    return code, out, err


def stage_package():
    def keep(ti):
        parts = ti.name.split("/")
        skip_dirs = {"target", ".git", "node_modules", "accounts", "__pycache__"}
        if any(p in skip_dirs for p in parts):
            return None
        if ti.name.endswith((".log", ".pyc")):
            return None
        if parts[-1] in ("config.toml", "config.test.toml"):
            return None
        return ti

    with tarfile.open(TGZ, "w:gz") as tar:
        tar.add(LOCAL, arcname="ccodex", filter=keep)
    size_mb = os.path.getsize(TGZ) / 1e6
    print(f"packaged {TGZ} ({size_mb:.1f} MB)")


def stage_upload(c):
    run(c, f"mkdir -p {REMOTE_SRC}")
    sftp = c.open_sftp()
    total = os.path.getsize(TGZ)
    done = [0]

    def progress(sent, _):
        if sent - done[0] > 10 * 1024 * 1024 or sent == total:
            done[0] = sent
            print(f"  upload {sent / 1e6:.0f}/{total / 1e6:.0f} MB", flush=True)

    sftp.put(TGZ, "/tmp/ccodex.tgz", callback=progress)
    sftp.close()
    run(c, f"rm -rf {REMOTE_SRC}/ccodex && tar xzf /tmp/ccodex.tgz -C {REMOTE_SRC} && ls {REMOTE_SRC}/ccodex")


def stage_deps(c):
    run(c, "dnf install -y gcc gcc-c++ make cmake perl pkgconf-pkg-config openssl-devel curl tar",
        timeout=1200)
    code, _, _ = run(c, "command -v cargo", check=False, quiet=True)
    if code != 0:
        run(c, "export RUSTUP_DIST_SERVER=https://rsproxy.cn RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup; "
               "curl --proto '=https' --tlsv1.2 -sSf https://rsproxy.cn/rustup-init.sh | sh -s -- -y "
               "--default-toolchain stable", timeout=1200)
    run(c, r"""mkdir -p $HOME/.cargo && cat > $HOME/.cargo/config.toml <<'EOF'
[source.crates-io]
replace-with = 'rsproxy-sparse'
[source.rsproxy-sparse]
registry = 'sparse+https://rsproxy.cn/index/'
[registries.rsproxy]
index = 'sparse+https://rsproxy.cn/index/'
[net]
git-fetch-with-cli = true
EOF
source $HOME/.cargo/env && cargo --version && rustc --version""")


def stage_build(c):
    run(c, f"cd {REMOTE_SRC}/ccodex && source $HOME/.cargo/env && "
           "nohup cargo build --release -p ccodex > /tmp/ccodex-build.log 2>&1 & echo started")
    for i in range(240):  # up to 40 min
        time.sleep(10)
        code, out, _ = run(c, "pgrep -f 'cargo build' >/dev/null && echo running || echo done",
                           check=False, quiet=True)
        if "done" in out:
            break
        if i % 6 == 5:
            _, tail, _ = run(c, "tail -1 /tmp/ccodex-build.log", check=False, quiet=True)
            print(f"  build... {tail.strip()[:100]}", flush=True)
    run(c, "tail -5 /tmp/ccodex-build.log")
    run(c, f"ls -la {REMOTE_SRC}/ccodex/target/release/ccodex")


def stage_install(c):
    api_key = "sk-ccodex-" + secrets.token_urlsafe(24)
    run(c, f"mkdir -p {REMOTE_HOME}/accounts && "
           f"cp {REMOTE_SRC}/ccodex/target/release/ccodex {REMOTE_HOME}/ccodex && "
           f"echo '{api_key}' > {API_KEY_FILE} && chmod 600 {API_KEY_FILE}")
    # Downstream keys live only in the managed store (keys.json); config carries no api_keys.
    import json as _json
    keys_doc = _json.dumps([{"name": "bootstrap", "key": api_key,
                             "created_at_unix": int(time.time())}])
    run(c, f"cat > {REMOTE_HOME}/keys.json <<'EOF'\n{keys_doc}\nEOF\nchmod 600 {REMOTE_HOME}/keys.json")
    run(c, f"""cat > {REMOTE_HOME}/config.toml <<'EOF'
listen = "0.0.0.0:8317"
accounts_dir = "{REMOTE_HOME}/accounts"

[identity]
ua_platform = "native"
EOF
chmod 600 {REMOTE_HOME}/config.toml""")
    run(c, f"""cat > /etc/systemd/system/ccodex.service <<'EOF'
[Unit]
Description=ccodex relay
After=network-online.target
Wants=network-online.target

[Service]
ExecStart={REMOTE_HOME}/ccodex serve --config {REMOTE_HOME}/config.toml
WorkingDirectory={REMOTE_HOME}
Environment=RUST_LOG=info
Restart=always
RestartSec=3

[Install]
WantedBy=multi-user.target
EOF
systemctl daemon-reload && systemctl enable --now ccodex""")
    code, out, _ = run(c, "systemctl is-active firewalld", check=False, quiet=True)
    if "active" in out:
        run(c, "firewall-cmd --permanent --add-port=8317/tcp && firewall-cmd --reload", check=False)
    print(f"API key: {api_key}")


def stage_verify(c):
    time.sleep(2)
    run(c, "systemctl status ccodex --no-pager -l | head -15", check=False)
    _, key, _ = run(c, f"cat {API_KEY_FILE}", quiet=True)
    key = key.strip()
    run(c, f"curl -sS --max-time 10 -H 'Authorization: Bearer {key}' http://127.0.0.1:8317/health")
    run(c, "curl -sS -o /dev/null -w 'chatgpt.com -> HTTP %{http_code}\\n' --max-time 12 https://chatgpt.com/",
        check=False)


STAGES = {
    "package": lambda c: stage_package(),
    "upload": stage_upload,
    "deps": stage_deps,
    "build": stage_build,
    "install": stage_install,
    "verify": stage_verify,
}


def main():
    stage = sys.argv[1] if len(sys.argv) > 1 else "all"
    if stage == "all":
        stage_package()
        c = connect()
        for name in ("upload", "deps", "build", "install", "verify"):
            print(f"\n===== {name} =====", flush=True)
            STAGES[name](c)
        c.close()
    elif stage == "package":
        stage_package()
    elif stage in STAGES:
        c = connect()
        STAGES[stage](c)
        c.close()
    else:
        sys.exit(f"unknown stage {stage}; choose from {list(STAGES)} or all")


if __name__ == "__main__":
    main()
