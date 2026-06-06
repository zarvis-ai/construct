# 0022-operator-ambient-loop-runs-as-monitor-subagent

Status: accepted
Date: 2026-06-06
Area: harness
Scope: How the Operator's ambient loop scans the fleet and decides what reaches the Operator.

## Decision

The ambient loop ([0020](0020-operator-runs-ambiently.md)) no longer injects the fleet snapshot + previews ([0021](0021-operator-ambient-tick-carries-fleet-snapshot.md)) into the Operator's own conversation. Instead each tick runs a **one-shot monitor triage** — a separate, ideally cheaper completion — that judges the (data-only) snapshot + previews off the Operator's context and returns either a concise finding or "nothing". Only a finding becomes an Operator turn; a "nothing" tick never touches the Operator.

The monitor model is configurable via `AGENTD_OPERATOR_MONITOR_MODEL`. When unset, it **defaults to a cheaper tier on the same provider** as the Operator (so auth is already present): `codex-oauth` → `gpt-5.4-mini`, `openai` → `gpt-5-mini`, `anthropic` → `claude-sonnet-4-5`; already-small Operator models and providers without an obvious cheap tier (gemini/ollama/unknown) keep the Operator's model. A wrong model name resolves fine but 400s at call time (which would silently blind the monitor), so the resolved model is **health-checked once at startup** and falls back to the Operator's own model if it can't be resolved or doesn't answer.

## Reason

Run in the Operator's own session, the now-rich snapshot + previews accumulated in the Operator's persistent conversation: every tick (and every real user turn) ran near the budget ceiling on a frontier model, with stale per-tick snapshots crowding out real conversation and driving compaction. Splitting it makes the bulky, stale, every-5-minutes material live and die in a throwaway triage on a cheap model; the Operator carries *findings*, not *scans*, and only wakes when there's something. The monitor triages mechanically (no user-context), the Operator filters with context.

## Consequences

The Operator's context grows only on real findings (a couple of sentences each, with an evidence snippet + session id) — not on quiet ticks. The triage is a bounded, stateless call (snapshot + previews only), so its cost doesn't grow over time and is cheap on the default small model. The Operator's awareness of monitoring becomes structural (system prompt) plus the findings it receives, rather than a pile of no-op receipts. Triage liberality is a prompt dial: too eager pings the Operator for routine activity (cheap, filtered with `noted`); too conservative misses things. The small default model catches high-value "blocked/stuck" findings but is less exhaustive than the frontier model (e.g. it may miss subtler opportunities); override `AGENTD_OPERATOR_MONITOR_MODEL` for more thorough triage.

## Non-Goals

Not a managed subagent *session* (it's a one-shot completion, not a tracked fleet session); not agentic deep inspection (the triage judges the Rust-gathered previews, it doesn't fetch more); does not change the loop interval, the orchestrator-only gate, or the fleet-event observation pipeline.
