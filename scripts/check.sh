#!/usr/bin/env bash
# 本地等价 CI：三平台 workflow 的 mac/linux 可本地复现部分。
set -euo pipefail
cd "$(dirname "$0")/.."

if [ -f "$HOME/.cargo/env" ]; then
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
fi

# 检查步骤统一由 Node 入口维护，Windows 无需 bash 即可执行 npm run check。
exec npm run check
