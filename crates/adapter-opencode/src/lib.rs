//! OpenCode CLI adapter.
//!
//! Runs OpenCode's native TUI under construct's PTY and injects a tiny local
//! OpenCode plugin that records the active native session id and the model
//! that session is answering with. The id lets a construct session resume the
//! same OpenCode conversation after a daemon restart, including after
//! OpenCode's `/new` or session-switching commands; the model keeps the
//! session's recorded model in step with OpenCode's own `/models` picker.
//!
//! Honors `CONSTRUCT_OPENCODE_CMD` for a full command prefix, falling back to
//! `CONSTRUCT_OPENCODE_BIN`, then `opencode` on `PATH`, then the standard
//! OpenCode installer location at `~/.opencode/bin/opencode`.

use construct_protocol::adapter::pty::{run_session as run_pty, PtySpec};
use construct_protocol::adapter::{run as adapter_run, AdapterContext, EventEmitter};
use construct_protocol::{
    agent_context, Capabilities, InitializeResult, PtySize, SessionEvent, SessionStartParams,
};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

const SESSION_ID_FILE: &str = "opencode_session_id.txt";
const MODEL_FILE: &str = "opencode_model.txt";
const USAGE_FILE: &str = "opencode_usage.json";
const PLUGIN_FILE: &str = "construct-opencode-session.js";

/// OpenCode keeps its conversations in a database shared by every OpenCode
/// process, so the active session id, the model it answers with, and its
/// token consumption all have to be observed from inside our own process.
/// The plugin records the session id on creation and, for every assistant
/// reply on that session, the `provider/model` pair OpenCode actually used —
/// the same form OpenCode's own model flag takes, so a resumed session can
/// be put back on it — plus a cumulative token tally (spec 0103): each
/// COMPLETED assistant message's tokens are added once (deduped by message
/// id — `message.updated` fires repeatedly while a reply streams) and the
/// running totals written as one JSON object the adapter polls for deltas.
/// Assistant replies from child sessions (OpenCode's own subagents) carry a
/// different session id and are ignored, so a subagent's model/usage never
/// bleeds into the conversation's.
const SESSION_PLUGIN: &str = r#"export const ConstructSession = async () => {
  const sessionFile = process.env.CONSTRUCT_OPENCODE_SESSION_FILE
  const modelFile = process.env.CONSTRUCT_OPENCODE_MODEL_FILE
  const usageFile = process.env.CONSTRUCT_OPENCODE_USAGE_FILE
  const forkFrom = process.env.CONSTRUCT_OPENCODE_FORK_FROM
  let rootId = null
  const usageTotals = { input: 0, output: 0, cached: 0, context: 0 }
  const usageSeen = new Set()
  const root = async () => {
    if (!rootId && sessionFile) {
      const recorded = await Bun.file(sessionFile).text().catch(() => "")
      rootId = recorded.trim() || null
    }
    return rootId
  }
  return {
    event: async ({ event }) => {
      const info = event.properties?.info
      if (event.type === "session.created") {
        if (!info?.id || (info.parentID && info.parentID !== forkFrom)) return
        rootId = info.id
        if (sessionFile) await Bun.write(sessionFile, info.id + "\n")
        return
      }
      if (event.type === "message.updated") {
        if (info?.role !== "assistant") return
        if (info.sessionID !== (await root())) return
        if (modelFile && info.providerID && info.modelID) {
          await Bun.write(modelFile, info.providerID + "/" + info.modelID + "\n")
        }
        const t = info.tokens
        if (usageFile && t && info.time?.completed && info.id && !usageSeen.has(info.id)) {
          usageSeen.add(info.id)
          const cache = t.cache || {}
          const promptSide = (t.input || 0) + (cache.read || 0) + (cache.write || 0)
          usageTotals.input += promptSide
          usageTotals.output += (t.output || 0) + (t.reasoning || 0)
          usageTotals.cached += cache.read || 0
          usageTotals.context = promptSide
          await Bun.write(usageFile, JSON.stringify(usageTotals) + "\n")
        }
      }
    },
  }
}
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
    let default_bin = construct_protocol::adapter::default_cli_bin_with_home_fallback(
        "opencode",
        Path::new(".opencode/bin/opencode"),
    );
    let command = construct_protocol::adapter::resolve_command_override(
        "CONSTRUCT_OPENCODE_CMD",
        "CONSTRUCT_OPENCODE_BIN",
        &default_bin,
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
                let model_file = dir.join(MODEL_FILE);
                let usage_file = dir.join(USAGE_FILE);
                env.retain(|(key, _)| key != "OPENCODE_CONFIG_CONTENT");
                env.push(("OPENCODE_CONFIG_CONTENT".into(), config));
                env.push((
                    "CONSTRUCT_OPENCODE_SESSION_FILE".into(),
                    session_file.to_string_lossy().into_owned(),
                ));
                env.push((
                    "CONSTRUCT_OPENCODE_MODEL_FILE".into(),
                    model_file.to_string_lossy().into_owned(),
                ));
                env.push((
                    "CONSTRUCT_OPENCODE_USAGE_FILE".into(),
                    usage_file.to_string_lossy().into_owned(),
                ));
                spawn_native_id_watcher(session_file.to_path_buf(), native_id, ctx.emit.clone());
                spawn_model_watcher(model_file, params.model.clone(), ctx.emit.clone());
                spawn_usage_watcher(usage_file, ctx.emit.clone());
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

/// The `provider/model` pair the plugin last recorded, rejecting anything
/// that isn't in that shape — a half-written file, or a future OpenCode that
/// stops reporting one of the two halves, must not be reported as the
/// session's model. The model half may itself contain slashes (an OpenRouter
/// id like `openrouter/anthropic/claude-sonnet-4`), so only the first
/// separator is structural.
fn read_model(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.contains(char::is_whitespace))
        .filter(|value| {
            value
                .split_once('/')
                .is_some_and(|(provider, model)| !provider.is_empty() && !model.is_empty())
        })
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

/// Report the model OpenCode is actually answering with. Seeded with the
/// model the daemon asked for at launch (a resume re-injects the recorded
/// one), so a session that comes back on the same model stays quiet and only
/// a real change — the first reply of a session started without an explicit
/// model, or a mid-session switch through OpenCode's own picker — is
/// reported.
fn spawn_model_watcher(path: PathBuf, requested: Option<String>, emit: EventEmitter) {
    tokio::spawn(async move {
        let mut current = requested;
        let mut timer = tokio::time::interval(std::time::Duration::from_millis(100));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            timer.tick().await;
            let Some(observed) = read_model(&path) else {
                continue;
            };
            if update_model(&mut current, observed.clone()) {
                emit.emit(SessionEvent::ModelChanged { model: observed });
            }
        }
    });
}

