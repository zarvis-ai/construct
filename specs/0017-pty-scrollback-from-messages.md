# 0017-pty-scrollback-from-messages

Status: accepted
Date: 2026-06-01
Area: tui
Scope: How the TUI renders scroll-up (scrollback) for PTY-backed harness sessions.

## Decision

For a PTY-backed session that emits structured `Message` / `Reasoning`
events and has no tool blocks, the TUI renders **scroll-up** (any frame
with `scrollback > 0`) from the **structured message log**, rebuilt and
reflowed to the current width — not from a reconstruction of the child's
raw PTY frames.

- The **live view** (`scrollback == 0`) is unchanged: it stays the
  faithful PTY render.
- A session that emits **no** messages (a raw shell) keeps the natural /
  viewport-snapshot scrollback path.
- A session **with tool blocks** keeps the synth (`replay_full`) path; it
  never reaches the shadow path this rule governs.

Mechanically: the client feeds each PTY-session `Message`/`Reasoning`
event into a side "shadow message" buffer (on both the hydration/replay
path and the live event path), and the scroll-up renderer prefers that
buffer whenever it is non-empty.

## Reason

Modern harness TUIs do not produce clean terminal scrollback, and the
TUI's `vt100`-backed reconstruction makes it worse:

- **codex** pins its input box with a DECSTBM scroll region. `vt100`
  0.16 only saves a scrolled-off line to scrollback when *no* region is
  active (`grid.rs: scroll_up` gates on `!scroll_region_active()`), so
  codex's conversation lines are dropped — scrollback is empty.
- **claude** sets no region but rewrites lines with cursor-up plus a
  bordered input box. The viewport-snapshot fallback (which exists
  precisely because `vt100` scrollback is empty for these children) then
  duplicates answer lines and splices chrome (spinner, borders, the
  input box) into them.

The web client does not have this problem because xterm.js keeps a real
native scrollback for the same byte stream. Rather than vendor/patch
`vt100` to match xterm.js, we use the clean, complete, reflowable source
the harness already provides: its structured message stream. This is the
same content the headless render path already folds into history; here we
route it to scroll-up for PTY sessions without disturbing the live view.

## Consequences

- Scroll-up shows the conversation cleanly and reflows to the current
  width, for any redrawing harness that emits messages (codex, claude,
  and future ones) — uniformly, without per-harness detection.
- Scroll-up content is the **semantic** message log, which can differ
  from the exact pixels the child painted (e.g. a harness's box-drawn UI
  appears as its message text). For prose this is equivalent or better.
- Messages must be fed into the shadow on **both** the hydration/replay
  path and the live event path, or scroll-up silently falls back to the
  snapshot reconstruction.
- The live (`scrollback == 0`) render is untouched, so the cost is paid
  only while actually scrolled back.

## Non-Goals

- Not changing the live view.
- Not fixing the upstream `vt100` scroll-region scrollback gap (a valid
  alternative, but it would mean vendoring the crate).
- Not covering tool-block sessions, which render via `replay_full`.

## Examples

- codex: prints a 60-line answer above a footer pinned by `ESC[1;Nr`;
  before, scroll-up was empty or a fragmented snapshot; now it shows the
  60 messages, one per line, reflowed.
- claude: streams an answer while redrawing a spinner/input box with
  cursor-up; before, scroll-up duplicated lines and mangled numbers into
  border rows; now it shows the clean message log.
- shell: emits only PTY bytes and no messages; scroll-up keeps the
  natural scrollback path, unchanged.
