#!/usr/bin/env bash
# 在 Linux 机器上构建 ccodex（与 .github/workflows/release.yml 同一条路径）
set -euo pipefail
cd "$(dirname "$0")/.."

if command -v apt-get >/dev/null 2>&1; then
  sudo apt-get update
  sudo apt-get install -y --no-install-recommends build-essential pkg-config libssl-dev python3
fi

python3 tools/sync_upstream.py

if command -v npm >/dev/null 2>&1; then
  (cd web && npm ci && npm run build)
else
  echo "WARN: npm 不可用，管理前端将使用占位页面（中继功能不受影响）" >&2
fi

cargo build --release
echo "完成: target/release/ccodex"
