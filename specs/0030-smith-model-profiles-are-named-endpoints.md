# 0030-smith-model-profiles-are-named-endpoints

Status: accepted
Date: 2026-06-17
Area: harness
Scope: Smith user-defined model endpoints declared in config and switched at runtime.

## Decision

Smith supports user-defined **model profiles**: named entries in the shared
`config.toml` that each bind a wire protocol (OpenAI / Anthropic / Gemini /
Ollama / Grok) to its own base URL, credential, and default model. A profile is
referenced with an explicit `@<name>` prefix (optionally `@<name>:<model>` to
override the model), usable anywhere a model spec is accepted — `--model`,
`CONSTRUCT_SMITH_MODEL`, and the `/model` slash command.

Profiles exist so that multiple distinct endpoints — including several
OpenAI-compatible vendors plus the first-party API — can coexist in a single
session and be switched at runtime, which the single per-protocol base-URL env
var (`OPENAI_BASE_URL`, etc.) cannot express.

## Reason

The per-protocol base-URL env vars bind exactly one endpoint per wire protocol
for the life of the process. A user who wants to flip between, say, real OpenAI
and an OpenAI-compatible vendor mid-session cannot, because both would read the
same `OPENAI_BASE_URL`. Named profiles lift that limit while keeping each
endpoint's URL and credential declared in one place.

## Consequences

- The `@` prefix is reserved for profile references and must never be produced
  by bare-name provider sniffing. Switching endpoint/billing path stays an
  explicit act, consistent with [0028](0028-smith-oauth-providers-are-explicit.md).
- A profile's underlying wire protocol remains the key for context-window
  heuristics and learned token limits; the `@name` is only a user-facing label.
  Internal keying must not switch to the label.
- OAuth-backed providers (`codex-oauth`, `claude-oauth`, `grok-oauth`) are not
  expressible as profiles: they have no base-URL/credential surface and keep
  their explicit prefixes.
  Declaring one in a profile is a configuration error.
- Credentials should be referenced indirectly (an env var name) rather than
  written inline, though inline is accepted. When neither is given, the wire
  protocol's standard env var(s) are the fallback.
- The set of endpoints a session can reach is now config-driven, so behavior
  can differ between machines with different `config.toml` files. Resolution
  failures (missing profile, missing key, unknown provider) must report
  actionable errors rather than silently falling back to a different endpoint.

## Non-Goals

This does not add profiles for OAuth/subscription providers, does not change
bare-name or explicit-prefix routing, and does not make any profile a default —
a profile is used only when explicitly named.

## Examples

`config.toml`:

```toml
[smith.models.deepseek]
provider    = "openai"
base_url    = "https://api.deepseek.com/v1"
api_key_env = "DEEPSEEK_API_KEY"
model       = "deepseek-chat"

[smith.models.groq-llama]
provider    = "openai"
base_url    = "https://api.groq.com/openai/v1"
api_key_env = "GROQ_API_KEY"
model       = "llama-3.3-70b-versatile"
```

In one session: `/model openai:gpt-5` reaches first-party OpenAI, then
`/model @deepseek` reaches DeepSeek, then `/model @groq-llama:llama-3.1-8b-instant`
reaches Groq with a one-off model override — no restart, no env changes.
