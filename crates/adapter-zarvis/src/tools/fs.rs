//! Filesystem tools — read/write/edit a file, list a directory, and
//! a basic find-by-glob. Designed to obviate the most common shell
//! incantations so the agent doesn't have to escape strings.

use super::{Tool, ToolCtx, ToolOutcome};
use agentd_protocol::ToolRisk;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;

pub(crate) fn resolve(cwd: &std::path::Path, p: &str) -> PathBuf {
    let pb = PathBuf::from(p);
    if pb.is_absolute() {
        pb
    } else {
        cwd.join(pb)
    }
}

pub struct ReadFile;

#[async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "Read a UTF-8 text file. Use `start_line`/`end_line` (1-based, inclusive) to \
         page through large files. Returns the file's bytes lossily decoded."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":       { "type": "string" },
                "start_line": { "type": "integer", "minimum": 1 },
                "end_line":   { "type": "integer", "minimum": 1 }
            },
            "required": ["path"]
        })
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Safe
    }
    fn args_summary(&self, input: &Value) -> String {
        input
            .get("path")
            .and_then(|s| s.as_str())
            .unwrap_or("(missing path)")
            .to_string()
    }
    async fn run(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let path = input
            .get("path")
            .and_then(|s| s.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing 'path'"))?;
        let path = resolve(&ctx.cwd, path);
        let start = input.get("start_line").and_then(|n| n.as_u64()).map(|n| n as usize);
        let end = input.get("end_line").and_then(|n| n.as_u64()).map(|n| n as usize);

        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) => {
                return Ok(ToolOutcome {
                    ok: false,
                    output: format!("read {}: {e}", path.display()),
                });
            }
        };
        let text = String::from_utf8_lossy(&bytes);
        let out = match (start, end) {
            (None, None) => text.to_string(),
            (s, e) => {
                let lines: Vec<&str> = text.lines().collect();
                let s = s.unwrap_or(1).saturating_sub(1);
                let e = e.unwrap_or(lines.len()).min(lines.len());
                let s = s.min(e);
                lines[s..e].join("\n")
            }
        };
        Ok(ToolOutcome { ok: true, output: out })
    }
}

pub struct WriteFile;

#[async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &str {
        "write_file"
    }
    fn description(&self) -> &str {
        "Write `contents` to `path`, creating parent directories as needed. \
         Overwrites any existing file."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":     { "type": "string" },
                "contents": { "type": "string" }
            },
            "required": ["path", "contents"]
        })
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Risky
    }
    fn args_summary(&self, input: &Value) -> String {
        let p = input.get("path").and_then(|s| s.as_str()).unwrap_or("(missing path)");
        let n = input
            .get("contents")
            .and_then(|s| s.as_str())
            .map(|s| s.len())
            .unwrap_or(0);
        format!("{} ({} bytes)", p, n)
    }
    async fn run(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let path = input
            .get("path")
            .and_then(|s| s.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing 'path'"))?;
        let contents = input
            .get("contents")
            .and_then(|s| s.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing 'contents'"))?;
        let path = resolve(&ctx.cwd, path);
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        match tokio::fs::write(&path, contents).await {
            Ok(_) => Ok(ToolOutcome {
                ok: true,
                output: format!("wrote {} ({} bytes)", path.display(), contents.len()),
            }),
            Err(e) => Ok(ToolOutcome {
                ok: false,
                output: format!("write {}: {e}", path.display()),
            }),
        }
    }
}

pub struct EditFile;

#[async_trait]
impl Tool for EditFile {
    fn name(&self) -> &str {
        "edit_file"
    }
    fn description(&self) -> &str {
        "Replace exactly one occurrence of `find` with `replace` in `path`. \
         Errors if `find` is not present or appears more than once — caller \
         must include enough surrounding context to make the match unique."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":    { "type": "string" },
                "find":    { "type": "string" },
                "replace": { "type": "string" }
            },
            "required": ["path", "find", "replace"]
        })
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Risky
    }
    fn args_summary(&self, input: &Value) -> String {
        input.get("path").and_then(|s| s.as_str()).unwrap_or("(missing path)").to_string()
    }
    async fn run(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let path = input
            .get("path")
            .and_then(|s| s.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing 'path'"))?;
        let find = input
            .get("find")
            .and_then(|s| s.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing 'find'"))?;
        let replace = input
            .get("replace")
            .and_then(|s| s.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing 'replace'"))?;
        let path = resolve(&ctx.cwd, path);
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) => {
                return Ok(ToolOutcome {
                    ok: false,
                    output: format!("read {}: {e}", path.display()),
                });
            }
        };
        let text = String::from_utf8_lossy(&bytes).to_string();
        let count = text.matches(find).count();
        if count == 0 {
            return Ok(ToolOutcome {
                ok: false,
                output: "no occurrences of `find` in file".into(),
            });
        }
        if count > 1 {
            return Ok(ToolOutcome {
                ok: false,
                output: format!("{count} occurrences of `find`; include more context to make it unique"),
            });
        }
        let diff = edit_preview(&path.to_string_lossy(), &text, find, replace);
        let new_text = text.replacen(find, replace, 1);
        match tokio::fs::write(&path, &new_text).await {
            Ok(_) => Ok(ToolOutcome {
                ok: true,
                output: format!("edited {} (1 replacement)\n{}", path.display(), diff),
            }),
            Err(e) => Ok(ToolOutcome {
                ok: false,
                output: format!("write {}: {e}", path.display()),
            }),
        }
    }
}

