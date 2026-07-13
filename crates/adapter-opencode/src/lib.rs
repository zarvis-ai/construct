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
use construct_protocol::adapter::{run as adapter_run, AdapterContext, EventEmitter};
use construct_protocol::{
    agent_context, Capabilities, InitializeResult, PtySize, SessionEvent, SessionStartParams,
};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

const SESSION_ID_FILE: &str = "opencode_session_id.txt";
const PLUGIN_FILE: &str = "construct-opencode-session.js";

const SESSION_PLUGIN: &str = r#"export const ConstructSession = async () => ({
  event: async ({ event }) => {
    if (event.type !== "session.created") return
    const info = event.properties?.info
    const forkFrom = process.env.CONSTRUCT_OPENCODE_FORK_FROM
    if (!info?.id || (info.parentID && info.parentID !== forkFrom)) return
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

    let native_id = resuming
        .then(|| session_file.as_deref().and_then(read_native_id))
        .flatten();
    let fork_from = (!resuming)
        .then(|| {
            std::env::var("CONSTRUCT_OPENCODE_FORK_FROM")
                .ok()
                .filter(|value| !value.is_empty())
        })
        .flatten();
    let mut args = command.args.clone();
    args.extend(params.args.clone());
    append_launch_args(
        &mut args,
        params.model.as_deref(),
        params.prompt.as_deref(),
        resuming,
        native_id.as_deref(),
        fork_from.as_deref(),
    );
    if resuming && native_id.is_none() {
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
        let mcp = construct_mcp_entry(&ctx.session_id);
        match install_session_integration(dir, inherited_config.as_ref(), mcp) {
            Ok(config) => {
                env.retain(|(key, _)| key != "OPENCODE_CONFIG_CONTENT");
                env.push(("OPENCODE_CONFIG_CONTENT".into(), config));
                env.push((
                    "CONSTRUCT_OPENCODE_SESSION_FILE".into(),
                    session_file.to_string_lossy().into_owned(),
                ));
                spawn_native_id_watcher(session_file.to_path_buf(), native_id, ctx.emit.clone());
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

fn append_launch_args(
    args: &mut Vec<String>,
    model: Option<&str>,
    prompt: Option<&str>,
    resuming: bool,
    native_id: Option<&str>,
    fork_from: Option<&str>,
) {
    if let Some(model) = model {
        args.extend(["--model".into(), model.into()]);
    }
    if let Some(id) = native_id {
        args.extend(["--session".into(), id.into()]);
    } else if let Some(parent) = fork_from {
        args.extend(["--session".into(), parent.into(), "--fork".into()]);
    }
    if !resuming {
        if let Some(prompt) = prompt.filter(|value| !value.trim().is_empty()) {
            args.extend(["--prompt".into(), prompt.into()]);
        }
    }
}

fn read_native_id(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| value.starts_with("ses_") && value.len() > 4)
}

fn install_session_integration(
    data_dir: &Path,
    existing_config: Option<&String>,
    construct_mcp: Option<Value>,
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
    if let Some(entry) = construct_mcp {
        let mcp = object
            .entry("mcp")
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("OPENCODE_CONFIG_CONTENT.mcp must be an object"))?;
        mcp.insert("construct".into(), entry);
    }
    Ok(serde_json::to_string(&config)?)
}

fn construct_mcp_entry(session_id: &str) -> Option<Value> {
    if std::env::var("CONSTRUCT_INJECT_MCP").as_deref() == Ok("0") {
        return None;
    }
    let bin = construct_protocol::paths::locate_sibling_binary("construct")?;
    Some(construct_mcp_entry_from(session_id, &bin, |name| {
        std::env::var(name).ok()
    }))
}

fn construct_mcp_entry_from(
    session_id: &str,
    bin: &Path,
    lookup: impl Fn(&str) -> Option<String>,
) -> Value {
    let mut environment = Map::new();
    environment.insert(
        agent_context::ENV_SESSION_ID.into(),
        Value::String(session_id.into()),
    );
    for name in agent_context::MCP_CONTEXT_ENV_VARS {
        if let Some(value) = lookup(name) {
            environment.insert((*name).into(), Value::String(value));
        }
    }
    serde_json::json!({
        "type": "local",
        "command": [bin.to_string_lossy(), "__mcp"],
        "environment": environment,
        "enabled": true,
    })
}

fn spawn_native_id_watcher(path: PathBuf, initial_id: Option<String>, emit: EventEmitter) {
    tokio::spawn(async move {
        let mut current = initial_id;
        let mut timer = tokio::time::interval(std::time::Duration::from_millis(100));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            timer.tick().await;
            let Some(observed) = read_native_id(&path) else {
                continue;
            };
            if let Some((prior_native_id, new_native_id)) = update_native_id(&mut current, observed)
            {
                emit.emit(SessionEvent::NativeIdChanged {
                    prior_native_id,
                    new_native_id,
                });
            }
        }
    });
}

fn update_native_id(current: &mut Option<String>, observed: String) -> Option<(String, String)> {
    match current {
        None => {
            *current = Some(observed);
            None
        }
        Some(existing) if *existing == observed => None,
        Some(existing) => {
            let prior = std::mem::replace(existing, observed.clone());
            Some((prior, observed))
        }
    }
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
        let merged = install_session_integration(tmp.path(), Some(&existing), None).unwrap();
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

    #[test]
    fn opencode_mcp_entry_carries_construct_context() {
        let entry = construct_mcp_entry_from("s123", Path::new("/tmp/construct"), |name| {
            (name == agent_context::ENV_PROJECT_ID).then(|| "g456".to_string())
        });
        assert_eq!(entry["type"], "local");
        assert_eq!(
            entry["command"],
            serde_json::json!(["/tmp/construct", "__mcp"])
        );
        assert_eq!(entry["environment"][agent_context::ENV_SESSION_ID], "s123");
        assert_eq!(entry["environment"][agent_context::ENV_PROJECT_ID], "g456");
    }

    #[test]
    fn integration_merge_preserves_user_mcp_and_adds_construct() {
        let tmp = tempfile::tempdir().unwrap();
        let existing =
            r#"{"mcp":{"user":{"type":"remote","url":"https://example.test"}}}"#.to_string();
        let construct = construct_mcp_entry_from("s1", Path::new("/bin/construct"), |_| None);
        let merged =
            install_session_integration(tmp.path(), Some(&existing), Some(construct)).unwrap();
        let value: Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(value["mcp"]["user"]["type"], "remote");
        assert_eq!(value["mcp"]["construct"]["type"], "local");
    }

    #[test]
    fn native_id_changes_ignore_initial_capture_and_detect_reset() {
        let mut current = None;
        assert_eq!(update_native_id(&mut current, "ses_a".into()), None);
        assert_eq!(update_native_id(&mut current, "ses_a".into()), None);
        assert_eq!(
            update_native_id(&mut current, "ses_b".into()),
            Some(("ses_a".into(), "ses_b".into()))
        );
    }

    #[test]
    fn native_fork_uses_parent_and_keeps_typed_prompt() {
        let mut args = Vec::new();
        append_launch_args(
            &mut args,
            Some("openai/gpt-5"),
            Some("try another approach"),
            false,
            None,
            Some("ses_parent"),
        );
        assert_eq!(
            args,
            [
                "--model",
                "openai/gpt-5",
                "--session",
                "ses_parent",
                "--fork",
                "--prompt",
                "try another approach",
            ]
        );
    }

    #[test]
    fn resume_uses_native_id_without_replaying_prompt() {
        let mut args = Vec::new();
        append_launch_args(
            &mut args,
            None,
            Some("original prompt"),
            true,
            Some("ses_resume"),
            None,
        );
        assert_eq!(args, ["--session", "ses_resume"]);
    }
}
