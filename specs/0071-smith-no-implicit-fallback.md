# 0071-smith-no-implicit-fallback

Status: accepted
Date: 2026-07-06
Area: harness
Scope: What smith's model auto-detect ladder does when it finds no usable direct-API-key credential, for both the main agent loop and auto-title generation.

## Decision

When smith is started with no explicit model (no `--model`, no `CONSTRUCT_SMITH_MODEL`) and none of `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, or `GEMINI_API_KEY`/`GOOGLE_API_KEY` is set, smith fails to start with a curated error rather than silently defaulting to a local Ollama server. The same applies to the separate, lighter auto-title ladder (`construct-adapter-smith --title-mode`): it now returns an error instead of guessing Ollama, and the daemon's existing best-effort caller already treats that as "leave the title unset" — no behavior change needed on the caller side.

OAuth-subscription providers (`claude-oauth:`, `codex-oauth:`, `grok-oauth:`) and Ollama remain fully supported — they just require an explicit `<prefix>:<model>` spec, via `--model`, `CONSTRUCT_SMITH_MODEL`, an `@profile`, or picking that method in the `/configure` dialog's smith-auth tab (which pins `CONSTRUCT_SMITH_MODEL` for you — see [[0070-smith-model-pin-persistence]]). None of those paths are guessed automatically when no model is specified at all.

The curated startup error message points at `/configure` in the construct TUI (or `M-x configure`) as the discoverability path for seeing every auth method smith supports and how to set each one up.

**Orchestrator exception.** An ordinary user session still fails hard at start exactly as above. The minibuffer's smith session (`CONSTRUCT_SESSION_KIND=orchestrator`) does not: it starts its REPL regardless of whether a model resolved, because slash commands — including the fleet-dispatch ones the orchestrator exists for, e.g. `/construct restart` — never touch the provider at all. The curated error still fires immediately as a visible message in the orchestrator's panel; it just doesn't take the whole panel down with it. The first real turn that actually needs a model (plain text, not a slash command) re-attempts resolution at that point — so exporting a key or picking a method in `/configure` recovers the *same* orchestrator session without needing a restart, unlike the smith-auth pin in [[0070-smith-model-pin-persistence]] which does need one.

## Reason

The previous ladder's last rung silently built an `ollama:llama3.1` spec whenever no API key was found, regardless of whether a local Ollama server was actually running. A zero-config machine got a session that looked healthy at start (no error, no warning) and then died mid-turn with a raw transport error the first time the agent tried to call the provider — a confusing failure mode disconnected from its real cause (no credential configured at all). Failing at session start instead surfaces the same underlying problem immediately, with a message that names what's missing and how to fix it, instead of deferring the failure to an unrelated-looking point in the conversation.

## Consequences

- A session-start failure from this path is not itself a Program/session-list-degrading event beyond the ordinary `Errored` state a startup error already produces — no new failure-handling machinery was needed, only a different failure trigger and a better message.
- Any future rung added to the ladder must be an explicit, deliberate choice about what "auto-detect" should guess — silently falling through to a network-dependent default is exactly the failure mode this decision closes off.
- Existing explicit Ollama users (`--model ollama:<name>`, `CONSTRUCT_SMITH_MODEL=ollama:<name>`, an `@profile` pointing at `provider = "ollama"`) are unaffected — this only changes what happens when no model is specified at all.
- The orchestrator exception is scoped to the main conversational turn only — the ambient fleet monitor's periodic background tick does not itself re-attempt resolution or fire the curated error; it silently produces no finding until either the main turn's lazy re-resolve succeeds or the session restarts. This is a deliberate scope boundary, not an oversight: the monitor is a background convenience, and re-resolving on every tick would add complexity for a path with no user waiting on it.
- A future session kind or client surface that is similarly slash-capable and prompt-optional (i.e. useful without ever needing a model call) should follow the same exception rather than inventing a new one — the distinguishing property is "can this surface do useful work with zero model calls," not "is this specifically the orchestrator."

## Non-Goals

- This does not add OAuth subscriptions or Ollama-reachability to the *auto-detect* ladder itself — it deliberately keeps auto-detect scoped to direct API keys only, since those are the only credential kind that can be checked with a cheap, side-effect-free presence test at session-start time consistent with the ladder's existing three rungs.
- This does not change the daemon's harness-availability probe (spec 0068), which already reports smith as `available` when an OAuth subscription or reachable Ollama exists, even though the auto-detect ladder alone wouldn't pick either without an explicit prefix. That asymmetry is pre-existing and out of scope here — the probe answers "could a session start via *some* explicit choice," not "would auto-detect guess this."

## Examples

- No API keys, no `CONSTRUCT_SMITH_MODEL`, no Ollama running: `construct new smith "..."` immediately errors with a message naming the missing credentials and pointing at `/configure`, instead of the session appearing to start and then failing on the first turn.
- `CONSTRUCT_SMITH_MODEL=ollama:llama3.1` set explicitly, no Ollama server running: unaffected by this change — the session still starts (an explicit choice was made) and still fails at the first request the same way it always has when the server isn't reachable.
- A machine with only a Claude Code subscription (no `ANTHROPIC_API_KEY`) and no pin: auto-detect alone does not find it and errors; picking "Claude subscription" in `/configure` (which writes `CONSTRUCT_SMITH_MODEL = "claude-oauth:claude-sonnet-4-6"`) resolves it after a daemon restart.
- A completely keyless machine boots the TUI: the orchestrator (minibuffer) session still comes up and shows the curated "no model configured" message, but `/construct restart`, `/loop`'s ToolCall-routed variants, and other slash commands keep working from it. The user exports `ANTHROPIC_API_KEY` in the daemon's shell and restarts the daemon; the *next* daemon-respawned orchestrator resolves normally. If instead they type a plain (non-slash) message to the still-running orchestrator without restarting anything, smith re-attempts resolution right then — if the daemon's own process env now has the key (e.g. it was already set but the daemon predates it), that same session recovers without a restart.
