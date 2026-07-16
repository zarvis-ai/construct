//! SSH clipboard bridge (spec 0098).
//!
//! `construct ssh <dest>` runs on the machine the user is physically at: it
//! serves that machine's clipboard over a private Unix socket, reverse-forwards
//! the socket over the SSH connection, and launches `construct` on the remote
//! host with `CONSTRUCT_CLIPBOARD_SOCK` pointing at the forwarded end. The
//! remote TUI then prefers the bridge for clipboard traffic: selection copies
//! land on the local clipboard (which OSC 52 can't reach in e.g. macOS
//! Terminal.app), and paste can carry images/files, which OSC 52 never could.
//!
//! The protocol is a private JSON-lines request/response over the socket —
//! one request per line, one response line back:
//!
//! ```text
//! → {"op":"copy","data":"<base64>","mime":"text/plain; charset=utf-8"}
//! ← {"ok":true}
//! → {"op":"paste"}
//! ← {"ok":true,"data":"<base64>","mime":"image/png","filename":"clipboard.png"}
//! ← {"ok":true}                      (empty clipboard: no data field)
//! ← {"ok":false,"error":"..."}
//! ```
//!
//! Both ends are this binary; the protocol is not a public surface and can
//! change freely as long as the graceful-degradation rules in spec 0098 hold.

use anyhow::{Context, Result};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

pub(crate) const ENV_SOCK: &str = "CONSTRUCT_CLIPBOARD_SOCK";

/// Cap on a single clipboard payload (either direction), pre-encoding.
/// Clipboard items are user-scale (text, a screenshot, a dragged file);
/// the cap bounds transfer time over slow links and memory on both ends.
const MAX_ITEM_BYTES: usize = 32 * 1024 * 1024;

/// Per-syscall socket timeout for the TUI-side client. Connect failures
/// (dead bridge) surface immediately; this only bounds a live-but-stuck
/// agent so a copy/paste can never wedge the TUI for long.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Request {
    Copy { data: String, mime: String },
    Paste,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Response {
    ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    data: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mime: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    filename: Option<String>,
}

/// One clipboard payload crossing the bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PasteItem {
    pub bytes: Vec<u8>,
    pub mime: String,
    /// Original name for file payloads; None for text/images taken straight
    /// off the pasteboard.
    pub filename: Option<String>,
}

impl PasteItem {
    pub fn text(text: String) -> Self {
        Self {
            bytes: text.into_bytes(),
            mime: "text/plain; charset=utf-8".to_string(),
            filename: None,
        }
    }

    pub fn is_text(&self) -> bool {
        self.mime.starts_with("text/")
    }

    /// Attachment name when the pasteboard item has no natural one.
    pub fn default_filename(&self) -> String {
        let ext = match self.mime.split(';').next().unwrap_or("").trim() {
            "image/png" => "png",
            "image/jpeg" => "jpg",
            "image/gif" => "gif",
            "image/webp" => "webp",
            "image/tiff" => "tiff",
            "application/pdf" => "pdf",
            _ => "bin",
        };
        format!("clipboard.{ext}")
    }
}

/// The bridge socket handed to this process, if any. Set by `construct ssh`
/// on the remote command line; absent in every non-bridged invocation.
pub(crate) fn socket_from_env() -> Option<PathBuf> {
    let v = std::env::var_os(ENV_SOCK)?;
    if v.is_empty() {
        return None;
    }
    Some(PathBuf::from(v))
}

// ---------------------------------------------------------------------------
// TUI-side client (remote end). Synchronous on purpose: callers sit in the
// copy/paste paths of the event loop and want bounded, simple IO.
// ---------------------------------------------------------------------------

fn roundtrip(sock: &Path, req: &Request) -> Result<Response> {
    let mut stream = std::os::unix::net::UnixStream::connect(sock)
        .with_context(|| format!("connect clipboard bridge at {}", sock.display()))?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let mut buf = String::new();
    reader.read_line(&mut buf).context("read bridge response")?;
    let resp: Response =
        serde_json::from_str(buf.trim()).context("parse bridge response")?;
    if !resp.ok {
        anyhow::bail!(
            "bridge error: {}",
            resp.error.unwrap_or_else(|| "unknown".to_string())
        );
    }
    Ok(resp)
}

