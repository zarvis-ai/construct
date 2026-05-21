//! Unix-socket IPC server. Dispatches JSON-RPC requests to [`SessionManager`]
//! and forwards subscribed broadcast events back to the client.

use crate::remote::{token_from_uri_path, RemoteState};
use crate::session::{BroadcastMsg, SessionManager};
use agentd_protocol::jsonrpc::{self, MessageKind};
use agentd_protocol::{
    ipc_method, ipc_notif, transport, CreateSessionParams, ErrorObject, GroupCreateParams,
    GroupCreateResult, GroupDeleteParams, GroupMoveParams, GroupRenameParams,
    GroupSetCollapsedParams, Notification, PingResult, Request, Response, SessionIdParams,
    SessionInputParams, SessionMoveParams, SessionPtyInputParams, SessionPtyResizeParams,
    SessionSetAutomodeParams, SessionSetGroupParams, SessionSetPinnedParams,
    SessionSetTitleParams, SessionToolActionParams, SessionToolDecisionParams, SubscribeParams,
    TranscriptParams, IPC_VERSION,
};
use anyhow::{Context, Result};
use futures::{SinkExt as _, StreamExt as _};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::BufReader;
use tokio::net::{TcpListener, UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite;

pub async fn serve(manager: Arc<SessionManager>, socket_path: PathBuf) -> Result<()> {
    let _ = std::fs::remove_file(&socket_path);
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let listener = UnixListener::bind(&socket_path)?;
    tracing::info!(socket = %socket_path.display(), "listening");
    loop {
        let (stream, _) = listener.accept().await?;
        let manager = manager.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, manager).await {
                tracing::debug!(error = ?e, "connection closed with error");
            }
        });
    }
}

#[derive(Debug)]
enum SubCmd {
    Subscribe(Option<String>),
    Unsubscribe,
}

/// Which transport a request arrived on. Drives the activity-
/// driven PTY-resize policy: whichever kind most recently sent a
/// `pty_input` or `pty_resize` to a given session "owns" that
/// session's PTY size, and the daemon resizes the OS PTY to that
/// kind's last-known viewport. Switching attention between TUI
/// and phone flips the size on the next interaction.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ClientKind {
    /// Unix-socket caller — the desktop TUI, MCP, or CLI scripts.
    Tui,
    /// WebSocket caller — the `/remote-control` web client on
    /// phone (or any other authenticated WS consumer).
    Remote,
}

async fn handle_connection(stream: UnixStream, manager: Arc<SessionManager>) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<serde_json::Value>();

    // Outbound writer: drain the shared channel and emit as
    // newline-delimited JSON on the Unix socket.
    let writer_task = tokio::spawn(async move {
        while let Some(v) = out_rx.recv().await {
            if transport::write_message(&mut writer, &v).await.is_err() {
                break;
            }
        }
    });

    let inbound = ReadFromUnix {
        reader: BufReader::new(reader),
    };
    run_session(inbound, out_tx, manager, ClientKind::Tui).await;
    writer_task.abort();
    Ok(())
}

/// Transport-agnostic per-connection dispatch loop. The transport
/// shim is responsible for parsing inbound bytes into JSON values
/// (`Inbound::next`) and for emitting outbound values from
/// `out_tx`. Used by both the Unix socket and the WebSocket
/// listeners so request dispatch, subscription forwarding, and the
/// subscribe-cmd channel only live in one place.
async fn run_session<I: Inbound>(
    mut inbound: I,
    out_tx: mpsc::UnboundedSender<serde_json::Value>,
    manager: Arc<SessionManager>,
    kind: ClientKind,
) {
    let (sub_cmd_tx, sub_cmd_rx) = mpsc::channel::<SubCmd>(8);

    let sub_out_tx = out_tx.clone();
    let sub_manager = manager.clone();
    let sub_task = tokio::spawn(async move {
        run_subscription_loop(sub_manager, sub_out_tx, sub_cmd_rx).await;
    });

    loop {
        let raw = match inbound.next().await {
            Some(Ok(v)) => v,
            Some(Err(e)) => {
                tracing::warn!(error = %e, "client sent bad JSON");
                continue;
            }
            None => break,
        };
        if !matches!(jsonrpc::classify(&raw), Some(MessageKind::Request)) {
            continue;
        }
        let req: Request = match serde_json::from_value(raw) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "invalid request shape");
                continue;
            }
        };
        let resp = dispatch(&manager, &sub_cmd_tx, kind, req).await;
        let v = match serde_json::to_value(&resp) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "serialize response failed");
                continue;
            }
        };
        if out_tx.send(v).is_err() {
            break;
        }
    }

    sub_task.abort();
}

