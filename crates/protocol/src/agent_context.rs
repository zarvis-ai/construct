//! Shared agentd context surfaced to agents through `agentd_context`.
//!
//! The daemon passes memory file paths in env vars. This module reads those
//! files and formats one stable JSON shape used by both `agentd-mcp` and
//! zarvis's native tool layer.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const TOOL_NAME: &str = "agentd_context";
pub const ENV_GLOBAL_MEMORY_FILE: &str = "AGENTD_GLOBAL_MEMORY_FILE";
pub const ENV_PROJECT_MEMORY_FILE: &str = "AGENTD_PROJECT_MEMORY_FILE";
pub const ENV_PROJECT_ID: &str = "AGENTD_PROJECT_ID";
pub const ENV_SESSION_ID: &str = "AGENTD_SESSION_ID";
pub const ENV_SESSION_WIDGETS_DIR: &str = "AGENTD_SESSION_WIDGETS_DIR";
pub const MAX_MEMORY_BYTES: usize = 24 * 1024;

pub const MCP_CONTEXT_ENV_VARS: &[&str] = &[
    ENV_GLOBAL_MEMORY_FILE,
    ENV_PROJECT_MEMORY_FILE,
    ENV_PROJECT_ID,
    ENV_SESSION_WIDGETS_DIR,
    "AGENTD_RUNTIME_DIR",
    "AGENTD_STATE_DIR",
    "AGENTD_DATA_DIR",
    "AGENTD_CONFIG_DIR",
];

pub const TOOL_DESCRIPTION: &str =
    "Load agentd global/project memory, session widget paths, and operating context. Call this before starting any user task, before planning, and before using other tools. Use the returned memory as durable context, follow its maintenance policy, update listed Markdown memory files with normal file tools when you learn durable information, and create/update session widgets when compact task status or actions would help the user.";

const WIDGET_POLICY: &[&str] = &[
    "Use session widgets for compact task status, checklists, decision prompts, and action links that help the user monitor or steer the current session.",
    "Widget creation, updates, and deletion should be mostly automated: use best judgment to decide what widget to create, when to refresh it, and when to remove it; ask the user first only when approval is absolutely required by normal safety/tool policy or the widget would make a significant product/user-facing decision.",
    "Prefer updating an existing widget when it represents the same task state, but use multiple widgets when they show distinct dimensions of state, separate decisions, or independently useful status surfaces.",
    "When a widget is superseded by later work, stale, or no longer useful, rewrite it to the latest truth, collapse it into a broader status widget, or delete it; do not leave conflicting completed widgets visible.",
    "At task completion, keep only concise final widgets that remain useful to the user, and remove or consolidate the rest after reporting the outcome in chat.",
    "Create or update widgets as Markdown files in session_widgets.dir using normal file tools; the daemon auto-reloads `*.md` changes and the TUI updates the session popover live.",
    "Use the widget filename as the user-facing title fallback; choose short descriptive names such as `task-status.md` or `review.md`.",
    "Consult widget_markdown_extensions for supported custom widget syntax; use extensions such as timeline blocks when they communicate task state better than plain Markdown.",
    "Keep widget Markdown concise and safe; prefer headings, checklists, tables, supported widget_markdown_extensions such as timeline blocks, and agentd action links like `[Run checks](agentd:action/run-checks)` or `[Run checks](agentd:action/run-checks?key=r)` for a keyboard shortcut; shortcuts are only active when `?key=` is explicit.",
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
    pub widget_markdown_extensions: Vec<WidgetMarkdownExtension>,
    pub global_memory: Option<MemoryFile>,
    pub project_memory: Option<MemoryFile>,
    pub session_widgets: Option<WidgetDirectory>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WidgetMarkdownExtension {
    pub name: String,
    pub description: String,
    pub syntax: String,
    pub use_when: String,
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

pub fn build_from_env() -> AgentdContext {
    let global_path = std::env::var_os(ENV_GLOBAL_MEMORY_FILE).map(PathBuf::from);
    let project_path = std::env::var_os(ENV_PROJECT_MEMORY_FILE).map(PathBuf::from);
    let widgets_dir = std::env::var_os(ENV_SESSION_WIDGETS_DIR).map(PathBuf::from);
    AgentdContext {
        session_id: std::env::var(ENV_SESSION_ID).ok(),
        project_id: std::env::var(ENV_PROJECT_ID).ok(),
        instructions: vec![
            "Use this context before starting work in this agentd session.".to_string(),
            "Read global_memory and project_memory, if present, before planning or making changes."
                .to_string(),
            "When you learn durable information, update the listed Markdown memory file directly with normal file tools according to memory_policy.".to_string(),
            "When compact task status or actions would help the user, create/update Markdown widgets in session_widgets.dir according to widget_policy.".to_string(),
        ],
        memory_policy: MEMORY_POLICY.iter().map(|s| s.to_string()).collect(),
        widget_policy: WIDGET_POLICY.iter().map(|s| s.to_string()).collect(),
        widget_markdown_extensions: widget_markdown_extensions(),
        global_memory: global_path.as_deref().and_then(load_bounded),
        project_memory: project_path.as_deref().and_then(load_bounded),
        session_widgets: widgets_dir.as_deref().map(widget_directory),
    }
}

fn widget_markdown_extensions() -> Vec<WidgetMarkdownExtension> {
    vec![WidgetMarkdownExtension {
        name: "timeline".to_string(),
        description: "Render top-level bullet/checklist items as a vertical timeline with connector rows between bullet icons. Indented nested lines render below their parent item at arbitrary list depth, and each top-level item keeps bottom padding. Supports [x] done, [~] active/current, [ ] todo, [!] blocked/warning, plain bullet items, and inline agentd action links with optional ?key= shortcuts."
            .to_string(),
        syntax: ":::timeline\n- [x] [Run checks](agentd:action/run-checks?key=r) and [Start demo](agentd:action/start-demo?key=d)\n  - [x] Nested done\n    - [ ] Deeper todo\n- [~] Active/current\n- [ ] Todo\n- [!] Blocked\n- Plain milestone\n:::"
            .to_string(),
        use_when: "Use for multi-step task progress, mission plans, status history, and review/check workflows where connected bullets read better than a plain list."
            .to_string(),
    }]
}

fn widget_directory(path: &Path) -> WidgetDirectory {
    WidgetDirectory {
        dir: path.to_string_lossy().to_string(),
        glob: "*.md".to_string(),
        title_source: "filename".to_string(),
        action_link_scheme: "agentd:action/<action-id>[?key=<key>]".to_string(),
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
            .widget_markdown_extensions
            .iter()
            .any(|ext| ext.name == "timeline" && ext.syntax.contains(":::timeline")));
        assert_eq!(
            context.session_widgets.as_ref().map(|w| w.dir.as_str()),
            Some(widgets.to_str().unwrap())
        );
        assert_eq!(
            context
                .session_widgets
                .as_ref()
                .map(|w| w.action_link_scheme.as_str()),
            Some("agentd:action/<action-id>[?key=<key>]")
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

    fn clear_env() {
        std::env::remove_var(ENV_SESSION_ID);
        std::env::remove_var(ENV_GLOBAL_MEMORY_FILE);
        std::env::remove_var(ENV_PROJECT_MEMORY_FILE);
        std::env::remove_var(ENV_PROJECT_ID);
        std::env::remove_var(ENV_SESSION_WIDGETS_DIR);
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("agentd-context-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
