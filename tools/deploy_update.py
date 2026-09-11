#!/usr/bin/env python3
"""增量部署更新：把当前工作树同步到服务器并原地重编译、换二进制、重启。

与 tools/deploy_server.py（首次装机：建 config/keys/systemd）不同，本脚本
**绝不触碰** 服务器上的 config.toml / keys.json / accounts/ / admin.json /
usage.json —— 只做 源码同步 → cargo build --release → 换二进制 → 重启 → 自检。

认证：SSH 公钥（该机 root 口令登录已禁用）。默认用 D:\secure_linux_server 工作区的
AI 维护私钥；可用环境变量覆盖：
    DEPLOY_KEY           私钥路径（默认 D:\secure_linux_server\keys\publickey-ai_ed25519）
    DEPLOY_KEY_PASSPHRASE 私钥口令文件（默认同目录 .passphrase.txt）
    DEPLOY_HOST / DEPLOY_USER / DEPLOY_PORT
    DEPLOY_SRC / DEPLOY_HOME / DEPLOY_SERVICE

用法:
    python tools/deploy_update.py all      # 或单步: package upload build install verify

阶段:
    package  本地打包（含 vendor/ 与 web/dist，排除 target/.git/账号数据）
    upload   上传 + 解包到 /opt/ccodex-src
    build    服务器上 cargo build --release -p ccodex
    install  备份旧二进制 → 换新 → systemctl restart
    verify   /health + journal 尾部
"""
import os
import sys
import tarfile
import time

import paramiko

HOST = os.environ.get("DEPLOY_HOST", "109.244.63.120")
USER = os.environ.get("DEPLOY_USER", "root")
PORT = int(os.environ.get("DEPLOY_PORT", "22"))
KEY = os.environ.get("DEPLOY_KEY",
                     r"D:\secure_linux_server\keys\publickey-ai_ed25519")
KEY_PASSPHRASE_FILE = os.environ.get(
    "DEPLOY_KEY_PASSPHRASE",
    r"D:\secure_linux_server\keys\publickey-ai_ed25519.passphrase.txt")
LOCAL = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TGZ = os.path.join(os.path.dirname(LOCAL), "ccodex-update.tgz")
REMOTE_SRC = os.environ.get("DEPLOY_SRC", "/opt/ccodex-src")
REMOTE_HOME = os.environ.get("DEPLOY_HOME", "/opt/ccodex")
SERVICE = os.environ.get("DEPLOY_SERVICE", "ccodex")


