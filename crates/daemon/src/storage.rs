//! File-based session storage.
//!
//! Layout under the data dir:
//!
//! ```text
//! sessions/<id>/
//!     meta.json          # SessionSummary (JSON)
//!     transcript.jsonl   # one TimestampedEvent per line
//!     worktree/          # optional git worktree
//! ```

use agentd_protocol::{GroupSummary, SessionSummary, TimestampedEvent, TranscriptResult};
use anyhow::{Context, Result};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

pub struct Storage {
    data_dir: PathBuf,
}

impl Storage {
    pub fn new(data_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(data_dir.join("sessions"))
            .with_context(|| format!("create {}", data_dir.display()))?;
        std::fs::create_dir_all(data_dir.join("groups"))
            .with_context(|| format!("create {}", data_dir.join("groups").display()))?;
        Ok(Self { data_dir })
    }

    pub fn groups_root(&self) -> PathBuf {
        self.data_dir.join("groups")
    }

    pub fn group_path(&self, id: &str) -> PathBuf {
        self.groups_root().join(format!("{id}.json"))
    }

    pub fn save_group(&self, g: &GroupSummary) -> Result<()> {
        std::fs::create_dir_all(self.groups_root())?;
        let path = self.group_path(&g.id);
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(g)?;
        std::fs::write(&tmp, json).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("rename {}", path.display()))?;
        Ok(())
    }

    pub fn load_groups(&self) -> Result<Vec<GroupSummary>> {
        let mut out = Vec::new();
        let root = self.groups_root();
        if !root.exists() {
            return Ok(out);
        }
        for entry in std::fs::read_dir(&root)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match std::fs::read(&path)
                .and_then(|b| serde_json::from_slice::<GroupSummary>(&b).map_err(Into::into))
            {
                Ok(g) => out.push(g),
                Err(e) => tracing::warn!(path = %path.display(), error = %e, "skip unreadable group"),
            }
        }
        out.sort_by_key(|g| g.position);
        Ok(out)
    }

    pub fn remove_group(&self, id: &str) -> Result<()> {
        let p = self.group_path(id);
        if p.exists() {
            std::fs::remove_file(&p).with_context(|| format!("remove {}", p.display()))?;
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

    pub fn ensure_session_dir(&self, id: &str) -> Result<()> {
        let dir = self.session_dir(id);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create {}", dir.display()))
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
        let s: SessionSummary = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse {}", path.display()))?;
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

    pub fn load_start_params(
        &self,
        id: &str,
    ) -> Result<agentd_protocol::SessionStartParams> {
        let path = self.start_params_path(id);
        let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        let p: agentd_protocol::SessionStartParams = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse {}", path.display()))?;
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
    /// empty buffer if the file doesn't exist. Used to rehydrate the
    /// in-memory ring buffer on daemon startup so scrollback survives
    /// restarts.
    pub fn read_pty_tail(&self, id: &str, max_bytes: usize) -> Result<Vec<u8>> {
        let path = self.pty_log_path(id);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let mut f = std::fs::File::open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        let len = f.metadata()?.len() as usize;
        let offset = len.saturating_sub(max_bytes);
        if offset > 0 {
            use std::io::Seek;
            f.seek(std::io::SeekFrom::Start(offset as u64))?;
        }
        use std::io::Read;
        let mut buf = Vec::with_capacity(len - offset);
        f.read_to_end(&mut buf)?;
        Ok(buf)
    }

    /// Remove the entire session directory (meta + transcript + worktree).
    /// Idempotent: missing directory is not an error.
    pub fn remove_session(&self, id: &str) -> Result<()> {
        let dir = self.session_dir(id);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .with_context(|| format!("remove {}", dir.display()))?;
        }
        Ok(())
    }
}