/// Inbound-value source for `run_session`. Yields the next JSON
/// value the client sent (or `None` when the connection closes).
/// Errors are non-fatal — `run_session` logs and continues.
trait Inbound: Send {
    async fn next(&mut self) -> Option<std::io::Result<serde_json::Value>>;
}

struct ReadFromUnix {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
}

impl Inbound for ReadFromUnix {
    async fn next(&mut self) -> Option<std::io::Result<serde_json::Value>> {
        match transport::read_message(&mut self.reader).await {
            Ok(Some(v)) => Some(Ok(v)),
            Ok(None) => None,
            Err(e) => Some(Err(std::io::Error::other(e))),
        }
    }
}

struct ReadFromWs<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    rx: futures::stream::SplitStream<tokio_tungstenite::WebSocketStream<S>>,
}

impl<S> Inbound for ReadFromWs<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    async fn next(&mut self) -> Option<std::io::Result<serde_json::Value>> {
        loop {
            let frame = self.rx.next().await?;
            match frame {
                Ok(tungstenite::Message::Text(s)) => {
                    return Some(
                        serde_json::from_str::<serde_json::Value>(&s)
                            .map_err(std::io::Error::other),
                    );
                }
                Ok(tungstenite::Message::Binary(b)) => {
                    // Treat binary frames as JSON too — some WS
                    // clients prefer binary for non-text payloads,
                    // and JSON-RPC is small enough that either
                    // works.
                    return Some(
                        serde_json::from_slice::<serde_json::Value>(&b)
                            .map_err(std::io::Error::other),
                    );
                }
                // tungstenite handles pings + close frames itself
                // when we use the standard `next()` interface — but
                // we still receive them here so we can ignore them.
                Ok(tungstenite::Message::Ping(_))
                | Ok(tungstenite::Message::Pong(_))
                | Ok(tungstenite::Message::Frame(_)) => continue,
                Ok(tungstenite::Message::Close(_)) => return None,
                Err(e) => return Some(Err(std::io::Error::other(e))),
            }
        }
    }
}

/// Run the accept loop on an already-bound WebSocket listener.
/// Splitting the bind out of the loop lets callers pick an
/// ephemeral port (`bind("127.0.0.1:0")`) and report the chosen
/// port back to the caller before the accept loop starts — that's
/// what `SessionManager::start_remote` does so it can return a URL
/// without a port-discovery race.
///
/// Each connection runs through `run_session` over a chained
/// stream that demuxes WS upgrade vs. plain HTTP at the TCP level;
/// see `handle_ws_connection`.
pub async fn serve_ws_on(
    manager: Arc<SessionManager>,
    remote: RemoteState,
    listener: TcpListener,
) -> Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        let manager = manager.clone();
        let remote = remote.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_ws_connection(stream, manager, remote).await {
                tracing::debug!(?peer, error = ?e, "ws connection closed with error");
            }
        });
    }
}

/// Embedded mobile-first web client served on the same URL the QR
/// code points at. One file, inline JS + CSS, no build pipeline —
/// loaded via `include_str!` at compile time. See
/// `crates/daemon/assets/index.html` to edit.
const REMOTE_INDEX_HTML: &str = include_str!("../assets/index.html");
/// xterm.js UMD bundle (pinned to `@xterm/xterm@5.5.0`). Embedded
/// at build time so the phone web client doesn't need to reach a
/// public CDN — the captive-network failure mode (phone reaches
/// the cloudflared tunnel but not jsdelivr.net) was real and
/// triggered on the first hand-test.
const REMOTE_XTERM_JS: &[u8] = include_bytes!("../assets/static/xterm.js");
/// xterm.js default stylesheet.
const REMOTE_XTERM_CSS: &[u8] = include_bytes!("../assets/static/xterm.css");
/// addon-fit UMD bundle (pinned to `@xterm/addon-fit@0.10.0`).
/// Sizes the terminal grid to the container, used on resize +
/// soft-keyboard show/hide.
const REMOTE_XTERM_ADDON_FIT_JS: &[u8] =
    include_bytes!("../assets/static/xterm-addon-fit.js");

/// Cap on the HTTP request prelude (request-line + headers, up to
/// `\r\n\r\n`). 16 KiB is generous enough for any real browser
/// request and small enough that a malicious client can't grow our
/// buffer unbounded.
const MAX_HTTP_PRELUDE_BYTES: usize = 16 * 1024;

/// What we extract from the HTTP prelude — path, WS-upgrade
/// hint, and the raw `Authorization` header value (if present) so
/// the demux can enforce Basic auth without a second header pass.
struct PreludeInfo {
    path: String,
    is_ws_upgrade: bool,
    /// Raw `Authorization` header value, e.g. `"Basic dXNlcjpwdw=="`.
    /// `None` when the client sent no Authorization header.
    authorization: Option<String>,
}

