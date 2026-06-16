//! Linux bubblewrap backend: run commands inside a `bwrap` sandbox built from
//! the policy.
//!
//! Strategy mirrors the Seatbelt one — "everything readable, only the writable
//! roots writable, no network":
//!
//! - `--ro-bind / /` makes the whole host filesystem readable (`ReadScope::All`)
//!   but read-only, then each writable root is re-bound read-write *on top*
//!   (later binds win), so writes outside the roots hit the read-only mount and
//!   fail with `EROFS`/`EACCES`.
//! - `--proc /proc` + `--dev /dev` give fresh pseudo-filesystems (the blanket
//!   `--ro-bind / /` would otherwise capture the host's).
//! - `--unshare-net` drops the process into an empty network namespace when the
//!   policy denies network; omitted when network is allowed.
//!
//! `bwrap` needs to create user+mount namespaces. On a host where that's
//! blocked (locked-down container, `kernel.unprivileged_userns_clone=0`), the
//! binary exists but can't actually sandbox — [`Bubblewrap::available`] runs a
//! one-shot smoke test so we fall back to `Noop` instead of breaking every
//! command.

use std::path::Path;
use std::process::Stdio;

use super::{NetworkPolicy, Sandbox, SandboxMode, SandboxPolicy};

pub struct Bubblewrap;

impl Bubblewrap {
    /// `bwrap` is present *and* can actually create the namespaces it needs.
    pub fn available(&self) -> bool {
        match which_bwrap() {
            Some(bwrap) => can_sandbox(&bwrap),
            None => false,
        }
    }
}

/// Locate the `bwrap` binary: common absolute paths first, then `PATH`.
pub(crate) fn which_bwrap() -> Option<String> {
    for p in ["/usr/bin/bwrap", "/bin/bwrap"] {
        if Path::new(p).exists() {
            return Some(p.to_string());
        }
    }
    let path = std::env::var("PATH").ok()?;
    for dir in path.split(':') {
        if dir.is_empty() {
            continue;
        }
        let cand = Path::new(dir).join("bwrap");
        if cand.exists() {
            return Some(cand.to_string_lossy().to_string());
        }
    }
    None
}

