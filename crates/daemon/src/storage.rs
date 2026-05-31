//! File-based session storage.
//!
//! Layout under the data dir:
//!
//! ```text
//! sessions/<id>/
//!     meta.json          # SessionSummary (JSON)
//!     transcript.jsonl   # one TimestampedEvent per line
//!     worktree/          # optional git worktree
//! global/
//!     memory.md          # cross-project memory
//! projects/<id>/
//!     meta.json          # project metadata (GroupSummary JSON)
//!     memory.md          # project-specific memory
//! ```

use agentd_protocol::{
    GroupSummary, SessionSummary, TimestampedEvent, TranscriptResult, UiPanel, UiPlacement,
};
use anyhow::{Context, Result};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

const GLOBAL_MEMORY_TEMPLATE: &str =
    "# Global Memory\n\n## Preferences\n\n## Workflows\n\n## Pitfalls\n";
const PROJECT_MEMORY_TEMPLATE: &str =
    "# Project Memory\n\n## Overview\n\n## Architecture\n\n## Workflows\n\n## Decisions\n\n## Pitfalls\n";

#[derive(Debug, Default)]
struct WidgetFrontmatter {
    placement: Option<UiPlacement>,
    title: Option<String>,
}

pub struct Storage {
    data_dir: PathBuf,
}

impl Storage {
    pub fn new(data_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(data_dir.join("sessions"))
            .with_context(|| format!("create {}", data_dir.display()))?;
        std::fs::create_dir_all(data_dir.join("projects"))
            .with_context(|| format!("create {}", data_dir.join("projects").display()))?;
        Ok(Self { data_dir })
    }

    pub fn global_memory_path(&self) -> PathBuf {
        self.data_dir.join("global").join("memory.md")
    }

    pub fn projects_root(&self) -> PathBuf {
        self.data_dir.join("projects")
    }

    pub fn project_dir(&self, id: &str) -> PathBuf {
        self.projects_root().join(safe_memory_segment(id))
    }

    pub fn project_meta_path(&self, id: &str) -> PathBuf {
        self.project_dir(id).join("meta.json")
    }

    pub fn project_memory_path(&self, project_id: &str) -> PathBuf {
        self.project_dir(project_id).join("memory.md")
    }

    pub fn ensure_global_memory(&self) -> Result<PathBuf> {
        ensure_memory_file(&self.global_memory_path(), GLOBAL_MEMORY_TEMPLATE)
    }

    pub fn ensure_project_memory(&self, project_id: &str) -> Result<PathBuf> {
        ensure_memory_file(
            &self.project_memory_path(project_id),
            PROJECT_MEMORY_TEMPLATE,
        )
    }

    pub fn groups_root(&self) -> PathBuf {
        self.data_dir.join("groups")
    }

    pub fn group_path(&self, id: &str) -> PathBuf {
        self.groups_root().join(format!("{id}.json"))
    }

