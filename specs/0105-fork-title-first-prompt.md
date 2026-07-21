# 0105-fork-title-first-prompt

Status: accepted
Date: 2026-07-08
Area: ux
Scope: How provisional fork titles transition after the fork receives user input.

## Decision

A newly forked session initially displays a provisional title derived from its source, prefixed with `(fork)`. The first substantive user prompt triggers the same automatic title generation used for a new untitled session, and a successful generated title replaces that provisional title.

The provisional state is durable across daemon restarts. Any explicit user rename before automatic generation completes opts the session out, including clearing the title; automatic generation must not overwrite that user choice.

Leading slash commands continue to defer generation until the first non-command prompt, following the normal session auto-title rules.

## Reason

The inherited title preserves context while an empty fork is easy to identify, but it stops describing the branch once the user gives it a new purpose. Treating it as a provisional system label provides useful naming in both phases without clobbering user intent.

## Consequences

Clients creating lineage forks must use the normal fork relationship. The daemon derives and persists provisional-title eligibility from that relationship, replaces only provisional or absent titles, and clears eligibility on every manual title update.

## Examples

- Forking `Investigate cache misses` initially shows `(fork) Investigate cache misses`; entering `Compare Redis and in-memory latency` generates a new title from that prompt.
- Renaming the empty fork to `Redis comparison` before sending its first prompt preserves `Redis comparison`.
- Entering `/model sonnet` first keeps the provisional title; the next ordinary prompt triggers generation using the accumulated prompt context.
