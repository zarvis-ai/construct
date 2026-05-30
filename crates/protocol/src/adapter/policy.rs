//! Adapter-side auto-approval policy.
//!
//! The daemon defines a single auto-approval policy per session and exposes it
//! to adapters via the `AGENTD_AUTO_APPROVE_PATHS` env var (colon-separated
//! absolute directories). A file-mutating tool whose target path is under any
//! of those directories may run without prompting the user.
//!
//! Adapters consume this policy differently depending on how their harness
//! gates tool calls:
//!
//! - **Native (zarvis)** — the adapter implements its own approval gate; it
//!   calls [`AutoApprovePolicy::allows_path_write`] before prompting and
//!   skips the prompt for matches.
//! - **Wrapper (claude)** — agentd doesn't sit in the harness's tool-call
//!   loop, so the only lever is what the upstream CLI accepts at spawn time.
//!   The adapter translates the policy via
//!   [`AutoApprovePolicy::claude_allowed_tools_args`].
//! - **Wrapper (codex, antigravity)** — neither upstream CLI exposes a
//!   path-scoped allow-list today, so these adapters can read the policy but
//!   have nothing to translate it into. The helper still lives here so a
//!   future upstream change (or a different translation, e.g. a settings
//!   file) only touches one place.

use std::path::{Path, PathBuf};

/// Env var holding the list of directories whose contents are auto-approved
/// for harness writes. Colon-separated (Unix path separator) absolute paths.
pub const ENV_AUTO_APPROVE_PATHS: &str = "AGENTD_AUTO_APPROVE_PATHS";

/// Centralized auto-approval policy. Built from the
/// [`ENV_AUTO_APPROVE_PATHS`] env var by [`Self::from_env`], or directly with
/// [`Self::new`] for tests.
#[derive(Debug, Clone, Default)]
pub struct AutoApprovePolicy {
    allow_paths: Vec<PathBuf>,
}

impl AutoApprovePolicy {
    pub fn new(allow_paths: Vec<PathBuf>) -> Self {
        Self { allow_paths }
    }

    /// Load the policy from [`ENV_AUTO_APPROVE_PATHS`]. Missing or empty env
    /// var means "no auto-approvals" (every write goes through the normal
    /// gate).
    pub fn from_env() -> Self {
        let raw = match std::env::var(ENV_AUTO_APPROVE_PATHS) {
            Ok(v) => v,
            Err(_) => return Self::default(),
        };
        let allow_paths = raw
            .split(':')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect();
        Self { allow_paths }
    }

    pub fn allow_paths(&self) -> &[PathBuf] {
        &self.allow_paths
    }

    pub fn is_empty(&self) -> bool {
        self.allow_paths.is_empty()
    }

    /// True if a write/edit targeting `path` is auto-approved. Matches when
    /// `path` (after stripping `.`/empty components) is equal to or nested
    /// under any allowed directory. The check is lexical — it does not touch
    /// the filesystem — so it works for paths that don't exist yet (the
    /// common case: a harness creating a new widget file).
    pub fn allows_path_write(&self, path: &Path) -> bool {
        if self.allow_paths.is_empty() {
            return false;
        }
        let normalized = normalize(path);
        self.allow_paths
            .iter()
            .any(|root| starts_with_components(&normalized, &normalize(root)))
    }

    /// Translate the policy into Claude CLI `--allowed-tools` flag values.
    /// Returns a flat vec ready to extend a spawn command's args. The Claude
    /// permission rule format is `Tool(pattern)`; for file writes we cover
    /// `Write`, `Edit`, and `MultiEdit` with a `<dir>/**` glob each.
    pub fn claude_allowed_tools_args(&self) -> Vec<String> {
        let mut out = Vec::with_capacity(self.allow_paths.len() * 6);
        for root in &self.allow_paths {
            let glob = format!("{}/**", root.display());
            for tool in ["Write", "Edit", "MultiEdit"] {
                out.push("--allowed-tools".into());
                out.push(format!("{tool}({glob})"));
            }
        }
        out
    }
}

fn normalize(path: &Path) -> Vec<std::ffi::OsString> {
    use std::path::Component;
    let mut out = Vec::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::Normal(_) | Component::RootDir | Component::Prefix(_) => {
                out.push(c.as_os_str().to_os_string());
            }
            Component::ParentDir => {
                // `..` is rare in tool args; collapse against the previous
                // component when possible so a path that traverses up stays
                // comparable to an allow root that doesn't.
                if !out.pop().is_some() {
                    out.push(c.as_os_str().to_os_string());
                }
            }
        }
    }
    out
}

fn starts_with_components(
    path: &[std::ffi::OsString],
    prefix: &[std::ffi::OsString],
) -> bool {
    path.len() >= prefix.len() && path.iter().zip(prefix).all(|(a, b)| a == b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_policy_allows_nothing() {
        let p = AutoApprovePolicy::default();
        assert!(p.is_empty());
        assert!(!p.allows_path_write(Path::new("/tmp/anything")));
    }

    #[test]
    fn allows_writes_under_allow_root() {
        let p = AutoApprovePolicy::new(vec![PathBuf::from("/var/agentd/widgets")]);
        assert!(p.allows_path_write(Path::new("/var/agentd/widgets/foo.md")));
        assert!(p.allows_path_write(Path::new("/var/agentd/widgets/sub/bar.md")));
        // The allow root itself counts as inside.
        assert!(p.allows_path_write(Path::new("/var/agentd/widgets")));
    }

    #[test]
    fn rejects_paths_outside_allow_root() {
        let p = AutoApprovePolicy::new(vec![PathBuf::from("/var/agentd/widgets")]);
        assert!(!p.allows_path_write(Path::new("/var/agentd/other/foo")));
        assert!(!p.allows_path_write(Path::new("/etc/passwd")));
        // Sibling whose name is a *prefix string* of an allow root must not
        // match — that's the bug component-wise comparison prevents.
        assert!(!p.allows_path_write(Path::new("/var/agentd/widgets-evil/foo")));
    }

    #[test]
    fn collapses_curdir_and_parent_components() {
        let p = AutoApprovePolicy::new(vec![PathBuf::from("/var/agentd/widgets")]);
        assert!(p.allows_path_write(Path::new("/var/agentd/widgets/./foo.md")));
        assert!(p.allows_path_write(Path::new("/var/agentd/widgets/sub/../foo.md")));
        assert!(!p.allows_path_write(Path::new("/var/agentd/widgets/../foo.md")));
    }

    #[test]
    fn claude_allowed_tools_args_cover_write_tools() {
        let p = AutoApprovePolicy::new(vec![PathBuf::from("/widgets")]);
        let args = p.claude_allowed_tools_args();
        assert_eq!(
            args,
            vec![
                "--allowed-tools".to_string(),
                "Write(/widgets/**)".to_string(),
                "--allowed-tools".to_string(),
                "Edit(/widgets/**)".to_string(),
                "--allowed-tools".to_string(),
                "MultiEdit(/widgets/**)".to_string(),
            ]
        );
    }
}
