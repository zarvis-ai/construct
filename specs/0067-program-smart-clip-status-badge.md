# 0067-program-smart-clip-status-badge

Status: accepted
Date: 2026-07-02
Area: ux
Scope: Live status badge on `@{session:…}` Program smart-clip chips.

## Decision

A `@{session:<id>}` smart-clip chip renders the target session's current lifecycle status as a badge: a subtle status-tinted color plus the existing small glyph, with a plain-language tooltip on hover ("running", "awaiting input", "done", "exited with error", "session deleted"). The badge is derived entirely from the daemon's own session state — the same `SessionSummary`/`session/state` stream every client already keeps live for the session list — never from agent-authored text embedded in the Program document. No new protocol type or broadcast payload is required: both the TUI and the web client already receive the full session state live and already cache it (the TUI's `app.sessions`, the web client's `state.sessions`), so this is a rendering change on top of an existing feed, not a new one.

A chip whose target session id does not resolve against that live state — the daemon has no such session (deleted, archived, or never existed) — renders as **missing**: visually distinct (muted color, struck through in the TUI, struck through in the web client) and its tooltip reads "session deleted" rather than silently keeping whatever color or blank fallback it had before. A resolved session in the terminal `Done` state keeps the chip's prior fixed color unchanged, so the common "everything's fine" case is visually identical to before this badge existed; every other status (pending, running/awaiting-input, paused, errored, missing) gets its own color so a state change — especially a worker dying — is visible without reading the label text.

Renderers must degrade gracefully whenever the live status can't be determined precisely: a client without a live `App`/`state.sessions` context yet (e.g. the TUI's width-measurement helpers used before a session list has loaded) may keep rendering the plain unstyled chip rather than guessing at a status. This is not a regression the badge needs to eliminate — it only needs to apply the badge whenever live status *is* available, which in real usage is effectively always (both clients hydrate session state before mounting the Program view).

Hovering a session clip chip that has no richer preview available (unknown session, or a resolved session with no captured PTY output yet, per `0060`) falls back to this plain-language status tooltip instead of showing nothing, in the TUI. The web client renders the tooltip as the chip's native hover title unconditionally, since it has no PTY-preview-card affordance to prefer.

## Reason

The chip already displays a session's identity (title/harness) and, in the TUI, a static lifecycle glyph — but that glyph was frozen at whatever it was when the clip was last rendered, and a missing session silently fell back to plain unstyled text indistinguishable from a normal chip. A viewer scanning a Program document with several delegated workers had no way to tell, at a glance, which one just died without opening each session. Since the daemon already pushes every session's live state to every connected client for the session list, reusing that same feed for the chip costs no new machinery — it only wires an existing signal into an existing render path.

Keeping the badge daemon-truth-only (never agent-authored) matches the rest of the Program model (`0041`): clip identity and target resolution are structural facts about the document and the fleet, not content an agent writes back. An agent narrating "worker finished" in prose is normal Program content; the chip's own color is not something an agent should be able to spoof by writing a particular clip syntax.

## Consequences

- No new protocol type, RPC, or broadcast payload. The badge is computed client-side from state every client already holds.
- The TUI's session-kind chip background is now a function of `SessionState` (or "missing") instead of a fixed color; `Done` intentionally maps to the same color the chip always used, so most already-settled references are visually unchanged.
- The web client's `.program-clip` gains a `data-status` attribute and matching CSS rules, plus a native `title` attribute for the tooltip. A `session/state` or `session/deleted` push repaints only the affected mounted chip DOM nodes in place (text/attributes, not node identity), so it never disturbs an in-progress edit's caret or selection.
- A renderer that cannot yet resolve live session state for a clip must render the existing plain/neutral chip rather than fabricate a status; this is the explicit degrade path referenced by `0041`'s general renderer-degradation stance.

## Non-Goals

- This does not change smart-clip syntax, persistence, or the daemon's clip registry (`0041`).
- This does not touch the Program shimmer lifecycle or its per-block tooltip (`0042`, `0053`, `0057`, `0060`) — those remain about run-in-progress blocks, not the identity chip's session lifecycle.
- This does not add a "missing" variant to the `SessionState` protocol enum; "missing" is purely a client-side rendering conclusion drawn from a clip id having no matching entry in the live session set.

## Examples

- A Program lists `Building the PR @{session:worker}`. While `worker` is `Running`, the chip reads with a running-status tint. The worker crashes and its state flips to `Errored`; the very next `session/state` push recolors the chip to the errored tint on every open client, with no Program edit or agent action required.
- A Program written weeks ago still contains `@{session:s_old}`, and that session was since deleted. Its chip renders muted and struck through; hovering it (TUI) or reading its title attribute (web) shows "session deleted".
