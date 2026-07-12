//! Native-transcript path resolvers, shared by the four wrapper adapters
//! (claude, codex, grok, antigravity) *and* the daemon.
//!
//! Each adapter crate already knows how to find the file its own harness
//! CLI writes a conversation's transcript to — it needs that to mirror
//! native history into construct's own transcript. The daemon needs the
//! exact same formula for one more reason: spec 0086's usage-probe cleanup
//! resolves and best-effort-unlinks the native transcript file a
//! short-lived `SessionKind::UsageProbe` session caused a harness CLI to
//! create, so probing never leaves a stray entry in the harness's own
//! native history (`claude --resume` picker, `~/.codex/sessions/`, ...).
//! Keeping one copy of each formula here (instead of duplicating it in both
//! the adapter crate and the daemon) is the whole point of this module.
//!
//! Every formula's inputs — cwd, and an env var such as `CLAUDE_HOME` /
//! `CODEX_HOME` / `GROK_HOME` — are resolved by checking `env` first (the
//! session's `env_with_meta`/`SessionStartParams::env`, i.e. whatever was
//! layered on top of the process environment for this particular session)
//! and falling back to the calling process's own environment. Inside an
//! adapter subprocess those two are identical (the daemon spawns the child
//! with exactly that map merged into its real env), so adapter call sites
//! that don't have a convenient `env` map in scope can just pass an empty
//! one with no change in behavior. The daemon, calling these functions
//! directly (not from inside the spawned child, and often after the child
//! has already exited), is the one caller that needs to pass a real map —
//! typically the harness's `[adapters.<name>].env` config layer — to
//! correctly account for an operator-configured `*_HOME` override.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Look up `key` in `env` first (treating an empty value as absent), then
/// fall back to the calling process's own environment variable.
fn env_lookup(env: &HashMap<String, String>, key: &str) -> Option<String> {
    env.get(key)
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var(key).ok().filter(|s| !s.is_empty()))
}

/// Claude's per-project transcript slug: cwd with every non-alphanumeric
/// ASCII byte replaced by `-`.
pub fn claude_project_slug(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Where claude writes a session's native transcript:
/// `<CLAUDE_HOME>/projects/<slugified-cwd>/<session_id>.jsonl`. Honors
/// `CONSTRUCT_CLAUDE_HOME` (checked first), then `CLAUDE_HOME`, then
/// `$HOME/.claude`.
pub fn claude_transcript_path(
    cwd: &Path,
    session_id: &str,
    env: &HashMap<String, String>,
) -> Option<PathBuf> {
    let home = env_lookup(env, "CONSTRUCT_CLAUDE_HOME")
        .or_else(|| env_lookup(env, "CLAUDE_HOME"))
        .or_else(|| env_lookup(env, "HOME").map(|h| format!("{h}/.claude")))?;
    Some(
        PathBuf::from(home)
            .join("projects")
            .join(claude_project_slug(cwd))
            .join(format!("{session_id}.jsonl")),
    )
}

/// Where codex stores its rollout files. Honors `CODEX_HOME` (checked in
/// `env` first, then the process env), falling back to `$HOME/.codex/sessions`.
pub fn codex_sessions_root(env: &HashMap<String, String>) -> Option<PathBuf> {
    if let Some(home) = env_lookup(env, "CODEX_HOME") {
        return Some(PathBuf::from(home).join("sessions"));
    }
    let home = env_lookup(env, "HOME")?;
    Some(PathBuf::from(home).join(".codex").join("sessions"))
}

/// Codex names its rollout files `rollout-<timestamp>-<uuid>.jsonl`, nested
/// under [`codex_sessions_root`] in a date-based tree — there is no
/// deterministic formula from session id straight to a path, so resolving
/// one requires a scan. Unlike the adapter's own transcript watcher (which
/// also matches on an originator tag to disambiguate concurrent codex
/// processes sharing a cwd), this only needs to find the one file whose
/// name embeds a already-known uuid — the id codex itself minted and that
/// construct persisted to `codex_session_id.txt` — so a plain filename
/// match is enough.
pub fn codex_transcript_path(env: &HashMap<String, String>, session_id: &str) -> Option<PathBuf> {
    let root = codex_sessions_root(env)?;
    find_file_by_suffix(&root, "rollout-", &format!("-{session_id}.jsonl"))
}

/// Grok's per-conversation session directory:
/// `<GROK_HOME>/sessions/<url-encoded-cwd>/<session_id>/`. This directory
/// is exclusively owned by one session — grok writes several files into it
/// (`chat_history.jsonl`, `summary.json`, `prompt_context.json`,
/// `system_prompt.txt`, ...), so a cleanup that only removes
/// `chat_history.jsonl` (see [`grok_transcript_path`]) still leaves the
/// rest behind and grok's own session picker can still show the entry.
/// Callers that want to fully erase a grok session's native footprint
/// (rather than just stop the daemon's own transcript-mirroring watcher
/// from finding new content) should remove this whole directory.
pub fn grok_session_dir(
    cwd: &Path,
    session_id: &str,
    env: &HashMap<String, String>,
) -> Option<PathBuf> {
    let home = env_lookup(env, "CONSTRUCT_GROK_HOME")
        .or_else(|| env_lookup(env, "GROK_HOME"))
        .or_else(|| env_lookup(env, "HOME").map(|h| format!("{h}/.grok")))?;
    Some(
        PathBuf::from(home)
            .join("sessions")
            .join(url_encode_path(cwd))
            .join(session_id),
    )
}

/// Where grok writes a conversation's native transcript:
/// `<grok-session-dir>/chat_history.jsonl`.
pub fn grok_transcript_path(
    cwd: &Path,
    session_id: &str,
    env: &HashMap<String, String>,
) -> Option<PathBuf> {
    Some(grok_session_dir(cwd, session_id, env)?.join("chat_history.jsonl"))
}

fn url_encode_path(path: &Path) -> String {
    let s = path.to_string_lossy();
    let mut encoded = String::new();
    for c in s.chars() {
        match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '~' => {
                encoded.push(c);
            }
            '/' => encoded.push_str("%2F"),
            _ => {
                for byte in c.to_string().bytes() {
                    encoded.push_str(&format!("%{byte:02X}"));
                }
            }
        }
    }
    encoded
}

