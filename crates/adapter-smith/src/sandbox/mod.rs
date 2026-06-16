//! Pluggable OS-level sandbox for smith's tool execution (spec 0029).
//!
//! A [`Sandbox`] backend turns a [`SandboxPolicy`] into real enforcement around
//! a spawned command (`shell`/`proc`) or a file write (`edit_file`). Backends
//! are swappable: [`seatbelt`] (macOS), `Noop` (unsupported/disabled →
//! cooperative gate only), and future bubblewrap/Landlock/custom backends.
//!
//! Selected once at startup via [`select`]. **Opt-in** for now:
//! `CONSTRUCT_SMITH_SANDBOX=auto|seatbelt|none` (default `none` → `Noop`), so
//! default behavior is unchanged while the first cut is validated.

mod policy;
#[cfg(target_os = "macos")]
pub mod seatbelt;

pub use policy::{NetworkPolicy, SandboxMode, SandboxPolicy};
// `canon` (a path util) and `ReadScope` are referenced from the backends' unit
// tests and are part of the module's surface; not yet from non-test consumers.
#[allow(unused_imports)]
pub use policy::{canon, ReadScope};

use std::io;
use std::path::Path;
use std::process::Stdio;

/// A sandbox backend.
pub trait Sandbox: Send + Sync {
    /// Short name for logs/diagnostics.
    fn name(&self) -> &'static str;

    /// Whether this backend actually enforces (false for `Noop`). Callers
    /// surface a "no backstop" notice when this is false.
    fn enforces(&self) -> bool;

    /// Rewrite `(program, args)` to run under `policy`. `FullAccess` (an
    /// approved/escalated action) returns the command unchanged.
    fn wrap_command(&self, policy: &SandboxPolicy, program: &str, args: &[String])
        -> (String, Vec<String>);

    /// Write `content` to `path` under `policy`. Default routes through a
    /// sandboxed `tee` subprocess so the kernel returns `EPERM` outside the
    /// writable roots; `FullAccess`/`Noop` write directly.
    fn write_file(&self, policy: &SandboxPolicy, path: &Path, content: &[u8]) -> io::Result<()> {
        if !self.enforces() || policy.mode == SandboxMode::FullAccess {
            return std::fs::write(path, content);
        }
        let path_s = path.to_string_lossy().to_string();
        let (prog, args) = self.wrap_command(policy, "tee", &[path_s.clone()]);
        let mut child = std::process::Command::new(&prog)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        use std::io::Write;
        child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("no stdin"))?
            .write_all(content)?;
        let out = child.wait_with_output()?;
        if out.status.success() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "sandboxed write to {path_s} failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            ))
        }
    }
}

/// No-op backend: identity wrap, direct writes. Used on unsupported platforms
/// or when disabled. Smith then relies on the cooperative approval gate only.
pub struct Noop;
impl Sandbox for Noop {
    fn name(&self) -> &'static str {
        "noop"
    }
    fn enforces(&self) -> bool {
        false
    }
    fn wrap_command(&self, _p: &SandboxPolicy, program: &str, args: &[String]) -> (String, Vec<String>) {
        (program.to_string(), args.to_vec())
    }
}

/// Pick the backend from `CONSTRUCT_SMITH_SANDBOX` (default off → `Noop`).
pub fn select() -> Box<dyn Sandbox> {
    let choice = std::env::var("CONSTRUCT_SMITH_SANDBOX")
        .unwrap_or_default()
        .to_ascii_lowercase();
    match choice.as_str() {
        "auto" | "1" | "on" => auto(),
        "seatbelt" => seatbelt_or_noop(),
        // "none" | "0" | "" | unknown
        _ => Box::new(Noop),
    }
}

fn auto() -> Box<dyn Sandbox> {
    #[cfg(target_os = "macos")]
    {
        seatbelt_or_noop()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Box::new(Noop)
    }
}

fn seatbelt_or_noop() -> Box<dyn Sandbox> {
    #[cfg(target_os = "macos")]
    {
        let sb = seatbelt::Seatbelt;
        if sb.available() {
            return Box::new(sb);
        }
    }
    Box::new(Noop)
}