    pub fn save_group(&self, g: &GroupSummary) -> Result<()> {
        std::fs::create_dir_all(self.project_dir(&g.id))?;
        let path = self.project_meta_path(&g.id);
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(g)?;
        std::fs::write(&tmp, json).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("rename {}", path.display()))?;
        Ok(())
    }

    pub fn load_groups(&self) -> Result<Vec<GroupSummary>> {
        let mut out = Vec::new();
        let root = self.projects_root();
        if root.exists() {
            for entry in std::fs::read_dir(&root)? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                let path = entry.path().join("meta.json");
                if !path.exists() {
                    continue;
                }
                match std::fs::read(&path)
                    .and_then(|b| serde_json::from_slice::<GroupSummary>(&b).map_err(Into::into))
                {
                    Ok(g) => out.push(g),
                    Err(e) => {
                        tracing::warn!(path = %path.display(), error = %e, "skip unreadable project")
                    }
                }
            }
        }

        // Compatibility migration from the pre-project layout:
        // `<data>/groups/<id>.json` -> `<data>/projects/<id>/meta.json`.
        let legacy_root = self.groups_root();
        if legacy_root.exists() {
            for entry in std::fs::read_dir(&legacy_root)? {
                let entry = entry?;
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                match std::fs::read(&path)
                    .and_then(|b| serde_json::from_slice::<GroupSummary>(&b).map_err(Into::into))
                {
                    Ok(g) => {
                        if !out.iter().any(|existing| existing.id == g.id) {
                            if let Err(e) = self.save_group(&g) {
                                tracing::warn!(project = %g.id, error = ?e, "project migration save failed");
                            } else if let Err(e) = std::fs::remove_file(&path) {
                                tracing::warn!(path = %path.display(), error = ?e, "legacy group cleanup failed");
                            }
                            out.push(g);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(path = %path.display(), error = %e, "skip unreadable legacy group")
                    }
                }
            }
        }
        out.sort_by_key(|g| g.position);
        Ok(out)
    }

    pub fn remove_group(&self, id: &str) -> Result<()> {
        let p = self.project_meta_path(id);
        if p.exists() {
            std::fs::remove_file(&p).with_context(|| format!("remove {}", p.display()))?;
        }
        let legacy = self.group_path(id);
        if legacy.exists() {
            std::fs::remove_file(&legacy)
                .with_context(|| format!("remove {}", legacy.display()))?;
        }
        Ok(())
    }

    pub fn sessions_root(&self) -> PathBuf {
        self.data_dir.join("sessions")
    }

    pub fn session_dir(&self, id: &str) -> PathBuf {
        self.sessions_root().join(id)
    }

    pub fn meta_path(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("meta.json")
    }

    pub fn transcript_path(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("transcript.jsonl")
    }

    pub fn pty_log_path(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("pty.log")
    }

    pub fn worktree_path(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("worktree")
    }

    pub fn widgets_dir(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("widgets")
    }

    pub fn ensure_widgets_dir(&self, id: &str) -> Result<PathBuf> {
        self.ensure_session_dir(id)?;
        let dir = self.widgets_dir(id);
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        Ok(dir)
    }

    pub fn read_widgets(&self, id: &str) -> Result<Vec<UiPanel>> {
        let dir = self.widgets_dir(id);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut panels = Vec::new();
        for entry in std::fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Ok(raw_markdown) = std::fs::read_to_string(&path) else {
                continue;
            };
            let (frontmatter, markdown) = parse_widget_frontmatter(&raw_markdown);
            panels.push(UiPanel {
                id: stem.to_string(),
                source: path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map(str::to_string),
                title: frontmatter
                    .title
                    .or_else(|| Some(stem.replace(['-', '_'], " "))),
                placement: frontmatter.placement.unwrap_or(UiPlacement::Sticky),
                markdown,
            });
        }
        panels.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(panels)
    }

    pub fn delete_widget(&self, session_id: &str, panel_id: &str) -> Result<()> {
        if !is_safe_widget_id(panel_id) {
            anyhow::bail!("invalid widget id: {panel_id}");
        }
        let dir = self.widgets_dir(session_id);
        let path = dir.join(format!("{panel_id}.md"));
        if path.exists() {
            std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        }
        Ok(())
    }

    pub fn ensure_session_dir(&self, id: &str) -> Result<()> {
        let dir = self.session_dir(id);
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))
    }

    pub fn save_summary(&self, s: &SessionSummary) -> Result<()> {
        self.ensure_session_dir(&s.id)?;
        let path = self.meta_path(&s.id);
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(s)?;
        std::fs::write(&tmp, json).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("rename {}", path.display()))?;
        Ok(())
    }

    pub fn load_summary(&self, id: &str) -> Result<SessionSummary> {
        let path = self.meta_path(id);
        let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        let s: SessionSummary =
            serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
        Ok(s)
    }

    pub fn start_params_path(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("start.json")
    }

    /// Persist the params used to spawn this session, so a daemon restart
    /// can re-spawn with the same shape (cwd, harness, env, args, …).
    pub fn save_start_params(
        &self,
        id: &str,
        params: &agentd_protocol::SessionStartParams,
    ) -> Result<()> {
        self.ensure_session_dir(id)?;
        let path = self.start_params_path(id);
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(params)?;
        std::fs::write(&tmp, json).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("rename {}", path.display()))?;
        Ok(())
    }

    pub fn load_start_params(&self, id: &str) -> Result<agentd_protocol::SessionStartParams> {
        let path = self.start_params_path(id);
        let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        let p: agentd_protocol::SessionStartParams =
            serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
        Ok(p)
    }

    /// Path to the per-session cache of the most recent PTY size. Kept
    /// separate from `start.json` so the "params we were created with"
    /// stay immutable. Used on daemon respawn so the new adapter's PTY
    /// starts at the size the user last sized to, not the creation
    /// default — without this, claude/codex render their resume banner
    /// at the placeholder 80×10 and the TUI shows a tiny garbled box
    /// until the user nudges the terminal to trigger a fresh resize.
    pub fn pty_size_path(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("pty_size.json")
    }

    pub fn save_pty_size(&self, id: &str, size: agentd_protocol::PtySize) -> Result<()> {
        self.ensure_session_dir(id)?;
        let path = self.pty_size_path(id);
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string(&size)?;
        std::fs::write(&tmp, json).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("rename {}", path.display()))?;
        Ok(())
    }

    pub fn load_pty_size(&self, id: &str) -> Option<agentd_protocol::PtySize> {
        let path = self.pty_size_path(id);
        let bytes = std::fs::read(&path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub fn list_summaries(&self) -> Result<Vec<SessionSummary>> {
        let mut out = Vec::new();
        let root = self.sessions_root();
        if !root.exists() {
            return Ok(out);
        }
        for entry in std::fs::read_dir(&root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().to_string();
            match self.load_summary(&id) {
                Ok(s) => out.push(s),
                Err(e) => tracing::warn!(%id, error = ?e, "skipping unreadable session"),
            }
        }
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(out)
    }

    pub fn append_event(&self, id: &str, ev: &TimestampedEvent) -> Result<()> {
        self.ensure_session_dir(id)?;
        let path = self.transcript_path(id);
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        let line = serde_json::to_string(ev)?;
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")?;
        Ok(())
    }

    pub fn read_transcript(
        &self,
        id: &str,
        from: u64,
        limit: Option<usize>,
    ) -> Result<TranscriptResult> {
        let path = self.transcript_path(id);
        if !path.exists() {
            return Ok(TranscriptResult {
                events: Vec::new(),
                total: 0,
            });
        }
        let f = std::fs::File::open(&path)?;
        let reader = std::io::BufReader::new(f);
        let mut events: Vec<TimestampedEvent> = Vec::new();
        let mut total: u64 = 0;
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            total += 1;
            let ev: TimestampedEvent = match serde_json::from_str(&line) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(%id, error = %e, "skip bad transcript line");
                    continue;
                }
            };
            if ev.seq < from {
                continue;
            }
            events.push(ev);
            if let Some(lim) = limit {
                if events.len() >= lim {
                    break;
                }
            }
        }
        Ok(TranscriptResult { events, total })
    }

    /// Read the most-recent `n` events of the session's transcript without
    /// scanning the whole file. Seeks backward in 64 KiB chunks until we've
    /// gathered at least `n + 1` newlines (or hit the start), then parses
    /// only those lines as JSON. For a multi-GB transcript the cost is
    /// O(`n` × average line size), not O(file size), which is what makes
    /// the webui's tail-pagination fast enough to render the live tail
    /// before the user notices a wait.
    pub fn read_transcript_tail(&self, id: &str, n: usize) -> Result<Vec<TimestampedEvent>> {
        if n == 0 {
            return Ok(Vec::new());
        }
        let path = self.transcript_path(id);
        if !path.exists() {
            return Ok(Vec::new());
        }
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(&path)?;
        let size = f.metadata()?.len();
        if size == 0 {
            return Ok(Vec::new());
        }
        const CHUNK: u64 = 64 * 1024;
        let mut buf: Vec<u8> = Vec::new();
        let mut offset = size;
        let mut newlines = 0usize;
        // Read at least n + 1 newlines so the first (partial) line we slice
        // off doesn't accidentally chop a real event in half.
        while offset > 0 && newlines <= n {
            let to_read = CHUNK.min(offset);
            offset -= to_read;
            f.seek(SeekFrom::Start(offset))?;
            let mut chunk = vec![0u8; to_read as usize];
            f.read_exact(&mut chunk)?;
            newlines += chunk.iter().filter(|&&b| b == b'\n').count();
            chunk.extend_from_slice(&buf);
            buf = chunk;
        }
        // Take the last n non-empty lines.
        let text = String::from_utf8_lossy(&buf);
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        let start = lines.len().saturating_sub(n);
        let mut events: Vec<TimestampedEvent> = Vec::with_capacity(lines.len() - start);
        for line in &lines[start..] {
            match serde_json::from_str(line) {
                Ok(e) => events.push(e),
                Err(e) => tracing::warn!(%id, error = %e, "skip bad transcript line"),
            }
        }
        Ok(events)
    }

    pub fn truncate_transcript(&self, id: &str) -> Result<()> {
        let path = self.transcript_path(id);
        if !path.exists() {
            return Ok(());
        }
        std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
            .with_context(|| format!("truncate {}", path.display()))?;
        Ok(())
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Append raw PTY bytes to the session's `pty.log`. Best-effort; this
    /// gets called on every Pty event so it has to stay cheap. Append-only,
    /// no rotation — operators can truncate / rotate externally if needed.
    /// Truncate the session's `pty.log` to zero bytes. Called on
    /// session respawn so the new adapter's child can render into a
    /// clean PTY without bytes from the previous incarnation
    /// interfering with vt100 state.
    pub fn truncate_pty_log(&self, id: &str) -> Result<()> {
        let path = self.pty_log_path(id);
        if !path.exists() {
            return Ok(());
        }
        std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
            .with_context(|| format!("truncate {}", path.display()))?;
        Ok(())
    }

    pub fn append_pty_bytes(&self, id: &str, bytes: &[u8]) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.ensure_session_dir(id)?;
        use std::io::Write;
        let path = self.pty_log_path(id);
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        f.write_all(bytes)?;
        Ok(())
    }

    /// Read the last `max_bytes` of the session's `pty.log`. Returns an
    /// empty buffer if the file doesn't exist. Used by `pty_replay` to feed
    /// a TUI's vt100 parser on attach so scrollback covers the on-disk log,
    /// not just a small in-memory window.
    pub fn read_pty_tail(&self, id: &str, max_bytes: usize) -> Result<Vec<u8>> {
        let (bytes, _, _, _) = self.read_pty_range_before(id, max_bytes, None)?;
        Ok(bytes)
    }

    /// Read up to `max_bytes` ending before `before_offset` from `pty.log`.
    /// Offsets are absolute byte offsets in the file; `before_offset: None`
    /// means the current end. Returns `(bytes, start, end, total_len)`.
    pub fn read_pty_range_before(
        &self,
        id: &str,
        max_bytes: usize,
        before_offset: Option<u64>,
    ) -> Result<(Vec<u8>, u64, u64, u64)> {
        let path = self.pty_log_path(id);
        if !path.exists() || max_bytes == 0 {
            return Ok((Vec::new(), 0, 0, 0));
        }
        let mut f =
            std::fs::File::open(&path).with_context(|| format!("open {}", path.display()))?;
        let total = f.metadata()?.len();
        let end = before_offset.unwrap_or(total).min(total);
        let start = end.saturating_sub(max_bytes as u64);
        use std::io::{Read, Seek};
        f.seek(std::io::SeekFrom::Start(start))?;
        let mut buf = vec![0; (end - start) as usize];
        f.read_exact(&mut buf)?;
        Ok((buf, start, end, total))
    }

    /// Remove the entire session directory (meta + transcript + worktree).
    /// Idempotent: missing directory is not an error.
    pub fn remove_session(&self, id: &str) -> Result<()> {
        let dir = self.session_dir(id);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).with_context(|| format!("remove {}", dir.display()))?;
        }
        Ok(())
    }
}