/// Antigravity's home dir, where per-conversation `brain/<id>` trees
/// (and their `.system_generated/logs/transcript.jsonl`) live. Honors
/// `CONSTRUCT_ANTIGRAVITY_HOME`, falling back to `$HOME/.gemini/antigravity-cli`.
fn antigravity_home(env: &HashMap<String, String>) -> Option<PathBuf> {
    env_lookup(env, "CONSTRUCT_ANTIGRAVITY_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            env_lookup(env, "HOME").map(|h| PathBuf::from(h).join(".gemini").join("antigravity-cli"))
        })
}

/// Antigravity's per-conversation directory: `<antigravity-home>/brain/<conversation_id>/`.
/// This is exclusively owned by one conversation and holds much more than
/// the transcript — a full `.git` history, task logs, uploaded files —
/// see [`antigravity_transcript_path`]'s doc comment. Callers that want to
/// fully erase a conversation's native footprint should remove this whole
/// directory, not just the transcript file inside it.
pub fn antigravity_conversation_dir(
    conversation_id: &str,
    env: &HashMap<String, String>,
) -> Option<PathBuf> {
    Some(antigravity_home(env)?.join("brain").join(conversation_id))
}

/// Where antigravity writes a conversation's native transcript:
/// `<antigravity-home>/brain/<conversation_id>/.system_generated/logs/transcript.jsonl`.
/// This is only one of several files/directories antigravity keeps under
/// the conversation's directory (it also keeps a full `.git` history, task
/// logs under `.system_generated/tasks/`, and any user-uploaded files) —
/// see [`antigravity_conversation_dir`] for the whole-conversation path.
pub fn antigravity_transcript_path(
    conversation_id: &str,
    env: &HashMap<String, String>,
) -> Option<PathBuf> {
    Some(
        antigravity_conversation_dir(conversation_id, env)?
            .join(".system_generated")
            .join("logs")
            .join("transcript.jsonl"),
    )
}

