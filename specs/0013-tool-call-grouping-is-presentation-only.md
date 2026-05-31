# 0013-tool-call-grouping-is-presentation-only

Status: accepted
Date: 2026-05-31
Area: ux
Scope: Group repeated tool-call chrome without changing session history or tool semantics.

## Decision

Clients may group adjacent rendered tool calls when they have the same tool name. Grouping is presentation-only: every individual tool call and result remains represented in the persisted transcript and must remain inspectable from the grouped UI.

A group is formed by run-length compression over the rendered tool-call stream. It must not depend on whether calls were issued sequentially or through a parallel-call helper, and it must not cross non-tool content or a different tool name.

Grouped rows should show both a count and a compact factual summary derived from call arguments and/or results. Failures, pending calls, and running/backgrounded calls must remain visible in the group status.

## Reason

Zarvis often performs bursts of repeated calls, such as reading several files or querying several sessions. Rendering each call as a full top-level block creates noisy transcripts in both the terminal TUI and web UI. Grouping adjacent same-tool calls keeps dense work readable while preserving auditability.

## Consequences

Future clients that synthesize tool-call UI should apply the same adjacent same-tool rule so replay after restart remains deterministic and cross-client behavior stays predictable.

Approval prompts, tool execution, result matching, persistence, and transcript APIs are unchanged by grouping. Expanding a group should reveal the individual calls rather than a generated narrative summary.

## Non-Goals

Grouping does not introduce special handling for parallel calls. Parallel batches naturally group only when their flattened rendered tool-call events are adjacent and share a tool name.

Grouping is not a summarization feature. The compact note should be factual and derived from structured data, not model-generated prose.