/// Token totals as the plugin writes them: cumulative `input`/`output`/
/// `cached` (spec 0103; `input` already covers the full prompt side —
/// fresh + cache reads + cache writes — and `cached` is the read subset),
/// plus `context`, the LAST completed message's prompt side — the live
/// context gauge (spec 0104), a snapshot rather than a sum.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct UsageTotals {
    input: u64,
    output: u64,
    cached: u64,
    context: u64,
}

/// Turn the plugin's cumulative usage file into per-poll Cost deltas. The
/// baseline seeds from whatever the file already holds at spawn, so a
/// respawn never re-reports history the daemon's transcript already counted
/// (a stale file from before the restart). A shrinking total means the
/// plugin restarted and began a fresh count — rebase silently, then report
/// growth from there.
fn spawn_usage_watcher(path: PathBuf, emit: EventEmitter) {
    tokio::spawn(async move {
        let mut reported = read_usage(&path).unwrap_or_default();
        let mut timer = tokio::time::interval(std::time::Duration::from_millis(100));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            timer.tick().await;
            let Some(observed) = read_usage(&path) else {
                continue;
            };
            for event in usage_delta_events(&mut reported, observed) {
                emit.emit(event);
            }
        }
    });
}

fn read_usage(path: &Path) -> Option<UsageTotals> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(text.trim()).ok()?;
    let field = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
    Some(UsageTotals {
        input: field("input"),
        output: field("output"),
        cached: field("cached"),
        context: field("context"),
    })
}

fn usage_delta_events(reported: &mut UsageTotals, observed: UsageTotals) -> Vec<SessionEvent> {
    if observed.input < reported.input
        || observed.output < reported.output
        || observed.cached < reported.cached
    {
        // Plugin restart: totals rebased to zero. Adopt the new baseline
        // without emitting — the drop isn't negative usage.
        *reported = observed;
        return Vec::new();
    }
    let d_in = observed.input - reported.input;
    let d_out = observed.output - reported.output;
    let d_cached = observed.cached - reported.cached;
    let context_changed = observed.context != reported.context && observed.context > 0;
    if d_in == 0 && d_out == 0 && d_cached == 0 && !context_changed {
        return Vec::new();
    }
    *reported = observed;
    let mut out = Vec::new();
    if d_in > 0 || d_out > 0 || d_cached > 0 {
        out.push(SessionEvent::Cost {
            usd: 0.0,
            tokens_in: d_in,
            tokens_out: d_out,
            tokens_cached: d_cached,
        });
    }
    if context_changed {
        // OpenCode states no window size; bare usage only (spec 0104).
        out.push(SessionEvent::ContextUsage {
            used_tokens: observed.context,
            window_tokens: None,
        });
    }
    out
}

