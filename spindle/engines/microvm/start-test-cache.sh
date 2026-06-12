#!/usr/bin/env bash
set -euo pipefail

# start a local ncps binary cache
# usage: ./start-test-cache.sh <test-dir> [port]

if [ "$#" -lt 1 ]; then
    echo "Usage: $0 <test-dir> [ncps-port]"
    exit 1
fi

TEST_DIR="$(mkdir -p "$1" && cd "$1" && pwd)"
PORT="${2:-8501}"

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

SECRET_KEY_PATH="$TEST_DIR/test-cache-key.secret"
PUBLIC_KEY_PATH="$TEST_DIR/test-cache-key.pub"
DB_PATH="$TEST_DIR/ncps.sqlite"
CONFIG_PATH="$TEST_DIR/ncps-config.yaml"
STORAGE_DIR="$TEST_DIR/storage"
ENV_PATH="$TEST_DIR/env.sh"
PID_PATH="$TEST_DIR/ncps.pid"

mkdir -p "$STORAGE_DIR"

echo "generating binary cache keys.."
nix-store --generate-binary-cache-key test-cache-key "$SECRET_KEY_PATH" "$PUBLIC_KEY_PATH"
PUBKEY_VAL=$(cat "$PUBLIC_KEY_PATH")

echo "initializing ncps db..."
nix shell nixpkgs#dbmate --command dbmate \
    --migrations-dir "$(nix build --no-link --print-out-paths nixpkgs#ncps)/share/ncps/db/migrations/sqlite" \
    -u "sqlite:$DB_PATH" \
    up

echo "writing ncps configuration..."
cat <<EOF > "$CONFIG_PATH"
cache:
  allow-delete-verb: true
  allow-put-verb: true
  hostname: "cache.local"
  database-url: "sqlite:$DB_PATH"
  secret-key-path: "$SECRET_KEY_PATH"
  sign-narinfo: true
  storage:
    local: "$STORAGE_DIR"
  upstream:
    urls:
      - https://cache.nixos.org
    public-keys:
      - cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=
server:
  addr: "127.0.0.1:$PORT"
EOF

echo "starting ncps on port $PORT..."
export CACHE_ALLOW_PUT_VERB=true
nix shell nixpkgs#ncps --command ncps serve --config "$CONFIG_PATH" &
NCPS_PID=$!
echo "$NCPS_PID" > "$PID_PATH"

# wait for connection
for i in {1..30}; do
    if curl -s "http://127.0.0.1:$PORT/nix-cache-info" > /dev/null; then
        echo "ncps is healthy."
        break
    fi
    sleep 0.5
    if ! kill -0 "$NCPS_PID" 2>/dev/null; then
        echo "ncps exited unexpectedly during startup."
        exit 1
    fi
done

cat <<EOF > "$ENV_PATH"
export CACHE_PUBKEY="$PUBKEY_VAL"
export CACHE_PORT="$PORT"
export CACHE_URL="http://127.0.0.1:$PORT"
export CACHE_UPLOAD_URL="http://127.0.0.1:$PORT/upload"
export CACHE_SECRET_KEY_PATH="$SECRET_KEY_PATH"
export NCPS_PID="$NCPS_PID"
export TEST_DIR="$TEST_DIR"
EOF

echo "cache server started successfully. source $ENV_PATH to use, and kill PID $NCPS_PID or check $PID_PATH to stop it."
