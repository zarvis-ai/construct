# AGENTS.md

## Development workflow

All code changes go through a branch, worktree, and PR — no exceptions.

- **Branch + worktree + PR for every change.** Create a fresh branch off latest `main`, materialize it as a git worktree under `.claude/worktrees/<branch-name>`, make changes there, and open a PR. The top-level checkout at `~/agentd` stays on `main` — never edit files there directly.
- **No direct push to `main`.** Changes land on `main` only via a merged PR.
- **No `Co-Authored-By: Claude` trailer in commits.** Don't append model attribution to commit messages. `Co-authored-by:` for other humans is fine.
- **Clean up after merge.** Remove the worktree (`git worktree remove <path>`), delete the local branch (`git branch -d <name>`), and delete the remote branch (e.g. via GitHub's "delete branch after merge", or `git push <remote> --delete <name>`).
- **When the change is testable, build all binaries in the worktree and report paths.** Run `cargo build --release` inside the worktree (so the binaries live under `.claude/worktrees/<branch>/target/release/`), then print the absolute path of every binary the workspace produces — `agent`, `agentd`, `agentd-mcp`, and every `agentd-adapter-*`. Explicitly call out *which* binary the PR's code lives in so the user can run the right one without grepping the diff (e.g. "this PR only touches `crates/cli` → relevant binary is `agent`; the others are built but unchanged from main").
- **Produce a before/after recording or screenshot when it makes sense, and attach it to the PR.** For visible TUI / rendering / UX changes, follow the procedure in [Producing before/after recordings](#producing-beforeafter-recordings) below and post the artifact (gif or mp4) as a PR comment so reviewers can see the effect without rebuilding. For non-visual changes (refactors, internal API tweaks, daemon-only logic with no user-visible surface) skip it. **If unsure whether a change qualifies, ask the user before recording.**

## Producing before/after recordings

For changes that alter what the user sees in the TUI, record a deterministic before/after pair with [vhs](https://github.com/charmbracelet/vhs) and post both to the PR.

- **Two worktrees, one per side.** Create one worktree off the merge base (or the last `main` commit before the change) and another off the PR's tip — never re-record `before` from a working tree that has the change applied. Build `--release` in each so the binaries don't change behaviour between sides.
- **Isolated daemon.** Run vhs against a fresh `AGENTD_RUNTIME_DIR` / `AGENTD_STATE_DIR` / `AGENTD_DATA_DIR` / `AGENTD_CONFIG_DIR` under `/tmp/` so it doesn't collide with the user's running daemon. Each variant gets its own dir and its own daemon process; tear them down at the end.
- **Drive realistic activity.** If the change depends on session activity (matrix rain, modeline, list ordering, …), create a handful of interactive shell sessions via `agent new shell ""` and then push a noise loop into each with `agent send <id> "<cmd>"`. Don't pass the loop as the `new shell` prompt — under PTY, both bash and zsh observed-fall back to interactive mode and never actually run `-lc <cmd>`, leaving the daemon silent.
- **Inherit env into vhs, don't `Env` it.** vhs's `Env` directive splits on whitespace, so a `PATH` with colons errors out and writing one `Env` per variable is fragile. Export the env in the outer shell that invokes `vhs`; the spawned `ttyd` / `bash` inside the tape inherit it. Inside the tape, type the absolute path of the worktree's `agent` binary instead of relying on `PATH`.
- **Quote every string in the tape.** vhs 0.11+ requires quoted values for `Output`, `Env`, etc. Unquoted paths are parsed as command tokens and fail.
- **Same script for both sides.** Wrap the recording in one shell driver invoked as `record.sh before` and `record.sh after` so the only difference between runs is which worktree's binaries are on `PATH`. Reference recipe (this is what we used for the matrix-rain reveal change):

  ```bash
  #!/usr/bin/env bash
  set -euo pipefail
  VARIANT="${1:?usage: record.sh before|after}"
  BIN_DIR="/Users/you/agentd/.claude/worktrees/rain-${VARIANT}/target/release"
  DEMO_DIR="/tmp/agentd-rain-demo-${VARIANT}"
  TAPE="/tmp/rain-${VARIANT}.tape"
  OUTPUT="/tmp/rain-${VARIANT}.mp4"

  rm -rf "$DEMO_DIR"
  mkdir -p "$DEMO_DIR/run" "$DEMO_DIR/state" "$DEMO_DIR/data" "$DEMO_DIR/config"

  export AGENTD_RUNTIME_DIR="$DEMO_DIR/run"
  export AGENTD_STATE_DIR="$DEMO_DIR/state"
  export AGENTD_DATA_DIR="$DEMO_DIR/data"
  export AGENTD_CONFIG_DIR="$DEMO_DIR/config"
  export AGENTD_SHELL_BIN="/bin/bash"        # adapter discovery
  export PATH="$BIN_DIR:$PATH"

  "$BIN_DIR/agentd" run >"/tmp/rain-${VARIANT}-daemon.log" 2>&1 &
  DAEMON_PID=$!
  trap 'kill $DAEMON_PID 2>/dev/null || true; wait $DAEMON_PID 2>/dev/null || true' EXIT
  for _ in $(seq 1 50); do "$BIN_DIR/agent" ping >/dev/null 2>&1 && break; sleep 0.2; done

  SESSION_IDS=()
  for _ in 1 2 3; do
    SID=$("$BIN_DIR/agent" new shell "" | tr -d '[:space:]')
    [[ -n "$SID" ]] && SESSION_IDS+=("$SID")
  done
  sleep 1
  NOISE='while true; do printf "Editing src/main.rs Reading tests/foo.rs Running tests\n"; sleep 0.3; done'
  for SID in "${SESSION_IDS[@]}"; do "$BIN_DIR/agent" send "$SID" "$NOISE"; done
  sleep 3   # let intensity ramp + the reveal queue prime

  cat >"$TAPE" <<TAPE_EOF
  Output "${OUTPUT}"
  Set FontSize 14
  Set Width 1600
  Set Height 800
  Set TypingSpeed 30ms

  Type "${BIN_DIR}/agent"
  Enter
  Sleep 30s
  Ctrl+X
  Ctrl+C
  Sleep 500ms
  TAPE_EOF

  vhs "$TAPE"
  ```

- **Verify before posting.** Extract a midpoint frame from each video (`ffmpeg -ss 00:00:15 -i out.mp4 -vframes 1 mid.png`) and confirm the TUI rendered the change you're trying to show. If the first attempt missed the moment (the rain panel was idle, a popup was open, etc.) re-record — don't ship a video that doesn't actually demo the diff.
- **Attach to the PR.** Drop both files into the PR with `gh pr comment <n> --body "..."` (or upload via the web UI). Label the artifacts `before` and `after` and link the commit each was recorded from.

## The minibuffer is just another session

Most TUIs make the bottom command bar a special UI primitive. We don't — it's a regular zarvis session, persisted on disk like any other. Differences:

- **Hidden from the list.** `kind: SessionKind::Orchestrator` filters it out of `list_items`.
- **Auto-created.** `SessionManager::ensure_orchestrator()` runs at daemon start.
- **Rendered in the bottom strip.** Same `ItemHistory::replay` pipeline as the main view, just a different Rect.
- **Specialized system prompt.** Zarvis branches on `AGENTD_SESSION_KIND` to act as the fleet dispatcher instead of a worker.
- **Subscribes to fleet events.** A second IPC connection turns other sessions' `Status{AwaitingInput|Errored|Done}` and `ToolApprovalRequest` into `OBSERVATION:` messages the orchestrator can react to.
- **Approvals render inline in the PTY.** No global minibuffer preempt — the panel *is* the PTY.

Everything else — slash commands, tool-block expand/collapse, input queue during turns, persistence across daemon restart, automode, resume — works identically to any zarvis session, *because the minibuffer is one*. Add minibuffer features as session features.

## Rendering across resize and restart

- **Resize is instant.** No full-history replay. Older content keeps its original line wraps; new content uses the new width.
- **History survives daemon restart.** When a harness can resume silently, the prior scrollback stays visible. When a harness must repaint itself on resume, the daemon hands it a clean slate instead — partial reuse leaves the terminal half-rendered.
- **Sessions come back at the size the user last had.** A resumed session must render at the user's current dimensions on the very first frame, not at a creation-time default.