/// Parse an HTTP/1.1 request prelude using `httparse`. Returns
/// `None` on malformed input (we treat it as a 400 + close, not a
/// fatal error).
fn parse_http_prelude(buf: &[u8]) -> Option<PreludeInfo> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    match req.parse(buf).ok()? {
        httparse::Status::Complete(_) => {
            let path = req.path?.to_string();
            let is_ws_upgrade = req.headers.iter().any(|h| {
                h.name.eq_ignore_ascii_case("Upgrade")
                    && std::str::from_utf8(h.value)
                        .map(|v| v.eq_ignore_ascii_case("websocket"))
                        .unwrap_or(false)
            });
            let authorization = req.headers.iter().find_map(|h| {
                if h.name.eq_ignore_ascii_case("Authorization") {
                    std::str::from_utf8(h.value).ok().map(|s| s.to_string())
                } else {
                    None
                }
            });
            Some(PreludeInfo {
                path,
                is_ws_upgrade,
                authorization,
            })
        }
        httparse::Status::Partial => None,
    }
}

/// Extract `(username, password)` from a `Basic <base64>`
/// Authorization header. Returns `None` on malformed input. The
/// caller decides what to do with the username — we enforce
/// `"remote"` in the dispatch (see `REMOTE_USERNAME`).
fn credentials_from_basic_auth(header: &str) -> Option<(String, String)> {
    use base64::Engine as _;
    let rest = header
        .trim()
        .strip_prefix("Basic ")
        .or_else(|| header.trim().strip_prefix("basic "))?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(rest.trim())
        .ok()?;
    let s = std::str::from_utf8(&decoded).ok()?;
    let (u, p) = s.split_once(':')?;
    Some((u.to_string(), p.to_string()))
}

/// Find the offset *just past* the `\r\n\r\n` terminator that ends
/// an HTTP prelude. Returns `None` if the buffer doesn't (yet)
/// contain a full prelude.
fn find_prelude_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

/// RAII guard around an active remote WS connection. On creation it
/// bumps `RemoteState::clients` and emits a `remote/state`
/// broadcast so the local TUI can paint the "remote attached"
/// badge. On drop it decrements and broadcasts again — Drop runs
/// regardless of whether the connection ended cleanly, panicked,
/// or the future was aborted, so the counter never leaks.
struct RemoteClientGuard {
    remote: RemoteState,
    manager: Arc<SessionManager>,
}

impl RemoteClientGuard {
    fn new(remote: RemoteState, manager: Arc<SessionManager>) -> Self {
        let n = remote.add_client();
        manager.broadcast_remote_state(n);
        Self { remote, manager }
    }
}

impl Drop for RemoteClientGuard {
    fn drop(&mut self) {
        let n = self.remote.sub_client();
        self.manager.broadcast_remote_state(n);
    }
}

/// Write a 401 Unauthorized response with a Basic auth
/// challenge. Browsers respond by prompting the user for
/// username + password and replaying the request with an
/// `Authorization: Basic <base64>` header — that's the whole
/// password gate.
async fn write_basic_auth_challenge<W>(wr: &mut W) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt as _;
    let body = b"authentication required";
    let head = format!(
        "HTTP/1.1 401 Unauthorized\r\n\
         WWW-Authenticate: Basic realm=\"agentd\"\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n",
        body.len(),
    );
    wr.write_all(head.as_bytes()).await?;
    wr.write_all(body).await?;
    wr.flush().await
}

/// Write a 3xx redirect to `location`. Used to canonicalize the
/// trailing slash on `/t/<token>` so relative URLs (static JS /
/// CSS) resolve against `/t/<token>/` instead of `/t/`.
async fn write_redirect_response<W>(
    wr: &mut W,
    status: u16,
    location: &str,
) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt as _;
    let reason = match status {
        301 => "Moved Permanently",
        302 => "Found",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        _ => "Redirect",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Location: {location}\r\n\
         Content-Length: 0\r\n\
         Connection: close\r\n\
         \r\n",
    );
    wr.write_all(head.as_bytes()).await?;
    wr.flush().await
}

