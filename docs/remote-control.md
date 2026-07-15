# Remote control

`/remote-control` exposes the running daemon through a browser-accessible web
client so you can check and steer the same fleet from another device. The TUI
shows a modal with a QR code, the addresses this machine can be reached at, a
username, and a password; the local modeline shows a `remote` badge while remote
clients are attached.

Opening the dialog does **not** expose the daemon to the internet. It binds the
listener and shows how to reach it on the local network — a phone on the same
Wi-Fi can scan the QR and connect right away, no tunnel involved. Reaching the
daemon from *outside* the local network is a separate, explicit choice you make
from the buttons in the dialog.

| Command / setting | Purpose |
|---|---|
| `/remote-connect` | Guided public-tunnel flow: select `tunnel.zarvis.ai` and authorize in the browser. |
| `/remote-control` | Open the dialog: bind the listener, show the LAN address + QR, and offer a tunnel. No tunnel is started until you pick one. |
| `/remote-control <password>` | Same, with a user-chosen Basic-auth password. |
| `/remote-control cloudflare` | Skip the dialog and start a Cloudflare tunnel directly. |
| `/remote-control construct` | Start the first-party flow directly; the service assigns the name. |
| `/remote-control stop` | Stop the listener + tunnel entirely and rotate credentials for the next start. |
| `/remote-control debug` | Alias for `/remote-control` — kept because the plain dialog is now the local-only resting state. |
| `CONSTRUCT_REMOTE_WS_PORT=<port>` | Start the remote WebSocket listener on daemon boot for scripted/headless use. |
| `CONSTRUCT_REMOTE_PROVIDER=<cloudflare\|construct\|none>` | Tunnel provider for the boot-time listener above. Defaults to `cloudflare`. |
| `CONSTRUCT_WEBUI_PORT=<port>` | Override the always-on localhost web UI port. Defaults to `5746`. |

## The tunnel

Reaching the daemon from beyond the local network means starting a tunnel.
**Cloudflare** runs a `cloudflared` quick tunnel: the URL is reachable from
anywhere and is unguessable, but it rotates on every run and its only protection
is that nobody learns it. It needs no account — just `cloudflared` on `PATH`.
The dialog shows the Cloudflare button even when `cloudflared` isn't installed,
greyed out with an install hint, so you can see what's missing.

Once the tunnel's QR is up, the ready view offers two buttons:

- **back** — return to the local-network view with the tunnel still running.
  Re-selecting Cloudflare shows the same URL; nothing was torn down.
- **stop** — stop the tunnel and drop the public URL, but keep the LAN listener
  and its password. A phone connected over the LAN keeps working.

The **Construct** provider links the `wstunnel` Rust library directly; there is
no separate executable to install or configure. Construct opens a short-lived
`tunnel.zarvis.ai` browser login (and shows
the link in the dialog). After GitHub or Google OAuth succeeds, the running
daemon receives authorization directly, and the service assigns a unique,
human-friendly random name before opening a reverse tunnel restricted to that
registration. No owner token is shown,
copied, placed in an environment variable, or written to a configuration file.

The service publishes a `<name>.tunnel.zarvis.ai` URL only
after the reverse endpoint answers. The ready view displays that URL and its QR
code without showing the gateway's internal upstream Basic credentials.
Visitors sign in with GitHub or Google. The service maps the active name to its
owner and reverse endpoint in memory; the signed-in provider and immutable
provider subject must match the tunnel owner.

To turn remote control off completely — listener included — use
`/remote-control stop`.

## Authentication and binding

The remote listener binds every interface and gates every request with HTTP
Basic auth. The username is `remote`; the password is generated per session (a
memorable `word.word.NNNN` string) unless you supply your own. Failed attempts
are throttled daemon-wide, so the short password is safe to be the only
credential — but that throttle is load-bearing, and the listener is reachable
from the whole local network, so treat the auth path as security-sensitive.

The daemon also starts a separate localhost-only browser UI at
`http://127.0.0.1:5746/`. That UI is bound to loopback and has **no** auth at
all — it must never be exposed off-machine, which is exactly why it stays on
loopback while the remote-control listener does not.

Tunnel + listener state is persisted under the runtime directory so a daemon
restart preserves the active URL, password, and provider when possible; a
restart never silently rotates the URL or switches how the machine is exposed.
Cloudflare's child process can be adopted across a daemon restart. A
`tunnel.zarvis.ai` connection runs in-process and keeps its authorization only
in memory, so it stops on restart and requires an explicit `/remote-connect`
and browser authorization afterward.
