# 0015-approval-modes

Status: accepted
Date: 2026-06-05
Area: harness
Scope: Risky tool calls are governed by explicit per-session approval modes rather than a boolean automode.

## Decision

Sessions that support tool approval use one of three approval modes:

- `manual`: Safe tools run immediately; Risky tools ask the user.
- `auto_review`: Safe tools run immediately; Risky tools are reviewed by a specialized approval prompt that may approve, deny, or ask the user.
- `unsafe_auto`: Safe and Risky tools run without asking the user.

Approval prompts expose `a` for `auto_review` and `f` for `unsafe_auto`. `unsafe_auto` is intentionally named to make the risk explicit.

When a session renders its own inline approval prompt, clients should not also open a global minibuffer approval prompt for the same request. The user's approval keystrokes should go to the session that asked for approval, and other sessions should not lose input focus because a background session needs a decision.

When an inline approval prompt changes the session's future approval mode, the adapter must report that change back to the daemon so all clients render the current mode from the shared session summary.

## Reason

A boolean automode conflated two different needs: high-throughput trusted operation and model-mediated review. Naming the modes separately lets users choose a guarded middle ground without hiding the risk of fully bypassing approvals.

## Consequences

Clients should present approval mode as a session mode and use `unsafe_auto` terminology instead of generic “automode.” Adapters that do not gate tools may ignore approval-mode changes. `auto_review` is not a security boundary; it is a convenience review layer that must fall back to asking the user when uncertain. The reviewer prompt should encourage approving bounded routine development work, including ordinary file edits inside the active git worktree, while still asking the user for broad, ambiguous, unrelated, outside-worktree, or sensitive actions.

Interactive clients should keep the selected tool-gating session's current mode visible and allow direct mode changes from that status surface when the UI has one. Cycling order is `manual` → `auto_review` → `unsafe_auto` → `manual`.

One approval decision applies to the whole pending tool call (including every hunk of a batched edit). The prompt conveys this through the call summary rather than the action labels: for batched edit calls the summary should include affected file paths and edit-level hints rather than only aggregate counts. Action labels stay simple verbs (`approve` / `deny` / `auto-review`) — not `approve all` / `deny all`, which read as approving or denying all *future* calls (the role of `unsafe_auto`) rather than the parts of the current one.

Fleet observers and operator/minibuffer surfaces should not duplicate inline approval prompts as proactive observations. The requesting session remains the canonical interaction surface for that approval.

## Non-Goals

This does not make model approval equivalent to user approval for high-risk or ambiguous operations. It also does not require non-tool-gating harnesses to implement approval modes.