fn is_safe_widget_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
}

fn parse_widget_frontmatter(raw: &str) -> (WidgetFrontmatter, String) {
    let Some(rest) = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))
    else {
        return (WidgetFrontmatter::default(), raw.to_string());
    };
    let mut byte_offset = raw.len().saturating_sub(rest.len());
    let mut frontmatter = String::new();
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        byte_offset += line.len();
        if trimmed == "---" {
            let parsed = parse_widget_frontmatter_fields(&frontmatter);
            return (parsed, raw[byte_offset..].to_string());
        }
        frontmatter.push_str(line);
    }
    (WidgetFrontmatter::default(), raw.to_string())
}

fn parse_widget_frontmatter_fields(frontmatter: &str) -> WidgetFrontmatter {
    let mut parsed = WidgetFrontmatter::default();
    for line in frontmatter.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().trim_matches(['"', '\'']);
        match key.trim() {
            "placement" if value.eq_ignore_ascii_case("inline") => {
                parsed.placement = Some(UiPlacement::Inline);
            }
            "placement" if value.eq_ignore_ascii_case("sticky") => {
                parsed.placement = Some(UiPlacement::Sticky);
            }
            "title" if !value.is_empty() => parsed.title = Some(value.to_string()),
            _ => {}
        }
    }
    parsed
}