pub struct ListDir;

#[async_trait]
impl Tool for ListDir {
    fn name(&self) -> &str {
        "list_dir"
    }
    fn description(&self) -> &str {
        "List immediate entries of a directory. Each line is `<type> <name>` \
         where type is `f` (file), `d` (directory), or `l` (symlink)."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"]
        })
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Safe
    }
    fn args_summary(&self, input: &Value) -> String {
        input.get("path").and_then(|s| s.as_str()).unwrap_or("(missing path)").to_string()
    }
    async fn run(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let path = input
            .get("path")
            .and_then(|s| s.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing 'path'"))?;
        let path = resolve(&ctx.cwd, path);
        let mut rd = match tokio::fs::read_dir(&path).await {
            Ok(r) => r,
            Err(e) => {
                return Ok(ToolOutcome {
                    ok: false,
                    output: format!("list_dir {}: {e}", path.display()),
                });
            }
        };
        let mut entries: Vec<String> = Vec::new();
        while let Some(e) = rd.next_entry().await? {
            let ft = e.file_type().await?;
            let kind = if ft.is_dir() {
                "d"
            } else if ft.is_symlink() {
                "l"
            } else {
                "f"
            };
            entries.push(format!("{kind} {}", e.file_name().to_string_lossy()));
        }
        entries.sort();
        Ok(ToolOutcome {
            ok: true,
            output: entries.join("\n"),
        })
    }
}

pub struct FindFiles;

#[async_trait]
impl Tool for FindFiles {
    fn name(&self) -> &str {
        "find_files"
    }
    fn description(&self) -> &str {
        "Find files matching a simple substring or glob (\"*\" wildcard) in a \
         subtree. Returns up to 200 paths relative to the search root."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Substring or simple glob, e.g. `*.rs`." },
                "cwd":     { "type": "string", "description": "Search root (defaults to session cwd)." }
            },
            "required": ["pattern"]
        })
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Safe
    }
    fn args_summary(&self, input: &Value) -> String {
        input.get("pattern").and_then(|s| s.as_str()).unwrap_or("(missing pattern)").to_string()
    }
    async fn run(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let pattern = input
            .get("pattern")
            .and_then(|s| s.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing 'pattern'"))?
            .to_string();
        let root = input
            .get("cwd")
            .and_then(|s| s.as_str())
            .map(|p| resolve(&ctx.cwd, p))
            .unwrap_or_else(|| ctx.cwd.clone());
        let matcher = SimpleGlob::compile(&pattern);
        let max = 200;
        let mut out: Vec<String> = Vec::new();
        let root_clone = root.clone();
        // Walk synchronously in a blocking task to keep code simple.
        let scanned = tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
            let mut stack: Vec<PathBuf> = vec![root_clone.clone()];
            let mut hits: Vec<String> = Vec::new();
            while let Some(dir) = stack.pop() {
                let read = match std::fs::read_dir(&dir) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                for entry in read.flatten() {
                    let path = entry.path();
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.starts_with('.') {
                        continue; // skip dotfiles / .git
                    }
                    if path.is_dir() {
                        stack.push(path);
                    } else {
                        let rel = path
                            .strip_prefix(&root_clone)
                            .ok()
                            .map(|p| p.to_string_lossy().to_string())
                            .unwrap_or_else(|| path.to_string_lossy().to_string());
                        if !matcher.matches(&name) && !matcher.matches(&rel) {
                            continue;
                        }
                        hits.push(rel);
                        if hits.len() >= max {
                            return Ok(hits);
                        }
                    }
                }
            }
            Ok(hits)
        })
        .await
        .context("find_files join")??;
        out.extend(scanned);
        Ok(ToolOutcome {
            ok: true,
            output: out.join("\n"),
        })
    }
}

