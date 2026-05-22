#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WT="$(cd "$SCRIPT_DIR/.." && pwd)"
BIN_DIR="$WT/target/debug"
AGENT_BIN="$BIN_DIR/agent"

if [[ ! -x "$AGENT_BIN" ]]; then
  echo "missing $AGENT_BIN; run: cargo build" >&2
  exit 1
fi

if [[ -n "${AGENTD_TEST_DIR:-}" ]]; then
  DEMO_DIR="$AGENTD_TEST_DIR"
else
  WT_NAME="$(basename "$WT")"
  SAFE_WT_NAME="$(printf '%s' "$WT_NAME" | tr -c 'A-Za-z0-9_.-' '-')"
  DEMO_DIR="/tmp/agentd-test-${SAFE_WT_NAME}"
fi
export AGENTD_RUNTIME_DIR="$DEMO_DIR/run"
export AGENTD_STATE_DIR="$DEMO_DIR/state"
export AGENTD_DATA_DIR="$DEMO_DIR/data"
export AGENTD_CONFIG_DIR="$DEMO_DIR/config"
export AGENTD_SHELL_BIN="${AGENTD_SHELL_BIN:-/bin/bash}"
export PATH="$BIN_DIR:$PATH"

if ! "$AGENT_BIN" ping >/dev/null 2>&1; then
  "$SCRIPT_DIR/test_agentd.sh" >/dev/null
fi

exec "$AGENT_BIN" "$@"
