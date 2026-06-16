//! macOS Seatbelt backend: compile the policy to an SBPL profile and front
//! commands with `sandbox-exec -p`.
//!
//! Strategy is "allow by default, then deny network + writes-outside-roots"
//! (SBPL last-matching-rule wins). That gives the two boundaries that matter —
//! no network, no writes outside the worktree — without the fragility of a
//! full `(deny default)` profile (which has to allowlist every dylib/mach
//! lookup just to let a command start).

use std::path::Path;

use super::{NetworkPolicy, Sandbox, SandboxMode, SandboxPolicy};

pub struct Seatbelt;

impl Seatbelt {
    pub fn available(&self) -> bool {
        Path::new("/usr/bin/sandbox-exec").exists()
    }
}

impl Sandbox for Seatbelt {
    fn name(&self) -> &'static str {
        "seatbelt"
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
        let mut a = vec!["-p".to_string(), profile(policy), program.to_string()];
        a.extend_from_slice(args);
        ("/usr/bin/sandbox-exec".to_string(), a)
    }
}

pub(crate) fn profile(policy: &SandboxPolicy) -> String {
    let mut p = String::from("(version 1)\n(allow default)\n");
    if policy.network == NetworkPolicy::Denied {
        p.push_str("(deny network*)\n");
    }
    match policy.mode {
        SandboxMode::FullAccess => {} // never reached: wrap_command returns early
        SandboxMode::ReadOnly => p.push_str("(deny file-write*)\n"),
        SandboxMode::WorkspaceWrite => {
            p.push_str("(deny file-write*)\n");
            p.push_str("(allow file-write*\n");
            for root in &policy.writable_roots {
                p.push_str(&format!("  (subpath {})\n", sbpl_quote(root)));
            }
            // Writers/commands need their stdio + a couple of devices.
            p.push_str(
                "  (literal \"/dev/null\") (literal \"/dev/stdout\") \
                 (literal \"/dev/stderr\") (literal \"/dev/tty\")\n)\n",
            );
        }
    }
    p
}

/// SBPL double-quoted string with backslash escaping.
fn sbpl_quote(p: &Path) -> String {
    let s = p.to_string_lossy();
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
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

    #[test]
    fn profile_denies_network_and_confines_writes() {
        let prof = profile(&policy(&["/work/tree"], NetworkPolicy::Denied));
        assert!(prof.contains("(deny network*)"), "{prof}");
        assert!(prof.contains("(deny file-write*)"), "{prof}");
        assert!(prof.contains("(subpath \"/work/tree\")"), "{prof}");
    }

    #[test]
    fn profile_omits_network_deny_when_allowed() {
        let prof = profile(&policy(&["/work/tree"], NetworkPolicy::Allowed));
        assert!(!prof.contains("network"), "{prof}");
    }

    #[test]
    fn wrap_fronts_with_sandbox_exec_then_returns_to_direct_on_full_access() {
        let sb = Seatbelt;
        let (prog, args) = sb.wrap_command(
            &policy(&["/work/tree"], NetworkPolicy::Denied),
            "bash",
            &["-lc".into(), "echo hi".into()],
        );
        assert_eq!(prog, "/usr/bin/sandbox-exec");
        assert_eq!(args[0], "-p");
        assert_eq!(args[2], "bash");
        assert_eq!(args[3], "-lc");

        let mut full = policy(&["/work/tree"], NetworkPolicy::Denied);
        full.mode = SandboxMode::FullAccess;
        let (prog, args) = sb.wrap_command(&full, "bash", &["-lc".into(), "x".into()]);
        assert_eq!(prog, "bash"); // unchanged
        assert_eq!(args, vec!["-lc".to_string(), "x".to_string()]);
    }

    /// Real enforcement check — macOS only (sandbox-exec). Proves a write
    /// outside the writable root is denied and one inside succeeds.
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_actually_blocks_out_of_root_writes() {
        let sb = Seatbelt;
        if !sb.available() {
            return;
        }
        let root = std::env::temp_dir().join(format!("smith-sb-enf-{}", std::process::id()));
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

        let outside = std::env::temp_dir().join(format!("smith-sb-OUT-{}", std::process::id()));
        let _ = std::fs::remove_file(&outside);
        assert!(
            !run(&format!("echo hi > {}", outside.display())),
            "write outside the writable roots must be blocked by the sandbox"
        );
        assert!(!outside.exists(), "blocked write must not have created the file");

        let _ = std::fs::remove_dir_all(&root);
    }
}