/// Write a minimal HTTP/1.1 response with the given status, body,
/// and content type. Uses `Connection: close` so the wire shape is
/// dead-simple (no chunked encoding, no keep-alive bookkeeping).
///
/// `Cache-Control: no-store` is set so the phone browser always
/// fetches a fresh copy — without it, Safari and Chrome on iOS /
/// Android cache HTML aggressively over HTTPS even without
/// validators, and the user sees stale UI for hours after we push
/// a fix. The xterm.js bundle (~290KB) re-downloads on each page
/// load as a side effect; acceptable trade for "edits actually
/// appear on next refresh" until we wire up content-addressed
/// asset URLs.
async fn write_http_response<W>(
    wr: &mut W,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt as _;
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\
         X-Content-Type-Options: nosniff\r\n\
         \r\n",
        body.len(),
    );
    wr.write_all(head.as_bytes()).await?;
    wr.write_all(body).await?;
    wr.flush().await
}

async fn handle_ws_connection(
    stream: tokio::net::TcpStream,
    manager: Arc<SessionManager>,
    remote: RemoteState,
) -> Result<()> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    // Token gate + transport demux. The same URL the QR code points
    // at serves two flows:
    //
    //   GET /t/<token>                              → HTML web client
    //   GET /t/<token>  + Upgrade: websocket header → WS handshake
    //
    // tungstenite's upgrade handshake validates the WS-specific
    // headers (`Sec-WebSocket-Key`, etc.) before ever calling its
    // callback, and refuses any 2xx response from the callback —
    // so we can't use `accept_hdr_async` to also serve HTML.
    // Instead we peek the HTTP prelude ourselves, route, and either
    // write a plain HTTP response or hand a replay-prefixed stream
    // to `accept_async`.
    let (mut rd, mut wr) = stream.into_split();

    // Buffered HTTP request prelude. We grow this until we see
    // `\r\n\r\n`, capped at `MAX_HTTP_PRELUDE_BYTES` to keep
    // resource use bounded against a slow / malicious client.
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    let mut chunk = [0u8; 1024];
    let prelude_end = loop {
        let n = match rd.read(&mut chunk).await {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(_) => return Ok(()),
        };
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = find_prelude_end(&buf) {
            break end;
        }
        if buf.len() > MAX_HTTP_PRELUDE_BYTES {
            let _ = write_http_response(
                &mut wr,
                413,
                "Payload Too Large",
                "text/plain; charset=utf-8",
                b"prelude too large",
            )
            .await;
            return Ok(());
        }
    };

    let info = match parse_http_prelude(&buf[..prelude_end]) {
        Some(i) => i,
        None => {
            let _ = write_http_response(
                &mut wr,
                400,
                "Bad Request",
                "text/plain; charset=utf-8",
                b"bad request",
            )
            .await;
            return Ok(());
        }
    };

    let candidate = match token_from_uri_path(&info.path) {
        Some(t) => t.to_string(),
        None => {
            let _ = write_http_response(
                &mut wr,
                403,
                "Forbidden",
                "text/plain; charset=utf-8",
                b"forbidden: missing token; expected /t/<token>",
            )
            .await;
            return Ok(());
        }
    };
    if !remote.token_matches(&candidate) {
        let _ = write_http_response(
            &mut wr,
            403,
            "Forbidden",
            "text/plain; charset=utf-8",
            b"forbidden: invalid token",
        )
        .await;
        return Ok(());
    }

    // HTTP Basic auth gate. Defense-in-depth on top of the token —
    // the URL alone (screenshot, terminal history, bookmark) is no
    // longer enough; the user also has to know the password. The
    // browser prompts natively on the 401 + WWW-Authenticate
    // challenge and remembers credentials within the session so
    // the user types the password exactly once per phone visit.
    // Username MUST equal `REMOTE_USERNAME` ("remote") so the
    // value shown in the popup is the value the user has to type
    // — otherwise the displayed user id would be a polite lie.
    // The same gate runs for the WS upgrade — browsers re-include
    // Authorization on same-origin WS handshakes automatically.
    let creds = info
        .authorization
        .as_deref()
        .and_then(credentials_from_basic_auth);
    let authed = match creds.as_ref() {
        Some((u, p)) => u == crate::remote::REMOTE_USERNAME && remote.password_matches(p),
        None => false,
    };
    if !authed {
        let _ = write_basic_auth_challenge(&mut wr).await;
        return Ok(());
    }

    if !info.is_ws_upgrade {
        // Authenticated plain GET. Route by path suffix so the
        // HTML can pull xterm + css via relative URLs without
        // ever depending on a public CDN — the phone reached the
        // cloudflared tunnel but couldn't reach jsdelivr in the
        // first hand-test.
        let token_prefix = format!("/t/{candidate}");
        let suffix = info.path.strip_prefix(&token_prefix).unwrap_or("");
        match suffix {
            // Bare `/t/<token>` — relative paths in the HTML
            // would resolve against `/t/` rather than
            // `/t/<token>/`, so xterm.js etc. would 404. Redirect
            // once to add the trailing slash; everything else
            // routes to the same handlers either way.
            "" => {
                let location = format!("{token_prefix}/");
                let _ = write_redirect_response(&mut wr, 301, &location).await;
                return Ok(());
            }
            "/" | "/index.html" => {
                let _ = write_http_response(
                    &mut wr,
                    200,
                    "OK",
                    "text/html; charset=utf-8",
                    REMOTE_INDEX_HTML.as_bytes(),
                )
                .await;
                return Ok(());
            }
            "/static/xterm.js" => {
                let _ = write_http_response(
                    &mut wr,
                    200,
                    "OK",
                    "application/javascript; charset=utf-8",
                    REMOTE_XTERM_JS,
                )
                .await;
                return Ok(());
            }
            "/static/xterm.css" => {
                let _ = write_http_response(
                    &mut wr,
                    200,
                    "OK",
                    "text/css; charset=utf-8",
                    REMOTE_XTERM_CSS,
                )
                .await;
                return Ok(());
            }
            "/static/xterm-addon-fit.js" => {
                let _ = write_http_response(
                    &mut wr,
                    200,
                    "OK",
                    "application/javascript; charset=utf-8",
                    REMOTE_XTERM_ADDON_FIT_JS,
                )
                .await;
                return Ok(());
            }
            _ => {
                let _ = write_http_response(
                    &mut wr,
                    404,
                    "Not Found",
                    "text/plain; charset=utf-8",
                    b"not found",
                )
                .await;
                return Ok(());
            }
        }
    }

    // WS upgrade path. We've already consumed the prelude bytes from
    // the TCP stream, so we hand tungstenite a stream that replays
    // `buf` (containing the full prelude tungstenite still wants to
    // parse) before reading further bytes from `rd`. `tokio::io::join`
    // recombines this with `wr` into a single `AsyncRead + AsyncWrite`
    // stream the way `accept_async` expects.
    let replay = std::io::Cursor::new(buf).chain(rd);
    let joined = tokio::io::join(replay, wr);
    let ws_stream = tokio_tungstenite::accept_async(joined)
        .await
        .context("ws upgrade")?;

    // From here on the WS is open and we're about to run a real
    // session loop. Bump the active-client counter (with a RAII
    // guard) and broadcast `remote/state` so the local TUI repaints
    // its badge. Drop runs on any exit path — clean close, panic,
    // task abort — so the counter doesn't leak.
    let _client_guard = RemoteClientGuard::new(remote, manager.clone());

    let (mut ws_tx, ws_rx) = ws_stream.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<serde_json::Value>();

    // Outbound writer: drain the shared channel and emit each value
    // as a single WS text frame.
    let writer_task = tokio::spawn(async move {
        while let Some(v) = out_rx.recv().await {
            let s = match serde_json::to_string(&v) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if ws_tx.send(tungstenite::Message::Text(s.into())).await.is_err() {
                break;
            }
        }
    });

    let inbound = ReadFromWs { rx: ws_rx };
    run_session(inbound, out_tx, manager, ClientKind::Remote).await;
    writer_task.abort();
    Ok(())
}

