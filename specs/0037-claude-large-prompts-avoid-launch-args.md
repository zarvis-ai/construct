# 0037-claude-large-prompts-avoid-launch-args

Status: accepted
Date: 2026-06-23
Area: harness
Scope: Initial prompts passed to interactive Claude sessions.

## Decision

Interactive Claude sessions must not pass oversized initial prompts directly as terminal launch arguments. When an initial prompt is too large for reliable launch metadata, construct stores it in session-local storage and launches Claude with a short instruction that points at that file.

## Reason

Claude validates terminal launch metadata and rejects individual launch-argument entries over its size limit. Forked sessions can produce large seeded prompts from prior transcript context, so passing the seed as a single CLI argument can prevent the session from starting.

## Consequences

Forks keep their full seeded context without violating Claude's launch metadata limits. Future Claude adapter changes must keep launch arguments small and use session-local files, stdin, or another non-argument transport for large context.

## Non-Goals

This does not change how headless Claude sessions receive prompts, nor does it impose a global seed limit on non-Claude harnesses.
