# Adding a harness (and auditing an existing one)

This is the developer-facing checklist for integrating a new harness into
construct, and for re-auditing an existing harness after its upstream CLI
gains features. The user-facing overview lives in [harnesses.md](harnesses.md).

[PR #840](https://github.com/construct-worlds/construct/pull/840) (OpenCode)
is a good worked example of a complete integration — its file list is
essentially section 1 of this document.

Two principles run through everything below:

- **Report what the harness states; never fabricate.** Token splits, context
  windows, model names, dollar costs: emit them when the harness exposes them
  on some surface, and emit nothing when it doesn't. A missing gauge is
  honest; a guessed one poisons trust in every real number next to it.
- **Exactly-once semantics survive restarts.** Anything an adapter re-reads
  from a harness's own files (transcripts, rollouts, wire logs) must not
  re-emit history the daemon already persisted — use line cursors, delta
  baselines, and id dedupe, and seed them correctly on resume.

## 1. Wiring checklist (the mechanical part)

Every new harness touches roughly the same files:

| Where | What |
| --- | --- |
| `crates/adapter-<name>/` | New adapter crate. Entry point is `run()` calling `construct_protocol::adapter::run` with an `InitializeResult` (name, version, `Capabilities`). |
| `Cargo.toml` (workspace) | Add the crate as a member and CLI dependency. |
| `crates/cli/src/main.rs` | Register the hidden `__adapter <name>` subcommand the daemon spawns. |
| `crates/daemon/src/config.rs` | `BUILTIN_ADAPTERS` entry (`binary: "construct"`, `args: ["__adapter", "<name>"]`), plus a `default_usage_probe` entry if the harness has a usage panel (spec 0086). |
| `crates/daemon/src/availability.rs` | Availability probe: `probe_wrapper_cli(CMD env, BIN env, default binary)`, plus auth-file / login checks when detectable (see the codex/grok probes). Semantics in spec 0068. |
| `crates/daemon/src/session.rs` | `native_id_file_name()` entry if the harness supports native resume/fork (see §2.4). |
| `crates/cli/src/app/configure.rs` | First-run configure picker entry. |
| `crates/client/src/lib.rs` | Fork-session wiring if the harness needs custom fork flags. |
| `docs/harnesses.md`, `docs/configuration.md` | User-facing tables: harness row, mode support, `CONSTRUCT_<NAME>_CMD` / `_BIN` knobs. |
| `specs/NNNN-<name>-native-session-tracking.md` | A spec recording how native ids, resume, and reset are tracked for this harness (pattern: specs 0091 OpenCode, 0102 Kimi). |

Conventions every adapter follows:

- **Command override envs**: `CONSTRUCT_<NAME>_CMD` (full command prefix) and
  `CONSTRUCT_<NAME>_BIN` (binary only), resolved via
  `adapter::resolve_command_override`, with a home-dir fallback for standard
  installer locations where one exists.
- **Session data dir**: all adapter bookkeeping files (native id files,
  captured model files, usage files, injected settings) live under
  `CONSTRUCT_SESSION_DATA_DIR`.
- **stderr**: pipe the child's stderr through `spawn_stderr_log` so failures
  are diagnosable from daemon logs.
- **Env hygiene**: the daemon scrubs `CONSTRUCT_*` from child environments so
  a session spawned from inside another construct session doesn't inherit
  `CONSTRUCT_RESUME=1` etc. Don't reintroduce leaks.

## 2. Behavioral checklist (the actual contract)

Each item names the protocol surface, the rule, and reference adapters worth
copying. Not every harness can implement every item — the point is to make
each gap a *known, recorded* gap (see the matrix in §3), not an accident.

### 2.1 Modes: interactive PTY and/or headless

- Interactive: spawn the native TUI under construct's PTY
  (`adapter::pty::run_session`), declare `supports_pty`.
- Headless: emit structured events per turn (`drive_turn` in
  `adapter-common`).
- Mode resolution: explicit `--mode`/`CONSTRUCT_<NAME>_MODE` wins; otherwise
  interactive iff the client supplied a PTY size (see `resolve_mode` in
  adapter-claude). Interactive-only harnesses (opencode, kimi) simply ignore
  the headless request and document it.
- Initial prompt: pass as a CLI arg where the harness accepts one without
  leaving TUI mode; otherwise type it into the PTY as two separate writes
  (text, then Enter — bracketed paste swallows a combined chunk; see
  adapter-kimi). Oversize prompts go to a file with a short pointer arg
  (adapter-claude).

### 2.2 Structured transcript events (chat-mode fidelity)

Emit `Message` / `Reasoning` / `ToolUse` / `ToolResult` from whatever
structured surface the harness has, in interactive mode too — usually by
tailing the harness's own on-disk transcript with a line cursor:

- claude: native `~/.claude/projects/**/<session>.jsonl` watcher.
- codex: rollout watcher over `~/.codex/sessions/**` (originator-tagged
  discovery, spec 0088-adjacent).
- kimi: `wire.jsonl` watcher.
- opencode: injected plugin writing capture files the adapter polls.

Rules: skip already-persisted history on resume (cursor seeded from line
count); rebind + restart the cursor when the native id changes; never let
bookkeeping output (token footers, status lines) leak into the transcript as
assistant prose.

### 2.3 Model and effort reporting

- `SessionEvent::ModelChanged` whenever the model actually changes —
  including mid-session switches through the harness's own picker. Verify
  against real transcripts that the field you parse is *live*, not frozen at
  session start (codex's `turn_context.model` is live; grok's `model_id`
  turned out to be frozen — that investigation note lives in
  adapter-codex).
- `SessionEvent::EffortChanged` where the harness has a reasoning-effort
  concept (codex, kimi, grok, antigravity do today).

### 2.4 Native session id: capture, resume, reset, fork

- **Capture**: persist the harness's native conversation id to
  `<CONSTRUCT_SESSION_DATA_DIR>/<name>_session_id.txt` and register that
  filename in the daemon's `native_id_file_name()`. Capture must track the
  *live* id across `/clear`, `/new`, compact — via hooks (claude's
  SessionStart hook), plugins (opencode), or file discovery (codex, grok).
- **Resume**: on `CONSTRUCT_RESUME=1`, relaunch with the harness's resume
  flag (`--resume <id>`, `--session <id>`, …) and skip already-seen
  transcript history.
- **Reset detection** (specs 0079/0085): when the native id changes
  mid-session, emit `SessionEvent::NativeIdChanged` — the daemon synthesizes
  the archived reset-snapshot fork and lineage edge. Suppress the one
  expected rebind a fresh fork produces (placeholder id → real id) so it
  isn't recorded as a reset.
- **Native fork** (spec 0031/0078): when the harness can fork a conversation
  (`--fork-session`, fork commands), same-harness construct forks should use
  it so the fork has real model memory. Registered via the same
  `native_id_file_name` machinery.
- **Resume rendering**: declare `supports_silent_resume` only if the adapter
  emits no startup escapes on resume (today: smith only). Otherwise the
  daemon clears the PTY ring so the harness repaints cleanly.

### 2.5 Token usage (spec 0103)

Emit `SessionEvent::Cost` per model call with the full split when available:
`tokens_in` = whole prompt side (fresh input + cache reads + cache writes),
`tokens_cached` = cache-read subset (`tokens_cached ⊆ tokens_in`),
`tokens_out` = completion (+ reasoning where the harness splits it out).
Declare `supports_cost`.

Exactly-once patterns, by data shape:

- **Per-message usage repeated across records** (claude: one API message
  spans several transcript records repeating the same usage) → dedupe by
  message id.
- **Cumulative totals** (codex `token_count.total_token_usage`) → emit
  deltas against a running baseline; seed the baseline from the file's last
  snapshot on resume so history isn't re-reported.
- **One record per call** (kimi `step.end`) → emit directly; the line cursor
  is the dedupe.
- **Cumulative capture file** (opencode plugin) → poll + delta; a shrinking
  total means the writer restarted — rebase silently.

An unsplit total (a harness that only reports one figure) goes in
`tokens_in` with zero out/cached; clients render it as "total". Never
estimate tokens for harnesses that report nothing.

### 2.6 Context-usage gauge (spec 0104)

Emit `SessionEvent::ContextUsage` with `used_tokens` = the prompt side of
the most recent call (what actually filled the window) and `window_tokens`
only when the harness itself states the window (codex's
`model_context_window`, grok's `contextWindowTokens`). Report on change, not
per poll. Never guess the window from a model-name table outside the
harness's own report.

### 2.7 Dollar cost

`Cost.usd` when the harness prices calls itself (smith; opencode stores
per-message `cost`; claude's headless `result` carries `total_cost_usd`).
Subscription harnesses legitimately have none.

### 2.8 MCP tool injection (unified tool layer, specs 0011/0036/0095)

Inject construct's MCP server so the agent can drive the fleet, via the
harness's own config mechanism: `--mcp-config` file (claude), CLI args
(codex), config-content merge through the injected plugin (opencode).
Always honor `CONSTRUCT_INJECT_MCP=0`.

### 2.9 Approval policy translation

Translate `adapter::policy::AutoApprovePolicy::from_env()` (which carries
`CONSTRUCT_AUTO_APPROVE_PATHS` etc.) into the harness's native mechanism
where one exists — e.g. claude's `--allowed-tools` patterns. Where the CLI
exposes no such control, the gap is acceptable and documented (the upstream
CLI keeps its own approval UX inside the PTY).

### 2.10 Native subagent mirrors (spec 0079)

If the harness has its own delegation/subagent mechanism, mirror children as
read-only `(native)` rows: `NativeSubagent` / `NativeSubagentSnapshot` /
`NativeSubagentRemoved` events with deterministic per-child ordinals
(`next_native_seq`) so re-scans never duplicate. Claude, codex, grok, and
antigravity do this today.

### 2.11 State detection

`Status`/`AwaitingInput`/`Done` from structured events where possible. Pure
PTY harnesses rely on daemon quiescence heuristics — set
`detect_prompt_via_pgroup` appropriately in the `PtySpec` (full-screen TUIs
hold the foreground group; one-shot commands don't).

### 2.12 Usage-quota probe (spec 0086)

If the harness has an interactive usage/status panel (`/usage`, `/status`),
add its command to `default_usage_probe` so `usage.query` can scrape
subscription quota. This is *quota*, distinct from the per-session context
gauge (§2.6).

### 2.13 Free capabilities (nothing to implement)

Widgets (`CONSTRUCT_SESSION_WIDGETS_DIR`), session env context
(`CONSTRUCT_SESSION_ID`, parent id, data dirs), transcript persistence,
resume-at-last-size PTY behavior, and lineage rendering all come from the
daemon once the events above are emitted.

## 3. Capability matrix

Snapshot of where each harness stands (2026-07-21). Re-audit by checking the
listed data surface, not by trusting this table — upstream CLIs grow
surfaces between releases (codex's token splits and grok's context figures
both existed for months before we consumed them).

| Capability | smith | claude | codex | opencode | kimi | grok | antigravity | shell |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Interactive / headless | both | both | both | interactive | interactive | both | both | PTY |
| Structured chat events | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | n/a |
| ModelChanged | ✓ | ✓ | ✓ | ✓ | ✓ | ✓¹ | ✓ | n/a |
| EffortChanged | ✓ | — | ✓ | — | ✓ | ✓ | ✓ | n/a |
| Token split (0103) | ✓ | ✓ | ✓ | ✓ | ✓ | gap² | none³ | n/a |
| Context gauge (0104) | planned | in flight | in flight | in flight | in flight | planned² | none³ | n/a |
| USD cost | ✓ | headless only | — | gap⁴ | — | — | — | n/a |
| Native resume | ✓ (own state) | ✓ | ✓ | ✓ | ✓ | ✓ | — | fresh shell |
| Reset detection (0085) | n/a | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | n/a |
| Native fork | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | — | n/a |
| Native subagent mirrors | n/a⁵ | ✓ | ✓ | — | — | ✓ | ✓ | n/a |
| MCP injection | native tools | ✓ | ✓ | ✓ | gap | gap | gap | — |
| Approval translation | native | ✓ | — | — | — | ✓ | — | n/a |
| Usage probe (0086) | disabled | `/usage` | `/status` | — | — | `/usage show` | `/usage` | disabled |

¹ grok reports the model, but its upstream `model_id` was observed frozen
per session — a mid-session switch may go unreported (investigation note in
adapter-codex's `codex_model_change` doc).
² grok's session files expose `contextTokensUsed`/`contextWindowTokens`
(exact context gauge) and a cumulative unsplit `totalTokens` — real data,
not yet consumed.
³ antigravity exposes no token/cost data in print mode or its logs
(checked; documented in the adapter).
⁴ opencode stores exact per-message USD `cost` in the same event the plugin
already reads — a one-line add.
⁵ smith subagents are real construct sessions (spec 0014), not mirrors.

## 4. Where each harness's data lives

The fastest way to audit a gap is to look at the harness's own files after a
short real session:

| Harness | Surface | Notable fields |
| --- | --- | --- |
| claude | `~/.claude/projects/<cwd-slug>/<session>.jsonl` | per-record `message.usage` (incl. cache read/creation, service tier), `model`, `stop_reason`, file-history snapshots, git branch |
| codex | `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` | `event_msg`/`token_count` (full split + `model_context_window` + live rate limits/plan), `turn_context` (model, effort, sandbox policy) |
| kimi | `~/.kimi-code/sessions/<wd>/<session>/agents/*/wire.jsonl` | `step.end` usage (full split) + per-step latency metrics, `config.update` (model, effort), `usage.record` per turn |
| opencode | `~/.local/share/opencode/opencode.db` (sqlite `message`) — reachable live via the plugin's `message.updated` events | `tokens` (input/output/reasoning/cache r+w), exact `cost` USD, provider/model, per-message timestamps |
| grok | `~/.grok/sessions/<cwd-enc>/<session>/` (`signals.json`, `updates.jsonl`, `chat_history.jsonl`) | `contextTokensUsed`/`contextWindowTokens`, `totalTokens` (unsplit), turn/tool counters, session summary |
| antigravity | — | nothing usable found in print mode or logs |

## 5. Verification checklist

- Unit tests against **real** transcript/rollout samples, not invented
  shapes — and say so in the test comment ("verified against N real rollouts
  on this machine" is the house style; see adapter-codex).
- Exactly-once tests: replay the same record twice, resume mid-file, rebind
  after reset — no double counting.
- `cargo test --workspace` (unfiltered — shared helpers live outside the
  harness's naming pattern).
- Drive the real thing end-to-end with the `/verify` flow (isolated daemon,
  fresh `CONSTRUCT_*_DIR`s) and confirm events arrive in the transcript:
  `construct new <name> "<prompt that exercises the change>"`.
- Update the matrix in §3 and the user-facing tables in
  [harnesses.md](harnesses.md) / [configuration.md](configuration.md).
- Record a TUI clip when the change is user-visible (AGENTS.md, "Recording
  the TUI").
