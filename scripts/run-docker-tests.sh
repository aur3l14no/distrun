#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE="${DISTRUN_TEST_IMAGE:-distrun-ssh-tmux:local}"
TMP_DIR="$(mktemp -d)"
CONTAINER=""

cleanup() {
    if [[ -n "$CONTAINER" ]]; then
        docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
    fi
    rm -rf "$TMP_DIR"
}
trap cleanup EXIT

ssh-keygen -t ed25519 -N "" -f "$TMP_DIR/id_ed25519" >/dev/null
mkdir -p "$TMP_DIR/bin"

docker build -t "$IMAGE" "$ROOT/tests/fixtures/ssh-tmux"
CONTAINER="$(docker run -d -p 127.0.0.1::22 "$IMAGE")"

PORT=""
for _ in {1..50}; do
    PORT="$(docker port "$CONTAINER" 22/tcp | awk -F: 'NR == 1 {print $NF}')"
    if [[ -n "$PORT" ]]; then
        break
    fi
    sleep 0.1
done

docker exec "$CONTAINER" mkdir -p /home/distrun/.ssh
docker cp "$TMP_DIR/id_ed25519.pub" "$CONTAINER:/home/distrun/.ssh/authorized_keys"
docker exec "$CONTAINER" chown -R distrun:distrun /home/distrun/.ssh
docker exec "$CONTAINER" chmod 700 /home/distrun/.ssh
docker exec "$CONTAINER" chmod 600 /home/distrun/.ssh/authorized_keys

cat > "$TMP_DIR/ssh_config" <<EOF
Host distrun-test
    HostName 127.0.0.1
    Port $PORT
    User distrun
    IdentityFile $TMP_DIR/id_ed25519
    StrictHostKeyChecking no
    UserKnownHostsFile /dev/null
    LogLevel ERROR
EOF
chmod 600 "$TMP_DIR/ssh_config"

cat > "$TMP_DIR/bin/ssh" <<EOF
#!/usr/bin/env bash
exec /usr/bin/ssh -F "$TMP_DIR/ssh_config" "\$@"
EOF
chmod +x "$TMP_DIR/bin/ssh"

for _ in {1..50}; do
    if "$TMP_DIR/bin/ssh" distrun-test true; then
        break
    fi
    sleep 0.2
done

PATH="$TMP_DIR/bin:$PATH" \
DISTRUN_TEST_SSH_TARGET="distrun-test" \
    cargo test --test ssh_tmux -- --ignored --nocapture