fn ensure_memory_file(path: &Path, template: &str) -> Result<PathBuf> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    if !path.exists() {
        std::fs::write(path, template).with_context(|| format!("write {}", path.display()))?;
    }
    Ok(path.to_path_buf())
}

fn safe_memory_segment(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "unknown".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod widget_tests {
    use super::*;

    #[test]
    fn delete_widget_rejects_path_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();

        let err = storage.delete_widget("s1", "../memory").unwrap_err();

        assert!(err.to_string().contains("invalid widget id"));
    }

    #[test]
    fn read_widgets_parses_inline_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();
        let dir = storage.ensure_widgets_dir("s1").unwrap();
        std::fs::write(
            dir.join("confirm.md"),
            "---\nplacement: inline\ntitle: Confirm action\n---\n# Confirm\n\n[OK](agentd:action/ok?close=1)\n",
        )
        .unwrap();

        let widgets = storage.read_widgets("s1").unwrap();

        assert_eq!(widgets.len(), 1);
        assert_eq!(widgets[0].id, "confirm");
        assert_eq!(widgets[0].title.as_deref(), Some("Confirm action"));
        assert_eq!(widgets[0].placement, UiPlacement::Inline);
        assert!(widgets[0].markdown.starts_with("# Confirm"));
        assert!(!widgets[0].markdown.contains("placement:"));
    }
}

