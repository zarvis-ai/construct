//! Discovery of Codex-style agent skills for Zarvis.
//!
//! Skills are advertised by `SKILL.md` files with a small YAML-ish
//! frontmatter block containing `name` and `description`. We append a
//! compact catalog to the system prompt and let the model read the
//! selected skill file on demand, instead of eagerly stuffing every
//! skill body into context.
//!
//! Disable with `AGENTD_ZARVIS_SKILLS=off`.

use std::path::{Path, PathBuf};

const MAX_SCAN_DEPTH: usize = 8;
const MAX_ASCEND: usize = 6;
const MAX_SKILL_FILES: usize = 512;
const MAX_SKILLS: usize = 128;
const MAX_SKILL_BYTES: usize = 16 * 1024;
const MAX_FIELD_CHARS: usize = 1_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Skill {
    name: String,
    description: String,
    path: PathBuf,
}

pub(crate) fn discover(cwd: &Path) -> Vec<Skill> {
    if std::env::var("AGENTD_ZARVIS_SKILLS").as_deref() == Ok("off") {
        return Vec::new();
    }

    let mut paths = Vec::new();
    if let Some(home) = codex_home() {
        collect_skill_files(&home.join("skills"), MAX_SCAN_DEPTH, &mut paths);
        collect_skill_files(
            &home.join("plugins").join("cache"),
            MAX_SCAN_DEPTH,
            &mut paths,
        );
    }
    if let Some(home) = claude_home() {
        collect_skill_files(&home.join("skills"), MAX_SCAN_DEPTH, &mut paths);
        collect_skill_files(&home.join("plugins"), MAX_SCAN_DEPTH, &mut paths);
    }
    for root in project_claude_skill_roots(cwd) {
        collect_skill_files(&root, MAX_SCAN_DEPTH, &mut paths);
    }
    paths.sort();
    paths.dedup();

    let mut skills = paths
        .into_iter()
        .take(MAX_SKILLS)
        .filter_map(|path| parse_skill_file(&path))
        .collect::<Vec<_>>();
    skills.sort_by(|a, b| {
        a.name.cmp(&b.name).then_with(|| {
            a.path
                .display()
                .to_string()
                .cmp(&b.path.display().to_string())
        })
    });
    skills
}

pub(crate) fn format_section(cwd: &Path) -> Option<String> {
    let skills = discover(cwd);
    if skills.is_empty() {
        return None;
    }

    let mut out = String::from(
        "## Agent skills\n\n\
         The user has Codex/Claude-style agent skills installed. Use a skill when the user explicitly names it, or when the task clearly matches its description. Before using a skill, read its `SKILL.md` with `shell` (e.g. `cat`) and follow that file's workflow. Resolve referenced scripts, assets, and relative paths from the directory containing that `SKILL.md`. Load only the skill you need, and only the referenced files needed for the task. If a skill requires a tool that Zarvis does not have, explain the limitation and continue with the closest available workflow.\n\n\
         Available skills:\n",
    );
    for skill in skills {
        out.push_str(&format!(
            "- `{}`: {} (file: `{}`)\n",
            skill.name,
            skill.description,
            skill.path.display()
        ));
    }
    Some(out)
}

fn codex_home() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CODEX_HOME") {
        return Some(PathBuf::from(path));
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".codex"))
}

fn claude_home() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CLAUDE_HOME") {
        return Some(PathBuf::from(path));
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".claude"))
}

fn project_claude_skill_roots(cwd: &Path) -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut roots = Vec::new();
    let mut dir = cwd;
    for _ in 0..=MAX_ASCEND {
        let candidate = dir.join(".claude").join("skills");
        if candidate.is_dir() {
            roots.push(candidate);
        }
        if let Some(h) = home.as_deref() {
            if dir == h {
                break;
            }
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => break,
        }
    }
    roots
}

fn collect_skill_files(root: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if out.len() >= MAX_SKILL_FILES || depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };

    for entry in entries.flatten() {
        if out.len() >= MAX_SKILL_FILES {
            return;
        }
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_file() && path.file_name().and_then(|s| s.to_str()) == Some("SKILL.md") {
            out.push(path);
        } else if meta.is_dir() {
            collect_skill_files(&path, depth - 1, out);
        }
    }
}

fn parse_skill_file(path: &Path) -> Option<Skill> {
    let bytes = std::fs::read(path).ok()?;
    let trimmed = if bytes.len() > MAX_SKILL_BYTES {
        &bytes[..MAX_SKILL_BYTES]
    } else {
        &bytes
    };
    let text = std::str::from_utf8(trimmed).ok()?;
    let (name, description) = parse_frontmatter(text)?;
    Some(Skill {
        name,
        description,
        path: path.to_path_buf(),
    })
}