async fn run_subscription_loop(
    manager: Arc<SessionManager>,
    out_tx: mpsc::UnboundedSender<serde_json::Value>,
    mut cmd_rx: mpsc::Receiver<SubCmd>,
) {
    let mut sub_rx: Option<broadcast::Receiver<BroadcastMsg>> = None;
    let mut filter: Option<String> = None;

    loop {
        if let Some(rx) = sub_rx.as_mut() {
            tokio::select! {
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(SubCmd::Subscribe(f)) => {
                            filter = f;
                            sub_rx = Some(manager.subscribe());
                        }
                        Some(SubCmd::Unsubscribe) => {
                            sub_rx = None;
                            filter = None;
                        }
                        None => return,
                    }
                }
                msg = rx.recv() => {
                    match msg {
                        Ok(m) => {
                            forward_broadcast(&out_tx, &filter, m);
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(skipped = n, "subscriber lagged");
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            sub_rx = None;
                        }
                    }
                }
            }
        } else {
            match cmd_rx.recv().await {
                Some(SubCmd::Subscribe(f)) => {
                    filter = f;
                    sub_rx = Some(manager.subscribe());
                }
                Some(SubCmd::Unsubscribe) => {}
                None => return,
            }
        }
    }
}

fn forward_broadcast(
    out_tx: &mpsc::UnboundedSender<serde_json::Value>,
    filter: &Option<String>,
    msg: BroadcastMsg,
) {
    if let Some(f) = filter {
        let matches = match &msg {
            BroadcastMsg::Event(e) => e.session_id == *f,
            BroadcastMsg::State(s) => s.session.id == *f,
            BroadcastMsg::Deleted(d) => d.session_id == *f,
            // Group + remote-state notifications aren't session-
            // specific; always forward even when a session filter
            // is set so the local TUI's remote badge stays accurate
            // while a single-session view is active.
            BroadcastMsg::GroupState(_)
            | BroadcastMsg::GroupDeleted(_)
            | BroadcastMsg::RemoteState(_) => true,
        };
        if !matches {
            return;
        }
    }
    let notif = match msg {
        BroadcastMsg::Event(e) => {
            let p = match serde_json::to_value(&e) {
                Ok(v) => v,
                Err(_) => return,
            };
            Notification::new(ipc_notif::EVENT, Some(p))
        }
        BroadcastMsg::State(s) => {
            let p = match serde_json::to_value(&s) {
                Ok(v) => v,
                Err(_) => return,
            };
            Notification::new(ipc_notif::STATE, Some(p))
        }
        BroadcastMsg::Deleted(d) => {
            let p = match serde_json::to_value(&d) {
                Ok(v) => v,
                Err(_) => return,
            };
            Notification::new(ipc_notif::DELETED, Some(p))
        }
        BroadcastMsg::GroupState(g) => {
            let p = match serde_json::to_value(&g) {
                Ok(v) => v,
                Err(_) => return,
            };
            Notification::new(ipc_notif::GROUP_STATE, Some(p))
        }
        BroadcastMsg::GroupDeleted(g) => {
            let p = match serde_json::to_value(&g) {
                Ok(v) => v,
                Err(_) => return,
            };
            Notification::new(ipc_notif::GROUP_DELETED, Some(p))
        }
        BroadcastMsg::RemoteState(r) => {
            let p = match serde_json::to_value(&r) {
                Ok(v) => v,
                Err(_) => return,
            };
            Notification::new(ipc_notif::REMOTE_STATE, Some(p))
        }
    };
    let v = match serde_json::to_value(&notif) {
        Ok(v) => v,
        Err(_) => return,
    };
    let _ = out_tx.send(v);
}

