#!/usr/bin/env python3
"""ccodex 上游同步 + 锚定补丁 + 验证脚本。

每次升级官方 openai/codex 源码后执行：

    python tools/sync_upstream.py            # 同步 + 打补丁 + 验证
    python tools/sync_upstream.py --check    # 上面这些 + cargo check 编译验证

流程：
  1. 按 upstream.lock.json 把 vendor/codex 同步到锁定 commit
  2. 按 patches/patches.json 逐条应用锚定补丁（幂等）
  3. 验证每条补丁确实生效（replacement 在文件中存在）
  4. 可选：cargo check 验证整个 workspace 编译通过

任何一步失败都会以非零退出码中止 —— 宁可构建失败，也不能静默产出行为分叉的二进制。
"""
from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
VENDOR = ROOT / "vendor" / "codex"
LOCK = ROOT / "upstream.lock.json"
PATCHES = ROOT / "patches" / "patches.json"
STAMP = VENDOR / ".ccodex-sync.json"


def log(msg: str) -> None:
    print(f"[sync] {msg}", flush=True)


def die(msg: str, code: int = 1) -> "SystemExit":
    print(f"[sync] ERROR: {msg}", file=sys.stderr, flush=True)
    return SystemExit(code)


def run(cmd: list[str], cwd: Path | None = None) -> str:
    proc = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True)
    if proc.returncode != 0:
        raise die(f"命令失败: {' '.join(cmd)}\n{proc.stdout}\n{proc.stderr}")
    return proc.stdout.strip()


def load_json(path: Path) -> dict:
    with path.open(encoding="utf-8") as f:
        return json.load(f)


def sync_vendor(lock: dict, from_path: str | None) -> None:
    commit = lock["commit"]
    repo = lock["repo"]
    if from_path:
        # 开发便利：直接从本地已有的克隆拷贝（跳过网络）。
        src = Path(from_path)
        if not (src / ".git").exists():
            raise die(f"--from-path 指向的不是 git 仓库: {src}")
        if VENDOR.exists():
            shutil.rmtree(VENDOR)
        log(f"从本地仓库拷贝 {src} -> {VENDOR}")
        run(["git", "clone", "--no-hardlinks", str(src), str(VENDOR)])
        run(["git", "checkout", "--detach", commit], cwd=VENDOR)
    elif not VENDOR.exists():
        log(f"克隆 {repo} -> {VENDOR}（如需要代理请先设置 HTTPS_PROXY 环境变量）")
        VENDOR.parent.mkdir(parents=True, exist_ok=True)
        run(["git", "clone", "--filter=blob:none", "--no-checkout", repo, str(VENDOR)])
        run(["git", "fetch", "--depth", "1", "origin", commit], cwd=VENDOR)
        run(["git", "checkout", "--detach", commit], cwd=VENDOR)
    else:
        head = run(["git", "rev-parse", "HEAD"], cwd=VENDOR)
        if head != commit:
            log(f"上游版本切换: {head[:12]} -> {commit[:12]}")
            run(["git", "fetch", "--depth", "1", "origin", commit], cwd=VENDOR)
            run(["git", "reset", "--hard", commit], cwd=VENDOR)
            run(["git", "clean", "-fdx", "-e", ".ccodex-sync.json"], cwd=VENDOR)
        else:
            log(f"vendor 已在锁定 commit {commit[:12]}")

    head = run(["git", "rev-parse", "HEAD"], cwd=VENDOR)
    if head != commit:
        raise die(f"vendor HEAD {head} 与锁定 commit {commit} 不一致")
    # 同步后必须没有残留改动，否则补丁状态不可信。
    if not from_path:
        status = run(["git", "status", "--porcelain"], cwd=VENDOR)
        if status:
            log("检测到 vendor 内已有本地改动，重置后重新打补丁")
            run(["git", "reset", "--hard", commit], cwd=VENDOR)
            run(["git", "clean", "-fd"], cwd=VENDOR)


def resolve_codex_version(lock: dict) -> str | None:
    """解析锁定 commit 对应的发布版本：lock 显式 version 优先，其次 npm registry 最新版。"""
    if lock.get("version"):
        return lock["version"]
    try:
        import urllib.request

        req = urllib.request.Request(
            "https://registry.npmjs.org/@openai/codex/latest",
            headers={"Accept": "application/json"},
        )
        with urllib.request.urlopen(req, timeout=15) as resp:
            return json.load(resp)["version"]
    except Exception as e:  # noqa: BLE001
        log(f"npm registry 版本查询失败: {e}")
        return None


