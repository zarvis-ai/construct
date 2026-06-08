#!/usr/bin/env bash
# Quick end-to-end smoke test against a freshly built workspace.
#
# Spins up the daemon under an isolated $CONSTRUCT_*_DIR sandbox, exercises the
# IPC surface (ping / harnesses / create / list / show / send / stop), and
# tears down. Run from the workspace root:
#
#     cargo build --workspace && scripts/smoke.sh

set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
SANDBOX=${CONSTRUCT_SMOKE_DIR:-/tmp/construct-smoke}
rm -rf "$SANDBOX"
mkdir -p "$SANDBOX"/{state,data,config,runtime}

export CONSTRUCT_STATE_DIR="$SANDBOX/state"
export CONSTRUCT_DATA_DIR="$SANDBOX/data"
export CONSTRUCT_CONFIG_DIR="$SANDBOX/config"
export CONSTRUCT_RUNTIME_DIR="$SANDBOX/runtime"

CONSTRUCT_CLI="$ROOT/target/debug/construct"
[ -x "$CONSTRUCT_CLI" ]  || { echo "build first: cargo build --workspace" >&2; exit 1; }

"$CONSTRUCT_CLI" daemon run >"$SANDBOX/daemon.log" 2>&1 &
DAEMON_PID=$!
trap 'kill $DAEMON_PID 2>/dev/null || true' EXIT
sleep 0.4

echo "==> ping"
"$CONSTRUCT_CLI" ping

echo "==> harnesses"
"$CONSTRUCT_CLI" harnesses

echo "==> shell session"
SID=$("$CONSTRUCT_CLI" new shell "echo hello-from-shell; echo and-another-line" --cwd "$SANDBOX")
echo "  session: $SID"
sleep 0.6

echo "==> list"
"$CONSTRUCT_CLI" list

echo "==> show"
"$CONSTRUCT_CLI" show "$SID"

echo "==> stop (idempotent on done sessions)"
"$CONSTRUCT_CLI" stop "$SID" 2>/dev/null || true

echo "OK"