fn parse_params<T: serde::de::DeserializeOwned>(
    params: Option<serde_json::Value>,
) -> Result<T, ErrorObject> {
    let v = params.unwrap_or(serde_json::Value::Null);
    serde_json::from_value(v).map_err(|e| ErrorObject::invalid_params(e.to_string()))
}

async fn dispatch(
    manager: &Arc<SessionManager>,
    sub_cmd_tx: &mpsc::Sender<SubCmd>,
    kind: ClientKind,
    req: Request,
) -> Response {
    let id = req.id.clone();
    macro_rules! ok {
        ($v:expr) => {
            match serde_json::to_value($v) {
                Ok(v) => Response::ok(id.clone(), v),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        };
    }
    macro_rules! params {
        ($t:ty) => {{
            match parse_params::<$t>(req.params.clone()) {
                Ok(p) => p,
                Err(e) => return Response::err(id.clone(), e),
            }
        }};
    }
    match req.method.as_str() {
        m if m == ipc_method::PING => ok!(&PingResult {
            pong: true,
            version: IPC_VERSION.to_string(),
        }),
        m if m == ipc_method::HARNESS_LIST => ok!(&manager.harnesses()),
        m if m == ipc_method::SESSION_LIST => ok!(&manager.list().await),
        m if m == ipc_method::SESSION_CREATE => {
            let p = params!(CreateSessionParams);
            match manager.create(p).await {
                Ok(sid) => Response::ok(id.clone(), json!({ "session_id": sid })),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_GET => {
            let p = params!(SessionIdParams);
            match manager.detail(&p.session_id).await {
                Ok(d) => ok!(&d),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_INPUT => {
            let p = params!(SessionInputParams);
            match manager.send_input(&p.session_id, p.text).await {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_PTY_INPUT => {
            let p = params!(SessionPtyInputParams);
            let bytes = match p.decode() {
                Ok(b) => b,
                Err(e) => return Response::err(id.clone(), ErrorObject::invalid_params(e.to_string())),
            };
            // Mark this client kind as the active one for the
            // session and re-resize the PTY to its last-known
            // viewport. The OS PTY only has one size; "active
            // wins" lets users alternate between TUI and phone
            // without losing their preferred geometry.
            manager.note_pty_activity(&p.session_id, kind, None).await;
            match manager.pty_input(&p.session_id, bytes).await {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_PTY_RESIZE => {
            let p = params!(SessionPtyResizeParams);
            // Resize is an explicit "I want this viewport"
            // signal — store it for this client kind, mark
            // active, and apply.
            manager
                .note_pty_activity(&p.session_id, kind, Some((p.cols, p.rows)))
                .await;
            match manager.pty_resize(&p.session_id, p.cols, p.rows).await {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_PTY_REPLAY => {
            let p = params!(SessionIdParams);
            match manager.pty_replay(&p.session_id).await {
                Ok(r) => ok!(&r),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_INTERRUPT => {
            let p = params!(SessionIdParams);
            match manager.interrupt(&p.session_id).await {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_STOP => {
            let p = params!(SessionIdParams);
            match manager.stop(&p.session_id).await {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_KILL => {
            let p = params!(SessionIdParams);
            match manager.kill(&p.session_id).await {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_DELETE => {
            let p = params!(SessionIdParams);
            match manager.delete(&p.session_id).await {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_RESTART => {
            let p = params!(SessionIdParams);
            match manager.clone().restart(&p.session_id).await {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_SET_PINNED => {
            let p = params!(SessionSetPinnedParams);
            match manager.set_pinned(&p.session_id, p.pinned).await {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_SET_TITLE => {
            let p = params!(SessionSetTitleParams);
            match manager.set_title(&p.session_id, p.title).await {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_SET_AUTOMODE => {
            let p = params!(SessionSetAutomodeParams);
            match manager.set_automode(&p.session_id, p.on).await {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_TOOL_DECISION => {
            let p = params!(SessionToolDecisionParams);
            match manager
                .tool_decision(&p.session_id, p.call_id, p.decision)
                .await
            {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_TOOL_ACTION => {
            let p = params!(SessionToolActionParams);
            match manager
                .tool_action(&p.session_id, p.call_id, p.action)
                .await
            {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_LIST_TASKS => {
            let p = params!(agentd_protocol::ListTasksParams);
            match manager.list_tasks(&p.session_id).await {
                Ok(tasks) => Response::ok(
                    id.clone(),
                    serde_json::to_value(agentd_protocol::ListTasksResult { tasks })
                        .unwrap_or(serde_json::Value::Null),
                ),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::LOOP_CREATE => {
            let p = params!(agentd_protocol::LoopCreateParams);
            match manager.loop_create(p).await {
                Ok(l) => Response::ok(
                    id.clone(),
                    serde_json::to_value(&l).unwrap_or(serde_json::Value::Null),
                ),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::LOOP_LIST => {
            let p = params!(agentd_protocol::LoopListParams);
            let loops = manager.loop_list(p.session_id.as_deref()).await;
            Response::ok(
                id.clone(),
                serde_json::to_value(agentd_protocol::LoopListResult { loops })
                    .unwrap_or(serde_json::Value::Null),
            )
        }
        m if m == ipc_method::LOOP_UPDATE => {
            let p = params!(agentd_protocol::LoopUpdateParams);
            match manager.loop_update(p).await {
                Ok(l) => Response::ok(
                    id.clone(),
                    serde_json::to_value(&l).unwrap_or(serde_json::Value::Null),
                ),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::LOOP_REMOVE => {
            let p = params!(agentd_protocol::LoopRemoveParams);
            match manager.loop_remove(&p.loop_id).await {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_MOVE => {
            let p = params!(SessionMoveParams);
            match manager.move_session(&p.session_id, p.direction).await {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_SET_GROUP => {
            let p = params!(SessionSetGroupParams);
            match manager
                .set_session_group(&p.session_id, p.group_id, p.position)
                .await
            {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::GROUP_LIST => ok!(&manager.list_groups().await),
        m if m == ipc_method::GROUP_CREATE => {
            let p = params!(GroupCreateParams);
            match manager.create_group(p.name).await {
                Ok(gid) => ok!(&GroupCreateResult { group_id: gid }),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::GROUP_RENAME => {
            let p = params!(GroupRenameParams);
            match manager.rename_group(&p.group_id, p.name).await {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::GROUP_DELETE => {
            // Accept the new `GroupDeleteParams` shape (with optional
            // `delete_members`); older clients sending the bare
            // `{"group_id": "…"}` payload deserialize too because
            // `delete_members` is `#[serde(default)]`.
            let p = params!(GroupDeleteParams);
            match manager.delete_group(&p.group_id, p.delete_members).await {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::GROUP_SET_COLLAPSED => {
            let p = params!(GroupSetCollapsedParams);
            match manager.set_group_collapsed(&p.group_id, p.collapsed).await {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::GROUP_MOVE => {
            let p = params!(GroupMoveParams);
            match manager.move_group(&p.group_id, p.direction).await {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_DIFF => {
            let p = params!(SessionIdParams);
            match manager.diff(&p.session_id).await {
                Ok(patch) => Response::ok(id.clone(), json!({ "patch": patch })),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_TRANSCRIPT => {
            let p = params!(TranscriptParams);
            match manager.transcript(&p.session_id, p.from, p.limit).await {
                Ok(r) => ok!(&r),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SUBSCRIBE_EVENTS => {
            let p = params!(SubscribeParams);
            let _ = sub_cmd_tx.send(SubCmd::Subscribe(p.session_id)).await;
            Response::ok(id.clone(), serde_json::Value::Null)
        }
        m if m == ipc_method::UNSUBSCRIBE_EVENTS => {
            let _ = sub_cmd_tx.send(SubCmd::Unsubscribe).await;
            Response::ok(id.clone(), serde_json::Value::Null)
        }
        m if m == ipc_method::REMOTE_START => {
            // Params default to "tunnel mode" (the user-typed
            // `/remote-control` path). `local_only=true` is the
            // `/remote-control-debug` path — bind the local
            // listener only, never wait for cloudflared.
            let params: agentd_protocol::RemoteStartParams = match parse_params(req.params.clone()) {
                Ok(p) => p,
                Err(e) => return Response::err(id.clone(), e),
            };
            match manager.clone().start_remote(None, params).await {
                Ok(r) => ok!(&r),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::REMOTE_STOP => {
            // No params (`remote.stop` is a verb). Returns the
            // `was_running` flag so the CLI can render different
            // status messages for "we stopped it" vs "nothing to
            // stop" — both are Ok responses, not errors.
            match manager.clone().stop_remote().await {
                Ok(r) => ok!(&r),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::DAEMON_RESTART => {
            // Hand off to main's `tokio::select!` arm which calls
            // `exec()` on the resolved current_exe. The reply
            // races the kernel: we send it back here, then the
            // IPC socket closes when exec() replaces the process
            // image. Clients detect that as a disconnect and
            // reconnect.
            match manager.request_daemon_restart() {
                Ok(cmd) => {
                    // Cloudflared survives the daemon's exec()
                    // because it's spawned in a separate process
                    // group. The new daemon adopts it via the
                    // persisted snapshot, so the public URL +
                    // password stay valid across the restart. We
                    // report that to the caller — `false` only if
                    // remote was never running, or if the snapshot
                    // is missing for some reason.
                    let tunnel_preserved = manager
                        .remote_slot()
                        .ok()
                        .and_then(|g| g.as_ref().map(|h| h.state.tunnel_pid()))
                        .map(|pid| pid != 0)
                        .unwrap_or(false);
                    let r = agentd_protocol::DaemonRestartResult {
                        exe: cmd.exe.display().to_string(),
                        pid: std::process::id(),
                        tunnel_preserved,
                    };
                    ok!(&r)
                }
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        other => Response::err(id.clone(), ErrorObject::method_not_found(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `find_prelude_end` locates the `\r\n\r\n` separator that ends
    /// an HTTP/1.1 request prelude. Returns `None` while the buffer
    /// still has only a partial prelude, so the I/O loop knows to
    /// read more.
    #[test]
    fn find_prelude_end_locates_double_crlf() {
        let buf = b"GET / HTTP/1.1\r\nHost: x\r\n\r\nbody-bytes";
        assert_eq!(find_prelude_end(buf), Some(buf.len() - "body-bytes".len()));

        let partial = b"GET / HTTP/1.1\r\nHost: x\r\n";
        assert_eq!(find_prelude_end(partial), None);

        let empty = b"";
        assert_eq!(find_prelude_end(empty), None);
    }

    /// Plain `GET` request — path captured, `is_ws_upgrade` is false
    /// because there's no `Upgrade` header.
    #[test]
    fn parses_plain_get() {
        let buf = b"GET /t/abc HTTP/1.1\r\nHost: x\r\n\r\n";
        let info = parse_http_prelude(buf).expect("should parse");
        assert_eq!(info.path, "/t/abc");
        assert!(!info.is_ws_upgrade);
    }

    /// A real WS upgrade request — path captured and the upgrade
    /// flag set. Case-insensitive on both the header name and value
    /// (some clients lowercase `websocket`).
    #[test]
    fn parses_ws_upgrade() {
        let buf = b"GET /t/abc HTTP/1.1\r\n\
                    Host: x\r\n\
                    Upgrade: WebSocket\r\n\
                    Connection: Upgrade\r\n\
                    Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                    Sec-WebSocket-Version: 13\r\n\
                    \r\n";
        let info = parse_http_prelude(buf).expect("should parse");
        assert_eq!(info.path, "/t/abc");
        assert!(info.is_ws_upgrade);
    }

    /// Malformed input returns `None` so the caller writes a 400.
    #[test]
    fn parses_malformed_returns_none() {
        let buf = b"not an http request";
        assert!(parse_http_prelude(buf).is_none());
    }
}
