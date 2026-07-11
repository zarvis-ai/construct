//! The capability policy a sandbox backend enforces — see spec 0029.
//!
//! Derived at runtime from session context (no config file): the session
//! worktree + widgets dir + tmp are writable, network is denied. The approval
//! gate hands an [`SandboxPolicy::escalated`] copy to actions it has approved.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    /// No writes, no spawning of mutating processes. Part of the model (the
    /// SBPL/bwrap backends honor it); no consumer derives it yet — a session
    /// `read-only` mode is a later config layer.
    #[allow(dead_code)]
    ReadOnly,
    /// Write within `writable_roots`, network per `network`. The default.
    WorkspaceWrite,
    /// No enforcement (an approved/escalated action, or `unsafe-auto`).
    FullAccess,
}

#[derive(Debug, Clone)]
pub enum ReadScope {
    /// Reads are unrestricted (the default — confining reads breaks tools that
    /// legitimately read outside the worktree, and exfiltration is gated by the
    /// network + write boundaries instead).
    All,
    #[allow(dead_code)]
    Roots(Vec<PathBuf>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkPolicy {
    Denied,
    Allowed,
}

#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    pub mode: SandboxMode,
    /// Absolute, canonicalized roots an action may write within.
    pub writable_roots: Vec<PathBuf>,
    /// Read scope. Always `All` today (reads are unconfined — exfiltration is
    /// gated by the network + write boundaries); the field carries the model
    /// for a future read-restricting backend.
    #[allow(dead_code)]
    pub readable: ReadScope,
    pub network: NetworkPolicy,
}

impl SandboxPolicy {
    /// Runtime default for a session — no config file. The worktree (`cwd`),
    /// the auto-approve paths (widgets dir, …), and tmp are writable; network
    /// is denied.
    pub fn workspace_default(cwd: &Path) -> Self {
        let mut roots = vec![canon(cwd), canon(&std::env::temp_dir())];
        // On macOS `/tmp` is a symlink into `/private`; Seatbelt matches the
        // resolved path, so canonicalize it (→ `/private/tmp`).
        roots.push(canon(Path::new("/tmp")));
        // Paths the gate already auto-approves writes to (widgets dir, …) must
        // be writable, or those Safe writes would be blocked once confined.
        for p in construct_protocol::adapter::policy::AutoApprovePolicy::from_env().allow_paths() {
            roots.push(canon(p));
        }
        roots.sort();
        roots.dedup();
        Self {
            mode: SandboxMode::WorkspaceWrite,
            writable_roots: roots,
            readable: ReadScope::All,
            network: NetworkPolicy::Denied,
        }
    }

    /// The policy used to run an action the gate has approved: no enforcement.
    /// (A graduated "relaxed but still no ~/.ssh" tier is a later refinement.)
    pub fn escalated(&self) -> Self {
        Self {
            mode: SandboxMode::FullAccess,
            network: NetworkPolicy::Allowed,
            ..self.clone()
        }
    }

    /// Policy for a `read_only: true` shell call: writes are still confined to
    /// the worktree, but network is allowed. Network reads (gh, curl, etc.) are
    /// side-effect-free and must not be blocked by the default network=Denied
    /// floor.
    pub fn with_network_allowed(&self) -> Self {
        Self {
            network: NetworkPolicy::Allowed,
            ..self.clone()
        }
    }

    /// Would a write to `path` be permitted under this policy? The kernel is
    /// the real enforcer (the writer subprocess returns `EPERM`); this is the
    /// in-process predicate a future pre-flight planner uses to classify a
    /// write as in-sandbox vs a boundary crossing without spawning anything.
    #[allow(dead_code)]
    pub fn allows_write(&self, path: &Path) -> bool {
        match self.mode {
            SandboxMode::FullAccess => true,
            SandboxMode::ReadOnly => false,
            SandboxMode::WorkspaceWrite => {
                let p = canon(path);
                self.writable_roots.iter().any(|r| p.starts_with(r))
            }
        }
    }
}

/// Canonicalize, falling back to a lexical absolute path when the target
/// doesn't exist yet (e.g. a file about to be created) so prefix checks and
/// SBPL `subpath` rules still line up.
pub fn canon(p: &Path) -> PathBuf {
    if let Ok(c) = p.canonicalize() {
        return c;
    }
    // Canonicalize the deepest existing ancestor, then re-append the rest.
    let mut ancestor = p;
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    while let Some(parent) = ancestor.parent() {
        if let Ok(c) = parent.canonicalize() {
            let mut out = c;
            for seg in tail.iter().rev() {
                out.push(seg);
            }
            if let Some(name) = ancestor.file_name() {
                out.push(name);
            }
            return out;
        }
        if let Some(name) = ancestor.file_name() {
            tail.push(name);
        }
        ancestor = parent;
    }
    p.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_default_writes_inside_worktree_only() {
        let dir = std::env::temp_dir().join("smith-sb-policy");
        let _ = std::fs::create_dir_all(&dir);
        let p = SandboxPolicy::workspace_default(&dir);
        assert_eq!(p.mode, SandboxMode::WorkspaceWrite);
        assert_eq!(p.network, NetworkPolicy::Denied);
        assert!(p.allows_write(&dir.join("src/main.rs")));
        assert!(!p.allows_write(Path::new("/etc/hosts")));
        assert!(!p.allows_write(Path::new("/usr/local/lib/x.so")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn escalated_allows_everything() {
        let p = SandboxPolicy::workspace_default(Path::new("/tmp")).escalated();
        assert_eq!(p.mode, SandboxMode::FullAccess);
        assert_eq!(p.network, NetworkPolicy::Allowed);
        assert!(p.allows_write(Path::new("/etc/hosts")));
    }
}
