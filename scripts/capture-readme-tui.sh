#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ -z "${DISTRUN_TEST_SSH_TARGET:-}" ]]; then
    exec "$ROOT/scripts/run-docker-tests.sh" "$ROOT/scripts/capture-readme-tui.sh" "$@"
fi

PROJECT="${DISTRUN_CAPTURE_PROJECT:-readme-demo}"
OUTPUT="${DISTRUN_CAPTURE_OUTPUT:-$ROOT/docs/tui-screenshot.png}"
TMP_DIR="$(mktemp -d)"
CONFIG="$TMP_DIR/distrun.yml"

cleanup() {
    write_config initial
    "$ROOT/target/debug/distrun" -f "$CONFIG" down >/dev/null 2>&1 || true
    rm -rf "$TMP_DIR"
}
trap cleanup EXIT

write_config() {
    local mode="$1"
    cat > "$CONFIG" <<EOF
project: $PROJECT
hosts:
  edge:
    ssh: $DISTRUN_TEST_SSH_TARGET
services:
  api:
    host: edge
    cmd: bash -lc 'while true; do echo edge-api \$(date +%H:%M:%S) GET /health 200; sleep 1; done'
    stop_timeout: 1s
EOF

    if [[ "$mode" == "initial" ]]; then
        cat >> "$CONFIG" <<EOF
  worker:
    host: edge
    cmd: bash -lc 'while true; do echo edge-worker \$(date +%H:%M:%S) job complete; sleep 2; done'
    stop_timeout: 1s
EOF
    else
        cat >> "$CONFIG" <<EOF
  metrics:
    host: edge
    cmd: bash -lc 'while true; do echo edge-metrics scrape ok; sleep 2; done'
    stop_timeout: 1s
EOF
    fi

    cat >> "$CONFIG" <<EOF
  db:
    cmd: bash -lc 'while true; do echo local-db ready; sleep 2; done'
    stop_timeout: 1s
  ui:
    cmd: bash -lc 'echo local-ui build failed; exit 2'
    stop_timeout: 1s
EOF
}

cargo build
write_config initial
"$ROOT/target/debug/distrun" -f "$CONFIG" up
sleep 2
write_config final

"$ROOT/scripts/capture-tui-screenshot.sh" \
    --output "$OUTPUT" \
    --cols 104 \
    --rows 24 \
    --width 1100 \
    --height 620 \
    --wait-for "GET /health 200" \
    --timeout 20000 \
    -- "$ROOT/target/debug/distrun" -f "$CONFIG" tui --tail 8
