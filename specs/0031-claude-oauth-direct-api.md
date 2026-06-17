# 0031-claude-oauth-direct-api

Status: accepted
Date: 2026-06-17
Area: harness
Scope: The Smith `claude-oauth` model provider.

## Decision

The Smith `claude-oauth` provider calls the Anthropic Messages API
(`/v1/messages`) directly, authenticating with the user's Claude Code
subscription OAuth access token, and passes Smith's tools as native `tools`. It
shares its request/stream wire with the API-key `anthropic` provider. It does
not drive the `claude` CLI.

## Reason

The previous transport ran `claude -p` with the built-in tools disabled and a
JSON-schema structured-output shim, asking the model to return Smith tool calls
as structured output. Empirically the CLI ran its own multi-turn agent loop in
which the model tried to use a disabled/partial tool surface and then bailed
without emitting any Smith tool call roughly two-thirds of the time, at tens of
internal model turns and on the order of a dollar per Smith turn. Passing
Smith's tools as native `tools` over the same subscription auth makes the model
emit real `tool_use` blocks that Smith executes in its own agent loop, which
fixes both the reliability and the cost/turn-count problems and keeps the
provider on the standard one-turn `complete` abstraction every other provider
uses.

## Consequences

- Credentials are read from the Claude Code credential store (a macOS keychain
  generic password, or a `~/.claude/.credentials.json` file); a token refresh
  writes the rotated tokens back to the same store.
- Requests carry an OAuth beta header and a system prompt whose first block is
  the Claude Code identity. That header, the identity requirement, and the
  token refresh endpoint/client id are reverse-engineered from the Claude Code
  client and may change without notice.
- This is NOT the subscription-use surface Anthropic documents (`claude -p` or
  the Agent SDK). It is the user's own subscription on their own machine; treat
  enabling it as a user risk decision, not a sanctioned integration.
- The API-key `anthropic` provider and the subscription `claude-oauth` provider
  now share one Messages-API wire; changes to that shared wire affect both.

## Non-Goals

Does not change how providers are selected (still an explicit `claude-oauth:`
prefix, per 0028), the other model providers, or the separate `claude` wrapper
harness — which remains the documented way to use Claude Code with its own
tools.
