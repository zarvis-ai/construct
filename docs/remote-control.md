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
| `/remote-control debug` | Start a local-only URL without a public tunnel; mostly retained for troubleshooting remote-control credentials/tokens. For normal local browser use, open the always-on local web UI instead. |
| `AGENTD_REMOTE_WS_PORT=<port>` | Start the remote WebSocket listener on daemon boot for scripted/headless use. |
| `AGENTD_WEBUI_PORT=<port>` | Override the always-on localhost web UI port. Defaults to `5746`. |

The daemon also starts a localhost-only browser UI at `http://127.0.0.1:5746/`
by default. This local UI is bound to loopback and does **not** require the
remote-control token or HTTP Basic auth. `/remote-control` is still required to
create a public tunnel, and tunneled remote-control sessions still require HTTP
Basic auth. The username is `remote`; the password is generated per session
unless supplied explicitly. Public tunnel state is persisted under the runtime
directory so a daemon restart can preserve the active remote URL when possible.
