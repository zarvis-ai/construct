# 0091-smith-meta-model-api

Status: accepted
Date: 2026-07-13
Area: harness
Scope: Smith exposes Meta's Model API as an explicit provider and supports it throughout model selection and agent execution.

## Decision

Smith recognizes `meta:<model>` as a first-class provider backed by Meta's Responses API. The provider authenticates with `META_API_KEY`, falling back to Meta's example-compatible `MODEL_API_KEY`, and defaults to `meta:muse-spark-1.1` when either key is the first available credential in Smith's auto-detection order.

Smith sends `store: false` because it owns and replays the session history locally; Meta must not retain an additional server-side response history on Smith's behalf.

The provider must support streaming text, parallel function calls, function-call result replay, usage accounting, incomplete responses, context-overflow learning, named endpoint profiles, model completion, and credential status reporting. Meta remains a distinct provider instead of being represented as OpenAI because Meta exposes Responses semantics while Smith's `openai:` path intentionally uses Chat Completions.

## Reason

Selecting a provider identifies its endpoint, wire protocol, credential, and billing path. Treating Meta as an OpenAI-compatible base URL would route it through Smith's incompatible Chat Completions implementation and would make the selected provider misleading in persisted model state and UI status.

Muse Spark 1.1 advertises a one-million-token context window, so Smith starts with that input budget and retains its existing provider-error learning behavior if the service applies a lower limit.

## Consequences

- Users can start Smith with `--model meta:muse-spark-1.1`, pin the same value in configuration, select it from `/model`, or select the detected Meta key in `/configure`.
- A bare `muse-spark-1.1` remains an Ollama model name; changing endpoint or billing path requires the explicit `meta:` prefix unless credential auto-detection selected Meta.
- Other Meta Model API models can use the same explicit provider prefix without a Smith release, subject to compatible Responses semantics.

## Non-Goals

- Meta-hosted search, media inputs, and other provider-native tools are not exposed by this initial integration.
- Reasoning traces are not surfaced or persisted unless Meta exposes a replayable and user-visible reasoning format.
