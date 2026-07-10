# 0079-harness-native-session-id-tracks-clear

Status: accepted
Date: 2026-07-10
Area: harness
Scope: Construct must keep the harness-native conversation id current when the child clears or branches context.

## Decision

For interactive harnesses that persist a native conversation id for daemon resume and same-harness fork (Claude's `claude_session_id.txt`, Codex's `codex_session_id.txt`, Antigravity's `agy_conversation_id.txt`, Grok's `grok_session_id.txt`), construct must treat that id as **mutable over the life of the construct session**.

When the harness starts a fresh native conversation while the adapter process is still alive — Claude `/clear` / `/branch` / in-session `/resume`, Codex `/clear` / `/new`, and equivalent clear/new flows in Antigravity and Grok — construct updates the stored native id to the new conversation and rebinds any native-transcript watcher to it.

On daemon restart and on same-harness fork, construct uses the **current** native id (post-clear), not the id from the original spawn.

## Reason

These harnesses mint a new native session id when the user clears context or branches, while leaving the prior conversation resumable under the old id. Construct previously captured the id once at first spawn (or first log/rollout/dir match) and never rewrote it. After `/clear`, resume and same-harness fork therefore reattached to the stale, pre-clear conversation — or to an empty/wrong transcript path — instead of the active one the user is looking at.

## Consequences

- Claude adapters inject a `SessionStart` hook that rewrites the native id file whenever Claude reports a new `session_id` (including the `clear` source).
- Codex adapters keep scanning originator-tagged rollouts and adopt the newest matching rollout when `/clear` or `/new` creates one.
- Antigravity adapters re-parse `--log-file` for the **last** `Created conversation <uuid>` and rebind when it changes.
- Grok adapters keep selecting the newest session directory under the session cwd (by mtime) and rebind when that id changes.
- Transcript watchers rebind to the new native transcript after an id change so chat mode follows the active conversation.
- Headless multi-turn adapters adopt the latest observed native id per turn, not only the first.

## Non-Goals

- Changing construct's own session ids (`s…` fleet ids).
- Mapping one construct session onto multiple simultaneous native conversations (e.g. keeping both pre- and post-clear ids live). Only the active native id is tracked.
- Implementing a user-facing "resume the pre-clear conversation" UI; the old native transcripts remain on disk under the harness's own storage.
- Perfect isolation when multiple Grok sessions share one cwd (Grok has no originator tag; newest-mtime is best-effort).

## Examples

1. User runs Claude under construct, works, then types `/clear`. Construct updates `claude_session_id.txt`. Daemon restart resumes the post-clear conversation; same-harness fork uses `--fork-session` from the post-clear id.
2. User runs Codex under construct, then `/new`. A new originator-tagged rollout appears; construct overwrites `codex_session_id.txt` with the new uuid and resume uses that uuid.
3. User runs Antigravity under construct, then clears context. A second `Created conversation` line is appended to the session log; construct updates `agy_conversation_id.txt` to that uuid.
4. User runs Grok under construct, then starts a fresh chat. A new session directory appears under `~/.grok/sessions/<cwd>/`; construct updates `grok_session_id.txt` to the newest uuid.
