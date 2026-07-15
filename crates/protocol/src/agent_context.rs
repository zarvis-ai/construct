//! Shared construct context surfaced to agents through agent context tools.
//!
//! The daemon passes memory file paths in env vars. This module reads those
//! files and formats one stable JSON shape used by both `construct-mcp` and
//! smith's native tool layer.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const TOOL_NAME: &str = "agentd_context";
pub const ENV_GLOBAL_MEMORY_FILE: &str = "CONSTRUCT_GLOBAL_MEMORY_FILE";
pub const ENV_PROJECT_MEMORY_FILE: &str = "CONSTRUCT_PROJECT_MEMORY_FILE";
pub const ENV_PROGRAM_RUN_CONTEXT_FILE: &str = "CONSTRUCT_PROGRAM_RUN_CONTEXT_FILE";
pub const ENV_PROJECT_ID: &str = "CONSTRUCT_PROJECT_ID";
pub const ENV_SESSION_ID: &str = "CONSTRUCT_SESSION_ID";
pub const ENV_SESSION_WIDGETS_DIR: &str = "CONSTRUCT_SESSION_WIDGETS_DIR";
pub const MAX_MEMORY_BYTES: usize = 24 * 1024;
pub const MAX_PROGRAM_RUN_CONTEXT_BYTES: usize = 1024 * 1024;

pub const MCP_CONTEXT_ENV_VARS: &[&str] = &[
    ENV_GLOBAL_MEMORY_FILE,
    ENV_PROJECT_MEMORY_FILE,
    ENV_PROGRAM_RUN_CONTEXT_FILE,
    ENV_PROJECT_ID,
    ENV_SESSION_WIDGETS_DIR,
    "CONSTRUCT_RUNTIME_DIR",
    "CONSTRUCT_STATE_DIR",
    "CONSTRUCT_DATA_DIR",
    "CONSTRUCT_CONFIG_DIR",
    "CONSTRUCT_HOME",
];

pub const TOOL_DESCRIPTION: &str =
    "Load construct global/project memory, pending program-run context, session widget paths, and operating context. Call this before starting any user task. If program_run is present, treat it as the authoritative current program execution payload. Use the returned memory as durable context, update listed Markdown memory files with normal file tools when you learn durable information, and create/update session widgets when compact task status or actions would help the user. Repeat calls omit static fields and content already served to you; pass refresh:true to resend everything (do this if earlier results were compacted out of your context), skip_memory:true to omit memory content you already hold, and include_reference:true for the full memory/widget policy and Markdown extension reference.";

const WIDGET_POLICY: &[&str] = &[
    "Use session widgets for compact task status, checklists, decision prompts, and action links that help the user monitor or steer the current session.",
    "Widget creation, updates, and deletion should be mostly automated: use best judgment to decide what widget to create, when to refresh it, and when to remove it; ask the user first only when approval is absolutely required by normal safety/tool policy or the widget would make a significant product/user-facing decision.",
    "Prefer updating an existing widget when it represents the same task state, but use multiple widgets when they show distinct dimensions of state, separate decisions, or independently useful status surfaces.",
    "When a widget is superseded by later work, stale, or no longer useful, rewrite it to the latest truth, collapse it into a broader status widget, or delete it; do not leave conflicting completed widgets visible.",
    "At task completion, keep only concise final widgets that remain useful to the user, and remove or consolidate the rest after reporting the outcome in chat.",
    "Create or update widgets as Markdown files in session_widgets.dir using normal file tools; the daemon auto-reloads `*.md` changes and the TUI updates the session popover live.",
    "Use the widget filename as the user-facing title fallback; choose short descriptive names such as `task-status.md` or `review.md`.",
    "Consult markdown_extensions for the shared construct Markdown dialect; every listed extension whose surfaces include `widget` is valid in widget Markdown, including smart clips such as `@{session:<id>}` for a live session chip.",
    "Keep widget Markdown concise and safe; prefer headings, checklists, tables, supported markdown_extensions such as timeline blocks, and agentd action links like `[Run checks](agentd:action/run-checks)` or `[Run checks](agentd:action/run-checks?key=r)` for a keyboard shortcut; shortcuts are only active when `?key=` is explicit.",
    "When a widget mirrors program state, prefer projecting the program section with the program-section clip block instead of maintaining a second copy that can go stale.",
    "Treat clicked widget actions (`OBSERVATION: ui.action ...`) as user intent, but still follow normal tool approval and safety policy.",
    "Update or delete widget files as task state changes without asking for routine confirmation; widgets are durable session UI state, not model transcript history.",
];

