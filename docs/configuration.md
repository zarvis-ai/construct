# Configuration

## Paths

`agentd` reads/writes under XDG-style directories, with `AGENTD_*_DIR` overrides:

| Use | Default | Override |
|---|---|---|
| Config | `~/.config/agentd` | `AGENTD_CONFIG_DIR` |
| State (pid/log) | `~/.local/state/agentd` | `AGENTD_STATE_DIR` |
| Data (sessions) | `~/.local/share/agentd` | `AGENTD_DATA_DIR` |
| Socket | `$XDG_RUNTIME_DIR/agentd/agentd.sock` (falls back to state) | `AGENTD_RUNTIME_DIR` |

`agentd paths` prints the resolved layout.

## Remote control

`/remote-control` exposes the running daemon through a browser-accessible web
client so you can check and steer the same fleet from another device. The TUI
shows a modal with a URL, QR code, username, and password; the local modeline
shows a `remote` badge while remote clients are attached.

| Command / setting | Purpose |
|---|---|
| `/remote-control` | Start the local WebSocket server and public tunnel, then show URL + QR code. |
| `/remote-control <password>` | Start remote control with a user-chosen password. |
| `/remote-control stop` | Stop the remote listener/tunnel and rotate credentials for the next start. |
| `/remote-control debug` | Start a local-only URL without a public tunnel; useful for troubleshooting. |
| `AGENTD_REMOTE_WS_PORT=<port>` | Start the remote WebSocket listener on daemon boot for scripted/headless use. |

Remote-control sessions use HTTP basic auth. The username is `remote`; the
password is generated per session unless supplied explicitly. Public tunnel
state is persisted under the runtime directory so a daemon restart can preserve
the active remote URL when possible.

## TUI Theme

The TUI uses a built-in Matrix theme by default. Override any color slot in
`$AGENTD_CONFIG_DIR/theme.toml` (default `~/.config/agentd/theme.toml`):

```toml
[colors]
text = "#b8ffcc"
accent = "#39ff88"
border_focused = "#4bff82"
harness = "#96ffaa"
danger = "red"
matrix_dim = "indexed:34"
```

Colors accept `#rrggbb`, `indexed:N`, or ANSI names such as `green`, `cyan`,
`dark_gray`, and `light_yellow`. Omitted slots keep the Matrix default.
