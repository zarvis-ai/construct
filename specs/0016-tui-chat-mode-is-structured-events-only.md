# 0016-tui-chat-mode-is-structured-events-only

Status: accepted
Date: 2026-06-01
Area: tui
Scope: TUI chat-mode rendering and its relationship to terminal rendering.

## Decision

The TUI has a Chat Mode that renders only structured session events. It must not render raw PTY bytes, terminal snapshots, or other terminal-derived fallback content. PTY-backed sessions default to Terminal Mode and can toggle to Chat Mode for structured-event inspection; headless and other non-PTY sessions default to Chat Mode.

## Reason

Terminal output and structured conversation events have different semantics. Mixing PTY bytes into the chat renderer recreates terminal-history problems such as duplicated repaint snapshots, broken spacing, and inconsistent rendering across resize or restart. A structured-only Chat Mode keeps Terminal Mode responsible for terminal bytes while giving headless sessions and transcript inspection one readable, product-oriented presentation.

## Consequences

Adapters that want rich Chat Mode output must emit structured message, reasoning, tool, status, error, approval, or related events. A PTY-backed session that emits only terminal bytes may show an empty Chat Mode with guidance to use Terminal Mode. Future TUI transcript or headless-session changes should reuse the Chat Mode renderer instead of adding another event-log renderer or deriving chat rows from PTY output.

## Non-Goals

Chat Mode is not a terminal emulator, scrollback reconstruction mechanism, or ANSI snapshot viewer. It does not replace Terminal Mode for interactive shells or full-screen TUIs.