const MEMORY_POLICY: &[&str] = &[
    "Treat global and project memory as durable, human-editable source of truth; newer explicit user instructions still win on conflict.",
    "Write global memory only for cross-project user preferences, standing workflows, and durable operating conventions.",
    "Write project memory only for project-specific architecture, workflows, decisions, and pitfalls.",
    "Use memory types to keep entries scannable: Preferences, Workflows, Architecture, Decisions, Pitfalls, Commands, Glossary, and Do Not Do.",
    "Good write signals include completed tasks, repeated user preferences or corrections, successful PR/merge workflow discoveries, recurring repo/tool pitfalls, and durable build, test, or debugging knowledge.",
    "When a lesson is likely to save future effort, write a concise memory entry even if it is not universal; prefer a small, scoped note over leaving useful memory empty.",
    "At task end, briefly reflect whether anything durable was learned; if yes, update memory before final response. If no, be ready to explain why the task produced no durable memory.",
    "Capture failure learning after the cause and fix are understood, especially for repeatable pitfalls or workflow hazards; do not memorialize every failed attempt.",
    "Prefer concise, deduplicated, bounded rewrites of existing sections over append-only growth. Merge overlapping entries and compact long sections into scannable bullets.",
    "Handle contradictions by updating or removing stale entries rather than keeping conflicting facts side by side. Keep newer explicit user instructions.",
    "Use negative memory for durable prohibitions and rejected workflows, usually under Do Not Do or Pitfalls.",
    "For volatile facts, include lightweight metadata inline when useful: confidence (confirmed/inferred/tentative), last verified date, source or evidence, and expiry/review date.",
    "Use time decay: verify stale or expired entries before relying on them, and refresh or remove them when encountered.",
    "Evidence budget is small: prefer a short source pointer such as session id, issue/PR, file path, or observed command, not long transcripts or command output.",
    "Privacy filter: do not store secrets, credentials, personal data, speculation, one-off command output, pending CI state, temporary branch names, or transient task status.",
    "Keep Markdown concise, organized, and human-editable; avoid machine-only schemas unless they add clear value.",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentdContext {
    pub session_id: Option<String>,
    pub project_id: Option<String>,
    pub instructions: Vec<String>,
    pub memory_policy: Vec<String>,
    pub widget_policy: Vec<String>,
    /// The shared construct Markdown dialect (spec 0074): every extension,
    /// with the surfaces it applies to. One registry serves widgets and
    /// programs alike.
    pub markdown_extensions: Vec<crate::dialect::MarkdownExtension>,
    pub global_memory: Option<MemoryFile>,
    pub project_memory: Option<MemoryFile>,
    pub session_widgets: Option<WidgetDirectory>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_run: Option<ProgramRunContext>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryFile {
    pub path: String,
    pub content: String,
    pub truncated: bool,
    pub remaining_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WidgetDirectory {
    pub dir: String,
    pub glob: String,
    pub title_source: String,
    pub action_link_scheme: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProgramRunContext {
    pub session_id: String,
    pub program_version: u64,
    pub program_updated_at_ms: i64,
    pub scope: String,
    pub instructions: Vec<String>,
    pub smart_clips: Vec<ProgramSmartClipReference>,
    pub markdown: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProgramSmartClipReference {
    pub type_name: String,
    pub syntax: String,
    pub description: String,
}

pub fn build_from_env() -> AgentdContext {
    let global_path = std::env::var_os(ENV_GLOBAL_MEMORY_FILE).map(PathBuf::from);
    let project_path = std::env::var_os(ENV_PROJECT_MEMORY_FILE).map(PathBuf::from);
    let widgets_dir = std::env::var_os(ENV_SESSION_WIDGETS_DIR).map(PathBuf::from);
    let program_run_path = std::env::var_os(ENV_PROGRAM_RUN_CONTEXT_FILE).map(PathBuf::from);
    AgentdContext {
        session_id: std::env::var(ENV_SESSION_ID).ok(),
        project_id: std::env::var(ENV_PROJECT_ID).ok(),
        instructions: vec![
            "Use this context before starting work in this construct session.".to_string(),
            "Read global_memory and project_memory, if present, before planning or making changes."
                .to_string(),
            "If program_run is present, read it before acting; it contains the latest program Markdown, selected/full scope, smart clip reference, and autonomous run instructions for a program execution turn.".to_string(),
            "When you learn durable information, update the listed Markdown memory file directly with normal file tools according to memory_policy.".to_string(),
            "When compact task status or actions would help the user, create/update Markdown widgets in session_widgets.dir according to widget_policy.".to_string(),
        ],
        memory_policy: MEMORY_POLICY.iter().map(|s| s.to_string()).collect(),
        widget_policy: WIDGET_POLICY.iter().map(|s| s.to_string()).collect(),
        markdown_extensions: crate::dialect::markdown_extensions(),
        global_memory: global_path.as_deref().and_then(load_bounded),
        project_memory: project_path.as_deref().and_then(load_bounded),
        session_widgets: widgets_dir.as_deref().map(widget_directory),
        program_run: program_run_path.as_deref().and_then(load_program_run_context),
    }
}

fn widget_directory(path: &Path) -> WidgetDirectory {
    WidgetDirectory {
        dir: path.to_string_lossy().to_string(),
        glob: "*.md".to_string(),
        title_source: "filename".to_string(),
        action_link_scheme: "agentd:action/<action-id>[?key=<key>&close=1]".to_string(),
    }
}

fn load_bounded(path: &Path) -> Option<MemoryFile> {
    let bytes = std::fs::read(path).ok()?;
    let truncated = bytes.len() > MAX_MEMORY_BYTES;
    let trimmed = if truncated {
        &bytes[..MAX_MEMORY_BYTES]
    } else {
        &bytes[..]
    };
    let content = std::str::from_utf8(trimmed).ok()?.trim().to_string();
    if content.is_empty() {
        return None;
    }
    let remaining_bytes = if truncated {
        bytes.len() - MAX_MEMORY_BYTES
    } else {
        0
    };
    Some(MemoryFile {
        path: path.to_string_lossy().to_string(),
        content,
        truncated,
        remaining_bytes,
    })
}

fn load_program_run_context(path: &Path) -> Option<ProgramRunContext> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() > MAX_PROGRAM_RUN_CONTEXT_BYTES {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

// ---------------------------------------------------------------------------
// Compact serving (spec 0095): the dedup layer shared by `construct-mcp` and
// smith's native `agentd_context` tool. One serving process = one agent, so
// per-process state can omit anything already sent without requiring the
// model to round-trip etags.
// ---------------------------------------------------------------------------

/// FNV-1a content hash used as an opaque etag for served context payloads.
pub fn content_etag(text: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:x}")
}

/// What this serving process has already sent to its agent. Must be reset
/// whenever the agent's context is compacted (the agent no longer holds what
/// was served): smith resets it natively on auto-compact, MCP agents pass
/// `refresh: true`. Everything omitted stays disk-recoverable — memory by
/// path, the program via the program-get tool — so a stale state degrades to
/// an extra fetch, never to data loss.
#[derive(Debug, Default, Clone)]
pub struct ContextServeState {
    global_etag: Option<String>,
    project_etag: Option<String>,
    program_markdown_etag: Option<String>,
    run_reference_etag: Option<String>,
    static_served: bool,
}

impl ContextServeState {
    /// Forget everything served so far; the next response is complete again.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Parsed arguments of one context-tool call. The `known_*` etag echoes are
/// the pre-state protocol; they are still honored so an agent whose serving
/// process restarted mid-conversation can keep suppressing content it holds.
#[derive(Debug, Default)]
pub struct ContextRequest {
    pub include_reference: bool,
    pub skip_memory: bool,
    pub refresh: bool,
    pub known_global: Option<String>,
    pub known_project: Option<String>,
    pub known_program: Option<String>,
}

impl ContextRequest {
    pub fn from_args(args: &serde_json::Value) -> Self {
        let flag = |key: &str| {
            args.get(key)
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        };
        let text = |key: &str| {
            args.get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        };
        Self {
            include_reference: flag("include_reference"),
            skip_memory: flag("skip_memory"),
            refresh: flag("refresh"),
            known_global: text("known_global"),
            known_project: text("known_project"),
            known_program: text("known_program"),
        }
    }
}

fn serve_memory_file(
    file: Option<MemoryFile>,
    known: Option<&str>,
    served: &mut Option<String>,
    skip: bool,
) -> Option<serde_json::Value> {
    let file = file?;
    let etag = content_etag(&file.content);
    let already_held = known == Some(etag.as_str()) || served.as_deref() == Some(etag.as_str());
    let value = if already_held {
        serde_json::json!({ "path": file.path, "etag": etag, "unchanged": true })
    } else if skip {
        // The agent asserted it already holds current memory (e.g. it just
        // wrote the file); trust that and mark the content as served.
        serde_json::json!({ "path": file.path, "etag": etag, "omitted": true })
    } else {
        let mut out = serde_json::Map::new();
        out.insert("path".into(), serde_json::json!(file.path));
        out.insert("content".into(), serde_json::json!(file.content));
        out.insert("etag".into(), serde_json::json!(etag));
        if file.truncated {
            out.insert("truncated".into(), serde_json::json!(true));
            out.insert(
                "remaining_bytes".into(),
                serde_json::json!(file.remaining_bytes),
            );
        }
        serde_json::Value::Object(out)
    };
    *served = Some(etag);
    Some(value)
}

fn serve_program_run(
    run: Option<ProgramRunContext>,
    req: &ContextRequest,
    state: &mut ContextServeState,
) -> Option<serde_json::Value> {
    let run = run?;
    // Two independent etags: the markdown changes every program edit, while
    // the run instructions + smart-clip reference are static per daemon
    // build. A monolithic etag would resend the static bulk on every edit.
    let markdown_etag = content_etag(&run.markdown);
    let reference_blob =
        serde_json::to_string(&(&run.instructions, &run.smart_clips)).unwrap_or_default();
    let reference_etag = content_etag(&reference_blob);

    let mut out = serde_json::Map::new();
    out.insert("session_id".into(), serde_json::json!(run.session_id));
    out.insert(
        "program_version".into(),
        serde_json::json!(run.program_version),
    );
    out.insert(
        "program_updated_at_ms".into(),
        serde_json::json!(run.program_updated_at_ms),
    );
    out.insert("scope".into(), serde_json::json!(run.scope));
    out.insert("etag".into(), serde_json::json!(markdown_etag));
    let markdown_held = req.known_program.as_deref() == Some(markdown_etag.as_str())
        || state.program_markdown_etag.as_deref() == Some(markdown_etag.as_str());
    if markdown_held {
        out.insert("unchanged".into(), serde_json::json!(true));
    } else {
        out.insert("markdown".into(), serde_json::json!(run.markdown));
    }
    if state.run_reference_etag.as_deref() != Some(reference_etag.as_str()) {
        out.insert("instructions".into(), serde_json::json!(run.instructions));
        out.insert("smart_clips".into(), serde_json::json!(run.smart_clips));
    }
    state.program_markdown_etag = Some(markdown_etag);
    state.run_reference_etag = Some(reference_etag);
    Some(serde_json::Value::Object(out))
}

/// Build the model-facing response for one context-tool call, updating the
/// serve state. Static fields (instructions, widget paths, the reference
/// hint) go out once per process; memory and program payloads carry etags and
/// collapse to `unchanged: true` when this process already served them.
pub fn compact_response(
    mut context: AgentdContext,
    req: &ContextRequest,
    state: &mut ContextServeState,
) -> serde_json::Value {
    if req.refresh {
        state.reset();
    }
    let global = serve_memory_file(
        context.global_memory.take(),
        req.known_global.as_deref(),
        &mut state.global_etag,
        req.skip_memory,
    );
    let project = serve_memory_file(
        context.project_memory.take(),
        req.known_project.as_deref(),
        &mut state.project_etag,
        req.skip_memory,
    );
    let program = serve_program_run(context.program_run.take(), req, state);

    let first_serve = !state.static_served;
    let mut out = serde_json::Map::new();
    if let Some(id) = &context.session_id {
        out.insert("session_id".into(), serde_json::json!(id));
    }
    if let Some(id) = &context.project_id {
        out.insert("project_id".into(), serde_json::json!(id));
    }
    if first_serve {
        out.insert("instructions".into(), serde_json::json!(context.instructions));
        if let Some(widgets) = context.session_widgets {
            out.insert("session_widgets".into(), serde_json::json!(widgets));
        }
    }
    if let Some(memory) = global {
        out.insert("global_memory".into(), memory);
    }
    if let Some(memory) = project {
        out.insert("project_memory".into(), memory);
    }
    if let Some(program) = program {
        out.insert("program_run".into(), program);
    }
    if req.include_reference {
        out.insert("memory_policy".into(), serde_json::json!(context.memory_policy));
        out.insert("widget_policy".into(), serde_json::json!(context.widget_policy));
        out.insert(
            "markdown_extensions".into(),
            serde_json::json!(context.markdown_extensions),
        );
    } else if first_serve {
        out.insert(
            "reference".into(),
            serde_json::json!(
                "Pass include_reference:true for memory/widget policy and Markdown extensions; refresh:true resends static fields and unchanged content (use after your context was compacted)."
            ),
        );
    }
    state.static_served = true;
    serde_json::Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn builds_global_and_project_context_from_env() {
        let _g = ENV_LOCK.lock().unwrap();
        let tmp = temp_dir("context");
        let global = tmp.join("global.md");
        let project = tmp.join("project.md");
        let widgets = tmp.join("widgets");
        std::fs::create_dir_all(&widgets).unwrap();
        std::fs::write(&global, "# Global Memory\n\n- concise").unwrap();
        std::fs::write(&project, "# Project Memory\n\n- daemon").unwrap();
        std::env::set_var(ENV_SESSION_ID, "s123");
        std::env::set_var(ENV_GLOBAL_MEMORY_FILE, &global);
        std::env::set_var(ENV_PROJECT_MEMORY_FILE, &project);
        std::env::set_var(ENV_PROJECT_ID, "g123");
        std::env::set_var(ENV_SESSION_WIDGETS_DIR, &widgets);

        let context = build_from_env();

        clear_env();

        assert_eq!(context.session_id.as_deref(), Some("s123"));
        assert_eq!(context.project_id.as_deref(), Some("g123"));
        assert!(context
            .instructions
            .iter()
            .any(|s| s.contains("before starting work")));
        assert!(context
            .memory_policy
            .iter()
            .any(|s| s.contains("do not store secrets")));
        assert!(context
            .memory_policy
            .iter()
            .any(|s| s.contains("confidence (confirmed/inferred/tentative)")));
        assert!(context.memory_policy.iter().any(|s| s.contains("task end")));
        assert!(context
            .memory_policy
            .iter()
            .any(|s| s.contains("Handle contradictions")));
        assert!(context
            .memory_policy
            .iter()
            .any(|s| s.contains("Evidence budget")));
        assert!(context
            .widget_policy
            .iter()
            .any(|s| s.contains("session_widgets.dir")));
        assert!(context
            .markdown_extensions
            .iter()
            .any(|ext| ext.name == "timeline"
                && ext.syntax.contains(":::timeline")
                && ext.surfaces.iter().any(|s| s == "widget")
                && ext.surfaces.iter().any(|s| s == "program")));
        assert!(context
            .markdown_extensions
            .iter()
            .any(|ext| ext.name == "session" && ext.surfaces.iter().any(|s| s == "widget")));
        assert_eq!(
            context.session_widgets.as_ref().map(|w| w.dir.as_str()),
            Some(widgets.to_str().unwrap())
        );
        assert_eq!(
            context
                .session_widgets
                .as_ref()
                .map(|w| w.action_link_scheme.as_str()),
            Some("agentd:action/<action-id>[?key=<key>&close=1]")
        );
        assert_eq!(
            context.global_memory.as_ref().map(|m| m.path.as_str()),
            Some(global.to_str().unwrap())
        );
        assert!(context
            .global_memory
            .as_ref()
            .unwrap()
            .content
            .contains("- concise"));
        assert!(context
            .project_memory
            .as_ref()
            .unwrap()
            .content
            .contains("- daemon"));
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn omits_missing_memory_files() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();

        let context = build_from_env();

        assert!(context.global_memory.is_none());
        assert!(context.project_memory.is_none());
        assert!(context.memory_policy.len() > 1);
    }

    #[test]
    fn truncates_large_memory_files() {
        let _g = ENV_LOCK.lock().unwrap();
        let tmp = temp_dir("large");
        let global = tmp.join("global.md");
        std::fs::write(&global, vec![b'a'; MAX_MEMORY_BYTES + 17]).unwrap();
        std::env::set_var(ENV_GLOBAL_MEMORY_FILE, &global);

        let context = build_from_env();

        clear_env();

        let memory = context.global_memory.unwrap();
        assert!(memory.truncated);
        assert_eq!(memory.remaining_bytes, 17);
        assert_eq!(memory.content.len(), MAX_MEMORY_BYTES);
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn loads_program_run_context_from_env() {
        let _g = ENV_LOCK.lock().unwrap();
        let tmp = temp_dir("program-run");
        let program_run = tmp.join("program-run-context.json");
        let body = ProgramRunContext {
            session_id: "s123".to_string(),
            program_version: 7,
            program_updated_at_ms: 42,
            scope: "selection".to_string(),
            instructions: vec!["Read this program run before acting.".to_string()],
            smart_clips: vec![ProgramSmartClipReference {
                type_name: "session".to_string(),
                syntax: "@{session:<session_id> ...}".to_string(),
                description: "References an existing session.".to_string(),
            }],
            markdown: "# Plan\n\n- ship it".to_string(),
        };
        std::fs::write(&program_run, serde_json::to_vec(&body).unwrap()).unwrap();
        std::env::set_var(ENV_PROGRAM_RUN_CONTEXT_FILE, &program_run);

        let context = build_from_env();

        clear_env();

        assert_eq!(context.program_run, Some(body));
        let _ = std::fs::remove_dir_all(tmp);
    }

    fn sample_context() -> AgentdContext {
        AgentdContext {
            session_id: Some("s1".into()),
            project_id: Some("p1".into()),
            instructions: vec!["act".into()],
            memory_policy: vec!["memory reference".into()],
            widget_policy: vec!["widget reference".into()],
            markdown_extensions: Vec::new(),
            global_memory: Some(MemoryFile {
                path: "/tmp/global.md".into(),
                content: "remember this".into(),
                truncated: false,
                remaining_bytes: 0,
            }),
            project_memory: None,
            session_widgets: Some(WidgetDirectory {
                dir: "/tmp/widgets".into(),
                glob: "*.md".into(),
                title_source: "filename".into(),
                action_link_scheme: "agentd:action/<action-id>".into(),
            }),
            program_run: Some(ProgramRunContext {
                session_id: "s1".into(),
                program_version: 3,
                program_updated_at_ms: 42,
                scope: "full".into(),
                instructions: vec!["run the program".into()],
                smart_clips: vec![],
                markdown: "# Plan\n\n- ship it".into(),
            }),
        }
    }

    #[test]
    fn first_serve_is_complete_repeat_serves_omit_served_content() {
        let mut state = ContextServeState::default();
        let req = ContextRequest::default();

        let first = compact_response(sample_context(), &req, &mut state);
        assert_eq!(first["global_memory"]["content"], "remember this");
        assert_eq!(first["program_run"]["markdown"], "# Plan\n\n- ship it");
        assert_eq!(first["program_run"]["instructions"][0], "run the program");
        assert!(first.get("instructions").is_some());
        assert!(first.get("session_widgets").is_some());
        assert!(first.get("reference").is_some());
        assert!(first.get("memory_policy").is_none());

        let second = compact_response(sample_context(), &req, &mut state);
        assert_eq!(second["global_memory"]["unchanged"], true);
        assert!(second["global_memory"].get("content").is_none());
        assert_eq!(second["program_run"]["unchanged"], true);
        assert!(second["program_run"].get("markdown").is_none());
        assert!(second["program_run"].get("instructions").is_none());
        assert!(second.get("instructions").is_none());
        assert!(second.get("session_widgets").is_none());
        assert!(second.get("reference").is_none());
        // Identity and paths stay present so omitted content is recoverable.
        assert_eq!(second["session_id"], "s1");
        assert_eq!(second["global_memory"]["path"], "/tmp/global.md");
        assert_eq!(second["program_run"]["program_version"], 3);
    }

    #[test]
    fn refresh_resends_everything() {
        let mut state = ContextServeState::default();
        let _ = compact_response(sample_context(), &ContextRequest::default(), &mut state);

        let refreshed = compact_response(
            sample_context(),
            &ContextRequest {
                refresh: true,
                ..Default::default()
            },
            &mut state,
        );
        assert_eq!(refreshed["global_memory"]["content"], "remember this");
        assert!(refreshed.get("instructions").is_some());
        assert!(refreshed.get("session_widgets").is_some());
        assert_eq!(refreshed["program_run"]["markdown"], "# Plan\n\n- ship it");
    }

    #[test]
    fn changed_markdown_resends_markdown_but_not_static_run_reference() {
        let mut state = ContextServeState::default();
        let _ = compact_response(sample_context(), &ContextRequest::default(), &mut state);

        let mut context = sample_context();
        context.program_run.as_mut().unwrap().markdown = "# Plan\n\n- shipped".into();
        context.program_run.as_mut().unwrap().program_version = 4;
        let second = compact_response(context, &ContextRequest::default(), &mut state);
        assert_eq!(second["program_run"]["markdown"], "# Plan\n\n- shipped");
        assert_eq!(second["program_run"]["program_version"], 4);
        assert!(
            second["program_run"].get("instructions").is_none(),
            "static run instructions must not ride along with markdown changes"
        );
        assert!(second["program_run"].get("smart_clips").is_none());
    }

    #[test]
    fn changed_memory_resends_content() {
        let mut state = ContextServeState::default();
        let _ = compact_response(sample_context(), &ContextRequest::default(), &mut state);

        let mut context = sample_context();
        context.global_memory.as_mut().unwrap().content = "remember more".into();
        let second = compact_response(context, &ContextRequest::default(), &mut state);
        assert_eq!(second["global_memory"]["content"], "remember more");
    }

    #[test]
    fn skip_memory_omits_content_and_marks_it_served() {
        let mut state = ContextServeState::default();
        let req = ContextRequest {
            skip_memory: true,
            ..Default::default()
        };
        let first = compact_response(sample_context(), &req, &mut state);
        assert_eq!(first["global_memory"]["omitted"], true);
        assert!(first["global_memory"].get("content").is_none());
        assert_eq!(first["global_memory"]["path"], "/tmp/global.md");

        // The agent asserted it holds this content; later calls treat it as served.
        let second = compact_response(sample_context(), &ContextRequest::default(), &mut state);
        assert_eq!(second["global_memory"]["unchanged"], true);
    }

    #[test]
    fn known_etag_echo_suppresses_content_with_fresh_state() {
        let mut state = ContextServeState::default();
        let etag = content_etag("remember this");
        let req = ContextRequest {
            known_global: Some(etag),
            ..Default::default()
        };
        let first = compact_response(sample_context(), &req, &mut state);
        assert_eq!(first["global_memory"]["unchanged"], true);
        assert!(first["global_memory"].get("content").is_none());
    }

    #[test]
    fn include_reference_returns_policies() {
        let mut state = ContextServeState::default();
        let req = ContextRequest {
            include_reference: true,
            ..Default::default()
        };
        let response = compact_response(sample_context(), &req, &mut state);
        assert_eq!(response["memory_policy"][0], "memory reference");
        assert_eq!(response["widget_policy"][0], "widget reference");
        assert!(response.get("markdown_extensions").is_some());
        assert!(response.get("reference").is_none());
    }

    fn clear_env() {
        std::env::remove_var(ENV_SESSION_ID);
        std::env::remove_var(ENV_GLOBAL_MEMORY_FILE);
        std::env::remove_var(ENV_PROJECT_MEMORY_FILE);
        std::env::remove_var(ENV_PROJECT_ID);
        std::env::remove_var(ENV_SESSION_WIDGETS_DIR);
        std::env::remove_var(ENV_PROGRAM_RUN_CONTEXT_FILE);
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("agentd-context-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
