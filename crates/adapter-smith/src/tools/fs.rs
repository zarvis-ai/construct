//! Filesystem editing — `edit_file` applies one or more find/replace hunks
//! to files (and creates new ones), atomically. Reading, listing, and
//! searching go through the `shell` tool (`cat`/`sed`/`ls`/`rg`); this is the
//! codex-style minimal surface (`shell` + `edit_file` + `write_stdin`).

use super::{Tool, ToolCtx, ToolOutcome};
use construct_protocol::ToolRisk;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub(crate) fn resolve(cwd: &std::path::Path, p: &str) -> PathBuf {
    let pb = PathBuf::from(p);
    if pb.is_absolute() {
        pb
    } else {
        cwd.join(pb)
    }
}

pub struct EditFile;

/// One normalized find/replace against a resolved path.
struct Hunk {
    path: PathBuf,
    find: String,
    replace: String,
}

/// A file's computed new contents, ready to write in phase 2.
struct Pending {
    path: PathBuf,
    new_text: String,
    created: bool,
    replacements: usize,
    preview: String,
}

#[async_trait]
impl Tool for EditFile {
    fn name(&self) -> &str {
        "edit_file"
    }
    fn description(&self) -> &str {
        "Apply edits to files. Single edit: pass `path`, `find`, `replace` — replaces \
         exactly one occurrence of `find` (must be unique; include surrounding context). \
         Multi-hunk / multi-file: pass `edits`, a list of `{find, replace, path?}` applied \
         in order — each `find` matches against the file as modified by earlier edits in \
         the same call, and an edit's `path` overrides the top-level `path`. To create a \
         new file, target a non-existent `path` with an empty `find` and the file's \
         contents in `replace`. All hunks are validated first and applied atomically: if \
         any `find` fails to match (or matches more than once), nothing is written."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":    { "type": "string", "description": "Target file. Default for single-edit mode and for `edits` that omit their own path." },
                "find":    { "type": "string", "description": "Text to replace (single-edit mode). Empty `find` on a non-existent path creates the file." },
                "replace": { "type": "string", "description": "Replacement text (single-edit mode), or the new file's contents when creating." },
                "edits": {
                    "type": "array",
                    "description": "Multiple edits, applied top-to-bottom.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path":    { "type": "string" },
                            "find":    { "type": "string" },
                            "replace": { "type": "string" }
                        },
                        "required": ["find", "replace"]
                    }
                }
            }
        })
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Risky
    }
    fn args_summary(&self, input: &Value) -> String {
        if let Some(edits) = input.get("edits").and_then(|e| e.as_array()) {
            multi_edit_summary(input, edits)
        } else {
            let path = input
                .get("path")
                .and_then(|s| s.as_str())
                .unwrap_or("(missing path)");
            let find = input.get("find").and_then(|s| s.as_str()).unwrap_or("");
            let replace = input.get("replace").and_then(|s| s.as_str()).unwrap_or("");
            format!("1 edit in {path}: {}", edit_snippet(find, replace))
        }
    }
    async fn run(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let default_path = input.get("path").and_then(|s| s.as_str());

        // Normalize the request into a flat list of resolved hunks.
        let mut hunks: Vec<Hunk> = Vec::new();
        if let Some(edits) = input.get("edits").and_then(|e| e.as_array()) {
            if edits.is_empty() {
                return Ok(ToolOutcome {
                    ok: false,
                    output: "`edits` is empty".into(),
                });
            }
            for (i, e) in edits.iter().enumerate() {
                let p = e.get("path").and_then(|s| s.as_str()).or(default_path);
                let Some(p) = p else {
                    return Ok(ToolOutcome {
                        ok: false,
                        output: format!(
                            "edit #{}: no `path` (set it on the edit or at top level)",
                            i + 1
                        ),
                    });
                };
                hunks.push(Hunk {
                    path: resolve(&ctx.cwd, p),
                    find: e
                        .get("find")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string(),
                    replace: e
                        .get("replace")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string(),
                });
            }
        } else {
            let Some(p) = default_path else {
                return Ok(ToolOutcome {
                    ok: false,
                    output: "missing `path` (or `edits`)".into(),
                });
            };
            let (Some(find), Some(replace)) = (
                input.get("find").and_then(|s| s.as_str()),
                input.get("replace").and_then(|s| s.as_str()),
            ) else {
                return Ok(ToolOutcome {
                    ok: false,
                    output: "missing `find`/`replace` (or provide `edits`)".into(),
                });
            };
            hunks.push(Hunk {
                path: resolve(&ctx.cwd, p),
                find: find.to_string(),
                replace: replace.to_string(),
            });
        }

        // Group by file, preserving first-seen order.
        let mut order: Vec<PathBuf> = Vec::new();
        let mut by_file: BTreeMap<PathBuf, Vec<&Hunk>> = BTreeMap::new();
        for h in &hunks {
            if !by_file.contains_key(&h.path) {
                order.push(h.path.clone());
            }
            by_file.entry(h.path.clone()).or_default().push(h);
        }

        // Phase 1 — validate and compute new contents in memory (atomic):
        // a single failed match aborts the whole call before any write.
        let mut pending: Vec<Pending> = Vec::new();
        for abs in &order {
            let file_hunks = &by_file[abs];
            let existing = tokio::fs::read(abs)
                .await
                .ok()
                .map(|b| String::from_utf8_lossy(&b).to_string());
            match existing {
                Some(mut text) => {
                    let mut preview = String::new();
                    let mut n = 0usize;
                    for h in file_hunks {
                        if h.find.is_empty() {
                            return Ok(ToolOutcome {
                                ok: false,
                                output: format!(
                                    "{}: `find` is empty but the file exists (empty `find` only creates new files)",
                                    abs.display()
                                ),
                            });
                        }
                        let count = text.matches(h.find.as_str()).count();
                        if count == 0 {
                            return Ok(ToolOutcome {
                                ok: false,
                                output: format!(
                                    "{}: `find` not found:\n{}",
                                    abs.display(),
                                    snippet(&h.find)
                                ),
                            });
                        }
                        if count > 1 {
                            return Ok(ToolOutcome {
                                ok: false,
                                output: format!(
                                    "{}: {} occurrences of `find`; add context to make it unique:\n{}",
                                    abs.display(),
                                    count,
                                    snippet(&h.find)
                                ),
                            });
                        }
                        preview.push_str(&edit_preview(
                            &abs.to_string_lossy(),
                            &text,
                            &h.find,
                            &h.replace,
                        ));
                        text = text.replacen(h.find.as_str(), &h.replace, 1);
                        n += 1;
                    }
                    pending.push(Pending {
                        path: abs.clone(),
                        new_text: text,
                        created: false,
                        replacements: n,
                        preview,
                    });
                }
                None => {
                    // File does not exist → creation. Every hunk must have an
                    // empty `find`; their `replace` values concatenate.
                    let mut content = String::new();
                    for h in file_hunks {
                        if !h.find.is_empty() {
                            return Ok(ToolOutcome {
                                ok: false,
                                output: format!(
                                    "{}: file does not exist — to create it use an empty `find`",
                                    abs.display()
                                ),
                            });
                        }
                        content.push_str(&h.replace);
                    }
                    pending.push(Pending {
                        path: abs.clone(),
                        new_text: content,
                        created: true,
                        replacements: file_hunks.len(),
                        preview: String::new(),
                    });
                }
            }
        }

        // Phase 2 — write. Matching was fully validated above, so failures
        // here are I/O errors only.
        let mut out = String::new();
        for p in &pending {
            if p.created {
                if let Some(parent) = p.path.parent() {
                    let _ = tokio::fs::create_dir_all(parent).await;
                }
            }
            // Route the write through the sandbox so the kernel enforces the
            // policy on the in-process `edit_file` path too (spec 0029): a
            // confined write goes through a sandboxed writer subprocess and
            // gets `EPERM` outside the writable roots; `FullAccess`/`Noop`
            // writes directly. Run on the blocking pool — the writer may spawn
            // a subprocess. (For a confined `edit_file` the target is always an
            // auto-approved path, which is a writable root, so this succeeds;
            // an escalated/Risky edit runs `FullAccess`.)
            let sandbox = ctx.sandbox.clone();
            let policy = ctx.sandbox_policy.clone();
            let path = p.path.clone();
            let content = p.new_text.clone().into_bytes();
            let write_res =
                tokio::task::spawn_blocking(move || sandbox.write_file(&policy, &path, &content))
                    .await;
            let io_res = match write_res {
                Ok(r) => r,
                Err(e) => Err(std::io::Error::other(format!("writer task panicked: {e}"))),
            };
            if let Err(e) = io_res {
                return Ok(ToolOutcome {
                    ok: false,
                    output: format!("write {}: {e}", p.path.display()),
                });
            }
            if p.created {
                out.push_str(&format!(
                    "created {} ({} bytes)\n",
                    p.path.display(),
                    p.new_text.len()
                ));
            } else {
                out.push_str(&format!(
                    "edited {} ({} replacement{})\n{}",
                    p.path.display(),
                    p.replacements,
                    if p.replacements == 1 { "" } else { "s" },
                    p.preview
                ));
            }
        }
        Ok(ToolOutcome {
            ok: true,
            output: out,
        })
    }
}