fn update_model(current: &mut Option<String>, observed: String) -> bool {
    if current.as_deref() == Some(observed.as_str()) {
        return false;
    }
    *current = Some(observed);
    true
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
    fn usage_deltas_emit_once_and_rebase_on_plugin_restart() {
        let mut reported = UsageTotals::default();
        let first = UsageTotals {
            input: 11_000,
            output: 120,
            cached: 2_000,
            context: 11_000,
        };
        match usage_delta_events(&mut reported, first).as_slice() {
            [SessionEvent::Cost {
                tokens_in,
                tokens_out,
                tokens_cached,
                ..
            }, SessionEvent::ContextUsage {
                used_tokens,
                window_tokens,
            }] => {
                assert_eq!(*tokens_in, 11_000);
                assert_eq!(*tokens_out, 120);
                assert_eq!(*tokens_cached, 2_000);
                assert_eq!(*used_tokens, 11_000);
                assert_eq!(*window_tokens, None);
            }
            other => panic!("expected Cost + ContextUsage: {other:?}"),
        }
        // Unchanged totals stay quiet; growth reports only the delta.
        assert!(usage_delta_events(&mut reported, first).is_empty());
        let second = UsageTotals {
            input: 15_000,
            output: 200,
            cached: 5_000,
            context: 14_800,
        };
        match usage_delta_events(&mut reported, second).as_slice() {
            [SessionEvent::Cost {
                tokens_in,
                tokens_out,
                tokens_cached,
                ..
            }, SessionEvent::ContextUsage { used_tokens, .. }] => {
                assert_eq!(*tokens_in, 4_000);
                assert_eq!(*tokens_out, 80);
                assert_eq!(*tokens_cached, 3_000);
                assert_eq!(*used_tokens, 14_800);
            }
            other => panic!("expected delta Cost + ContextUsage: {other:?}"),
        }
        // A shrinking total = plugin restarted and rebased to zero: adopt
        // the new baseline silently, then report growth from there.
        let rebased = UsageTotals {
            input: 500,
            output: 10,
            cached: 0,
            context: 500,
        };
        assert!(usage_delta_events(&mut reported, rebased).is_empty());
        let grown = UsageTotals {
            input: 900,
            output: 25,
            cached: 100,
            context: 900,
        };
        match usage_delta_events(&mut reported, grown).as_slice() {
            [SessionEvent::Cost {
                tokens_in,
                tokens_out,
                tokens_cached,
                ..
            }, SessionEvent::ContextUsage { used_tokens, .. }] => {
                assert_eq!(*tokens_in, 400);
                assert_eq!(*tokens_out, 15);
                assert_eq!(*tokens_cached, 100);
                assert_eq!(*used_tokens, 900);
            }
            other => panic!("expected post-rebase delta: {other:?}"),
        }
    }

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
    fn model_requires_provider_and_model_halves() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(MODEL_FILE);
        std::fs::write(&path, "anthropic/claude-sonnet-4-5\n").unwrap();
        assert_eq!(
            read_model(&path).as_deref(),
            Some("anthropic/claude-sonnet-4-5")
        );
        // An OpenRouter-style id keeps the slashes past the provider.
        std::fs::write(&path, "openrouter/anthropic/claude-sonnet-4\n").unwrap();
        assert_eq!(
            read_model(&path).as_deref(),
            Some("openrouter/anthropic/claude-sonnet-4")
        );
        for junk in ["", "\n", "anthropic", "/claude", "anthropic/"] {
            std::fs::write(&path, junk).unwrap();
            assert_eq!(read_model(&path), None, "accepted junk model {junk:?}");
        }
    }

    #[test]
    fn model_changes_report_first_observation_and_live_switch() {
        let first = "meta/muse-spark-1.1";
        let switched = "anthropic/claude-sonnet-4-5";
        // No model requested at launch: the first reply establishes it.
        let mut current = None;
        assert!(update_model(&mut current, first.into()));
        assert!(!update_model(&mut current, first.into()));
        // A switch through OpenCode's own picker is a change.
        assert!(update_model(&mut current, switched.into()));
        assert_eq!(current.as_deref(), Some(switched));
    }

    #[test]
    fn resumed_model_is_not_rereported() {
        // Respawn re-injects the recorded model, so replies on that same
        // model must not churn the daemon with a redundant change.
        let resumed = "anthropic/claude-sonnet-4-5";
        let mut current = Some(resumed.to_string());
        assert!(!update_model(&mut current, resumed.into()));
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
