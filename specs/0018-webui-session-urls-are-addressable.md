# 0018-webui-session-urls-are-addressable

Status: accepted
Date: 2026-06-05
Area: webui
Scope: Browser-visible web UI URLs identify the selected session.

## Decision

The web UI must encode the currently selected session in the browser path as `/s/<session-id>` and honor that session identifier on page load. Refreshing, bookmarking, sharing, or navigating browser history for a session URL should return to that session when it still exists.

## Reason

Remote-control users rely on normal browser behavior, especially refresh on mobile browsers. If every session shares the same URL, a reload loses context and falls back to whichever session appears first in the list, making it easy to monitor or act on the wrong workload.

## Consequences

Session selection changes update URL state using history navigation. The daemon serves the same web UI shell at each `/s/<session-id>` path, while the client validates the addressed session against the session list and may fall back to the normal default when it no longer exists. Static assets and development endpoints use root-relative paths so nested session URLs do not break refresh.

## Non-Goals

This does not make session URLs permanent across daemon data deletion or remote-control credential changes.
