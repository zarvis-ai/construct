# 0015-approval-modes

Status: accepted
Date: 2026-05-31
Area: harness
Scope: Risky tool calls are governed by explicit per-session approval modes rather than a boolean automode.

## Decision

Sessions that support tool approval use one of three approval modes:

- `manual`: Safe tools run immediately; Risky tools ask the user.
- `auto_review`: Safe tools run immediately; Risky tools are reviewed by a specialized approval prompt that may approve, deny, or ask the user.
- `unsafe_auto`: Safe and Risky tools run without asking the user.

Approval prompts expose `a` for `auto_review` and `f` for `unsafe_auto`. `unsafe_auto` is intentionally named to make the risk explicit.

## Reason

A boolean automode conflated two different needs: high-throughput trusted operation and model-mediated review. Naming the modes separately lets users choose a guarded middle ground without hiding the risk of fully bypassing approvals.

## Consequences

Clients should present approval mode as a session mode and use `unsafe_auto` terminology instead of generic “automode.” Adapters that do not gate tools may ignore approval-mode changes. `auto_review` is not a security boundary; it is a convenience review layer that must fall back to asking the user when uncertain.

## Non-Goals

This does not make model approval equivalent to user approval for high-risk or ambiguous operations. It also does not require non-tool-gating harnesses to implement approval modes.