/// Send `bytes` to the local machine's clipboard.
pub(crate) fn copy(sock: &Path, bytes: &[u8], mime: &str) -> Result<()> {
    if bytes.len() > MAX_ITEM_BYTES {
        anyhow::bail!("clipboard payload too large ({} bytes)", bytes.len());
    }
    let req = Request::Copy {
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
        mime: mime.to_string(),
    };
    roundtrip(sock, &req).map(|_| ())
}

/// Fetch the local machine's clipboard. `Ok(None)` = empty clipboard.
pub(crate) fn paste(sock: &Path) -> Result<Option<PasteItem>> {
    let resp = roundtrip(sock, &Request::Paste)?;
    let Some(data) = resp.data else {
        return Ok(None);
    };
    let bytes = decode_b64_capped(&data, MAX_ITEM_BYTES)?;
    Ok(Some(PasteItem {
        bytes,
        mime: resp
            .mime
            .unwrap_or_else(|| "text/plain; charset=utf-8".to_string()),
        filename: resp.filename,
    }))
}

fn decode_b64_capped(data: &str, max: usize) -> Result<Vec<u8>> {
    // Base64 expands 3 bytes to 4 chars; reject before decoding so a huge
    // payload never materializes.
    if data.len() > max / 3 * 4 + 4 {
        anyhow::bail!("clipboard payload too large ({} base64 chars)", data.len());
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .context("decode clipboard payload")?;
    if bytes.len() > max {
        anyhow::bail!("clipboard payload too large ({} bytes)", bytes.len());
    }
    Ok(bytes)
}

// ---------------------------------------------------------------------------
// Agent (local end): serve the system clipboard over a Unix socket.
// ---------------------------------------------------------------------------

/// What the agent serves. Trait-shaped so tests can bridge a fake clipboard;
/// the real one shells out to the platform tools.
pub(crate) trait ClipboardBackend: Send + Sync + 'static {
    fn copy(&self, bytes: &[u8], mime: &str) -> Result<()>;
    fn paste(&self) -> Result<Option<PasteItem>>;
}

pub(crate) async fn serve(
    listener: tokio::net::UnixListener,
    backend: Arc<dyn ClipboardBackend>,
) {
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(e) => {
                tracing::debug!(error = %e, "clipboard bridge accept failed");
                continue;
            }
        };
        let backend = backend.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(stream, backend).await {
                tracing::debug!(error = %e, "clipboard bridge connection ended");
            }
        });
    }
}

async fn handle_conn(
    stream: tokio::net::UnixStream,
    backend: Arc<dyn ClipboardBackend>,
) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let (r, mut w) = stream.into_split();
    let mut lines = tokio::io::BufReader::new(r).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<Request>(&line) {
            Ok(req) => handle_request(req, backend.clone()).await,
            Err(e) => Response {
                ok: false,
                error: Some(format!("bad request: {e}")),
                ..Default::default()
            },
        };
        let mut out = serde_json::to_string(&resp)?;
        out.push('\n');
        w.write_all(out.as_bytes()).await?;
    }
    Ok(())
}

async fn handle_request(req: Request, backend: Arc<dyn ClipboardBackend>) -> Response {
    // Platform clipboard tools block; keep the accept loop responsive.
    let result = tokio::task::spawn_blocking(move || match req {
        Request::Copy { data, mime } => {
            let bytes = decode_b64_capped(&data, MAX_ITEM_BYTES)?;
            backend.copy(&bytes, &mime)?;
            Ok(Response {
                ok: true,
                ..Default::default()
            })
        }
        Request::Paste => {
            let item = backend.paste()?;
            Ok(match item {
                None => Response {
                    ok: true,
                    ..Default::default()
                },
                Some(item) => {
                    if item.bytes.len() > MAX_ITEM_BYTES {
                        anyhow::bail!(
                            "clipboard item too large ({} bytes)",
                            item.bytes.len()
                        );
                    }
                    Response {
                        ok: true,
                        data: Some(
                            base64::engine::general_purpose::STANDARD.encode(&item.bytes),
                        ),
                        mime: Some(item.mime),
                        filename: item.filename,
                        ..Default::default()
                    }
                }
            })
        }
    })
    .await;
    match result {
        Ok(Ok(resp)) => resp,
        Ok(Err(e)) => Response {
            ok: false,
            error: Some(e.to_string()),
            ..Default::default()
        },
        Err(e) => Response {
            ok: false,
            error: Some(format!("bridge task failed: {e}")),
            ..Default::default()
        },
    }
}

