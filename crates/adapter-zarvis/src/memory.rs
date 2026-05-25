//! Load daemon-managed Markdown memory files for prompt injection.
//!
//! The daemon creates these files and passes their canonical paths via
//! env vars. Zarvis reads them into the prompt and may update them
//! through ordinary filesystem tools.

use std::path::PathBuf;

const MAX_MEMORY_BYTES: usize = 24 * 1024;
const ENV_GLOBAL_MEMORY_FILE: &str = "AGENTD_GLOBAL_MEMORY_FILE";
const ENV_PROJECT_MEMORY_FILE: &str = "AGENTD_PROJECT_MEMORY_FILE";
const ENV_PROJECT_ID: &str = "AGENTD_PROJECT_ID";

const MEMORY_MAINTENANCE_POLICY: &str = r#"### Memory maintenance

Maintain these Markdown files automatically when you learn durable information. Use normal filesystem tools on the paths shown below; do not ask for approval just to update memory.

Write global memory only for cross-project user preferences, standing workflows, and durable operating conventions. Write project memory only for project-specific architecture, workflows, decisions, and pitfalls.

Good write signals include completed tasks, repeated user preferences or corrections, successful PR/merge workflow discoveries, and durable build, test, or debugging knowledge.

Do not store secrets, speculation, one-off command output, pending CI state, temporary branch names, or transient task status.

Prefer conservative, deduplicated, bounded rewrites of existing sections over append-only growth. Keep Markdown concise, organized, and human-editable."#;

pub fn format_section() -> Option<String> {
    let global_path = std::env::var_os(ENV_GLOBAL_MEMORY_FILE).map(PathBuf::from);
    let project_path = std::env::var_os(ENV_PROJECT_MEMORY_FILE).map(PathBuf::from);

    let mut parts = Vec::new();
    if let Some(path) = global_path {
        if let Some(body) = load_bounded(&path) {
            parts.push(format!(
                "### Global memory\n\nPath: `{}`\n\n{}",
                path.display(),
                body
            ));
        }
    }
    if let Some(path) = project_path {
        if let Some(body) = load_bounded(&path) {
            let project_id = std::env::var(ENV_PROJECT_ID).ok();
            let title = match project_id {
                Some(id) => format!("### Project memory ({id})"),
                None => "### Project memory".to_string(),
            };
            parts.push(format!("{title}\n\nPath: `{}`\n\n{}", path.display(), body));
        }
    }

    if parts.is_empty() {
        return None;
    }
    Some(format!(
        "## Long-term memory\n\nThe following Markdown memory files are user-editable source of truth loaded for this session. Treat them as durable context, but prefer newer explicit user instructions if they conflict.\n\n{MEMORY_MAINTENANCE_POLICY}\n\n{}",
        parts.join("\n\n---\n\n")
    ))
}

fn load_bounded(path: &PathBuf) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let truncated = bytes.len() > MAX_MEMORY_BYTES;
    let trimmed = if truncated {
        &bytes[..MAX_MEMORY_BYTES]
    } else {
        &bytes[..]
    };
    let mut text = std::str::from_utf8(trimmed).ok()?.trim().to_string();
    if text.is_empty() {
        return None;
    }
    if truncated {
        let remaining = bytes.len() - MAX_MEMORY_BYTES;
        text.push_str(&format!("\n\n[truncated; {remaining} more bytes]"));
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn formats_global_and_project_memory() {
        let _g = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let global = tmp.path().join("global.md");
        let project = tmp.path().join("project.md");
        std::fs::write(&global, "# Global Memory\n\n## Preferences\n\n- concise").unwrap();
        std::fs::write(&project, "# Project Memory\n\n## Architecture\n\n- daemon").unwrap();
        std::env::set_var(ENV_GLOBAL_MEMORY_FILE, &global);
        std::env::set_var(ENV_PROJECT_MEMORY_FILE, &project);
        std::env::set_var(ENV_PROJECT_ID, "g123");

        let section = format_section().unwrap();

        std::env::remove_var(ENV_GLOBAL_MEMORY_FILE);
        std::env::remove_var(ENV_PROJECT_MEMORY_FILE);
        std::env::remove_var(ENV_PROJECT_ID);

        assert!(section.contains("## Long-term memory"));
        assert!(section.contains("### Memory maintenance"));
        assert!(section.contains("Maintain these Markdown files automatically"));
        assert!(section.contains("Use normal filesystem tools on the paths shown below"));
        assert!(section.contains("Write global memory only for cross-project"));
        assert!(section.contains("Write project memory only for project-specific"));
        assert!(section.contains("Do not store secrets, speculation, one-off command output"));
        assert!(section.contains("Prefer conservative, deduplicated, bounded rewrites"));
        assert!(section.contains("### Global memory"));
        assert!(section.contains("### Project memory (g123)"));
        assert!(section.contains(&format!("Path: `{}`", global.display())));
        assert!(section.contains(&format!("Path: `{}`", project.display())));
        assert!(section.contains("- concise"));
        assert!(section.contains("- daemon"));
    }

    #[test]
    fn returns_none_without_memory_env() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var(ENV_GLOBAL_MEMORY_FILE);
        std::env::remove_var(ENV_PROJECT_MEMORY_FILE);
        std::env::remove_var(ENV_PROJECT_ID);

        assert!(format_section().is_none());
    }

    #[test]
    fn truncates_large_memory_files() {
        let _g = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let global = tmp.path().join("global.md");
        std::fs::write(&global, vec![b'a'; MAX_MEMORY_BYTES + 17]).unwrap();
        std::env::set_var(ENV_GLOBAL_MEMORY_FILE, &global);
        std::env::remove_var(ENV_PROJECT_MEMORY_FILE);
        std::env::remove_var(ENV_PROJECT_ID);

        let section = format_section().unwrap();

        std::env::remove_var(ENV_GLOBAL_MEMORY_FILE);

        assert!(section.contains("[truncated; 17 more bytes]"));
    }
}