/// Tiny `*`-wildcard glob (no character classes, no `?`). Substring
/// match when the pattern has no `*`.
struct SimpleGlob {
    parts: Vec<String>,
    leading_wild: bool,
    trailing_wild: bool,
    has_wildcard: bool,
}

impl SimpleGlob {
    fn compile(pat: &str) -> Self {
        let has_wildcard = pat.contains('*');
        let leading_wild = pat.starts_with('*');
        let trailing_wild = pat.ends_with('*');
        let trimmed = pat.trim_matches('*');
        let parts: Vec<String> = trimmed
            .split('*')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
        Self {
            parts,
            leading_wild,
            trailing_wild,
            has_wildcard,
        }
    }
    fn matches(&self, s: &str) -> bool {
        if !self.has_wildcard {
            return self.parts.first().map(|p| s.contains(p)).unwrap_or(true);
        }
        if self.parts.is_empty() {
            return self.leading_wild || self.trailing_wild;
        }
        let mut i = 0;
        let s_bytes = s.as_bytes();
        for (idx, part) in self.parts.iter().enumerate() {
            let part_b = part.as_bytes();
            let from = if idx == 0 && !self.leading_wild { i } else { i };
            let found = if idx == 0 && !self.leading_wild {
                if s.starts_with(part.as_str()) {
                    Some(0)
                } else {
                    None
                }
            } else {
                find_substr(&s_bytes[from..], part_b).map(|p| p + from)
            };
            match found {
                Some(pos) => i = pos + part_b.len(),
                None => return false,
            }
        }
        // last part must reach end if no trailing wildcard
        if !self.trailing_wild && i != s.len() {
            return false;
        }
        true
    }
}

fn find_substr(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    hay.windows(needle.len()).position(|w| w == needle)
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
    let old_start_line = old_text[..byte_start].bytes().filter(|b| *b == b'\n').count();
    let old_end_line = old_text[..byte_end].bytes().filter(|b| *b == b'\n').count();
    let new_text = old_text.replacen(find, replace, 1);

    let old_lines: Vec<&str> = old_text.lines().collect();
    let new_lines: Vec<&str> = new_text.lines().collect();
    let old_changed_end = (old_end_line + 1).min(old_lines.len());
    let new_byte_end = byte_start + replace.len();
    let new_start_line = new_text[..byte_start].bytes().filter(|b| *b == b'\n').count();
    let new_end_line = new_text[..new_byte_end].bytes().filter(|b| *b == b'\n').count();
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
    out.push_str(&format!(
        "{ANSI_DIM}{new_line:>5}{ANSI_RESET}   {text}\n"
    ));
}

fn push_numbered_removed(out: &mut String, old_line: usize, text: &str) {
    out.push_str(&format!(
        "{ANSI_RED}{old_line:>5} - {text}{ANSI_RESET}\n"
    ));
}

fn push_numbered_added(out: &mut String, new_line: usize, text: &str) {
    out.push_str(&format!(
        "{ANSI_GREEN}{new_line:>5} + {text}{ANSI_RESET}\n"
    ));
}

#[cfg(test)]
mod tests {
    use super::{edit_preview, SimpleGlob, ANSI_GREEN, ANSI_RED};

    #[test]
    fn simple_glob_no_wildcard_is_substring() {
        let matcher = SimpleGlob::compile("vis");
        assert!(matcher.matches("adapter-zarvis"));
        assert!(matcher.matches("crates/adapter-zarvis/src/tools/fs.rs"));
        assert!(!matcher.matches("adapter-codex"));
    }

    #[test]
    fn simple_glob_keeps_anchored_wildcard_semantics() {
        let matcher = SimpleGlob::compile("src/*.rs");
        assert!(matcher.matches("src/tools/fs.rs"));
        assert!(!matcher.matches("crates/adapter-zarvis/src/tools/fs.rs"));

        let matcher = SimpleGlob::compile("*.rs");
        assert!(matcher.matches("fs.rs"));
        assert!(matcher.matches("src/tools/fs.rs"));
        assert!(!matcher.matches("README.md"));
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
}
