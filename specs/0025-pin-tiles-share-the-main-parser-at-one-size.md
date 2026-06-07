# 0025-pin-tiles-share-the-main-parser-at-one-size

Status: accepted
Date: 2026-06-07
Area: tui
Scope: Applies to rendering a PTY session that is both visible in the main/split view and shown in the pin strip.

## Decision

A pinned session and its main-view rendering share a single cached vt100 parser per `ItemHistory`. The pin tile must render that session at the **same size the main/split render already used this frame** — it reads the parser's current `cached_dims()` and replays at that — rather than forcing its own "main view" size. Only a session that has no cached parser yet (pin-only, never opened in the main view) falls back to the main-view size to seed one.

## Reason

`replay` resizes the cached parser to the requested dimensions, and a width change rebuilds it (re-feeding the pending chunk through a freshly-sized grid). Rendering the same shared parser at two different widths on the same frame — once for the main view, once for the pin tile — rebuilds it twice every frame. Measured at ~45000x the cost of a no-op resize on a long history (≈2 ms per session per frame); a few pinned-and-split sessions blow the frame budget, which is the "extremely laggy with both split view and pinned sessions" report.

An earlier fix rendered the pin at the single "main view" size so it matched the (then) single main render. Split view broke that assumption: each split pane has its own width, so a session shown in a split pane (width A) and a pin tile (the whole-view width B) thrashed again. The robust invariant is not "render the pin at a fixed size" but "render the pin at whatever size the main render already set," which holds for any number of split panes.

## Consequences

The main/split render must run before the pin strip render each frame (it does), so the parser is already sized when the pin reads `cached_dims()`. `cached_dims()` is part of the render API, not test-only.

A pin-only session (never opened in the main view) keeps the width it was first seeded at and does not re-wrap on a later terminal resize until it is opened in the main view. The pin tile is a cropped preview, so this is cosmetic and self-correcting.

## Non-Goals

This does not give the pin tile its own parser. A second parser per pinned session would avoid the shared-size coupling but doubles the per-frame feed cost and memory; reusing the main parser at its current size is cheaper and sufficient.

## Examples

Two sessions open in a left/right split, both pinned. Each split pane renders its session at the pane width; the pin tiles then replay each at that same cached width — a no-op resize — so the parsers are never rebuilt and the UI stays responsive.
