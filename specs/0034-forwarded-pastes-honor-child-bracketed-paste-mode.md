# 0034-forwarded-pastes-honor-child-bracketed-paste-mode

Status: accepted
Date: 2026-06-23
Area: tui
Scope: When the client forwards a paste into a PTY-backed session, it frames the bytes the way a real terminal would for that child.

## Decision

A paste delivered to the client (the outer terminal's bracketed-paste event) and forwarded to a PTY-backed child must be wrapped in the child's expected bracketed-paste markers (`ESC[200~` … `ESC[201~`) **when, and only when, that child currently has DEC private mode 2004 enabled**. The client tracks the child's mode from the same byte stream it renders, so it knows whether the child asked for bracketed paste. Children that never enable mode 2004 receive the paste as raw bytes.

The closing marker is stripped from the payload before wrapping so an embedded `ESC[201~` cannot terminate the paste early.

## Reason

The client is the terminal for every child PTY. A child that enables bracketed paste is explicitly asking to be told "this input is a paste, not typing." Forwarding the bytes raw breaks that contract: the harness sees pasted content as ordinary keystrokes. The visible failure was a dragged image path arriving in a Claude Code session as literal text instead of an `[image #N]` reference — Claude Code's drag-image detection (and its multiline-paste guard) fire only on a real bracketed paste. Honoring the child's mode restores parity with running the harness directly in a terminal, for images and for any other paste-aware behavior (multiline guards, shell readline not executing until Enter).

## Consequences

- The client must keep a live view of each child's bracketed-paste mode, derived from the rendered byte stream rather than guessed. Sessions with no live parser yet, and chat/synth sessions that never run a real terminal child, fall back to raw forwarding.
- All pastes into a mode-2004 child are now framed, not just image paths — this is intended terminal fidelity, not an image-specific special case. Multiline and shell pastes change behavior accordingly (no premature submit / execute).
- Paste-injection via an embedded end-marker is prevented by stripping it from the payload, matching real terminals.

## Non-Goals

- The client does not itself detect image paths or synthesize attachment references for the bracketed-paste case; it stays a faithful pass-through and lets the harness decide what a paste means. (A separate, size-triggered path may still upload very large pastes as attachments.)
- This does not change how typed (non-paste) keystrokes are encoded.
