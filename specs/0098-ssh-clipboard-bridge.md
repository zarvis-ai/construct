# 0098-ssh-clipboard-bridge

Status: accepted
Date: 2026-07-16
Area: cli
Scope: Clipboard copy/paste between a remote construct TUI and the machine the user is physically sitting at, carried over an SSH-forwarded private socket.

## Decision

construct provides a first-party clipboard bridge for SSH use:

- A single wrapper command (`construct ssh …`) run on the user's local
  machine starts an in-process clipboard agent (no daemon, no TUI), binds it
  to a private Unix socket, and invokes the real `ssh` with a reverse
  socket forward plus a remote command that launches `construct` with an
  environment variable pointing at the forwarded socket. All user-supplied
  arguments are passed to `ssh` verbatim so existing SSH configuration
  (ports, jump hosts, aliases) keeps working.
- Both ends of the invocation are user-overridable, by flag or persistent
  environment variable: the transport command replacing `ssh` (for
  OpenSSH-compatible wrappers like Teleport's `tsh ssh` or Eternal
  Terminal), and the remote command replacing `construct` (for hosts where
  the binary isn't on the login PATH). Overrides don't change the bridge
  contract: the transport must accept OpenSSH-style option/forward/tty
  flags, and the remote command still receives the socket environment
  variable.
- The remote TUI, when that environment variable is present, prefers the
  bridge for clipboard traffic: selection copies are sent to the bridge
  (landing on the local machine's clipboard), and clipboard reads consult
  the bridge instead of the remote host's clipboard, which is the wrong
  machine's.
- Paste through the bridge supports more than text: the local agent can
  return an image or file payload with a MIME type, which the TUI turns
  into a session attachment (the same mechanism used for oversized text
  pastes) rather than raw keystrokes.
- The bridge is an explicit, user-initiated channel. Paste requests are
  only issued in response to a user action in the TUI; the daemon never
  reads the bridge, and the bridge socket belongs to one SSH session, not
  to the daemon or to other clients.
- Dragging a file onto the terminal pastes the file's local path as text,
  which is meaningless on the remote host. With a bridge attached, the TUI
  recognizes that shape and offers to upload the file's bytes instead. The
  local file read is double-gated: it happens only for a path the user
  dropped/pasted themselves AND explicitly confirmed per file in the TUI,
  and both ends restrict it to an allowlist of media types (images,
  PDF) with a size cap — the bridge must never become a general
  file-fetch channel.
- Attachments paste as a pointer the session can act on: images paste as
  the bare stored path so agent harnesses' native pasted-image detection
  fires; other types paste the file-reference token.

## Reason

Selection copy in the TUI must reach the clipboard of the machine the user
is physically at. Locally, the host clipboard is that machine. Over SSH the
only in-band mechanism is OSC 52, which several mainstream terminals
(notably macOS Terminal.app) do not support for writes and nearly all
terminals disable for reads; images and files cannot travel over OSC 52 at
all. A construct-owned side channel over the SSH connection closes both
gaps without asking users to change terminals, and the wrapper command
keeps setup at zero: one command, both ends are the same installed binary.

## Consequences

- Security posture must be preserved: the bridge socket is a private Unix
  socket with owner-only permissions on both ends — never a TCP listener —
  because clipboards carry secrets and a readable socket on a shared host
  would leak them. Socket paths are per-connection (unique per invocation)
  so concurrent bridges from different machines cannot collide or cross.
- The remote end that consumes the bridge is the TUI process, not the
  daemon: the forwarded socket's lifetime is the SSH session's, while the
  daemon is shared across clients and outlives it.
- A paste endpoint means the remote host can read the local clipboard when
  (and only when) the TUI asks. This is the same trust grant as OSC 52
  paste; preserving the "user-initiated only" rule above is what keeps it
  acceptable. Payload sizes are capped to bound transfers.
- Degradation must stay graceful in both directions: an older remote
  `construct` ignores the environment variable (OSC 52 fallback applies),
  and a dead or missing bridge must fall back to the pre-bridge behavior
  rather than fail the copy or paste.
- The wrapper appends its own remote command, so users must not pass one
  positionally; overriding what runs remotely is an explicit flag.

## Non-Goals

- Not a general clipboard sync: nothing is mirrored continuously; each
  copy/paste is one request over the socket.
- Not a daemon feature or IPC method: the daemon's attachment mechanism is
  reused unchanged, and the bridge protocol is private to the CLI.
- Copying non-text data from the remote TUI to the local machine is out of
  scope (the TUI's selection model is text).

## Examples

- `construct ssh devbox` opens the remote TUI; drag-selecting text there
  puts it on the local Mac's clipboard even in Terminal.app.
- Copying an image locally and invoking the TUI's paste-attachment action
  remotely attaches the image to the focused session and pastes its
  reference token, exactly as a local oversized-text paste would.
- `construct ssh -J bastion -p 2222 devbox` works because the flags go to
  `ssh` untouched.
- With no bridge attached (plain `ssh` + `construct`), copy behaves as
  before: OSC 52 where the terminal supports it.
