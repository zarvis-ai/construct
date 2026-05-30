# AGENTS.md

## Development workflow

All code changes go through a branch, worktree, and PR — no exceptions.

- **Branch + worktree + PR for every change.** Create a fresh branch off latest `main`, materialize it as a git worktree under `.claude/worktrees/<branch-name>`, make changes there, and open a PR. The top-level checkout at `~/agentd` stays on `main` — never edit files there directly.
- **No direct push to `main`.** Changes land on `main` only via a merged PR.
- **Release process lives in [`docs/RELEASING.md`](docs/RELEASING.md).** Use that guide for versioned releases and publishing prebuilt binaries.
- **No `Co-Authored-By: Claude` trailer in commits.** Don't append model attribution to commit messages. `Co-authored-by:` for other humans is fine.
- **Clean up after merge.** Remove the worktree (`git worktree remove <path>`), delete the local branch (`git branch -d <name>`), and delete the remote branch (e.g. via GitHub's "delete branch after merge", or `git push <remote> --delete <name>`).
- **After merge, update and build the main worktree.** Once a PR is merged and the feature worktree is cleaned up, switch to the top-level checkout (`~/agentd`), pull latest `main`, and run `cargo build` there (debug profile). This keeps the user's main worktree binaries current so `/agents restart` can pick up the latest merged `agent` / `agentd` changes, especially when operating from a remote-control session. Report the updated main-worktree debug binary paths when relevant.
- **When the change is testable, build all binaries in the worktree and report paths in the agent response.** Run `cargo build` inside the worktree (debug profile — much faster to iterate on than release; the binaries live under `.claude/worktrees/<branch>/target/debug/`), then print the absolute path of every binary the workspace produces — `agent`, `agentd`, `agentd-mcp`, and every `agentd-adapter-*` — in the agent response so the user can copy and run them. Explicitly call out *which* binary the PR's code lives in so the user can run the right one without grepping the diff (e.g. "this PR only touches `crates/cli` → relevant binary is `agent`; the others are built but unchanged from main").
- **Record a video / screenshot when it helps the reviewer, and post accessible artifacts on the PR.** This is a judgment call:
  - Sometimes only an "after" recording makes sense (a brand-new pane / popup / view that didn't exist before).
  - Sometimes a before/after pair is needed (a tweak to an existing render: a color, a fade rate, a layout shift).
  - Sometimes neither is useful (refactors, internal API changes, daemon logic with no user-visible surface).
  Use [Recording the TUI](#recording-the-tui) below for the mechanics. Report local artifact paths in the agent response so the user can open them; if posting on the PR, attach or upload the actual media so reviewers can access it. **When unsure which of the three applies, ask the user before recording.**

## Recording the TUI

Use [vhs](https://github.com/charmbracelet/vhs) to capture deterministic mp4 / gif clips of the TUI without needing a desktop session or screen-recording permissions. The notes below are the ones we wish we'd had on the first attempt.

- **Build the worktree's binaries.** vhs records whatever `agent` you point it at, so make sure the worktree has been built (`cargo build` per the workflow above) before recording. For a before/after pair, prepare two worktrees so each side has its own binaries — never re-record `before` from a tree that already has the change applied.
- **Isolated daemon.** Run vhs against a fresh `AGENTD_RUNTIME_DIR` / `AGENTD_STATE_DIR` / `AGENTD_DATA_DIR` / `AGENTD_CONFIG_DIR` under `/tmp/` so it doesn't collide with the user's running daemon. Each recording gets its own dir and its own daemon process; tear them down at the end.
- **Put the TUI in a state that actually shows your change.** This part varies most by change — pick whichever shape fits:
  - **Specific harness features** (a zarvis tool, codex output rendering, claude resume, …): spawn that harness with a representative prompt, e.g. `agent new zarvis "<task>"`. Use a prompt whose output exercises the diff (tool calls if you changed tool rendering, long messages if you changed wrapping, etc.).
  - **Minibuffer / keymap / popup / palette**: send the keystrokes from inside the vhs tape with `Type`, `Ctrl+X`, `Enter`, `Sleep`, etc. — no extra sessions needed if the feature is reachable from a stock TUI.
  - **Session-list / modeline / matrix rain / anything driven by fleet activity**: spawn 2–4 sessions producing ambient activity. The most robust pattern is `agent new shell ""` (interactive shell) followed by `agent send <id> "<command>"` pushing a noise loop into each. *Don't* pass the loop as the `new shell` prompt — both bash and zsh observed to fall back to interactive mode under PTY and never actually run `-lc <cmd>`, leaving the daemon silent.
  - **Single-session views** (transcript, scrollback, diff): spawn one session, then trigger the view via tape keystrokes (`C-x z` for zoom, mouse-wheel events, etc.).
  Whatever the shape, give the daemon a few seconds to settle (`sleep 3`) after setup so the first frames the tape captures aren't a half-loaded UI.
- **Inherit env into vhs, don't `Env` it.** vhs's `Env` directive splits on whitespace, so a `PATH` with colons errors out and writing one `Env` per variable is fragile. Export the env in the outer shell that invokes `vhs`; the spawned `ttyd` / `bash` inside the tape inherit it. Inside the tape, type the absolute path of the worktree's `agent` binary instead of relying on `PATH`.
- **Quote every string in the tape.** vhs 0.11+ requires quoted values for `Output`, `Env`, etc. Unquoted paths are parsed as command tokens and fail.
- **Same script for both sides (when recording a pair).** Wrap the recording in one shell driver invoked as `record.sh before` and `record.sh after` so the only difference between runs is which worktree's binaries are on `PATH`. Reference recipe (matrix-rain change — adapt the activity, sleeps, and output names to your case):

  ```bash
  #!/usr/bin/env bash
  set -euo pipefail
  VARIANT="${1:?usage: record.sh before|after}"
  BIN_DIR="/Users/you/agentd/.claude/worktrees/rain-${VARIANT}/target/debug"
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

  For a single "after"-only recording, drop the `VARIANT` argument and the second invocation.
- **Verify before posting.** Extract a midpoint frame from each video (`ffmpeg -ss 00:00:15 -i out.mp4 -vframes 1 mid.png`) and confirm the TUI rendered the change you're trying to show. If the first attempt missed the moment (the rain panel was idle, a popup was open, etc.) re-record — don't ship a video that doesn't actually demo the diff.
- **Attach to the PR.** Drop the file(s) into the PR with `gh pr comment <n> --body "..."` (or upload via the web UI). For a pair, label them `before` and `after` and link the commit each was recorded from.

## Hot-reloading the web UI (dev only)

`crates/daemon/assets/index.html` + `static/*` are `include_str!`/`include_bytes!`'d into the daemon, so a naive edit needs a recompile + restart + browser reconnect. To skip all that, point a **running** debug daemon at a worktree's assets directory and it serves them from disk, with a live-reload poller injected (browser auto-refreshes on save):

- **From a dev session (any worktree):** call the `webui_hot_reload` MCP tool with `dir: "<worktree>/crates/daemon/assets"` (debug builds only). `dir: null` reverts to the embedded assets.
- **At boot:** `AGENTD_ASSETS_DIR=<dir>` (honored in debug builds only).
- **Programmatically:** the `dev.set_assets` IPC method / `client.dev_set_assets()`.

Edit `index.html` → save → the browser reloads itself (the injected poller watches `/dev/version`, a combined mtime of the served files). No rebuild, no daemon restart. Release builds ignore all of this and always serve the embedded, tamper-proof assets.

## Design specs

Use `specs/` for durable design decisions that future agents must preserve while changing agentd. These are normative records of intent and constraints, not code navigation notes.

- **Add or update a spec when a change creates, changes, or depends on a key design decision.** This includes architecture, abstraction boundaries, UX semantics, persistence behavior, protocols/events, harness contracts, operational conventions, and cross-client behavior. Do not add a spec for routine implementation details or one-off bug fixes unless the fix establishes a reusable rule.
- **Keep each spec focused on one decision.** Prefer many small files over a broad document that mixes unrelated rules.
- **Name specs with a stable sequence and slug.** Use `specs/NNNN-title-kebab-case.md`. If `specs/` does not exist yet, create it in the same PR as the first spec.
- **Document intent, not current code.** Avoid file paths, function names, line numbers, or implementation anchors that will go stale. Agents should inspect the current codebase separately after reading the spec.
- **Update existing specs when behavior changes.** Do not leave conflicting design records in place. Mark old decisions as `superseded` or edit them when the decision itself has evolved.

Use this format:

```md
# NNNN-title-kebab-case

Status: accepted | proposed | superseded | deprecated
Date: YYYY-MM-DD
Area: architecture | protocol | ux | persistence | harness | cli | tui | webui | convention
Scope: one sentence

## Decision

The rule or design choice that should remain true.

## Reason

Why this decision exists: user need, system constraint, failure mode, tradeoff, or product intent.

## Consequences

What future changes must preserve, what tradeoffs are accepted, and what this decision makes harder.

## Non-Goals

Optional. Boundaries that prevent overgeneralizing the decision.

## Examples

Optional. Concrete behavior examples without relying on current file names or function names.
```

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