def apply_patches(manifest: dict) -> list[dict]:
    results = []
    for p in manifest["patches"]:
        path = VENDOR / p["file"]
        if not path.exists():
            raise die(f"补丁 {p['name']}: 目标文件不存在 {p['file']}（官方目录结构变了？）")
        text = path.read_text(encoding="utf-8")
        if p["replacement"] in text:
            results.append({"name": p["name"], "status": "already-applied"})
            continue
        count = text.count(p["anchor"])
        expect = p.get("occurrences", 1)
        if count != expect:
            raise die(
                f"补丁 {p['name']}: anchor 在 {p['file']} 中出现 {count} 次（期望 {expect} 次）。\n"
                f"官方代码已漂移，请人工检查并更新 patches/patches.json。\n"
                f"anchor: {p['anchor']!r}"
            )
        text = text.replace(p["anchor"], p["replacement"])
        path.write_text(text, encoding="utf-8", newline="")
        if p["replacement"] not in path.read_text(encoding="utf-8"):
            raise die(f"补丁 {p['name']}: 应用后校验失败")
        results.append({"name": p["name"], "status": "applied"})
    return results


def main() -> int:
    ap = argparse.ArgumentParser(description="ccodex upstream sync + patch + verify")
    ap.add_argument("--from-path", help="从本地已有克隆同步（跳过网络）")
    ap.add_argument("--check", action="store_true", help="补丁后运行 cargo check 验证编译")
    ap.add_argument("--skip-sync", action="store_true", help="只重新打补丁并验证，不动 git")
    args = ap.parse_args()

    lock = load_json(LOCK)
    manifest = load_json(PATCHES)

    if not args.skip_sync:
        sync_vendor(lock, args.from_path)
    elif not VENDOR.exists():
        raise die("vendor/codex 不存在，先运行不带 --skip-sync 的同步")

    results = apply_patches(manifest)
    for r in results:
        log(f"patch {r['name']}: {r['status']}")

    # 依赖钉版：官方 Cargo.lock 同步为 workspace 锁文件。
    # 否则 semver 会把官方依赖解析到更新的不兼容版本（实测 rama-error 0.3.0-alpha.4
    # 被解析到 0.3.0 正式版导致 rama-core 编译失败）。
    official_lock = VENDOR / "codex-rs" / "Cargo.lock"
    our_lock = ROOT / "Cargo.lock"
    if official_lock.exists():
        if not our_lock.exists() or official_lock.read_bytes() != our_lock.read_bytes():
            shutil.copy(official_lock, our_lock)
            log("已同步官方 Cargo.lock -> workspace 根")
        else:
            log("Cargo.lock 已与官方一致")
    else:
        raise die("官方 Cargo.lock 缺失（仓库结构变了？）")

    commit = run(["git", "rev-parse", "HEAD"], cwd=VENDOR)

    # 版本号：锁定 commit 对应的发布版本（烘焙进二进制，运行时不跟随网络）
    old_stamp = load_json(STAMP) if STAMP.exists() else {}
    codex_version = resolve_codex_version(lock) or old_stamp.get("codex_version")
    if codex_version:
        log(f"上游版本号: {codex_version}")
    else:
        log("WARNING: 无法确定上游版本号（可在 upstream.lock.json 里手动加 \"version\" 字段）")
        codex_version = "0.0.0"

    stamp = {
        "commit": commit,
        "codex_version": codex_version,
        "synced_at": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "patches": results,
    }
    STAMP.write_text(json.dumps(stamp, indent=2, ensure_ascii=False), encoding="utf-8")
    log(f"补丁全部生效，写入 {STAMP.name}")

    if args.check:
        log("运行 cargo check 验证编译（首次较久）…")
        code = subprocess.run(["cargo", "check", "--workspace"], cwd=ROOT).returncode
        if code != 0:
            raise die("cargo check 失败 —— 补丁与官方代码不兼容，请检查上方编译错误", code)
        log("cargo check 通过")

    log("完成")
    return 0


if __name__ == "__main__":
    sys.exit(main())
