//! XDG-style path conventions shared between daemon and client.
//!
//! Each layer respects `CONSTRUCT_*_DIR` env overrides, then `XDG_*_HOME`,
//! falling back to standard `$HOME/.config|.local/state|.local/share/construct`.

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

        let config_dir = env_dir("CONSTRUCT_CONFIG_DIR").unwrap_or_else(|| {
            env_dir("XDG_CONFIG_HOME")
                .unwrap_or_else(|| home.join(".config"))
                .join("construct")
        });
        let state_dir = env_dir("CONSTRUCT_STATE_DIR").unwrap_or_else(|| {
            env_dir("XDG_STATE_HOME")
                .unwrap_or_else(|| home.join(".local").join("state"))
                .join("construct")
        });
        let data_dir = env_dir("CONSTRUCT_DATA_DIR").unwrap_or_else(|| {
            env_dir("XDG_DATA_HOME")
                .unwrap_or_else(|| home.join(".local").join("share"))
                .join("construct")
        });
        let runtime_dir = env_dir("CONSTRUCT_RUNTIME_DIR")
            .or_else(|| env_dir("XDG_RUNTIME_DIR").map(|p| p.join("construct")))
            .unwrap_or_else(|| state_dir.clone());

        Self {
            config_dir,
            state_dir,
            data_dir,
            runtime_dir,
        }
    }

    /// Resolve the legacy `agentd` layout so startup can offer a migration
    /// message when existing `~/.config|.local|XDG_*` directories are still
    /// using pre-rename names.
    pub fn discover_legacy() -> Self {
        let home = home_dir();

        let config_dir = env_dir("XDG_CONFIG_HOME")
            .unwrap_or_else(|| home.join(".config"))
            .join("agentd");
        let state_dir = env_dir("XDG_STATE_HOME")
            .unwrap_or_else(|| home.join(".local").join("state"))
            .join("agentd");
        let data_dir = env_dir("XDG_DATA_HOME")
            .unwrap_or_else(|| home.join(".local").join("share"))
            .join("agentd");
        let runtime_dir = env_dir("XDG_RUNTIME_DIR")
            .map(|p| p.join("agentd"))
            .unwrap_or_else(|| state_dir.clone());

        Self {
            config_dir,
            state_dir,
            data_dir,
            runtime_dir,
        }
    }

    pub fn socket(&self) -> PathBuf {
        self.runtime_dir.join("construct.sock")
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

    pub fn config_template_file(&self) -> PathBuf {
        self.config_dir.join("config.toml.template")
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

    pub fn tui_state_file(&self) -> PathBuf {
        self.state_dir.join("tui-state.json")
    }

    /// Path to the learned per-model token-limit table — smith
    /// adapts this at runtime when providers reject requests as
    /// over-budget and bumps it on successful probe calls.
    pub fn smith_model_limits_file(&self) -> PathBuf {
        self.state_dir.join("smith-model-limits.json")
    }
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Default port for the localhost-only browser UI. Override with the
/// `CONSTRUCT_WEBUI_PORT` env var. The daemon binds `127.0.0.1:<port>`; the
/// CLI's `construct paths` prints the resolved URL.
pub const DEFAULT_WEBUI_PORT: u16 = 5746;

/// Resolve the localhost web-UI port from `CONSTRUCT_WEBUI_PORT`, falling back
/// to [`DEFAULT_WEBUI_PORT`] when the var is unset or unparseable.
pub fn local_webui_port() -> u16 {
    std::env::var("CONSTRUCT_WEBUI_PORT")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(DEFAULT_WEBUI_PORT)
}

/// The resolved localhost web-UI URL (`http://127.0.0.1:<port>/`).
pub fn local_webui_url() -> String {
    format!("http://127.0.0.1:{}/", local_webui_port())
}

/// Resolve a sibling binary (an adapter, `construct-mcp`, etc.) by name.
/// Search order: absolute path → next to the current executable → `$PATH`.
/// Returns `None` if not found. Used by the daemon to find adapter
/// binaries and by adapters to find auxiliary tools like `construct-mcp`.
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
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}
