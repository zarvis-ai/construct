#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WT="$(cd "$SCRIPT_DIR/.." && pwd)"
BIN_DIR="$WT/target/debug"
CLIENT_BIN="$BIN_DIR/construct"

if [[ ! -x "$CLIENT_BIN" ]]; then
  echo "missing $BIN_DIR binaries; run: cargo build" >&2
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

LOG="$DEMO_DIR/construct.log"
PID_FILE="$DEMO_DIR/construct.pid"
SEED_FILE="$DEMO_DIR/.seeded"

seed_sessions() {
  if [[ "${CONSTRUCT_TEST_SEED:-1}" == "0" || -e "$SEED_FILE" ]]; then
    return
  fi

  echo "seeding sessions..."
  local sid

  sid="$($CLIENT_BIN new --title "welcome shell" --no-tui shell "" | tr -d '[:space:]')"
  if [[ -n "$sid" ]]; then
    $CLIENT_BIN send "$sid" "printf 'Welcome to isolated construct test env\\nworktree: %s\\n\\n' '$WT'; pwd; ls scripts; echo ready" >/dev/null || true
  fi

  sid="$($CLIENT_BIN new --title "activity shell" --no-tui shell "" | tr -d '[:space:]')"
  if [[ -n "$sid" ]]; then
    $CLIENT_BIN send "$sid" 'for i in 1 2 3 4; do echo "[$i] reading files, running checks"; sleep 0.5; done; echo "activity complete"' >/dev/null || true
  fi

  sid="$($CLIENT_BIN new --title "canvas board" --no-tui shell "" | tr -d '[:space:]')"
  if [[ -n "$sid" ]]; then
    $CLIENT_BIN canvas set "$sid" --template kanban >/dev/null || true
    $CLIENT_BIN canvas get "$sid" >/dev/null 2>&1 || true
    $CLIENT_BIN send "$sid" "echo 'Canvas session ready. Try: construct canvas get $sid'" >/dev/null || true
  fi

  sid="$($CLIENT_BIN new --title "browser preview prompt" --mode headless --no-tui smith "Use browser_open to open https://example.com with preview true, then summarize what changed in the UI." | tr -d '[:space:]' || true)"
  if [[ -n "$sid" ]]; then
    :
  fi

  touch "$SEED_FILE"
}

if [[ -S "$CONSTRUCT_RUNTIME_DIR/construct.sock" ]] && "$CLIENT_BIN" ping >/dev/null 2>&1; then
  seed_sessions
  echo "construct already running for test env"
  echo "dir: $DEMO_DIR"
  echo "pid: $(cat "$PID_FILE" 2>/dev/null || echo unknown)"
  echo "log: $LOG"
  exit 0
fi

if [[ "${CONSTRUCT_TEST_KEEP:-0}" != "1" ]]; then
  rm -rf "$DEMO_DIR"
fi
mkdir -p "$DEMO_DIR/run" "$DEMO_DIR/state" "$DEMO_DIR/data" "$DEMO_DIR/config"

"$CLIENT_BIN" daemon run >"$LOG" 2>&1 &
PID=$!
echo "$PID" >"$PID_FILE"

for _ in $(seq 1 100); do
  if "$CLIENT_BIN" ping >/dev/null 2>&1; then
    seed_sessions
    echo "construct pid=$PID"
    echo "dir: $DEMO_DIR"
    echo "log: $LOG"
    echo "export CONSTRUCT_TEST_DIR=$DEMO_DIR"
    exit 0
  fi
  if ! kill -0 "$PID" 2>/dev/null; then
    echo "construct exited while starting; log follows:" >&2
    cat "$LOG" >&2 || true
    exit 1
  fi
  sleep 0.2
done

echo "construct did not become ready; log follows:" >&2
cat "$LOG" >&2 || true
exit 1
