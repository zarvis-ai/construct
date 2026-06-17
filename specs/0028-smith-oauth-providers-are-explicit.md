# 0028-smith-oauth-providers-are-explicit

Status: accepted
Date: 2026-06-16
Area: harness
Scope: Smith providers that draw from subscription-backed OAuth credentials.

## Decision

Smith subscription-backed providers must be selected with explicit model
prefixes such as `codex-oauth:`, `claude-oauth:`, or `grok-oauth:`. Bare
model names keep using their direct API provider routes.

## Reason

OAuth-backed subscription usage has different authentication, billing, limits,
and operational failure modes than direct API-key usage. A model string like
`claude-*` or `gpt-*` should not silently switch the user's billing path.

## Consequences

Future Smith OAuth providers need their own explicit prefixes and provider
labels. Provider implementations may delegate credential handling to official
local CLIs, or read those CLIs' stored credentials and call the API directly
(see 0031 for the `claude-oauth` direct-API transport), but Smith still owns
tool execution, approval semantics, and conversation persistence unless a user
intentionally chooses a separate CLI harness.

## Non-Goals

This does not make OAuth providers the default when their local CLIs are logged
in, and it does not change the separate `claude` or `codex` wrapper harnesses.
