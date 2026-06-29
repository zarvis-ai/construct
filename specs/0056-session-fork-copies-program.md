# 0056-session-fork-copies-program

Status: accepted
Date: 2026-06-29
Area: ux
Scope: Session forks preserve durable orchestration context.

## Decision

Forking a session creates a sibling session that copies the source session's Program document, including its Markdown and template identity.

The fork does not copy active Program execution state. Any in-flight run, transient progress, or pending execution affordance belongs to the original session.

## Reason

The Program is user-authored orchestration state for the session. A fork that keeps transcript context but starts with a blank Program loses the user's current plan, task board, or notes, making the fork less useful as a continuation point.

Active execution state is different: copying it would imply the fork is already running work that only the original session actually owns.

## Consequences

Future fork implementations and clients must treat the Program document as part of durable fork context alongside cwd, group placement, title, model inheritance, and optional transcript seed.

Forks may start with the same Program content as their source, but they must start without a copied active run.
