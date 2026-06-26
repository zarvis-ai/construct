#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WT="$(cd "$SCRIPT_DIR/.." && pwd)"
BIN_DIR="$WT/target/debug"
CLIENT_BIN="$BIN_DIR/construct"

if [[ ! -x "$CLIENT_BIN" ]]; then
  echo "missing $CLIENT_BIN; run: cargo build" >&2
  exit 1
fi

if [[ -n "${CONSTRUCT_TEST_DIR:-}" ]]; then
  DEMO_DIR="$CONSTRUCT_TEST_DIR"
else
  WT_NAME="$(basename "$WT")"
  SAFE_WT_NAME="$(printf '%s' "$WT_NAME" | tr -c 'A-Za-z0-9_.-' '-')"
  DEMO_DIR="/tmp/construct-test-${SAFE_WT_NAME}"
fi
export CONSTRUCT_RUNTIME_DIR="$DEMO_DIR/run"
export CONSTRUCT_STATE_DIR="$DEMO_DIR/state"
export CONSTRUCT_DATA_DIR="$DEMO_DIR/data"
export CONSTRUCT_CONFIG_DIR="$DEMO_DIR/config"
export CONSTRUCT_SHELL_BIN="${CONSTRUCT_SHELL_BIN:-/bin/bash}"
export BASH_SILENCE_DEPRECATION_WARNING="${BASH_SILENCE_DEPRECATION_WARNING:-1}"
export CONSTRUCT_REMOTE_NO_TUNNEL=1
export PATH="$BIN_DIR:$PATH"

if ! "$CLIENT_BIN" ping >/dev/null 2>&1; then
  "$SCRIPT_DIR/test_agentd.sh" >/dev/null
fi

if [[ $# -eq 0 ]]; then
  echo "isolated construct test env: $DEMO_DIR" >&2
  echo "try: $0 list" >&2
  echo "try: $0 canvas templates" >&2
fi

exec "$CLIENT_BIN" "$@"
