# Remote control

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