def connect():
    if not os.path.exists(KEY):
        sys.exit(f"找不到私钥 {KEY}（可用 DEPLOY_KEY 覆盖）")
    passphrase = None
    if os.path.exists(KEY_PASSPHRASE_FILE):
        passphrase = open(KEY_PASSPHRASE_FILE, encoding="utf-8").read().strip()
    # 私钥只读加载；口令从工作区口令文件读取，不打印、不落盘
    pkey = paramiko.Ed25519Key.from_private_key_file(KEY, password=passphrase)
    c = paramiko.SSHClient()
    c.set_missing_host_key_policy(paramiko.AutoAddPolicy())
    c.connect(HOST, port=PORT, username=USER, pkey=pkey, timeout=20,
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
        raise SystemExit(f"命令失败 ({code}): {cmd}")
    return code, out, err


def stage_package():
    def keep(ti):
        parts = ti.name.split("/")
        skip = {"target", ".git", "node_modules", "__pycache__", "accounts"}
        if any(p in skip for p in parts):
            return None
        if ti.name.endswith((".log", ".pyc", ".tgz")):
            return None
        if parts[-1] in ("config.toml", "config.test.toml", "config.local.toml"):
            return None
        # 运行时状态一律不上传
        if parts[-1] in ("keys.json", "admin.json", "usage.json",
                         "pricing.json", "proxies.json"):
            return None
        # web/dist（内嵌进二进制的前端产物）留在包里；其余全部保留
        return ti

    with tarfile.open(TGZ, "w:gz") as tar:
        tar.add(LOCAL, arcname="ccodex", filter=keep)
    print(f"打包完成 {TGZ} ({os.path.getsize(TGZ) / 1e6:.1f} MB)")


def stage_upload(c):
    run(c, f"mkdir -p {REMOTE_SRC} /tmp/ccodex-new")
    sftp = c.open_sftp()
    total = os.path.getsize(TGZ)
    done = [0]

    def progress(sent, _):
        if sent - done[0] > 4 * 1024 * 1024 or sent == total:
            done[0] = sent
            print(f"  上传 {sent / 1e6:.0f}/{total / 1e6:.0f} MB", flush=True)

    sftp.put(TGZ, "/tmp/ccodex-update.tgz", callback=progress)
    sftp.close()
    # 覆盖式同步：解包后用 tar 管道叠加到源码树，**不删目录**——服务器上的
    # target/（增量编译缓存）与 .cargo/（crates 镜像配置）必须原样保留，
    # 否则每次部署都会退化成一次从零全量编译。
    run(c, f"rm -rf /tmp/ccodex-new && mkdir -p /tmp/ccodex-new {REMOTE_SRC} && "
           f"tar xzf /tmp/ccodex-update.tgz -C /tmp/ccodex-new && "
           f"tar -C /tmp/ccodex-new/ccodex -cf - . | tar -C {REMOTE_SRC} -xf - && "
           f"rm -rf /tmp/ccodex-new")
    run(c, f"ls {REMOTE_SRC}; ls -d {REMOTE_SRC}/target 2>/dev/null || "
           "echo '(target 不存在：本次将全量编译)'")


def stage_build(c):
    run(c, f"cd {REMOTE_SRC} && source $HOME/.cargo/env && "
           "nohup cargo build --release -p ccodex > /tmp/ccodex-build.log 2>&1 & echo started")
    for i in range(240):  # 最多 40 分钟
        time.sleep(10)
        code, out, _ = run(c, "pgrep -f 'cargo build' >/dev/null && echo running || echo done",
                           check=False, quiet=True)
        if "done" in out:
            break
        if i % 6 == 5:
            _, tail, _ = run(c, "tail -1 /tmp/ccodex-build.log", check=False, quiet=True)
            print(f"  编译中... {tail.strip()[:110]}", flush=True)
    run(c, "tail -3 /tmp/ccodex-build.log")
    run(c, f"ls -la {REMOTE_SRC}/target/release/ccodex")


def stage_install(c):
    # config.toml / keys.json / /var/lib/ccodex 状态一律不触碰，只换二进制
    run(c, f"systemctl stop {SERVICE} 2>/dev/null; "
           f"cp -f {REMOTE_HOME}/{SERVICE} /tmp/{SERVICE}.bak.$(date +%s) 2>/dev/null; "
           f"install -m 755 {REMOTE_SRC}/target/release/ccodex {REMOTE_HOME}/{SERVICE} && "
           f"systemctl start {SERVICE}")
    time.sleep(3)
    code, out, _ = run(c, f"systemctl is-active {SERVICE}", check=False, quiet=True)
    if out.strip() != "active":
        # 启动失败：回滚到最近一次备份
        run(c, f"LATEST=$(ls -t /tmp/{SERVICE}.bak.* | head -1) && "
               f"install -m 755 \"$LATEST\" {REMOTE_HOME}/{SERVICE} && systemctl start {SERVICE}",
            check=False)
        run(c, f"journalctl -u {SERVICE} -n 20 --no-pager", check=False)
        raise SystemExit("新二进制启动失败，已回滚到备份")
    run(c, f"ls -la {REMOTE_HOME}/{SERVICE}")


def stage_verify(c):
    time.sleep(3)
    run(c, f"systemctl status {SERVICE} --no-pager -l | head -12", check=False)
    run(c, "curl -sS --max-time 10 http://127.0.0.1:8317/health", check=False)
    run(c, f"journalctl -u {SERVICE} -n 25 --no-pager", check=False)


STAGES = {
    "package": lambda c: stage_package(),
    "upload": stage_upload,
    "build": stage_build,
    "install": stage_install,
    "verify": stage_verify,
}


def main():
    stage = sys.argv[1] if len(sys.argv) > 1 else "all"
    if stage == "all":
        stage_package()
        c = connect()
        try:
            for name in ("upload", "build", "install", "verify"):
                print(f"\n===== {name} =====", flush=True)
                STAGES[name](c)
        finally:
            c.close()
    elif stage == "package":
        stage_package()
    elif stage in STAGES:
        c = connect()
        try:
            STAGES[stage](c)
        finally:
            c.close()
    else:
        sys.exit(f"未知阶段 {stage}；可选 {list(STAGES)} 或 all")


if __name__ == "__main__":
    main()
