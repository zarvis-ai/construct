#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WT="$(cd "$SCRIPT_DIR/.." && pwd)"
BIN_DIR="$WT/target/debug"
AGENTD_BIN="$BIN_DIR/agentd"
AGENT_BIN="$BIN_DIR/agent"

if [[ ! -x "$AGENTD_BIN" || ! -x "$AGENT_BIN" ]]; then
  echo "missing $BIN_DIR binaries; run: cargo build" >&2
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

LOG="$DEMO_DIR/agentd.log"
PID_FILE="$DEMO_DIR/agentd.pid"
SEED_FILE="$DEMO_DIR/.seeded"

seed_sessions() {
  if [[ "${AGENTD_TEST_SEED:-1}" == "0" || -e "$SEED_FILE" ]]; then
    return
  fi

  echo "seeding sessions..."
  local sid

  sid="$($AGENT_BIN new --title "welcome shell" shell "" | tr -d '[:space:]')"
  if [[ -n "$sid" ]]; then
    $AGENT_BIN send "$sid" "printf 'Welcome to isolated agentd test env\\nworktree: %s\\n\\n' '$WT'; pwd; ls scripts; echo ready" >/dev/null || true
  fi

  sid="$($AGENT_BIN new --title "activity shell" shell "" | tr -d '[:space:]')"
  if [[ -n "$sid" ]]; then
    $AGENT_BIN send "$sid" 'for i in 1 2 3 4; do echo "[$i] reading files, running checks"; sleep 0.5; done; echo "activity complete"' >/dev/null || true
  fi

  sid="$($AGENT_BIN new --title "browser preview prompt" --mode headless zarvis "Use browser_open to open https://example.com with preview true, then summarize what changed in the UI." | tr -d '[:space:]' || true)"
  if [[ -n "$sid" ]]; then
    :
  fi

  touch "$SEED_FILE"
}

if [[ -S "$AGENTD_RUNTIME_DIR/agentd.sock" ]] && "$AGENT_BIN" ping >/dev/null 2>&1; then
  seed_sessions
  echo "agentd already running for test env"
  echo "dir: $DEMO_DIR"
  echo "pid: $(cat "$PID_FILE" 2>/dev/null || echo unknown)"
  echo "log: $LOG"
  exit 0
fi

if [[ "${AGENTD_TEST_KEEP:-0}" != "1" ]]; then
  rm -rf "$DEMO_DIR"
fi
mkdir -p "$DEMO_DIR/run" "$DEMO_DIR/state" "$DEMO_DIR/data" "$DEMO_DIR/config"

"$AGENTD_BIN" run >"$LOG" 2>&1 &
PID=$!
echo "$PID" >"$PID_FILE"

for _ in $(seq 1 100); do
  if "$AGENT_BIN" ping >/dev/null 2>&1; then
    seed_sessions
    echo "agentd pid=$PID"
    echo "dir: $DEMO_DIR"
    echo "log: $LOG"
    echo "export AGENTD_TEST_DIR=$DEMO_DIR"
    exit 0
  fi
  if ! kill -0 "$PID" 2>/dev/null; then
    echo "agentd exited while starting; log follows:" >&2
    cat "$LOG" >&2 || true
    exit 1
  fi
  sleep 0.2
done

echo "agentd did not become ready; log follows:" >&2
cat "$LOG" >&2 || true
exit 1
