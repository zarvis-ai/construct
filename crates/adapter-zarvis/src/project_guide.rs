//! Discovery + load of the project's `AGENTS.md` file.
//!
//! `AGENTS.md` (also commonly `CLAUDE.md`) is the convention for
//! per-project guidance the user wants every AI-assistant action to
//! honor — coding style, file layout, prohibited patterns, etc.
//! Zarvis reads it once at session start (and on resume) and
//! appends the contents to the system prompt under a dedicated
//! `## Project guide` section. The file is the user's voice; the
//! model is told to honor it unless explicitly overridden.
//!
//! Disable with `AGENTD_ZARVIS_PROJECT_GUIDE=off`.

use std::path::{Path, PathBuf};

/// Filename we look for at each ancestor directory.
const GUIDE_FILENAMES: &[&str] = &["AGENTS.md"];

/// Maximum directory levels we'll walk upward from `cwd`. Avoids
/// surfacing a guide from way outside the user's project.
const MAX_ASCEND: usize = 6;

/// Maximum bytes we'll load. Larger files are truncated with a
/// marker so we don't blow out the context window — most guides are
/// well under this.
const MAX_BYTES: usize = 32 * 1024;

/// Find the nearest `AGENTS.md` searching `cwd` then ancestor dirs.
/// Bounded by [`MAX_ASCEND`] levels and by `$HOME` (we don't go
/// above the user's home directory, to avoid pulling in system-wide
/// files the user didn't intend).
pub fn find(cwd: &Path) -> Option<PathBuf> {
    if std::env::var("AGENTD_ZARVIS_PROJECT_GUIDE").as_deref() == Ok("off") {
        return None;
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut dir = cwd;
    for _ in 0..=MAX_ASCEND {
        for name in GUIDE_FILENAMES {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        // Stop at $HOME (inclusive — we check HOME's own AGENTS.md,
        // then break before walking above it).
        if let Some(h) = home.as_deref() {
            if dir == h {
                return None;
            }
        }
        match dir.parent() {
            Some(p) => dir = p,
            None => return None,
        }
    }
    None
}

/// Load the file's text. Caps at [`MAX_BYTES`]; appends a
/// `[truncated; N more bytes]` marker when over. Returns `None` if
/// the file vanished between discovery and read (race), or isn't
/// UTF-8.
pub fn load(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let truncated = bytes.len() > MAX_BYTES;
    let trimmed = if truncated {
        &bytes[..MAX_BYTES]
    } else {
        &bytes[..]
    };
    let mut s = std::str::from_utf8(trimmed).ok()?.to_string();
    if truncated {
        let remaining = bytes.len() - MAX_BYTES;
        s.push_str(&format!("\n\n[truncated; {remaining} more bytes]"));
    }
    Some(s)
}

/// Build the section to append to the system prompt. `cwd` is the
/// session's working directory. Returns `None` if no guide was
/// found or it can't be read — in which case the caller uses just
/// the base prompt unchanged.
pub fn format_section(cwd: &Path) -> Option<String> {
    let path = find(cwd)?;
    let body = load(&path)?;
    let display_path = path.display();
    Some(format!(
        "## Project guide\n\nThe user's project includes an `AGENTS.md` at `{display_path}`. Honor these principles unless explicitly directed otherwise; they are the user's voice:\n\n---\n{body}\n---"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Mutex;

    /// Serialize env-touching tests within this crate's test
    /// binary. Tests run in parallel by default; one test
    /// setting `AGENTD_ZARVIS_PROJECT_GUIDE=off` was racing with
    /// another that expected it unset. The mutex makes
    /// `set_var` + `find` + `remove_var` atomic w.r.t. peers.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Build a small tempdir with `AGENTS.md` and confirm `find`
    /// picks it up directly.
    #[test]
    fn finds_in_cwd() {
        let _g = ENV_LOCK.lock().unwrap();
        let tmp = tempdir();
        let path = tmp.join("AGENTS.md");
        std::fs::write(&path, b"hello").unwrap();
        let found = find(&tmp).unwrap();
        assert_eq!(found, path);
    }

    #[test]
    fn finds_in_parent() {
        let _g = ENV_LOCK.lock().unwrap();
        let tmp = tempdir();
        let nested = tmp.join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        let path = tmp.join("AGENTS.md");
        std::fs::write(&path, b"root guide").unwrap();
        let found = find(&nested).unwrap();
        assert_eq!(found, path);
    }

    #[test]
    fn returns_none_when_absent() {
        let _g = ENV_LOCK.lock().unwrap();
        let tmp = tempdir();
        // Ensure $HOME doesn't accidentally provide a guide for the
        // test. Set HOME to the tempdir so we stop early.
        std::env::set_var("HOME", &tmp);
        assert!(find(&tmp).is_none());
    }

    #[test]
    fn opt_out_respected() {
        let _g = ENV_LOCK.lock().unwrap();
        let tmp = tempdir();
        let path = tmp.join("AGENTS.md");
        std::fs::write(&path, b"hello").unwrap();
        std::env::set_var("AGENTD_ZARVIS_PROJECT_GUIDE", "off");
        let result = find(&tmp);
        std::env::remove_var("AGENTD_ZARVIS_PROJECT_GUIDE");
        assert!(result.is_none());
    }

    #[test]
    fn load_truncates_large_files() {
        let tmp = tempdir();
        let path = tmp.join("AGENTS.md");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&vec![b'a'; MAX_BYTES + 1000]).unwrap();
        let s = load(&path).unwrap();
        assert!(s.contains("[truncated"));
    }

    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "zarvis-guide-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