/// One-shot check that `bwrap` can build a minimal sandbox here (user/mount/net
/// namespaces permitted). Cheap enough to run once at backend selection.
fn can_sandbox(bwrap: &str) -> bool {
    std::process::Command::new(bwrap)
        .args([
            "--ro-bind",
            "/",
            "/",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--unshare-net",
            "--die-with-parent",
            "--",
            "/bin/sh",
            "-c",
            "exit 0",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

impl Sandbox for Bubblewrap {
    fn name(&self) -> &'static str {
        "bubblewrap"
    }
    fn enforces(&self) -> bool {
        true
    }
    fn wrap_command(
        &self,
        policy: &SandboxPolicy,
        program: &str,
        args: &[String],
    ) -> (String, Vec<String>) {
        if policy.mode == SandboxMode::FullAccess {
            return (program.to_string(), args.to_vec());
        }
        let bwrap = which_bwrap().unwrap_or_else(|| "/usr/bin/bwrap".to_string());
        let mut a = bwrap_args(policy);
        a.push("--".to_string());
        a.push(program.to_string());
        a.extend_from_slice(args);
        (bwrap, a)
    }
}

/// The `bwrap` flags (without the trailing `-- <cmd>`) for `policy`.
pub(crate) fn bwrap_args(policy: &SandboxPolicy) -> Vec<String> {
    let mut a: Vec<String> = vec![
        // Whole host fs readable, read-only.
        "--ro-bind".into(),
        "/".into(),
        "/".into(),
        // Fresh /proc + /dev (the ro-bind of / would otherwise capture host's).
        "--proc".into(),
        "/proc".into(),
        "--dev".into(),
        "/dev".into(),
    ];
    match policy.mode {
        // never reached: wrap_command returns early on FullAccess.
        SandboxMode::FullAccess => {}
        // No rw binds — the ro-bind of / makes everything read-only.
        SandboxMode::ReadOnly => {}
        SandboxMode::WorkspaceWrite => {
            for root in &policy.writable_roots {
                let s = root.to_string_lossy().to_string();
                a.push("--bind".into());
                a.push(s.clone());
                a.push(s);
            }
        }
    }
    if policy.network == NetworkPolicy::Denied {
        a.push("--unshare-net".into());
    }
    // Don't leak sandboxed children if the adapter dies.
    a.push("--die-with-parent".into());
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn policy(roots: &[&str], net: NetworkPolicy) -> SandboxPolicy {
        SandboxPolicy {
            mode: SandboxMode::WorkspaceWrite,
            writable_roots: roots.iter().map(PathBuf::from).collect(),
            readable: super::super::ReadScope::All,
            network: net,
        }
    }

    fn joined(args: &[String]) -> String {
        args.join(" ")
    }

    #[test]
    fn args_bind_writable_roots_and_unshare_net() {
        let a = bwrap_args(&policy(&["/work/tree", "/tmp"], NetworkPolicy::Denied));
        let s = joined(&a);
        assert!(s.contains("--ro-bind / /"), "{s}");
        assert!(s.contains("--proc /proc"), "{s}");
        assert!(s.contains("--dev /dev"), "{s}");
        assert!(s.contains("--bind /work/tree /work/tree"), "{s}");
        assert!(s.contains("--bind /tmp /tmp"), "{s}");
        assert!(s.contains("--unshare-net"), "{s}");
    }

    #[test]
    fn args_omit_unshare_net_when_allowed() {
        let a = bwrap_args(&policy(&["/work/tree"], NetworkPolicy::Allowed));
        assert!(!joined(&a).contains("--unshare-net"), "{}", joined(&a));
    }

    #[test]
    fn wrap_fronts_with_bwrap_then_returns_direct_on_full_access() {
        let sb = Bubblewrap;
        let (prog, args) = sb.wrap_command(
            &policy(&["/work/tree"], NetworkPolicy::Denied),
            "bash",
            &["-lc".into(), "echo hi".into()],
        );
        assert!(prog.ends_with("bwrap"), "prog should be bwrap: {prog}");
        // The wrapped command sits after the `--` separator.
        let sep = args.iter().position(|x| x == "--").expect("has -- separator");
        assert_eq!(args[sep + 1], "bash");
        assert_eq!(args[sep + 2], "-lc");
        assert_eq!(args[sep + 3], "echo hi");

        let mut full = policy(&["/work/tree"], NetworkPolicy::Denied);
        full.mode = SandboxMode::FullAccess;
        let (prog, args) = sb.wrap_command(&full, "bash", &["-lc".into(), "x".into()]);
        assert_eq!(prog, "bash"); // unchanged
        assert_eq!(args, vec!["-lc".to_string(), "x".to_string()]);
    }

    /// Real enforcement check — Linux only, and only when `bwrap` can actually
    /// build a sandbox here (skipped otherwise, e.g. a CI runner without
    /// bubblewrap or without unprivileged user namespaces). Proves a write
    /// outside the writable root is denied and one inside succeeds.
    #[cfg(target_os = "linux")]
    #[test]
    fn bubblewrap_actually_blocks_out_of_root_writes() {
        let sb = Bubblewrap;
        if !sb.available() {
            eprintln!("skipping: bwrap unavailable or cannot create namespaces here");
            return;
        }
        let root = std::env::temp_dir().join(format!("smith-bwrap-enf-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        // Only the test root is writable (not tmp), so a sibling under tmp is a
        // genuine out-of-root target.
        let pol = SandboxPolicy {
            mode: SandboxMode::WorkspaceWrite,
            writable_roots: vec![crate::sandbox::canon(&root)],
            readable: super::super::ReadScope::All,
            network: NetworkPolicy::Denied,
        };

        let run = |cmd: &str| {
            let (prog, args) = sb.wrap_command(&pol, "bash", &["-lc".into(), cmd.into()]);
            std::process::Command::new(prog)
                .args(args)
                .output()
                .unwrap()
                .status
                .success()
        };

        let inside = root.join("ok.txt");
        assert!(
            run(&format!("echo hi > {}", inside.display())),
            "write inside the worktree should succeed"
        );
        assert!(inside.exists());

        let outside = std::env::temp_dir().join(format!("smith-bwrap-OUT-{}", std::process::id()));
        let _ = std::fs::remove_file(&outside);
        assert!(
            !run(&format!("echo hi > {}", outside.display())),
            "write outside the writable roots must be blocked by the sandbox"
        );
        assert!(!outside.exists(), "blocked write must not have created the file");

        let _ = std::fs::remove_dir_all(&root);
    }
}