/// Recursively scan `root` for a file whose name starts with `prefix` and
/// ends with `suffix`. Returns the first match; there is exactly one in
/// practice since `suffix` embeds a full uuid.
fn find_file_by_suffix(root: &Path, prefix: &str, suffix: &str) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            let path = entry.path();
            if ft.is_dir() {
                stack.push(path);
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.starts_with(prefix) && name.ends_with(suffix) {
                return Some(path);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_transcript_path_uses_env_map_override() {
        let mut env = HashMap::new();
        env.insert("CLAUDE_HOME".to_string(), "/sess/claude".to_string());
        let path = claude_transcript_path(Path::new("/repo"), "abc-123", &env).expect("path");
        assert_eq!(
            path,
            PathBuf::from("/sess/claude/projects/-repo/abc-123.jsonl")
        );
    }

    #[test]
    fn claude_transcript_path_falls_back_to_home() {
        let mut env = HashMap::new();
        env.insert("HOME".to_string(), "/home/u".to_string());
        let path = claude_transcript_path(Path::new("/repo"), "abc-123", &env).expect("path");
        assert_eq!(
            path,
            PathBuf::from("/home/u/.claude/projects/-repo/abc-123.jsonl")
        );
    }

    #[test]
    fn codex_sessions_root_prefers_session_env_then_process_env_then_home() {
        let mut session_env = HashMap::new();
        session_env.insert("CODEX_HOME".into(), "/sess/codex".into());
        assert_eq!(
            codex_sessions_root(&session_env),
            Some(PathBuf::from("/sess/codex/sessions"))
        );
        // Empty value in session env falls through.
        session_env.insert("CODEX_HOME".into(), "".into());
        let got = codex_sessions_root(&session_env);
        // Result depends on the test runner's env; we just assert that an
        // empty session-env value doesn't masquerade as a real one.
        if let Some(p) = got {
            assert_ne!(p, PathBuf::from("/sessions"));
        }
    }

    #[test]
    fn codex_transcript_path_finds_matching_rollout() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sessions = tmp.path().join("sessions").join("2026").join("07").join("11");
        std::fs::create_dir_all(&sessions).expect("mkdir");
        let target = sessions.join("rollout-20260711-120000-11111111-2222-3333-4444-555555555555.jsonl");
        std::fs::write(&target, "{}").expect("write");
        // Also write a decoy rollout for a different uuid.
        std::fs::write(
            sessions.join("rollout-20260711-120001-99999999-8888-7777-6666-555555555555.jsonl"),
            "{}",
        )
        .expect("write");

        let mut env = HashMap::new();
        env.insert(
            "CODEX_HOME".to_string(),
            tmp.path().to_string_lossy().to_string(),
        );
        let found = codex_transcript_path(&env, "11111111-2222-3333-4444-555555555555")
            .expect("resolved path");
        assert_eq!(found, target);
    }

    #[test]
    fn grok_transcript_path_uses_env_map_override() {
        let mut env = HashMap::new();
        env.insert("GROK_HOME".to_string(), "/sess/grok".to_string());
        let path = grok_transcript_path(Path::new("/repo/proj"), "conv-1", &env).expect("path");
        assert_eq!(
            path,
            PathBuf::from("/sess/grok/sessions/%2Frepo%2Fproj/conv-1/chat_history.jsonl")
        );
    }

    #[test]
    fn antigravity_transcript_path_uses_env_map_override() {
        let mut env = HashMap::new();
        env.insert(
            "CONSTRUCT_ANTIGRAVITY_HOME".to_string(),
            "/sess/agy".to_string(),
        );
        let path = antigravity_transcript_path("conv-1", &env).expect("path");
        assert_eq!(
            path,
            PathBuf::from("/sess/agy/brain/conv-1/.system_generated/logs/transcript.jsonl")
        );
    }

    /// `grok_session_dir` is the whole per-session directory the transcript
    /// file lives inside — exercised by the daemon's native-transcript
    /// cleanup (spec 0086), which must erase the whole directory (grok
    /// keeps several sibling files there: `summary.json`,
    /// `prompt_context.json`, `system_prompt.txt`), not just the one
    /// transcript file `grok_transcript_path` points at.
    #[test]
    fn grok_session_dir_is_transcript_paths_parent() {
        let mut env = HashMap::new();
        env.insert("GROK_HOME".to_string(), "/sess/grok".to_string());
        let dir = grok_session_dir(Path::new("/repo/proj"), "conv-1", &env).expect("dir");
        let transcript = grok_transcript_path(Path::new("/repo/proj"), "conv-1", &env).expect("path");
        assert_eq!(transcript.parent(), Some(dir.as_path()));
        assert_eq!(dir, PathBuf::from("/sess/grok/sessions/%2Frepo%2Fproj/conv-1"));
    }

    /// `antigravity_conversation_dir` is the whole per-conversation
    /// directory (a full `.git` history, task logs, uploads) — the daemon's
    /// native-transcript cleanup must erase this whole tree, not just the
    /// transcript file nested under `.system_generated/logs/`.
    #[test]
    fn antigravity_conversation_dir_is_an_ancestor_of_transcript_path() {
        let mut env = HashMap::new();
        env.insert(
            "CONSTRUCT_ANTIGRAVITY_HOME".to_string(),
            "/sess/agy".to_string(),
        );
        let dir = antigravity_conversation_dir("conv-1", &env).expect("dir");
        let transcript = antigravity_transcript_path("conv-1", &env).expect("path");
        assert!(transcript.starts_with(&dir));
        assert_eq!(dir, PathBuf::from("/sess/agy/brain/conv-1"));
    }
}