#[cfg(test)]
mod transcript_tail_tests {
    use super::*;
    use agentd_protocol::SessionEvent;
    use chrono::Utc;

    fn make_event(seq: u64) -> TimestampedEvent {
        TimestampedEvent {
            seq,
            at: Utc::now(),
            event: SessionEvent::Message {
                role: agentd_protocol::MessageRole::Assistant,
                text: format!("msg #{seq}"),
            },
        }
    }

    #[test]
    fn tail_returns_last_n_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();
        for seq in 1..=1000 {
            storage.append_event("s1", &make_event(seq)).unwrap();
        }

        let tail = storage.read_transcript_tail("s1", 5).unwrap();

        assert_eq!(tail.len(), 5);
        assert_eq!(tail.first().unwrap().seq, 996);
        assert_eq!(tail.last().unwrap().seq, 1000);
    }

    #[test]
    fn tail_handles_partial_first_chunk_without_chopping_an_event() {
        // The seek-back read can land mid-line; the parser must drop that
        // partial head, not parse it as garbage. Use long events so a
        // 64 KiB chunk easily spans a line boundary that isn't on a chunk
        // boundary.
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();
        let big_text = "x".repeat(2048);
        for seq in 1..=100 {
            let ev = TimestampedEvent {
                seq,
                at: Utc::now(),
                event: SessionEvent::Message {
                    role: agentd_protocol::MessageRole::Assistant,
                    text: format!("{big_text} #{seq}"),
                },
            };
            storage.append_event("s1", &ev).unwrap();
        }

        let tail = storage.read_transcript_tail("s1", 3).unwrap();

        assert_eq!(tail.len(), 3);
        assert_eq!(tail.first().unwrap().seq, 98);
        assert_eq!(tail.last().unwrap().seq, 100);
    }

    #[test]
    fn tail_returns_all_events_when_n_exceeds_total() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();
        for seq in 1..=7 {
            storage.append_event("s1", &make_event(seq)).unwrap();
        }

        let tail = storage.read_transcript_tail("s1", 100).unwrap();

        assert_eq!(tail.len(), 7);
    }

    #[test]
    fn tail_on_missing_transcript_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();

        let tail = storage.read_transcript_tail("never-existed", 10).unwrap();

        assert!(tail.is_empty());
    }

    #[test]
    fn tail_with_zero_n_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();
        storage.append_event("s1", &make_event(1)).unwrap();

        let tail = storage.read_transcript_tail("s1", 0).unwrap();

        assert!(tail.is_empty());
    }
}