#[derive(Debug)]
struct FileEditSummary {
    path: String,
    snippets: Vec<String>,
}

fn multi_edit_summary(input: &Value, edits: &[Value]) -> String {
    let default_path = input.get("path").and_then(|p| p.as_str());
    let mut files: Vec<FileEditSummary> = Vec::new();
    for edit in edits {
        let path = edit
            .get("path")
            .and_then(|p| p.as_str())
            .or(default_path)
            .unwrap_or("(missing path)");
        let find = edit.get("find").and_then(|s| s.as_str()).unwrap_or("");
        let replace = edit.get("replace").and_then(|s| s.as_str()).unwrap_or("");
        let snippet = edit_snippet(find, replace);
        if let Some(existing) = files.iter_mut().find(|f| f.path == path) {
            existing.snippets.push(snippet);
        } else {
            files.push(FileEditSummary {
                path: path.to_string(),
                snippets: vec![snippet],
            });
        }
    }

    let file_word = if files.len() == 1 { "file" } else { "files" };
    let edit_word = if edits.len() == 1 { "edit" } else { "edits" };
    let details = files
        .iter()
        .map(|file| {
            let count = file.snippets.len();
            let count_word = if count == 1 { "edit" } else { "edits" };
            let mut snippets = file.snippets.iter().take(2).cloned().collect::<Vec<_>>();
            if count > snippets.len() {
                snippets.push(format!("+{} more", count - snippets.len()));
            }
            format!(
                "{} ({} {}: {})",
                file.path,
                count,
                count_word,
                snippets.join("; ")
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        "{} {} across {} {}: {}",
        edits.len(),
        edit_word,
        files.len().max(1),
        file_word,
        details
    )
}

fn edit_snippet(find: &str, replace: &str) -> String {
    let find = snippet(find);
    let replace = snippet(replace);
    if find.is_empty() {
        format!("create `{replace}`")
    } else if replace.is_empty() {
        format!("remove `{find}`")
    } else {
        format!("`{find}` -> `{replace}`")
    }
}

/// First line of `s`, truncated to ~120 chars on a char boundary, for error
/// messages that echo the failed `find`.
fn snippet(s: &str) -> String {
    let one = s.lines().next().unwrap_or("");
    let short: String = one.chars().take(120).collect();
    if short.len() < one.len() {
        format!("{short}…")
    } else {
        short
    }
}

const ANSI_RESET: &str = "\x1b[0m";
const ANSI_DIM: &str = "\x1b[2m";
const ANSI_RED: &str = "\x1b[31m";
const ANSI_GREEN: &str = "\x1b[32m";
const ANSI_CYAN: &str = "\x1b[36m";

fn edit_preview(path: &str, old_text: &str, find: &str, replace: &str) -> String {
    let Some(byte_start) = old_text.find(find) else {
        return String::new();
    };
    let byte_end = byte_start + find.len();
    let old_start_line = old_text[..byte_start]
        .bytes()
        .filter(|b| *b == b'\n')
        .count();
    let old_end_line = old_text[..byte_end].bytes().filter(|b| *b == b'\n').count();
    let new_text = old_text.replacen(find, replace, 1);

    let old_lines: Vec<&str> = old_text.lines().collect();
    let new_lines: Vec<&str> = new_text.lines().collect();
    let old_changed_end = (old_end_line + 1).min(old_lines.len());
    let new_byte_end = byte_start + replace.len();
    let new_start_line = new_text[..byte_start]
        .bytes()
        .filter(|b| *b == b'\n')
        .count();
    let new_end_line = new_text[..new_byte_end]
        .bytes()
        .filter(|b| *b == b'\n')
        .count();
    let new_changed_start = new_start_line.min(new_lines.len());
    let new_changed_end = (new_end_line + 1).min(new_lines.len());
    let old_context_start = old_start_line.saturating_sub(3);
    let new_context_start = new_changed_start.saturating_sub(3);
    let trailing_context = 3;
    let old_context_end = (old_changed_end + trailing_context).min(old_lines.len());
    let new_context_end = (new_changed_end + trailing_context).min(new_lines.len());

    let mut out = format!("{ANSI_CYAN}diff {path}{ANSI_RESET}\n{ANSI_DIM} line{ANSI_RESET}\n");

    push_numbered_line_diff(
        &mut out,
        &old_lines[old_context_start..old_context_end],
        &new_lines[new_context_start..new_context_end],
        old_context_start + 1,
        new_context_start + 1,
    );
    out
}

fn push_numbered_line_diff(
    out: &mut String,
    old_lines: &[&str],
    new_lines: &[&str],
    old_start_line: usize,
    new_start_line: usize,
) {
    let mut lcs = vec![vec![0usize; new_lines.len() + 1]; old_lines.len() + 1];
    for i in (0..old_lines.len()).rev() {
        for j in (0..new_lines.len()).rev() {
            lcs[i][j] = if old_lines[i] == new_lines[j] {
                1 + lcs[i + 1][j + 1]
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }

    let mut i = 0;
    let mut j = 0;
    let mut old_line = old_start_line;
    let mut new_line = new_start_line;
    while i < old_lines.len() && j < new_lines.len() {
        if old_lines[i] == new_lines[j] {
            push_numbered_context(out, old_line, new_line, old_lines[i]);
            i += 1;
            j += 1;
            old_line += 1;
            new_line += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            push_numbered_removed(out, old_line, old_lines[i]);
            i += 1;
            old_line += 1;
        } else {
            push_numbered_added(out, new_line, new_lines[j]);
            j += 1;
            new_line += 1;
        }
    }
    for line in &old_lines[i..] {
        push_numbered_removed(out, old_line, line);
        old_line += 1;
    }
    for line in &new_lines[j..] {
        push_numbered_added(out, new_line, line);
        new_line += 1;
    }
}

fn push_numbered_context(out: &mut String, _old_line: usize, new_line: usize, text: &str) {
    out.push_str(&format!("{ANSI_DIM}{new_line:>5}{ANSI_RESET}   {text}\n"));
}

fn push_numbered_removed(out: &mut String, old_line: usize, text: &str) {
    out.push_str(&format!("{ANSI_RED}{old_line:>5} - {text}{ANSI_RESET}\n"));
}

fn push_numbered_added(out: &mut String, new_line: usize, text: &str) {
    out.push_str(&format!("{ANSI_GREEN}{new_line:>5} + {text}{ANSI_RESET}\n"));
}

#[cfg(test)]
mod tests {
    use super::{edit_preview, EditFile, ANSI_GREEN, ANSI_RED};
    use crate::tools::{Tool, ToolCtx};
    use serde_json::json;

    fn ctx_with_cwd(cwd: std::path::PathBuf) -> ToolCtx {
        let sandbox_policy = crate::sandbox::SandboxPolicy::workspace_default(&cwd);
        ToolCtx {
            cwd,
            session_id: "test".to_string(),
            client: tokio::sync::OnceCell::new(),
            emit: None,
            procs: std::sync::Arc::new(crate::tools::proc::ProcRegistry::default()),
            context_serve: std::sync::Arc::new(std::sync::Mutex::new(Default::default())),
            sandbox: std::sync::Arc::new(crate::sandbox::Noop),
            sandbox_policy,
        }
    }

    #[test]
    fn edit_diff_shows_before_and_after_lines() {
        let diff = edit_preview("demo.txt", "one\ntwo\nthree\n", "two", "deux");
        assert!(diff.contains("diff demo.txt"));
        assert!(diff.contains("line"));
        assert!(diff.contains(" one\n"));
        assert!(diff.contains(&format!("{ANSI_RED}    2 - two")));
        assert!(diff.contains(&format!("{ANSI_GREEN}    2 + deux")));
        assert!(diff.contains(" three\n"));
    }

    #[test]
    fn edit_diff_handles_line_deletion() {
        let diff = edit_preview("demo.txt", "one\ntwo\nthree\n", "two\n", "");
        assert!(diff.contains(" one\n"));
        assert!(diff.contains(&format!("{ANSI_RED}    2 - two")));
        assert!(diff.contains(" three\n"));
    }

    #[test]
    fn edit_file_summary_lists_files_and_snippets() {
        let summary = EditFile.args_summary(&json!({
            "path": "crates/cli/src/app.rs",
            "edits": [
                {"find": "old app one", "replace": "new app one"},
                {"find": "old app two", "replace": "new app two"},
                {"path": "crates/adapter-smith/src/interactive.rs", "find": "old prompt", "replace": "new prompt"},
                {"path": "crates/adapter-smith/src/tools/fs.rs", "find": "old summary", "replace": "new summary"}
            ]
        }));

        assert!(summary.starts_with("4 edits across 3 files: "));
        assert!(summary.contains(
            "crates/cli/src/app.rs (2 edits: `old app one` -> `new app one`; `old app two` -> `new app two`)"
        ));
        assert!(summary.contains(
            "crates/adapter-smith/src/interactive.rs (1 edit: `old prompt` -> `new prompt`)"
        ));
        assert!(summary.contains(
            "crates/adapter-smith/src/tools/fs.rs (1 edit: `old summary` -> `new summary`)"
        ));
        assert!(!summary.contains("file(s)"));
    }

    #[test]
    fn edit_file_summary_makes_single_edit_explicit() {
        let summary = EditFile.args_summary(&json!({
            "path": "README.md",
            "find": "before",
            "replace": "after"
        }));

        assert_eq!(summary, "1 edit in README.md: `before` -> `after`");
    }

    /// End-to-end wiring check (macOS): a confined Seatbelt policy must make
    /// `edit_file`'s write go through the sandboxed writer and get blocked
    /// outside the writable roots, while an in-root edit succeeds — proving the
    /// in-process write actually routes through `ctx.sandbox.write_file`.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn confined_edit_file_blocks_out_of_root_write() {
        let sb = crate::sandbox::seatbelt::Seatbelt;
        if !sb.available() {
            return;
        }
        let root = std::env::temp_dir().join(format!("smith-edit-enf-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let policy = crate::sandbox::SandboxPolicy {
            mode: crate::sandbox::SandboxMode::WorkspaceWrite,
            writable_roots: vec![crate::sandbox::canon(&root)],
            readable: crate::sandbox::ReadScope::All,
            network: crate::sandbox::NetworkPolicy::Denied,
        };
        let ctx = ToolCtx {
            cwd: root.clone(),
            session_id: "test".to_string(),
            client: tokio::sync::OnceCell::new(),
            emit: None,
            procs: std::sync::Arc::new(crate::tools::proc::ProcRegistry::default()),
            context_serve: std::sync::Arc::new(std::sync::Mutex::new(Default::default())),
            sandbox: std::sync::Arc::new(sb),
            sandbox_policy: policy,
        };

        // In-root edit succeeds.
        let inside = root.join("in.txt");
        std::fs::write(&inside, "alpha\n").unwrap();
        let out = EditFile
            .run(
                json!({"path": inside.to_string_lossy(), "find": "alpha", "replace": "BETA"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.ok, "in-root edit should succeed: {}", out.output);
        assert_eq!(std::fs::read_to_string(&inside).unwrap(), "BETA\n");

        // Out-of-root edit is blocked by the kernel; content unchanged.
        let outside =
            std::env::temp_dir().join(format!("smith-edit-OUT-{}.txt", std::process::id()));
        std::fs::write(&outside, "alpha\n").unwrap();
        let out = EditFile
            .run(
                json!({"path": outside.to_string_lossy(), "find": "alpha", "replace": "BETA"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            !out.ok,
            "out-of-root edit must be blocked by the sandbox: {}",
            out.output
        );
        assert_eq!(
            std::fs::read_to_string(&outside).unwrap(),
            "alpha\n",
            "blocked write must not have changed the file"
        );

        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// End-to-end wiring check (Linux): the bubblewrap equivalent of
    /// `confined_edit_file_blocks_out_of_root_write`. Skipped when `bwrap` can't
    /// sandbox here.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn confined_edit_file_blocks_out_of_root_write_bwrap() {
        let sb = crate::sandbox::bubblewrap::Bubblewrap;
        if !sb.available() {
            eprintln!("skipping: bwrap unavailable or cannot create namespaces here");
            return;
        }
        let root = std::env::temp_dir().join(format!("smith-edit-bwrap-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let policy = crate::sandbox::SandboxPolicy {
            mode: crate::sandbox::SandboxMode::WorkspaceWrite,
            writable_roots: vec![crate::sandbox::canon(&root)],
            readable: crate::sandbox::ReadScope::All,
            network: crate::sandbox::NetworkPolicy::Denied,
        };
        let ctx = ToolCtx {
            cwd: root.clone(),
            session_id: "test".to_string(),
            client: tokio::sync::OnceCell::new(),
            emit: None,
            procs: std::sync::Arc::new(crate::tools::proc::ProcRegistry::default()),
            context_serve: std::sync::Arc::new(std::sync::Mutex::new(Default::default())),
            sandbox: std::sync::Arc::new(sb),
            sandbox_policy: policy,
        };

        // In-root edit succeeds.
        let inside = root.join("in.txt");
        std::fs::write(&inside, "alpha\n").unwrap();
        let out = EditFile
            .run(
                json!({"path": inside.to_string_lossy(), "find": "alpha", "replace": "BETA"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.ok, "in-root edit should succeed: {}", out.output);
        assert_eq!(std::fs::read_to_string(&inside).unwrap(), "BETA\n");

        // Out-of-root edit is blocked by the kernel; content unchanged.
        let outside =
            std::env::temp_dir().join(format!("smith-edit-bwOUT-{}.txt", std::process::id()));
        std::fs::write(&outside, "alpha\n").unwrap();
        let out = EditFile
            .run(
                json!({"path": outside.to_string_lossy(), "find": "alpha", "replace": "BETA"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            !out.ok,
            "out-of-root edit must be blocked by the sandbox: {}",
            out.output
        );
        assert_eq!(
            std::fs::read_to_string(&outside).unwrap(),
            "alpha\n",
            "blocked write must not have changed the file"
        );

        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn single_edit_replaces_unique_match() {
        let dir = std::env::temp_dir().join(format!("smith-edit-single-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("a.txt");
        std::fs::write(&f, "alpha\nbeta\ngamma\n").unwrap();
        let ctx = ctx_with_cwd(dir.clone());
        let out = EditFile
            .run(
                json!({"path": f.to_string_lossy(), "find": "beta", "replace": "BETA"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.ok, "{}", out.output);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "alpha\nBETA\ngamma\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn multi_hunk_is_atomic_on_failure() {
        let dir = std::env::temp_dir().join(format!("smith-edit-atomic-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("b.txt");
        std::fs::write(&f, "one\ntwo\nthree\n").unwrap();
        let ctx = ctx_with_cwd(dir.clone());
        // First hunk matches; second does not → whole call must abort, file unchanged.
        let out = EditFile
            .run(
                json!({"path": f.to_string_lossy(), "edits": [
                    {"find": "one", "replace": "ONE"},
                    {"find": "NOPE", "replace": "x"}
                ]}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.ok, "should fail: {}", out.output);
        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            "one\ntwo\nthree\n",
            "file must be untouched on partial failure"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn multi_hunk_applies_in_order() {
        let dir = std::env::temp_dir().join(format!("smith-edit-order-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("c.txt");
        std::fs::write(&f, "one\ntwo\nthree\n").unwrap();
        let ctx = ctx_with_cwd(dir.clone());
        let out = EditFile
            .run(
                json!({"path": f.to_string_lossy(), "edits": [
                    {"find": "one", "replace": "1"},
                    {"find": "three", "replace": "3"}
                ]}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.ok, "{}", out.output);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "1\ntwo\n3\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn empty_find_creates_new_file() {
        let dir = std::env::temp_dir().join(format!("smith-edit-create-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("nested/new.txt");
        let ctx = ctx_with_cwd(dir.clone());
        let out = EditFile
            .run(
                json!({"path": f.to_string_lossy(), "find": "", "replace": "hello\n"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.ok, "{}", out.output);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "hello\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
