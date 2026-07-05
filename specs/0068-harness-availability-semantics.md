# 0068-harness-availability-semantics

Status: accepted
Date: 2026-07-05
Area: harness
Scope: What "available" means per harness kind, and the contract clients can rely on when reading it.

## Decision

`HarnessInfo.available` reports whether a *new session under that harness would actually be able to start*, not whether the adapter's own launcher binary exists. The daemon computes it per harness kind:

- **`shell`**: always available. It has no external dependency beyond the daemon itself.
- **Wrapper adapters** (`claude`, `codex`, `antigravity`, `grok`): available when the underlying agent CLI resolves — honoring the harness's `CONSTRUCT_<H>_CMD` / `CONSTRUCT_<H>_BIN` override, then a PATH lookup of the resulting binary name, exactly as the adapter itself resolves the command it spawns at session-start time. The adapter's own `binary` config field (which names the AHP wrapper process — normally `construct` itself) is irrelevant to this check.
- **`smith`**: available when at least one credential path smith's own provider selection could pick up exists: an explicit `CONSTRUCT_SMITH_MODEL` pin, a direct-API key for a supported provider, an OAuth subscription credential (Claude Code, Codex, Grok), or a reachable local Ollama server. This is an existence check only — it does not validate that a found credential is unexpired or unrevoked; that failure mode still surfaces at session-start time, same as before this change.
- **Community adapters** (anything registered via `[adapters.<name>]` that isn't one of the above): available when the adapter's configured `binary` resolves. There is no protocol-level way to ask an arbitrary AHP adapter what CLI or credential it depends on, so this preserves the daemon's original (pre-probing) semantics as the sane default for third-party adapters.

Every `HarnessInfo` also carries `detail: Option<String>` — a short, human-readable reason for the boolean, e.g. `"ready"`, `"ready (Claude subscription)"`, `` "`claude` CLI not found on daemon PATH" ``, or `"no API key or OAuth credential found"`. `available` is the field clients branch on; `detail` is always present alongside it (never populated in isolation) and exists so a client can tell a user *why* without re-deriving the probe logic itself. Clients must treat `detail` as opaque, unstructured text — not parse it for a machine-readable reason code.

Probes must stay cheap and effectively non-blocking on the daemon's request path:

- File-existence and environment-variable checks (credential files, API-key env vars, `CONSTRUCT_SMITH_MODEL`) run on every call — they're stat/getenv calls, not worth caching.
- Checks that shell out or hit the network (macOS keychain read for Claude Code OAuth, Ollama TCP reachability) are cached for a short TTL and bounded by a short timeout, so a slow or firewalled endpoint can never make a harness-list request hang or a hot path stall waiting on a subprocess.

## Reason

Every built-in wrapper adapter's `binary` config field is `"construct"` — the daemon's own wrapper process, which is definitionally always present since it's the binary currently answering the request. Checking that field told a user nothing about whether `claude`, `codex`, `agy`, or `grok` were actually installed, or whether smith had any usable credential. `available` was always `true` for every built-in harness, and the picker's dimmed/struck-through rendering for "unavailable" harnesses — which already existed in the UI — had no real signal to ever act on. Users only discovered a missing CLI or missing credential after creating a session and watching it fail to spawn.

Scoping the probe to "would a session actually start" (rather than "does some file exist") means the signal stays trustworthy across the picker, the welcome card, and the CLI without each surface inventing its own notion of readiness.

## Consequences

- `HarnessInfo` gains `detail: Option<String>`, serde-defaulted so an older daemon's response (missing the field) still deserializes on a newer client, and a newer daemon talking to code that hasn't been updated to read it degrades to just the boolean.
- The TUI's harness picker (`C-x C-f`), its hover tooltip, its unavailable-click status-line message, and `construct harnesses`' CLI output all read from the same probe result — there is exactly one place (`agentd`'s availability probing) that decides whether a harness is usable, not one heuristic per surface.
- The welcome card's live-status section is a rendering of the same data, refreshed periodically while it's on screen, so installing a CLI or exporting an API key while the TUI is already running is reflected without a restart.
- A probe is a point-in-time snapshot. A harness reported available can still fail to start (token expired between the probe and the spawn, Ollama server stopped) — clients must keep handling session-start failures as before; the probe narrows how often that happens, it doesn't eliminate the failure path.

## Non-Goals

- This does not validate that a found credential is still valid (token expiry, revoked API key) — only that it exists. Token refresh/validation remains the adapter's job at session-start.
- This does not change what harnesses are registered or how `[adapters.<name>]` config works — only how their availability is computed and reported.
- This does not add per-harness capability negotiation beyond the existing `Capabilities` struct; `detail` is a human-readable string, not a new structured field for clients to branch on.

## Examples

- No `claude` binary on PATH, no `CONSTRUCT_CLAUDE_CMD`/`CONSTRUCT_CLAUDE_BIN` override set: `claude` reports `available: false`, `` detail: "`claude` CLI not found on daemon PATH" ``. The picker renders `claude` dimmed and struck through; clicking it sets the status line to that same detail text instead of a generic "not installed" message.
- `ANTHROPIC_API_KEY` is set in the daemon's environment and no other smith credential is configured: `smith` reports `available: true`, `detail: "ready (Anthropic API key)"`.
- No API keys, no OAuth credentials, no reachable Ollama server: `smith` reports `available: false`, `detail: "no API key or OAuth credential found"`.
- A user installs `codex` while the TUI is already running with no session selected (welcome card showing). Within the card's refresh interval, `codex`'s line flips from its "unavailable" state to `"ready"` without the user restarting the TUI.