/// The real system clipboard, via platform tools: `pbcopy`/`pbpaste` on
/// macOS (plus `osascript` for image and file payloads), `wl-copy`/`xclip`
/// fallbacks elsewhere.
pub(crate) struct SystemClipboard;

impl ClipboardBackend for SystemClipboard {
    fn copy(&self, bytes: &[u8], mime: &str) -> Result<()> {
        if !mime.starts_with("text/") {
            anyhow::bail!("bridge copy supports text only (got {mime})");
        }
        let candidates: &[(&str, &[&str])] = &[
            ("pbcopy", &[]),
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard"]),
        ];
        let mut last_err = None;
        for (bin, args) in candidates {
            match pipe_to_command(bin, args, bytes) {
                Ok(()) => return Ok(()),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no clipboard tool available")))
    }

    fn paste(&self) -> Result<Option<PasteItem>> {
        #[cfg(target_os = "macos")]
        {
            // Richest type first: a copied image, then a copied file, then
            // text. `pbpaste` alone would flatten an image to nothing and a
            // Finder file-copy to just its name.
            if let Some(png) = macos_clipboard_png() {
                return Ok(Some(PasteItem {
                    bytes: png,
                    mime: "image/png".to_string(),
                    filename: None,
                }));
            }
            if let Some(item) = macos_clipboard_file()? {
                return Ok(Some(item));
            }
        }
        let candidates: &[(&str, &[&str])] = &[
            ("pbpaste", &[]),
            ("wl-paste", &["--no-newline"]),
            ("xclip", &["-selection", "clipboard", "-o"]),
        ];
        let mut last_err = None;
        for (bin, args) in candidates {
            match read_from_command(bin, args) {
                Ok(text) if text.is_empty() => return Ok(None),
                Ok(text) => return Ok(Some(PasteItem::text(text))),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no clipboard tool available")))
    }
}

fn pipe_to_command(bin: &str, args: &[&str], bytes: &[u8]) -> Result<()> {
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("spawn {bin}"))?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(bytes)?;
    }
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("{bin} exited with {status}")
    }
}

fn read_from_command(bin: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(bin)
        .args(args)
        .output()
        .with_context(|| format!("spawn {bin}"))?;
    // Empty-clipboard exits nonzero in some tools (wl-paste); treat any
    // successful spawn as an authoritative (possibly empty) answer.
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// PNG bytes of a copied image, if the pasteboard holds one. Uses the
/// AppleScript coercion because `pbpaste` cannot emit image flavors.
#[cfg(target_os = "macos")]
fn macos_clipboard_png() -> Option<Vec<u8>> {
    let out = Command::new("osascript")
        .args(["-e", "the clipboard as \u{ab}class PNGf\u{bb}"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_osascript_data(&String::from_utf8_lossy(&out.stdout))
}

/// A file copied in Finder, read off disk. Falls back to None when the
/// pasteboard holds no file URL.
#[cfg(target_os = "macos")]
fn macos_clipboard_file() -> Result<Option<PasteItem>> {
    let out = Command::new("osascript")
        .args([
            "-e",
            "POSIX path of (the clipboard as \u{ab}class furl\u{bb})",
        ])
        .stderr(Stdio::null())
        .output();
    let out = match out {
        Ok(out) if out.status.success() => out,
        _ => return Ok(None),
    };
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if path.is_empty() {
        return Ok(None);
    }
    let path = PathBuf::from(path);
    let meta = std::fs::metadata(&path)
        .with_context(|| format!("stat copied file {}", path.display()))?;
    if !meta.is_file() {
        return Ok(None);
    }
    if meta.len() as usize > MAX_ITEM_BYTES {
        anyhow::bail!(
            "copied file {} is too large ({} bytes)",
            path.display(),
            meta.len()
        );
    }
    let bytes = std::fs::read(&path)
        .with_context(|| format!("read copied file {}", path.display()))?;
    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned());
    let mime = mime_for_path(&path).to_string();
    Ok(Some(PasteItem {
        bytes,
        mime,
        filename,
    }))
}

#[cfg(target_os = "macos")]
fn mime_for_path(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("tiff") | Some("tif") => "image/tiff",
        Some("pdf") => "application/pdf",
        Some("txt") | Some("md") | Some("log") => "text/plain",
        _ => "application/octet-stream",
    }
}

/// Parse osascript's raw-data literal: `«data PNGf89504E47…»` — a 4-char
/// type code followed by hex. Returns the decoded bytes.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_osascript_data(s: &str) -> Option<Vec<u8>> {
    let start = s.find("\u{ab}data ")? + "\u{ab}data ".len();
    let rest = &s[start..];
    let end = rest.find('\u{bb}')?;
    let payload = rest[..end].trim();
    // Skip the 4-character type code (PNGf, TIFF, …).
    let hex = payload.get(4..)?;
    if hex.is_empty() || hex.len() % 2 != 0 {
        return None;
    }
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    for chunk in hex.as_bytes().chunks(2) {
        let pair = std::str::from_utf8(chunk).ok()?;
        bytes.push(u8::from_str_radix(pair, 16).ok()?);
    }
    Some(bytes)
}

// ---------------------------------------------------------------------------
// `construct ssh`: agent + tunnel + remote TUI in one command.
// ---------------------------------------------------------------------------

/// Run `ssh` with a clipboard bridge attached. Blocks until ssh exits and
/// returns its exit code; the agent and both socket files die with us
/// (tempdir cleanup) — the remote socket file may linger in the remote /tmp,
/// which `StreamLocalBindUnlink` makes harmless.
pub(crate) async fn run_ssh(
    ssh_args: Vec<OsString>,
    remote_cmd: Option<String>,
) -> Result<i32> {
    use std::os::unix::fs::PermissionsExt;

    // Owner-only socket in an owner-only dir: the clipboard carries secrets,
    // and both ends may sit on multi-user hosts. Short prefix keeps the
    // path well under the Unix socket path limit.
    let dir = tempfile::Builder::new()
        .prefix("construct-clip-")
        .tempdir()
        .context("create clipboard bridge dir")?;
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))?;
    let local_sock = dir.path().join("clip.sock");
    let listener = tokio::net::UnixListener::bind(&local_sock)
        .with_context(|| format!("bind clipboard bridge at {}", local_sock.display()))?;
    std::fs::set_permissions(&local_sock, std::fs::Permissions::from_mode(0o600))?;

    let remote_sock = format!("/tmp/construct-clip-{}.sock", socket_nonce());
    tokio::spawn(serve(listener, Arc::new(SystemClipboard)));

    let argv = build_ssh_argv(&ssh_args, &local_sock, &remote_sock, remote_cmd.as_deref());
    let status = tokio::process::Command::new("ssh")
        .args(&argv)
        .status()
        .await
        .context("spawn ssh (is OpenSSH installed?)")?;
    Ok(status.code().unwrap_or(1))
}

