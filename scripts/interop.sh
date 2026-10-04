#!/usr/bin/env bash
# Run real-client interoperability suites against a local storlite.
#
#   scripts/interop.sh [ruby] [boto3] [awscli] [rails] [browser]   (default: all)
#
# Isolation: ambient AWS_* variables and config files are cleared, and every
# suite refuses any endpoint other than 127.0.0.1. Nothing contacts AWS.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${STORLITE_BIN:-$ROOT/target/debug/storlite}
VENV=${INTEROP_VENV:-$ROOT/.interop/venv}
SUITES=("$@")
[ ${#SUITES[@]} -eq 0 ] && SUITES=(ruby boto3 awscli rails browser)

for v in $(env | grep -o '^AWS_[A-Z0-9_]*' || true); do unset "$v"; done
export AWS_CONFIG_FILE=/dev/null AWS_SHARED_CREDENTIALS_FILE=/dev/null AWS_EC2_METADATA_DISABLED=true

free_port() { python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])'; }
PORT=$(free_port); MPORT=$(free_port)
WORK=$(mktemp -d "${TMPDIR:-/tmp}/storlite-interop.XXXXXX")
cleanup() { [ -n "${PID:-}" ] && kill "$PID" 2>/dev/null && wait "$PID" 2>/dev/null; rm -rf "$WORK"; }
trap cleanup EXIT

cat > "$WORK/config.toml" <<CFG
data_dir = "./data"
credentials_file = "./credentials.toml"
[http]
listen = "127.0.0.1:$PORT"
allow_insecure_loopback_http = true
[management]
listen = "127.0.0.1:$MPORT"
[limits]
min_disk_free_bytes = 67108864
min_disk_free_percent = 0
[logging]
format = "json"
level = "info"
CFG
cat > "$WORK/credentials.toml" <<CRED
[[credentials]]
id = "interop-admin"
secret_access_key = "interopsecretinteropsecretinterop01"
enabled = true
global_grants = ["admin"]
CRED
chmod 600 "$WORK/credentials.toml"
"$BIN" init --config "$WORK/config.toml" >/dev/null
"$BIN" serve --config "$WORK/config.toml" 2>"$WORK/serve.log" &
PID=$!
for _ in $(seq 1 100); do
  "$BIN" healthcheck --url "http://127.0.0.1:$MPORT/readyz" >/dev/null 2>&1 && break
  sleep 0.1
done

export STORLITE_ENDPOINT="http://127.0.0.1:$PORT"
export STORLITE_KEY_ID=interop-admin
export STORLITE_SECRET=interopsecretinteropsecretinterop01
export STORLITE_REGION=us-east-1
export INTEROP_WORK="$WORK"

status=0
for s in "${SUITES[@]}"; do
  echo "=== $s ==="
  case "$s" in
    ruby) ruby "$ROOT/tests/interop/ruby_sdk_test.rb" || status=1 ;;
    boto3) "$VENV/bin/python" "$ROOT/tests/interop/boto3_test.py" || status=1 ;;
    awscli) PATH="$VENV/bin:$PATH" bash "$ROOT/tests/interop/awscli_test.sh" || status=1 ;;
    rails) bash "$ROOT/tests/interop/rails_test.sh" || status=1 ;;
    browser) "$VENV/bin/python" "$ROOT/tests/interop/browser_test.py" || status=1 ;;
    *) echo "unknown suite $s"; status=1 ;;
  esac
done
echo "=== server errors (5xx) ==="
grep '"status":5' "$WORK/serve.log" | head -5 || echo "none"
exit $status
