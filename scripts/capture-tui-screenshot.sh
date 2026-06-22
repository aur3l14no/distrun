#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

cd "$ROOT"

export DISTRUN_CAPTURE_SCRIPT="$ROOT/scripts/capture-tui-screenshot.cjs"

command='playwright install chromium; first_bin="${PATH%%:*}"; export NODE_PATH="${first_bin%/.bin}${NODE_PATH:+:$NODE_PATH}"; node "$DISTRUN_CAPTURE_SCRIPT"'
for arg in "$@"; do
    printf -v quoted ' %q' "$arg"
    command+="$quoted"
done

npx -y \
    -p node-pty \
    -p playwright \
    -p @xterm/xterm \
    -c "$command"