/// Unique-enough suffix for the remote socket path so concurrent bridges to
/// the same host never collide. Not a secret — the socket's 0600 mode is the
/// access control.
fn socket_nonce() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("{:x}", nanos ^ ((std::process::id() as u64) << 32))
}

/// Assemble the ssh argv: our forward + tty flags first, the user's args
/// verbatim, then the remote command as the final argument. `env` (not a
/// `VAR=x` prefix) carries the socket path so it works under any remote
/// login shell, csh included.
fn build_ssh_argv(
    user_args: &[OsString],
    local_sock: &Path,
    remote_sock: &str,
    remote_cmd: Option<&str>,
) -> Vec<OsString> {
    let mut argv: Vec<OsString> = vec![
        // Replace a stale leftover socket from a crashed prior bridge
        // instead of failing the forward.
        "-o".into(),
        "StreamLocalBindUnlink=yes".into(),
        "-R".into(),
        format!("{}:{}", remote_sock, local_sock.display()).into(),
        // Passing a remote command disables tty allocation; the TUI needs one.
        "-t".into(),
    ];
    argv.extend(user_args.iter().cloned());
    let cmd = remote_cmd.unwrap_or("construct");
    argv.push(format!("env {ENV_SOCK}={remote_sock} {cmd}").into());
    argv
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn ssh_argv_wraps_user_args_and_appends_remote_cmd() {
        let argv = build_ssh_argv(
            &["-p".into(), "2222".into(), "devbox".into()],
            Path::new("/tmp/x/clip.sock"),
            "/tmp/construct-clip-abc.sock",
            None,
        );
        let argv: Vec<String> = argv
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            argv,
            vec![
                "-o",
                "StreamLocalBindUnlink=yes",
                "-R",
                "/tmp/construct-clip-abc.sock:/tmp/x/clip.sock",
                "-t",
                "-p",
                "2222",
                "devbox",
                "env CONSTRUCT_CLIPBOARD_SOCK=/tmp/construct-clip-abc.sock construct",
            ]
        );
    }

    #[test]
    fn ssh_argv_honors_remote_cmd_override() {
        let argv = build_ssh_argv(
            &["devbox".into()],
            Path::new("/l.sock"),
            "/r.sock",
            Some("/opt/construct/bin/construct --socket /x"),
        );
        let last = argv.last().unwrap().to_string_lossy().into_owned();
        assert_eq!(
            last,
            "env CONSTRUCT_CLIPBOARD_SOCK=/r.sock /opt/construct/bin/construct --socket /x"
        );
    }

    #[test]
    fn osascript_data_literal_parses_to_bytes() {
        let out = "\u{ab}data PNGf89504E470D0A1A0A\u{bb}\n";
        let bytes = parse_osascript_data(out).expect("parses");
        assert_eq!(bytes, vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);
    }

    #[test]
    fn osascript_data_rejects_malformed_output() {
        assert_eq!(parse_osascript_data("execution error: ..."), None);
        assert_eq!(parse_osascript_data("\u{ab}data PNGf\u{bb}"), None);
        assert_eq!(parse_osascript_data("\u{ab}data PNGfZZ\u{bb}"), None);
    }

    #[test]
    fn b64_cap_rejects_oversized_payloads() {
        let data = base64::engine::general_purpose::STANDARD.encode(vec![0u8; 64]);
        assert!(decode_b64_capped(&data, 64).is_ok());
        assert!(decode_b64_capped(&data, 16).is_err());
    }

    #[test]
    fn paste_item_default_filenames_follow_mime() {
        let png = PasteItem {
            bytes: vec![],
            mime: "image/png".into(),
            filename: None,
        };
        assert_eq!(png.default_filename(), "clipboard.png");
        let blob = PasteItem {
            bytes: vec![],
            mime: "application/octet-stream".into(),
            filename: None,
        };
        assert_eq!(blob.default_filename(), "clipboard.bin");
        assert!(PasteItem::text("x".into()).is_text());
        assert!(!png.is_text());
    }

    /// Fake clipboard: records copies, serves a canned paste.
    struct MockBackend {
        copied: Mutex<Option<(Vec<u8>, String)>>,
        paste: Option<PasteItem>,
    }

    impl ClipboardBackend for MockBackend {
        fn copy(&self, bytes: &[u8], mime: &str) -> Result<()> {
            *self.copied.lock().unwrap() = Some((bytes.to_vec(), mime.to_string()));
            Ok(())
        }
        fn paste(&self) -> Result<Option<PasteItem>> {
            Ok(self.paste.clone())
        }
    }

    async fn start_bridge(backend: Arc<MockBackend>) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("clip.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        tokio::spawn(serve(listener, backend));
        (dir, sock)
    }

    #[tokio::test]
    async fn bridge_roundtrips_copy_and_paste() {
        let backend = Arc::new(MockBackend {
            copied: Mutex::new(None),
            paste: Some(PasteItem {
                bytes: vec![1, 2, 3],
                mime: "image/png".into(),
                filename: Some("shot.png".into()),
            }),
        });
        let (_dir, sock) = start_bridge(backend.clone()).await;

        let sock2 = sock.clone();
        tokio::task::spawn_blocking(move || copy(&sock2, b"hello", "text/plain"))
            .await
            .unwrap()
            .expect("copy succeeds");
        assert_eq!(
            backend.copied.lock().unwrap().clone(),
            Some((b"hello".to_vec(), "text/plain".to_string()))
        );

        let item = tokio::task::spawn_blocking(move || paste(&sock))
            .await
            .unwrap()
            .expect("paste succeeds")
            .expect("clipboard not empty");
        assert_eq!(item.bytes, vec![1, 2, 3]);
        assert_eq!(item.mime, "image/png");
        assert_eq!(item.filename.as_deref(), Some("shot.png"));
    }

    #[tokio::test]
    async fn bridge_reports_empty_clipboard_as_none() {
        let backend = Arc::new(MockBackend {
            copied: Mutex::new(None),
            paste: None,
        });
        let (_dir, sock) = start_bridge(backend).await;
        let item = tokio::task::spawn_blocking(move || paste(&sock))
            .await
            .unwrap()
            .expect("paste succeeds");
        assert!(item.is_none());
    }
}