fn parse_frontmatter(text: &str) -> Option<(String, String)> {
    let mut lines = text.lines();
    if lines.next()? != "---" {
        return None;
    }

    let mut name = None;
    let mut description = None;
    for line in lines {
        if line.trim() == "---" {
            break;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim() {
            "name" => name = clean_value(value),
            "description" => description = clean_value(value),
            _ => {}
        }
    }

    Some((name?, description?))
}

fn clean_value(value: &str) -> Option<String> {
    let mut value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Some(stripped) = value.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        value = stripped;
    } else if let Some(stripped) = value.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        value = stripped;
    }
    Some(limit_chars(
        value.replace("\\\"", "\"").trim(),
        MAX_FIELD_CHARS,
    ))
}

fn limit_chars(value: &str, max: usize) -> String {
    let mut chars = value.chars();
    let mut out = chars.by_ref().take(max).collect::<String>();
    if chars.next().is_some() {
        out.push_str("...");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn parses_frontmatter() {
        let (name, description) = parse_frontmatter(
            r#"---
name: browser
description: "Browser automation for local targets."
metadata:
  short-description: Browser
---
# Browser
"#,
        )
        .unwrap();
        assert_eq!(name, "browser");
        assert_eq!(description, "Browser automation for local targets.");
    }

    #[test]
    fn discovers_personal_and_plugin_skills() {
        let _g = ENV_LOCK.lock().unwrap();
        let tmp = tempdir();
        let cwd = tmp.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        std::env::set_var("CODEX_HOME", &tmp);
        std::env::set_var("CLAUDE_HOME", tmp.join("claude-home"));
        std::env::remove_var("AGENTD_ZARVIS_SKILLS");

        write_skill(
            &tmp.join("skills/.system/imagegen/SKILL.md"),
            "imagegen",
            "Generate bitmap visuals.",
        );
        write_skill(
            &tmp.join(
                "plugins/cache/openai-bundled/browser-use/0.1.0-alpha2/skills/browser/SKILL.md",
            ),
            "browser",
            "Browser automation.",
        );
        write_skill(
            &tmp.join("claude-home/skills/review/SKILL.md"),
            "review",
            "Review code changes.",
        );
        write_skill(
            &cwd.join(".claude/skills/project-docs/SKILL.md"),
            "project-docs",
            "Use project docs.",
        );

        let skills = discover(&cwd);
        std::env::remove_var("CODEX_HOME");
        std::env::remove_var("CLAUDE_HOME");

        let names = skills.into_iter().map(|s| s.name).collect::<Vec<_>>();
        assert_eq!(names, vec!["browser", "imagegen", "project-docs", "review"]);
    }

    #[test]
    fn format_section_mentions_skill_md_and_paths() {
        let _g = ENV_LOCK.lock().unwrap();
        let tmp = tempdir();
        std::env::set_var("CODEX_HOME", &tmp);
        std::env::remove_var("CLAUDE_HOME");
        std::env::remove_var("AGENTD_ZARVIS_SKILLS");
        let path = tmp.join("skills/.system/openai-docs/SKILL.md");
        write_skill(&path, "openai-docs", "Use official OpenAI docs.");

        let section = format_section(&tmp).unwrap();
        std::env::remove_var("CODEX_HOME");

        assert!(section.contains("SKILL.md"));
        assert!(section.contains("openai-docs"));
        assert!(section.contains(&path.display().to_string()));
    }

    #[test]
    fn opt_out_respected() {
        let _g = ENV_LOCK.lock().unwrap();
        let tmp = tempdir();
        std::env::set_var("CODEX_HOME", &tmp);
        std::env::set_var("CLAUDE_HOME", tmp.join("claude-home"));
        std::env::set_var("AGENTD_ZARVIS_SKILLS", "off");
        write_skill(
            &tmp.join("skills/.system/imagegen/SKILL.md"),
            "imagegen",
            "Generate bitmap visuals.",
        );

        assert!(discover(&tmp).is_empty());
        std::env::remove_var("CODEX_HOME");
        std::env::remove_var("CLAUDE_HOME");
        std::env::remove_var("AGENTD_ZARVIS_SKILLS");
    }

    fn write_skill(path: &Path, name: &str, description: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            format!("---\nname: {name}\ndescription: {description}\n---\n# {name}\n"),
        )
        .unwrap();
    }

    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "zarvis-skills-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
