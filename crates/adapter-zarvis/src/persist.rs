//! Per-session message persistence — `zarvis.jsonl`.
//!
//! One JSON-serialized [`Message`] per line, append-only. The agent
//! loop writes to it as it pushes messages into the in-memory vec; on
//! daemon-restart resume the loop loads the file back to rebuild
//! context from where it left off.
//!
//! Best-effort throughout — disk errors log a warning and are otherwise
//! ignored. We never abandon a turn because we couldn't checkpoint.

use crate::provider::Message;
use anyhow::Result;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

pub struct Persist {
    path: PathBuf,
    file: Option<File>,
}

impl Persist {
    /// Create a persister rooted at `<session_data_dir>/zarvis.jsonl`,
    /// or `None` when no data dir was provided (the daemon should always
    /// set one, but in standalone invocations it may be missing).
    pub fn open(session_data_dir: Option<&Path>) -> Option<Self> {
        let dir = session_data_dir?;
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::warn!(dir = %dir.display(), error = ?e, "zarvis persist: mkdir failed");
            return None;
        }
        let path = dir.join("zarvis.jsonl");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| {
                tracing::warn!(path = %path.display(), error = ?e, "zarvis persist: open failed");
                e
            })
            .ok()?;
        Some(Self {
            path,
            file: Some(file),
        })
    }

    /// Append a single message. Best-effort: log + drop on failure.
    pub fn append(&mut self, msg: &Message) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let line = match serde_json::to_string(msg) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = ?e, "zarvis persist: serialize failed");
                return;
            }
        };
        if let Err(e) = writeln!(file, "{line}") {
            tracing::warn!(error = ?e, "zarvis persist: write failed");
        }
        let _ = file.flush();
    }

    /// Clear persisted conversation context and reopen the append handle.
    pub fn reset(&mut self) {
        self.file = None;
        match OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.path)
        {
            Ok(file) => self.file = Some(file),
            Err(e) => tracing::warn!(
                path = %self.path.display(),
                error = ?e,
                "zarvis persist: reset failed"
            ),
        }
    }

    /// Read every message from the file. Skips malformed lines (logged
    /// at warn) so a single corrupt entry doesn't abandon the rest.
    pub fn load(path: &Path) -> Result<Vec<Message>> {
        let f = File::open(path)?;
        let reader = BufReader::new(f);
        let mut out = Vec::new();
        for (i, line) in reader.lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Message>(&line) {
                Ok(m) => out.push(m),
                Err(e) => {
                    tracing::warn!(line = i + 1, error = ?e, "zarvis persist: skipping malformed line");
                }
            }
        }
        Ok(out)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Resolve the session data dir from env, if any.
pub fn session_data_dir_from_env() -> Option<PathBuf> {
    std::env::var("AGENTD_SESSION_DATA_DIR").ok().map(PathBuf::from)
}

/// True if the daemon signaled this is a resumed session.
pub fn is_resume() -> bool {
    std::env::var("AGENTD_RESUME").as_deref() == Ok("1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Content, Message, Role};

    #[test]
    fn reset_truncates_persisted_messages_and_keeps_appending() {
        let dir = std::env::temp_dir().join(format!(
            "agentd-zarvis-persist-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut persist = Persist::open(Some(&dir)).unwrap();
        persist.append(&Message {
            role: Role::User,
            content: Content::Text { text: "old".into() },
        });
        assert_eq!(Persist::load(persist.path()).unwrap().len(), 1);

        persist.reset();
        assert!(Persist::load(persist.path()).unwrap().is_empty());

        persist.append(&Message {
            role: Role::User,
            content: Content::Text { text: "new".into() },
        });
        let loaded = Persist::load(persist.path()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(matches!(
            &loaded[0].content,
            Content::Text { text } if text == "new"
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
