# 0020-operator-runs-ambiently

Status: accepted
Date: 2026-06-06
Area: harness
Scope: Defines the Operator's autonomous ambient loop behavior.

## Decision

The daemon-owned Operator session may run an ambient loop while idle. The loop injects a synthetic observation into the Operator at a bounded interval, causing the same normal zarvis turn machinery to decide whether to inspect context, update widgets, notify the user, or do nothing.

Ambient loop turns are advisory and non-critical. The Operator should prefer silence or widget updates, and if nothing is worth surfacing it should answer exactly `noted`. Normal tool policy and approval rules still apply; the ambient loop does not grant extra authority or bypass risky-action approvals.

## Reason

The Operator is intended to be an ambient companion rather than only a command handler. A bounded idle loop lets a frontier model notice stale work, blockers, workflow issues, or useful status updates without requiring explicit user prompts, while preserving auditability because the work still happens inside the persisted Operator session.

## Consequences

Ambient autonomy must remain low-noise and interruptible by ordinary session controls. The loop should only run for the Operator/orchestrator session, should not run while the Operator is already handling user input or an event observation, and should be rate bounded. Clients and widgets must not depend on ambient-loop output for critical user journeys.

## Non-Goals

This does not create a hidden daemon brain, guaranteed notification delivery, or a separate scheduler for arbitrary autonomous actions. It does not make ambient actions exempt from approvals.
