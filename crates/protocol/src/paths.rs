//! XDG-style path conventions shared between daemon and client.
//!
//! Each layer respects `AGENTD_*_DIR` env overrides, then `XDG_*_HOME`,
//! falling back to standard `$HOME/.config|.local/state|.local/share/agentd`.

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    pub data_dir: PathBuf,
    pub runtime_dir: PathBuf,
}

impl Paths {
    pub fn discover() -> Self {
        let home = home_dir();

        let config_dir = env_dir("AGENTD_CONFIG_DIR").unwrap_or_else(|| {
            env_dir("XDG_CONFIG_HOME")
                .unwrap_or_else(|| home.join(".config"))
                .join("agentd")
        });
        let state_dir = env_dir("AGENTD_STATE_DIR").unwrap_or_else(|| {
            env_dir("XDG_STATE_HOME")
                .unwrap_or_else(|| home.join(".local").join("state"))
                .join("agentd")
        });
        let data_dir = env_dir("AGENTD_DATA_DIR").unwrap_or_else(|| {
            env_dir("XDG_DATA_HOME")
                .unwrap_or_else(|| home.join(".local").join("share"))
                .join("agentd")
        });
        let runtime_dir = env_dir("AGENTD_RUNTIME_DIR")
            .or_else(|| env_dir("XDG_RUNTIME_DIR").map(|p| p.join("agentd")))
            .unwrap_or_else(|| state_dir.clone());

        Self {
            config_dir,
            state_dir,
            data_dir,
            runtime_dir,
        }
    }

    pub fn socket(&self) -> PathBuf {
        self.runtime_dir.join("agentd.sock")
    }

    pub fn pid_file(&self) -> PathBuf {
        self.state_dir.join("daemon.pid")
    }

    pub fn log_file(&self) -> PathBuf {
        self.state_dir.join("daemon.log")
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn keymap_file(&self) -> PathBuf {
        self.config_dir.join("keymap.toml")
    }

    pub fn sessions_root(&self) -> PathBuf {
        self.data_dir.join("sessions")
    }

    pub fn session_dir(&self, id: &str) -> PathBuf {
        self.sessions_root().join(id)
    }
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Resolve a sibling binary (an adapter, `agentd-mcp`, etc.) by name.
/// Search order: absolute path → next to the current executable → `$PATH`.
/// Returns `None` if not found. Used by the daemon to find adapter
/// binaries and by adapters to find auxiliary tools like `agentd-mcp`.
pub fn locate_sibling_binary(name: &str) -> Option<PathBuf> {
    let p = PathBuf::from(name);
    if p.is_absolute() {
        return p.exists().then_some(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(&p);
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from).filter(|p| !p.as_os_str().is_empty())
}
