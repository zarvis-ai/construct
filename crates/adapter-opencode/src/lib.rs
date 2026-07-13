//! OpenCode CLI adapter.
//!
//! Runs OpenCode's native TUI under construct's PTY and injects a tiny local
//! OpenCode plugin that records the active native session id. That id lets a
//! construct session resume the same OpenCode conversation after a daemon
//! restart, including after OpenCode's `/new` or session-switching commands.
//!
//! Honors `CONSTRUCT_OPENCODE_CMD` for a full command prefix, falling back to
//! `CONSTRUCT_OPENCODE_BIN`, then `opencode` on `PATH`.

use construct_protocol::adapter::pty::{run_session as run_pty, PtySpec};
use construct_protocol::adapter::{run as adapter_run, AdapterContext};
use construct_protocol::{Capabilities, InitializeResult, PtySize, SessionStartParams};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

const SESSION_ID_FILE: &str = "opencode_session_id.txt";
const PLUGIN_FILE: &str = "construct-opencode-session.js";

const SESSION_PLUGIN: &str = r#"export const ConstructSession = async () => ({
  event: async ({ event }) => {
    if (event.type !== "session.created" && event.type !== "session.updated") return
    const info = event.properties?.info
    if (!info?.id || info.parentID) return
    const file = process.env.CONSTRUCT_OPENCODE_SESSION_FILE
    if (file) await Bun.write(file, info.id + "\n")
  },
})
"#;

pub async fn run() -> anyhow::Result<()> {
    let metadata = InitializeResult {
        name: "opencode".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        capabilities: Capabilities {
            supports_input: true,
            supports_interrupt: true,
            supports_pty: true,
            ..Default::default()
        },
    };
    adapter_run(metadata, run_interactive).await
}

async fn run_interactive(params: SessionStartParams, ctx: AdapterContext) {
    let command = construct_protocol::adapter::resolve_command_override(
        "CONSTRUCT_OPENCODE_CMD",
        "CONSTRUCT_OPENCODE_BIN",
        "opencode",
    );
    let resuming = std::env::var("CONSTRUCT_RESUME").as_deref() == Ok("1");
    let data_dir = std::env::var("CONSTRUCT_SESSION_DATA_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    let session_file = data_dir.as_ref().map(|d| d.join(SESSION_ID_FILE));

    let mut args = command.args.clone();
    args.extend(params.args.clone());
    if let Some(model) = params.model.as_ref() {
        args.extend(["--model".into(), model.clone()]);
    }

    let native_id = resuming
        .then(|| session_file.as_deref().and_then(read_native_id))
        .flatten();
    if let Some(id) = native_id.as_ref() {
        args.extend(["--session".into(), id.clone()]);
    } else if !resuming {
        if let Some(prompt) = params.prompt.as_ref().filter(|p| !p.trim().is_empty()) {
            args.extend(["--prompt".into(), prompt.clone()]);
        }
    } else {
        ctx.emit
            .log("opencode respawn: no captured native session id; starting a fresh conversation");
    }

    let mut env: Vec<(String, String)> = params
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    env.push(("CONSTRUCT_SESSION_ID".into(), ctx.session_id.clone()));
    if let (Some(dir), Some(session_file)) = (data_dir.as_deref(), session_file.as_deref()) {
        let inherited_config = params
            .env
            .get("OPENCODE_CONFIG_CONTENT")
            .cloned()
            .or_else(|| std::env::var("OPENCODE_CONFIG_CONTENT").ok());
        match install_session_plugin(dir, inherited_config.as_ref()) {
            Ok(config) => {
                env.retain(|(key, _)| key != "OPENCODE_CONFIG_CONTENT");
                env.push(("OPENCODE_CONFIG_CONTENT".into(), config));
                env.push((
                    "CONSTRUCT_OPENCODE_SESSION_FILE".into(),
                    session_file.to_string_lossy().into_owned(),
                ));
            }
            Err(error) => ctx.emit.log(format!(
                "opencode native-session capture disabled: {error:#}"
            )),
        }
    }

    let label = command.argv_preview();
    let spec = PtySpec {
        bin: command.bin,
        args,
        cwd: PathBuf::from(&params.cwd),
        env,
        size: params.pty_size.unwrap_or(PtySize {
            cols: 100,
            rows: 30,
        }),
        status_detail: Some(format!("{label} (interactive)")),
        detect_prompt_via_pgroup: false,
    };
    let _ = run_pty(spec, ctx).await;
}

fn read_native_id(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| value.starts_with("ses_") && value.len() > 4)
}

fn install_session_plugin(
    data_dir: &Path,
    existing_config: Option<&String>,
) -> anyhow::Result<String> {
    std::fs::create_dir_all(data_dir)?;
    let plugin_path = data_dir.join(PLUGIN_FILE);
    std::fs::write(&plugin_path, SESSION_PLUGIN)?;
    let plugin_url = file_url(&plugin_path);

    let mut config = match existing_config.filter(|s| !s.trim().is_empty()) {
        Some(raw) => serde_json::from_str::<Value>(raw)?,
        None => Value::Object(Map::new()),
    };
    let object = config
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("OPENCODE_CONFIG_CONTENT must be a JSON object"))?;
    let plugins = object
        .entry("plugin")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("OPENCODE_CONFIG_CONTENT.plugin must be an array"))?;
    if !plugins
        .iter()
        .any(|value| value.as_str() == Some(&plugin_url))
    {
        plugins.push(Value::String(plugin_url));
    }
    Ok(serde_json::to_string(&config)?)
}

fn file_url(path: &Path) -> String {
    let encoded = path
        .to_string_lossy()
        .replace('%', "%25")
        .replace(' ', "%20")
        .replace('#', "%23")
        .replace('?', "%3F");
    format!("file://{encoded}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_id_requires_opencode_session_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(SESSION_ID_FILE);
        std::fs::write(&path, "not-a-session\n").unwrap();
        assert_eq!(read_native_id(&path), None);
        std::fs::write(&path, "ses_abc123\n").unwrap();
        assert_eq!(read_native_id(&path).as_deref(), Some("ses_abc123"));
    }

    #[test]
    fn plugin_injection_preserves_existing_inline_config() {
        let tmp = tempfile::tempdir().unwrap();
        let existing = r#"{"theme":"catppuccin","plugin":["existing-plugin"]}"#.to_string();
        let merged = install_session_plugin(tmp.path(), Some(&existing)).unwrap();
        let value: Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(value["theme"], "catppuccin");
        let plugins = value["plugin"].as_array().unwrap();
        assert_eq!(plugins[0], "existing-plugin");
        assert!(plugins[1]
            .as_str()
            .unwrap()
            .ends_with("construct-opencode-session.js"));
        assert!(tmp.path().join(PLUGIN_FILE).exists());
    }

    #[test]
    fn plugin_file_url_escapes_path_delimiters() {
        assert_eq!(
            file_url(Path::new("/tmp/a b/c#d?.js")),
            "file:///tmp/a%20b/c%23d%3F.js"
        );
    }
}