#[cfg(test)]
mod pty_range_tests {
    use super::*;

    #[test]
    fn pty_range_before_returns_offsets_and_requested_slice() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();
        storage
            .append_pty_bytes("s1", b"abcdefghijklmnopqrstuvwxyz")
            .unwrap();

        let (bytes, start, end, total) = storage.read_pty_range_before("s1", 5, Some(20)).unwrap();

        assert_eq!(bytes, b"pqrst");
        assert_eq!(start, 15);
        assert_eq!(end, 20);
        assert_eq!(total, 26);
    }

    #[test]
    fn pty_range_before_clamps_to_file_start_and_end() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();
        storage.append_pty_bytes("s1", b"abcdef").unwrap();

        let (bytes, start, end, total) = storage.read_pty_range_before("s1", 99, Some(99)).unwrap();

        assert_eq!(bytes, b"abcdef");
        assert_eq!(start, 0);
        assert_eq!(end, 6);
        assert_eq!(total, 6);
    }
}

#[cfg(test)]
mod memory_tests {
    use super::*;

    #[test]
    fn creates_default_memory_files() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();

        let global = storage.ensure_global_memory().unwrap();
        let project = storage.ensure_project_memory("g123").unwrap();

        assert_eq!(global, tmp.path().join("data/global/memory.md"));
        assert_eq!(project, tmp.path().join("data/projects/g123/memory.md"));
        assert!(std::fs::read_to_string(global)
            .unwrap()
            .contains("## Preferences"));
        assert!(std::fs::read_to_string(project)
            .unwrap()
            .contains("## Architecture"));
    }

    #[test]
    fn project_memory_path_sanitizes_project_id() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();

        let path = storage.project_memory_path("../bad/project");

        assert_eq!(
            path,
            tmp.path().join("data/projects/___bad_project/memory.md")
        );
    }

    #[test]
    fn group_metadata_migrates_to_project_meta() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();
        std::fs::create_dir_all(storage.groups_root()).unwrap();
        let group = GroupSummary {
            id: "g123".into(),
            name: "Agentd".into(),
            created_at: chrono::Utc::now(),
            position: 7,
            collapsed: true,
        };
        std::fs::write(
            storage.group_path(&group.id),
            serde_json::to_string_pretty(&group).unwrap(),
        )
        .unwrap();

        let loaded = storage.load_groups().unwrap();

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "g123");
        assert!(storage.project_meta_path("g123").exists());
        assert!(!storage.group_path("g123").exists());
    }

    #[test]
    fn removing_project_metadata_preserves_memory_file() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();
        let group = GroupSummary {
            id: "g123".into(),
            name: "Agentd".into(),
            created_at: chrono::Utc::now(),
            position: 7,
            collapsed: true,
        };
        storage.save_group(&group).unwrap();
        storage.ensure_project_memory(&group.id).unwrap();

        storage.remove_group(&group.id).unwrap();

        assert!(!storage.project_meta_path(&group.id).exists());
        assert!(storage.project_memory_path(&group.id).exists());
    }
}
