# 0070-smith-model-pin-persistence

Status: accepted
Date: 2026-07-06
Area: persistence
Scope: Where and how picking a smith auth method in the `/configure` dialog is persisted, and who owns the write.

## Decision

Picking an auth method on the `/configure` dialog's smith-auth tab (see [[0069-configure-dialog]]) persists it as smith's default model by writing `CONSTRUCT_SMITH_MODEL = "<prefix>:<default-model>"` under `[adapters.smith.env]` in the daemon's `config.toml` — the same key documented in that file's own comments for manual editing. Picking "Auto-detect" clears the key instead of writing one, restoring smith's ordinary auto-detect ladder (see [[0071-smith-no-implicit-fallback]]).

The daemon owns this write, exposed as an IPC method the TUI calls — the TUI never edits `config.toml` directly. The write is a format-preserving edit (not a wholesale rewrite): every other table, key, and comment already in the file is left untouched, since a user may have hand-written unrelated configuration (adapter overrides, model profiles, program template paths) in the same file.

The write takes effect for sessions started after it lands — it does not reach into already-running smith adapters (their env was fixed at spawn time). The IPC call's result always carries a note saying so, and the dialog always surfaces that note after a pick; there is no attempt to pretend the change applied live.

Selection in the dialog is guidance-first: picking a method whose credential is missing still persists the pin (the user may set up the credential afterward, before restarting the daemon), but the dialog shows how to obtain/set that credential rather than accepting a typed secret. There is no API-key or token text-entry field anywhere in this flow.

## Reason

`config.toml` is user-owned and frequently hand-edited (adapter overrides, named model profiles, program template directories) — a client-side wholesale rewrite would risk silently discarding content the user typed that the TUI doesn't know how to round-trip. Routing the write through the daemon keeps a single owner of the file and lets the edit be minimal and format-preserving instead of re-serializing the whole document from a partial in-memory model. Making the "restart required" caveat unavoidable (always shown, never silently skipped) matches the general principle that a probe or a config write is a point-in-time action, not a live guarantee — the same posture spec 0068 takes for availability probes.

## Consequences

- Any daemon IPC method that edits `config.toml` going forward should follow the same shape: a targeted, format-preserving edit scoped to the specific key(s) it owns, not a full-document rewrite.
- The TUI must not cache a smith-auth pick as if it were already active for running sessions — anywhere it surfaces "current" selection, that reflects config state, not live adapter state.
- Because there is no secret-entry UI, a user whose desired method needs a credential that requires interactive login (subscription OAuth) or an env var still needs to leave the TUI (or its host shell) to complete that step themselves.

## Non-Goals

- This does not add a general-purpose config.toml editor IPC method — the write is scoped specifically to `[adapters.smith.env] CONSTRUCT_SMITH_MODEL`.
- This does not change how `[adapters.smith.env]` or any other adapter env table is consumed at session-spawn time — only how one specific key within it gets written by a client action.

## Examples

- A user with a hand-written `[smith.models.deepseek]` profile block and unrelated comments opens `/configure`, picks "Anthropic API key": the resulting `config.toml` gains (or updates) `[adapters.smith.env] CONSTRUCT_SMITH_MODEL = "anthropic:claude-opus-4-8"` with the `[smith.models.deepseek]` block and every comment elsewhere in the file byte-for-byte unchanged.
- A user picks "Codex subscription" before ever running `codex login`: the pin is written immediately, the dialog shows "run `codex login` ... then restart the daemon" as guidance, and the session-start error message (if they start a smith session without restarting) still fires with the curated credential-missing message, not a raw transport error.
