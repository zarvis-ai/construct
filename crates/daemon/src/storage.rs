//! File-based session storage.
//!
//! Layout under the data dir:
//!
//! ```text
//! sessions/<id>/
//!     meta.json          # SessionSummary (JSON)
//!     transcript.jsonl   # one TimestampedEvent per line
//!     worktree/          # optional git worktree
//! global/
//!     memory.md          # cross-project memory
//! projects/<id>/
//!     meta.json          # project metadata (GroupSummary JSON)
//!     memory.md          # project-specific memory
//! ```

use agentd_protocol::{
    ProgramDocument, ProgramEdit, ProgramRevision, ProgramTemplate, ProgramUpdateActor, GroupSummary,
    SessionSummary, TimestampedEvent, TranscriptResult, UiPanel, UiPlacement,
};
use anyhow::{Context, Result};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const GLOBAL_MEMORY_TEMPLATE: &str =
    "# Global Memory\n\n## Preferences\n\n## Workflows\n\n## Pitfalls\n";
const PROJECT_MEMORY_TEMPLATE: &str =
    "# Project Memory\n\n## Overview\n\n## Architecture\n\n## Workflows\n\n## Decisions\n\n## Pitfalls\n";
const PROGRAM_REVISION_LIMIT: usize = 50;
const BLANK_PROGRAM: &str = "";
const TASKS_PROGRAM: &str = concat!(
    "# Tasks\n",
    "\n",
    "Press Run (or select a section, then Run) to put the agent to work: it ",
    "reads this board top to bottom, starts the unblocked Todo items, hands ",
    "heavy or parallel work to subagents, moves cards into Progress, and records ",
    "results under Done. Run again to keep it going.\n",
    "\n",
    "Aim a task at a worker with a smart clip — type @ to pick one. For example, ",
    "@{harness:codex} runs a task with Codex, or embed an existing session to ",
    "track and continue its work.\n",
    "\n",
    "## Todo\n",
    "\n",
    "- \n",
    "\n",
    "## Progress\n",
    "\n",
    "## Done\n",
);
const INVESTIGATION_PROGRAM: &str = concat!(
    "# Investigation\n",
    "\n",
    "Run this program to investigate autonomously: the agent works the Plan, ",
    "gathers evidence into Findings, and keeps going until the Question is ",
    "answered. Select a single step and Run to scope the work narrowly, or hand a ",
    "sub-investigation to a subagent by naming a harness like @{harness:claude}.\n",
    "\n",
    "## Question\n",
    "\n",
    "The one thing you want answered — keep it specific.\n",
    "\n",
    "## Context\n",
    "\n",
    "What is already known and where to look. Type @ to embed a live session or ",
    "a harness so the agent can follow it.\n",
    "\n",
    "## Plan\n",
    "\n",
    "The steps to answer the Question. The agent checks these off and adds new ",
    "ones as it learns.\n",
    "\n",
    "- \n",
    "\n",
    "## Findings\n",
    "\n",
    "Evidence and conclusions as they emerge, each tied to the step that produced ",
    "it.\n",
    "\n",
    "## Done\n",
    "\n",
    "Closed-out work and the final answer to the Question.\n",
);

#[derive(Debug, Default)]
struct WidgetFrontmatter {
    placement: Option<UiPlacement>,
    title: Option<String>,
}

/// Apply anchored edits to `base` in order, returning the new content.
///
/// Each edit replaces `old_string` with `new_string`. An empty `old_string`
/// appends `new_string` to the end of the document. A missing anchor or an
/// ambiguous one (multiple matches without `replace_all`) is an error — the
/// signal that the targeted text genuinely changed underneath the writer.
pub fn apply_program_edits(base: &str, edits: &[ProgramEdit]) -> Result<String> {
    let mut working = base.to_string();
    for (i, edit) in edits.iter().enumerate() {
        if edit.old_string.is_empty() {
            if working.is_empty() {
                working = edit.new_string.clone();
            } else {
                if !working.ends_with('\n') {
                    working.push('\n');
                }
                working.push_str(&edit.new_string);
            }
            continue;
        }
        let matches = working.matches(&edit.old_string).count();
        match matches {
            0 => anyhow::bail!(
                "program edit {}: old_string not found in the current program:\n{}",
                i + 1,
                edit.old_string
            ),
            n if n > 1 && !edit.replace_all => anyhow::bail!(
                "program edit {}: old_string is not unique ({} matches); add surrounding context or set replace_all",
                i + 1,
                n
            ),
            _ => {
                working = if edit.replace_all {
                    working.replace(&edit.old_string, &edit.new_string)
                } else {
                    working.replacen(&edit.old_string, &edit.new_string, 1)
                };
            }
        }
    }
    Ok(working)
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct ProgramMeta {
    #[serde(default)]
    version: u64,
    #[serde(default)]
    updated_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    template_id: Option<String>,
}

pub struct Storage {
    data_dir: PathBuf,
    /// When set, program templates are read from this directory instead of the
    /// default `data_dir/program/templates`. Resolved at daemon start from the
    /// `[program].templates_dir` config option / `CONSTRUCT_PROGRAM_TEMPLATES_DIR`
    /// env override. `None` keeps the legacy default location (and its
    /// `canvas/templates` → `program/templates` migration).
    program_templates_dir_override: Option<PathBuf>,
}

impl Storage {
    pub fn new(data_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(data_dir.join("sessions"))
            .with_context(|| format!("create {}", data_dir.display()))?;
        std::fs::create_dir_all(data_dir.join("projects"))
            .with_context(|| format!("create {}", data_dir.join("projects").display()))?;
        Ok(Self {
            data_dir,
            program_templates_dir_override: None,
        })
    }

    /// Override the directory program templates are read from. Set once at
    /// daemon start from config; `None` (the default) keeps `data_dir/program/templates`.
    pub fn with_program_templates_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.program_templates_dir_override = dir;
        self
    }

    pub fn global_memory_path(&self) -> PathBuf {
        self.data_dir.join("global").join("memory.md")
    }

    pub fn projects_root(&self) -> PathBuf {
        self.data_dir.join("projects")
    }

    pub fn project_dir(&self, id: &str) -> PathBuf {
        self.projects_root().join(safe_memory_segment(id))
    }

    pub fn project_meta_path(&self, id: &str) -> PathBuf {
        self.project_dir(id).join("meta.json")
    }

    pub fn project_memory_path(&self, project_id: &str) -> PathBuf {
        self.project_dir(project_id).join("memory.md")
    }

    pub fn ensure_global_memory(&self) -> Result<PathBuf> {
        ensure_memory_file(&self.global_memory_path(), GLOBAL_MEMORY_TEMPLATE)
    }

    pub fn ensure_project_memory(&self, project_id: &str) -> Result<PathBuf> {
        ensure_memory_file(
            &self.project_memory_path(project_id),
            PROJECT_MEMORY_TEMPLATE,
        )
    }

    pub fn groups_root(&self) -> PathBuf {
        self.data_dir.join("groups")
    }

    pub fn group_path(&self, id: &str) -> PathBuf {
        self.groups_root().join(format!("{id}.json"))
    }

    pub fn save_group(&self, g: &GroupSummary) -> Result<()> {
        std::fs::create_dir_all(self.project_dir(&g.id))?;
        let path = self.project_meta_path(&g.id);
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(g)?;
        std::fs::write(&tmp, json).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("rename {}", path.display()))?;
        Ok(())
    }

    pub fn load_groups(&self) -> Result<Vec<GroupSummary>> {
        let mut out = Vec::new();
        let root = self.projects_root();
        if root.exists() {
            for entry in std::fs::read_dir(&root)? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                let path = entry.path().join("meta.json");
                if !path.exists() {
                    continue;
                }
                match std::fs::read(&path)
                    .and_then(|b| serde_json::from_slice::<GroupSummary>(&b).map_err(Into::into))
                {
                    Ok(g) => out.push(g),
                    Err(e) => {
                        tracing::warn!(path = %path.display(), error = %e, "skip unreadable project")
                    }
                }
            }
        }

        // Compatibility migration from the pre-project layout:
        // `<data>/groups/<id>.json` -> `<data>/projects/<id>/meta.json`.
        let legacy_root = self.groups_root();
        if legacy_root.exists() {
            for entry in std::fs::read_dir(&legacy_root)? {
                let entry = entry?;
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                match std::fs::read(&path)
                    .and_then(|b| serde_json::from_slice::<GroupSummary>(&b).map_err(Into::into))
                {
                    Ok(g) => {
                        if !out.iter().any(|existing| existing.id == g.id) {
                            if let Err(e) = self.save_group(&g) {
                                tracing::warn!(project = %g.id, error = ?e, "project migration save failed");
                            } else if let Err(e) = std::fs::remove_file(&path) {
                                tracing::warn!(path = %path.display(), error = ?e, "legacy group cleanup failed");
                            }
                            out.push(g);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(path = %path.display(), error = %e, "skip unreadable legacy group")
                    }
                }
            }
        }
        out.sort_by_key(|g| g.position);
        Ok(out)
    }

    pub fn remove_group(&self, id: &str) -> Result<()> {
        let p = self.project_meta_path(id);
        if p.exists() {
            std::fs::remove_file(&p).with_context(|| format!("remove {}", p.display()))?;
        }
        let legacy = self.group_path(id);
        if legacy.exists() {
            std::fs::remove_file(&legacy)
                .with_context(|| format!("remove {}", legacy.display()))?;
        }
        Ok(())
    }

    pub fn sessions_root(&self) -> PathBuf {
        self.data_dir.join("sessions")
    }

    pub fn session_dir(&self, id: &str) -> PathBuf {
        self.sessions_root().join(id)
    }

    pub fn meta_path(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("meta.json")
    }

    pub fn transcript_path(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("transcript.jsonl")
    }

    pub fn pty_log_path(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("pty.log")
    }

    pub fn worktree_path(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("worktree")
    }

    pub fn widgets_dir(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("widgets")
    }

    pub fn program_path(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("program.md")
    }

    pub fn program_meta_path(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("program.json")
    }

    pub fn program_revisions_path(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("program-revisions.jsonl")
    }

    pub fn program_templates_dir(&self) -> PathBuf {
        self.program_templates_dir_override
            .clone()
            .unwrap_or_else(|| self.data_dir.join("program").join("templates"))
    }

    /// One-time, idempotent migration: the per-session program document was
    /// formerly named "canvas". Rename any surviving `canvas.*` artifacts to
    /// their `program.*` counterparts on first access so existing sessions keep
    /// their content after the rename. Only renames when the legacy file exists
    /// and the new file does not, so it is safe to call on every read.
    fn migrate_legacy_program_files(&self, id: &str) {
        let dir = self.session_dir(id);
        for (old, new) in [
            ("canvas.md", "program.md"),
            ("canvas.json", "program.json"),
            ("canvas-revisions.jsonl", "program-revisions.jsonl"),
            ("canvas-run-context.json", "program-run-context.json"),
        ] {
            let old_path = dir.join(old);
            let new_path = dir.join(new);
            if old_path.exists() && !new_path.exists() {
                if let Err(e) = std::fs::rename(&old_path, &new_path) {
                    tracing::warn!(session = %id, old, new, error = %e, "migrate legacy canvas file failed");
                }
            }
        }
    }

    pub fn read_program(&self, id: &str) -> Result<ProgramDocument> {
        self.ensure_session_dir(id)?;
        self.migrate_legacy_program_files(id);
        let markdown = std::fs::read_to_string(self.program_path(id)).unwrap_or_default();
        let meta = self.read_program_meta(id).unwrap_or_default();
        Ok(ProgramDocument {
            session_id: id.to_string(),
            markdown,
            version: meta.version,
            updated_at_ms: meta.updated_at_ms,
            template_id: meta.template_id,
        })
    }

    pub fn update_program(
        &self,
        id: &str,
        markdown: String,
        actor: ProgramUpdateActor,
        base_version: Option<u64>,
        template_id: Option<String>,
        note: Option<String>,
    ) -> Result<ProgramDocument> {
        let current = self.read_program(id)?;
        if let Some(base) = base_version {
            if base != current.version {
                anyhow::bail!(
                    "program conflict: current version is {}, attempted base version is {}",
                    current.version,
                    base
                );
            }
        }
        if actor == ProgramUpdateActor::Agent && current.version > 0 {
            self.append_program_revision(
                id,
                ProgramRevision {
                    version: current.version,
                    actor,
                    at_ms: current.updated_at_ms,
                    markdown: current.markdown,
                    note: note.clone(),
                },
            )?;
        }
        self.ensure_session_dir(id)?;
        let next = ProgramDocument {
            session_id: id.to_string(),
            markdown,
            version: current.version.saturating_add(1),
            updated_at_ms: chrono::Utc::now().timestamp_millis(),
            template_id: template_id.or(current.template_id),
        };
        let program_tmp = self.program_path(id).with_extension("md.tmp");
        std::fs::write(&program_tmp, &next.markdown)
            .with_context(|| format!("write {}", program_tmp.display()))?;
        std::fs::rename(&program_tmp, self.program_path(id))
            .with_context(|| format!("rename {}", self.program_path(id).display()))?;
        self.save_program_meta(&next)?;
        Ok(next)
    }

    /// Apply a sequence of anchored edits to the *latest* program content and
    /// persist the result as a new version. Unlike [`Self::update_program`],
    /// there is no `base_version` gate: edits are anchored to text, so
    /// concurrent changes to *other* regions merge cleanly. Returns an error
    /// — and writes nothing — when any edit's anchor is missing or ambiguous,
    /// so the caller can re-read and retry. A no-op edit set leaves the version
    /// untouched.
    pub fn edit_program(
        &self,
        id: &str,
        edits: &[ProgramEdit],
        actor: ProgramUpdateActor,
        note: Option<String>,
    ) -> Result<ProgramDocument> {
        if edits.is_empty() {
            anyhow::bail!("program edit: no edits provided");
        }
        let current = self.read_program(id)?;
        let markdown = apply_program_edits(&current.markdown, edits)?;
        if markdown == current.markdown {
            return Ok(current);
        }
        if actor == ProgramUpdateActor::Agent && current.version > 0 {
            self.append_program_revision(
                id,
                ProgramRevision {
                    version: current.version,
                    actor,
                    at_ms: current.updated_at_ms,
                    markdown: current.markdown.clone(),
                    note: note.clone(),
                },
            )?;
        }
        self.ensure_session_dir(id)?;
        let next = ProgramDocument {
            session_id: id.to_string(),
            markdown,
            version: current.version.saturating_add(1),
            updated_at_ms: chrono::Utc::now().timestamp_millis(),
            template_id: current.template_id.clone(),
        };
        let program_tmp = self.program_path(id).with_extension("md.tmp");
        std::fs::write(&program_tmp, &next.markdown)
            .with_context(|| format!("write {}", program_tmp.display()))?;
        std::fs::rename(&program_tmp, self.program_path(id))
            .with_context(|| format!("rename {}", self.program_path(id).display()))?;
        self.save_program_meta(&next)?;
        Ok(next)
    }

    pub fn read_program_revisions(&self, id: &str) -> Result<Vec<ProgramRevision>> {
        let path = self.program_revisions_path(id);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let f = std::fs::File::open(&path)?;
        let reader = std::io::BufReader::new(f);
        let mut revisions = Vec::new();
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<ProgramRevision>(&line) {
                Ok(revision) => revisions.push(revision),
                Err(e) => tracing::warn!(session = %id, error = %e, "skip bad program revision"),
            }
        }
        Ok(revisions)
    }

    pub fn program_templates(&self) -> Result<Vec<ProgramTemplate>> {
        let mut templates = vec![
            ProgramTemplate {
                id: "blank".to_string(),
                name: "Blank".to_string(),
                description: Some("Start with an empty orchestration program".to_string()),
                markdown: BLANK_PROGRAM.to_string(),
                built_in: true,
            },
            ProgramTemplate {
                id: "tasks".to_string(),
                name: "Tasks".to_string(),
                description: Some(
                    "Todo / Progress / Done board the agent runs and delegates".to_string(),
                ),
                markdown: TASKS_PROGRAM.to_string(),
                built_in: true,
            },
            ProgramTemplate {
                id: "investigation".to_string(),
                name: "Investigation".to_string(),
                description: Some(
                    "Question, context, plan, findings, and done — run to investigate".to_string(),
                ),
                markdown: INVESTIGATION_PROGRAM.to_string(),
                built_in: true,
            },
        ];
        let dir = self.program_templates_dir();
        // Migrate user templates from the former `canvas/templates` location.
        // Only for the default location — when an explicit override is set the
        // operator owns that directory, so we never move files into it.
        if self.program_templates_dir_override.is_none() {
            let legacy_dir = self.data_dir.join("canvas").join("templates");
            if legacy_dir.exists() && !dir.exists() {
                if let Some(parent) = dir.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if let Err(e) = std::fs::rename(&legacy_dir, &dir) {
                    tracing::warn!(error = %e, "migrate legacy canvas templates dir failed");
                }
            }
        }
        if dir.exists() {
            for entry in
                std::fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))?
            {
                let entry = entry?;
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("md") {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                let raw = match std::fs::read_to_string(&path) {
                    Ok(raw) => raw,
                    Err(e) => {
                        tracing::warn!(path = %path.display(), error = ?e, "skip unreadable program template");
                        continue;
                    }
                };
                templates.push(ProgramTemplate {
                    id: stem.to_string(),
                    name: prettify_template_name(stem),
                    description: None,
                    markdown: raw,
                    built_in: false,
                });
            }
        }
        templates.sort_by(|a, b| {
            a.built_in
                .cmp(&b.built_in)
                .reverse()
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(templates)
    }

    fn read_program_meta(&self, id: &str) -> Result<ProgramMeta> {
        let path = self.program_meta_path(id);
        if !path.exists() {
            return Ok(ProgramMeta::default());
        }
        let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
    }

    fn save_program_meta(&self, program: &ProgramDocument) -> Result<()> {
        self.ensure_session_dir(&program.session_id)?;
        let meta = ProgramMeta {
            version: program.version,
            updated_at_ms: program.updated_at_ms,
            template_id: program.template_id.clone(),
        };
        let path = self.program_meta_path(&program.session_id);
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(&meta)?;
        std::fs::write(&tmp, json).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("rename {}", path.display()))?;
        Ok(())
    }

    fn append_program_revision(&self, id: &str, revision: ProgramRevision) -> Result<()> {
        self.ensure_session_dir(id)?;
        let mut revisions = self.read_program_revisions(&id)?;
        revisions.push(revision);
        let start = revisions.len().saturating_sub(PROGRAM_REVISION_LIMIT);
        let path = self.program_revisions_path(&id);
        let tmp = path.with_extension("jsonl.tmp");
        let mut f =
            std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        for rev in &revisions[start..] {
            let line = serde_json::to_string(rev)?;
            f.write_all(line.as_bytes())?;
            f.write_all(b"\n")?;
        }
        std::fs::rename(&tmp, &path).with_context(|| format!("rename {}", path.display()))?;
        Ok(())
    }

    pub fn ensure_widgets_dir(&self, id: &str) -> Result<PathBuf> {
        self.ensure_session_dir(id)?;
        let dir = self.widgets_dir(id);
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        Ok(dir)
    }

    pub fn read_widgets(&self, id: &str) -> Result<Vec<UiPanel>> {
        let dir = self.widgets_dir(id);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut panels = Vec::new();
        for entry in std::fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Ok(raw_markdown) = std::fs::read_to_string(&path) else {
                continue;
            };
            let (frontmatter, markdown) = parse_widget_frontmatter(&raw_markdown);
            let created_at_ms = widget_created_at_ms(&path);
            panels.push(UiPanel {
                id: stem.to_string(),
                source: path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map(str::to_string),
                title: frontmatter
                    .title
                    .or_else(|| Some(stem.replace(['-', '_'], " "))),
                created_at_ms,
                placement: frontmatter.placement.unwrap_or(UiPlacement::Sticky),
                markdown,
            });
        }
        panels.sort_by(|a, b| {
            a.created_at_ms
                .cmp(&b.created_at_ms)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(panels)
    }

    pub fn delete_widget(&self, session_id: &str, panel_id: &str) -> Result<()> {
        if !is_safe_widget_id(panel_id) {
            anyhow::bail!("invalid widget id: {panel_id}");
        }
        let dir = self.widgets_dir(session_id);
        let path = dir.join(format!("{panel_id}.md"));
        if path.exists() {
            std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        }
        Ok(())
    }

    pub fn ensure_session_dir(&self, id: &str) -> Result<()> {
        let dir = self.session_dir(id);
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))
    }

    pub fn save_summary(&self, s: &SessionSummary) -> Result<()> {
        self.ensure_session_dir(&s.id)?;
        let path = self.meta_path(&s.id);
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(s)?;
        std::fs::write(&tmp, json).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("rename {}", path.display()))?;
        Ok(())
    }

    pub fn load_summary(&self, id: &str) -> Result<SessionSummary> {
        let path = self.meta_path(id);
        let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        let s: SessionSummary =
            serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
        Ok(s)
    }

    pub fn start_params_path(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("start.json")
    }

    /// Persist the params used to spawn this session, so a daemon restart
    /// can re-spawn with the same shape (cwd, harness, env, args, …).
    pub fn save_start_params(
        &self,
        id: &str,
        params: &agentd_protocol::SessionStartParams,
    ) -> Result<()> {
        self.ensure_session_dir(id)?;
        let path = self.start_params_path(id);
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(params)?;
        std::fs::write(&tmp, json).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("rename {}", path.display()))?;
        Ok(())
    }

    pub fn load_start_params(&self, id: &str) -> Result<agentd_protocol::SessionStartParams> {
        let path = self.start_params_path(id);
        let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        let p: agentd_protocol::SessionStartParams =
            serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
        Ok(p)
    }

    /// Path to the per-session cache of the most recent PTY size. Kept
    /// separate from `start.json` so the "params we were created with"
    /// stay immutable. Used on daemon respawn so the new adapter's PTY
    /// starts at the size the user last sized to, not the creation
    /// default — without this, claude/codex render their resume banner
    /// at the placeholder 80×10 and the TUI shows a tiny garbled box
    /// until the user nudges the terminal to trigger a fresh resize.
    pub fn pty_size_path(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("pty_size.json")
    }

    pub fn save_pty_size(&self, id: &str, size: agentd_protocol::PtySize) -> Result<()> {
        self.ensure_session_dir(id)?;
        let path = self.pty_size_path(id);
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string(&size)?;
        std::fs::write(&tmp, json).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("rename {}", path.display()))?;
        Ok(())
    }

    pub fn load_pty_size(&self, id: &str) -> Option<agentd_protocol::PtySize> {
        let path = self.pty_size_path(id);
        let bytes = std::fs::read(&path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub fn list_summaries(&self) -> Result<Vec<SessionSummary>> {
        let mut out = Vec::new();
        let root = self.sessions_root();
        if !root.exists() {
            return Ok(out);
        }
        for entry in std::fs::read_dir(&root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().to_string();
            match self.load_summary(&id) {
                Ok(s) => out.push(s),
                Err(e) => tracing::warn!(%id, error = ?e, "skipping unreadable session"),
            }
        }
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(out)
    }

    pub fn append_event(&self, id: &str, ev: &TimestampedEvent) -> Result<()> {
        self.ensure_session_dir(id)?;
        let path = self.transcript_path(id);
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        let line = serde_json::to_string(ev)?;
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")?;
        Ok(())
    }

    pub fn read_transcript(
        &self,
        id: &str,
        from: u64,
        limit: Option<usize>,
    ) -> Result<TranscriptResult> {
        let path = self.transcript_path(id);
        if !path.exists() {
            return Ok(TranscriptResult {
                events: Vec::new(),
                total: 0,
            });
        }
        let f = std::fs::File::open(&path)?;
        let reader = std::io::BufReader::new(f);
        let mut events: Vec<TimestampedEvent> = Vec::new();
        let mut total: u64 = 0;
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            total += 1;
            let ev: TimestampedEvent = match serde_json::from_str(&line) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(%id, error = %e, "skip bad transcript line");
                    continue;
                }
            };
            if ev.seq < from {
                continue;
            }
            events.push(ev);
            if let Some(lim) = limit {
                if events.len() >= lim {
                    break;
                }
            }
        }
        Ok(TranscriptResult { events, total })
    }

    /// Read the most-recent `n` events of the session's transcript without
    /// scanning the whole file. Seeks backward in 64 KiB chunks until we've
    /// gathered at least `n + 1` newlines (or hit the start), then parses
    /// only those lines as JSON. For a multi-GB transcript the cost is
    /// O(`n` × average line size), not O(file size), which is what makes
    /// the webui's tail-pagination fast enough to render the live tail
    /// before the user notices a wait.
    pub fn read_transcript_tail(&self, id: &str, n: usize) -> Result<Vec<TimestampedEvent>> {
        if n == 0 {
            return Ok(Vec::new());
        }
        let path = self.transcript_path(id);
        if !path.exists() {
            return Ok(Vec::new());
        }
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(&path)?;
        let size = f.metadata()?.len();
        if size == 0 {
            return Ok(Vec::new());
        }
        const CHUNK: u64 = 64 * 1024;
        let mut buf: Vec<u8> = Vec::new();
        let mut offset = size;
        let mut newlines = 0usize;
        // Read at least n + 1 newlines so the first (partial) line we slice
        // off doesn't accidentally chop a real event in half.
        while offset > 0 && newlines <= n {
            let to_read = CHUNK.min(offset);
            offset -= to_read;
            f.seek(SeekFrom::Start(offset))?;
            let mut chunk = vec![0u8; to_read as usize];
            f.read_exact(&mut chunk)?;
            newlines += chunk.iter().filter(|&&b| b == b'\n').count();
            chunk.extend_from_slice(&buf);
            buf = chunk;
        }
        // Take the last n non-empty lines.
        let text = String::from_utf8_lossy(&buf);
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        let start = lines.len().saturating_sub(n);
        let mut events: Vec<TimestampedEvent> = Vec::with_capacity(lines.len() - start);
        for line in &lines[start..] {
            match serde_json::from_str(line) {
                Ok(e) => events.push(e),
                Err(e) => tracing::warn!(%id, error = %e, "skip bad transcript line"),
            }
        }
        Ok(events)
    }

    pub fn truncate_transcript(&self, id: &str) -> Result<()> {
        let path = self.transcript_path(id);
        if !path.exists() {
            return Ok(());
        }
        std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
            .with_context(|| format!("truncate {}", path.display()))?;
        Ok(())
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Append raw PTY bytes to the session's `pty.log`. Best-effort; this
    /// gets called on every Pty event so it has to stay cheap. Append-only,
    /// no rotation — operators can truncate / rotate externally if needed.
    /// Truncate the session's `pty.log` to zero bytes. Called on
    /// session respawn so the new adapter's child can render into a
    /// clean PTY without bytes from the previous incarnation
    /// interfering with vt100 state.
    pub fn truncate_pty_log(&self, id: &str) -> Result<()> {
        let path = self.pty_log_path(id);
        if !path.exists() {
            return Ok(());
        }
        std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
            .with_context(|| format!("truncate {}", path.display()))?;
        Ok(())
    }

    pub fn append_pty_bytes(&self, id: &str, bytes: &[u8]) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.ensure_session_dir(id)?;
        use std::io::Write;
        let path = self.pty_log_path(id);
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        f.write_all(bytes)?;
        Ok(())
    }

    /// Read the last `max_bytes` of the session's `pty.log`. Returns an
    /// empty buffer if the file doesn't exist. Used by `pty_replay` to feed
    /// a TUI's vt100 parser on attach so scrollback covers the on-disk log,
    /// not just a small in-memory window.
    pub fn read_pty_tail(&self, id: &str, max_bytes: usize) -> Result<Vec<u8>> {
        let (bytes, _, _, _) = self.read_pty_range_before(id, max_bytes, None)?;
        Ok(bytes)
    }

    /// Read up to `max_bytes` ending before `before_offset` from `pty.log`.
    /// Offsets are absolute byte offsets in the file; `before_offset: None`
    /// means the current end. Returns `(bytes, start, end, total_len)`.
    pub fn read_pty_range_before(
        &self,
        id: &str,
        max_bytes: usize,
        before_offset: Option<u64>,
    ) -> Result<(Vec<u8>, u64, u64, u64)> {
        let path = self.pty_log_path(id);
        if !path.exists() || max_bytes == 0 {
            return Ok((Vec::new(), 0, 0, 0));
        }
        let mut f =
            std::fs::File::open(&path).with_context(|| format!("open {}", path.display()))?;
        let total = f.metadata()?.len();
        let end = before_offset.unwrap_or(total).min(total);
        let start = end.saturating_sub(max_bytes as u64);
        use std::io::{Read, Seek};
        f.seek(std::io::SeekFrom::Start(start))?;
        let mut buf = vec![0; (end - start) as usize];
        f.read_exact(&mut buf)?;
        Ok((buf, start, end, total))
    }

    /// Remove the entire session directory (meta + transcript + worktree).
    /// Idempotent: missing directory is not an error.
    pub fn remove_session(&self, id: &str) -> Result<()> {
        let dir = self.session_dir(id);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).with_context(|| format!("remove {}", dir.display()))?;
        }
        Ok(())
    }
}

fn widget_created_at_ms(path: &Path) -> u64 {
    let Ok(metadata) = std::fs::metadata(path) else {
        return 0;
    };
    metadata
        .created()
        .or_else(|_| metadata.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn is_safe_widget_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
}

fn parse_widget_frontmatter(raw: &str) -> (WidgetFrontmatter, String) {
    let Some(rest) = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))
    else {
        return (WidgetFrontmatter::default(), raw.to_string());
    };
    let mut byte_offset = raw.len().saturating_sub(rest.len());
    let mut frontmatter = String::new();
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        byte_offset += line.len();
        if trimmed == "---" {
            let parsed = parse_widget_frontmatter_fields(&frontmatter);
            return (parsed, raw[byte_offset..].to_string());
        }
        frontmatter.push_str(line);
    }
    (WidgetFrontmatter::default(), raw.to_string())
}

fn parse_widget_frontmatter_fields(frontmatter: &str) -> WidgetFrontmatter {
    let mut parsed = WidgetFrontmatter::default();
    for line in frontmatter.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().trim_matches(['"', '\'']);
        match key.trim() {
            "placement" if value.eq_ignore_ascii_case("inline") => {
                parsed.placement = Some(UiPlacement::Inline);
            }
            "placement" if value.eq_ignore_ascii_case("sticky") => {
                parsed.placement = Some(UiPlacement::Sticky);
            }
            "title" if !value.is_empty() => parsed.title = Some(value.to_string()),
            _ => {}
        }
    }
    parsed
}

/// Derive a display name from a custom template's filename stem: `-`/`_` become
/// spaces and each word is title-cased (e.g. `code-review` → "Code Review").
fn prettify_template_name(stem: &str) -> String {
    stem.replace(['-', '_'], " ")
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn ensure_memory_file(path: &Path, template: &str) -> Result<PathBuf> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    if !path.exists() {
        std::fs::write(path, template).with_context(|| format!("write {}", path.display()))?;
    }
    Ok(path.to_path_buf())
}

fn safe_memory_segment(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "unknown".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod widget_tests {
    use super::*;

    #[test]
    fn delete_widget_rejects_path_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();

        let err = storage.delete_widget("s1", "../memory").unwrap_err();

        assert!(err.to_string().contains("invalid widget id"));
    }

    #[test]
    fn read_widgets_parses_inline_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();
        let dir = storage.ensure_widgets_dir("s1").unwrap();
        std::fs::write(
            dir.join("confirm.md"),
            "---\nplacement: inline\ntitle: Confirm action\n---\n# Confirm\n\n[OK](agentd:action/ok?close=1)\n",
        )
        .unwrap();

        let widgets = storage.read_widgets("s1").unwrap();

        assert_eq!(widgets.len(), 1);
        assert_eq!(widgets[0].id, "confirm");
        assert_eq!(widgets[0].title.as_deref(), Some("Confirm action"));
        assert_eq!(widgets[0].placement, UiPlacement::Inline);
        assert!(widgets[0].markdown.starts_with("# Confirm"));
        assert!(!widgets[0].markdown.contains("placement:"));
        assert!(widgets[0].created_at_ms > 0);
    }

    #[test]
    fn read_widgets_sorts_by_creation_time_then_id() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();
        let dir = storage.ensure_widgets_dir("s1").unwrap();
        let old = dir.join("z-old.md");
        let new = dir.join("a-new.md");
        std::fs::write(&old, "old").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(&new, "new").unwrap();

        let widgets = storage.read_widgets("s1").unwrap();

        assert_eq!(
            widgets.iter().map(|w| w.id.as_str()).collect::<Vec<_>>(),
            vec!["z-old", "a-new"]
        );
        assert!(widgets[0].created_at_ms <= widgets[1].created_at_ms);
    }
}

#[cfg(test)]
mod transcript_tail_tests {
    use super::*;
    use agentd_protocol::SessionEvent;
    use chrono::Utc;

    fn make_event(seq: u64) -> TimestampedEvent {
        TimestampedEvent {
            seq,
            at: Utc::now(),
            event: SessionEvent::Message {
                role: agentd_protocol::MessageRole::Assistant,
                text: format!("msg #{seq}"),
            },
        }
    }

    #[test]
    fn tail_returns_last_n_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();
        for seq in 1..=1000 {
            storage.append_event("s1", &make_event(seq)).unwrap();
        }

        let tail = storage.read_transcript_tail("s1", 5).unwrap();

        assert_eq!(tail.len(), 5);
        assert_eq!(tail.first().unwrap().seq, 996);
        assert_eq!(tail.last().unwrap().seq, 1000);
    }

    #[test]
    fn tail_handles_partial_first_chunk_without_chopping_an_event() {
        // The seek-back read can land mid-line; the parser must drop that
        // partial head, not parse it as garbage. Use long events so a
        // 64 KiB chunk easily spans a line boundary that isn't on a chunk
        // boundary.
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();
        let big_text = "x".repeat(2048);
        for seq in 1..=100 {
            let ev = TimestampedEvent {
                seq,
                at: Utc::now(),
                event: SessionEvent::Message {
                    role: agentd_protocol::MessageRole::Assistant,
                    text: format!("{big_text} #{seq}"),
                },
            };
            storage.append_event("s1", &ev).unwrap();
        }

        let tail = storage.read_transcript_tail("s1", 3).unwrap();

        assert_eq!(tail.len(), 3);
        assert_eq!(tail.first().unwrap().seq, 98);
        assert_eq!(tail.last().unwrap().seq, 100);
    }

    #[test]
    fn tail_returns_all_events_when_n_exceeds_total() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();
        for seq in 1..=7 {
            storage.append_event("s1", &make_event(seq)).unwrap();
        }

        let tail = storage.read_transcript_tail("s1", 100).unwrap();

        assert_eq!(tail.len(), 7);
    }

    #[test]
    fn tail_on_missing_transcript_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();

        let tail = storage.read_transcript_tail("never-existed", 10).unwrap();

        assert!(tail.is_empty());
    }

    #[test]
    fn tail_with_zero_n_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();
        storage.append_event("s1", &make_event(1)).unwrap();

        let tail = storage.read_transcript_tail("s1", 0).unwrap();

        assert!(tail.is_empty());
    }
}

#[cfg(test)]
mod pty_range_tests {
    use super::*;

    #[test]
    fn pty_range_before_returns_offsets_and_requested_slice() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();
        storage
            .append_pty_bytes("s1", b"abcdefghijklmnopqrstuvwxyz")
            .unwrap();

        let (bytes, start, end, total) = storage.read_pty_range_before("s1", 5, Some(20)).unwrap();

        assert_eq!(bytes, b"pqrst");
        assert_eq!(start, 15);
        assert_eq!(end, 20);
        assert_eq!(total, 26);
    }

    #[test]
    fn pty_range_before_clamps_to_file_start_and_end() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();
        storage.append_pty_bytes("s1", b"abcdef").unwrap();

        let (bytes, start, end, total) = storage.read_pty_range_before("s1", 99, Some(99)).unwrap();

        assert_eq!(bytes, b"abcdef");
        assert_eq!(start, 0);
        assert_eq!(end, 6);
        assert_eq!(total, 6);
    }
}

#[cfg(test)]
mod program_tests {
    use super::*;

    #[test]
    fn program_update_rejects_stale_base_version() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();
        let first = storage
            .update_program(
                "s1",
                "# Todo\n".into(),
                agentd_protocol::ProgramUpdateActor::Human,
                Some(0),
                None,
                None,
            )
            .unwrap();
        assert_eq!(first.version, 1);

        let err = storage
            .update_program(
                "s1",
                "# Changed\n".into(),
                agentd_protocol::ProgramUpdateActor::Agent,
                Some(0),
                None,
                None,
            )
            .unwrap_err();

        assert!(err.to_string().contains("program conflict"));
    }

    fn edit(old: &str, new: &str) -> ProgramEdit {
        ProgramEdit {
            old_string: old.into(),
            new_string: new.into(),
            replace_all: false,
            keep_pending: false,
        }
    }

    #[test]
    fn apply_program_edits_replaces_unique_anchor() {
        let out = apply_program_edits(
            "# Todo\n- ship it\n# Done\n",
            &[edit("- ship it", "- ship it @{harness:claude}")],
        )
        .unwrap();
        assert_eq!(out, "# Todo\n- ship it @{harness:claude}\n# Done\n");
    }

    #[test]
    fn apply_program_edits_appends_on_empty_old_string() {
        // Empty doc: append sets the content.
        assert_eq!(apply_program_edits("", &[edit("", "first")]).unwrap(), "first");
        // Non-empty without trailing newline: a separator is inserted.
        assert_eq!(
            apply_program_edits("# Todo", &[edit("", "- new")]).unwrap(),
            "# Todo\n- new"
        );
        // Already ends in newline: no extra blank line forced.
        assert_eq!(
            apply_program_edits("# Todo\n", &[edit("", "- new")]).unwrap(),
            "# Todo\n- new"
        );
    }

    #[test]
    fn apply_program_edits_errors_on_missing_anchor() {
        let err = apply_program_edits("# Todo\n", &[edit("- nope", "x")]).unwrap_err();
        assert!(err.to_string().contains("old_string not found"));
    }

    #[test]
    fn apply_program_edits_errors_on_ambiguous_anchor_then_replace_all_works() {
        let base = "- a\n- a\n";
        let err = apply_program_edits(base, &[edit("- a", "- b")]).unwrap_err();
        assert!(err.to_string().contains("not unique"));

        let out = apply_program_edits(
            base,
            &[ProgramEdit {
                old_string: "- a".into(),
                new_string: "- b".into(),
                replace_all: true,
                keep_pending: false,
            }],
        )
        .unwrap();
        assert_eq!(out, "- b\n- b\n");
    }

    #[test]
    fn apply_program_edits_are_sequential() {
        let out = apply_program_edits("one two", &[edit("one", "1"), edit("two", "2")]).unwrap();
        assert_eq!(out, "1 2");
    }

    #[test]
    fn edit_program_applies_to_latest_without_a_version_gate() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();
        // A human writes v1.
        let v1 = storage
            .update_program(
                "s1",
                "# Todo\n- a\n# Done\n".into(),
                agentd_protocol::ProgramUpdateActor::Human,
                Some(0),
                None,
                None,
            )
            .unwrap();
        assert_eq!(v1.version, 1);
        // The agent edits an anchor with no base_version — it lands on the
        // latest content and bumps the version.
        let v2 = storage
            .edit_program(
                "s1",
                &[edit("- a", "- a (done)")],
                agentd_protocol::ProgramUpdateActor::Agent,
                None,
            )
            .unwrap();
        assert_eq!(v2.version, 2);
        assert_eq!(v2.markdown, "# Todo\n- a (done)\n# Done\n");
        // The overwritten version is retained in history.
        let revisions = storage.read_program_revisions("s1").unwrap();
        assert_eq!(revisions.last().unwrap().version, 1);
    }

    #[test]
    fn edit_program_noop_keeps_version() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();
        let v1 = storage
            .update_program(
                "s1",
                "# Todo\n- a\n".into(),
                agentd_protocol::ProgramUpdateActor::Human,
                Some(0),
                None,
                None,
            )
            .unwrap();
        // Replacing text with itself changes nothing → version is unchanged.
        let same = storage
            .edit_program(
                "s1",
                &[edit("- a", "- a")],
                agentd_protocol::ProgramUpdateActor::Agent,
                None,
            )
            .unwrap();
        assert_eq!(same.version, v1.version);
    }

    #[test]
    fn edit_program_errors_on_missing_anchor_and_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();
        storage
            .update_program(
                "s1",
                "# Todo\n- a\n".into(),
                agentd_protocol::ProgramUpdateActor::Human,
                Some(0),
                None,
                None,
            )
            .unwrap();
        let err = storage
            .edit_program(
                "s1",
                &[edit("- vanished", "x")],
                agentd_protocol::ProgramUpdateActor::Agent,
                None,
            )
            .unwrap_err();
        assert!(err.to_string().contains("old_string not found"));
        // Unchanged on disk.
        let current = storage.read_program("s1").unwrap();
        assert_eq!(current.version, 1);
        assert_eq!(current.markdown, "# Todo\n- a\n");
    }

    #[test]
    fn program_templates_use_filename_as_name_with_verbatim_markdown() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();
        let dir = storage.program_templates_dir();
        std::fs::create_dir_all(&dir).unwrap();
        // No frontmatter handling: the filename stem is the name and the file
        // contents are the markdown verbatim, including any leading `---`.
        let contents = "---\nstill: body\n---\n# Code Review\n- look\n";
        std::fs::write(dir.join("code-review.md"), contents).unwrap();

        let templates = storage.program_templates().unwrap();
        let review = templates.iter().find(|t| t.id == "code-review").unwrap();

        assert_eq!(review.name, "Code Review");
        assert_eq!(review.markdown, contents);
        assert_eq!(review.description, None);
        assert!(!review.built_in);
        assert!(templates.iter().any(|t| t.id == "tasks" && t.built_in));
    }

    #[test]
    fn program_templates_dir_override_redirects_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let custom = tmp.path().join("custom-templates");
        std::fs::create_dir_all(&custom).unwrap();
        std::fs::write(custom.join("ops.md"), "# Ops\n").unwrap();

        let storage = Storage::new(tmp.path().join("data"))
            .unwrap()
            .with_program_templates_dir(Some(custom.clone()));

        // The override directory is the one consulted.
        assert_eq!(storage.program_templates_dir(), custom);
        let templates = storage.program_templates().unwrap();
        assert!(templates.iter().any(|t| t.id == "ops" && t.name == "Ops"));
        // A user template dropped under the default location is NOT read when an
        // override is set.
        let default_dir = tmp.path().join("data").join("program").join("templates");
        std::fs::create_dir_all(&default_dir).unwrap();
        std::fs::write(default_dir.join("ignored.md"), "# Ignored\n").unwrap();
        let templates = storage.program_templates().unwrap();
        assert!(!templates.iter().any(|t| t.id == "ignored"));
    }
}

#[cfg(test)]
mod memory_tests {
    use super::*;

    #[test]
    fn creates_default_memory_files() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();

        let global = storage.ensure_global_memory().unwrap();
        let project = storage.ensure_project_memory("g123").unwrap();

        assert_eq!(global, tmp.path().join("data/global/memory.md"));
        assert_eq!(project, tmp.path().join("data/projects/g123/memory.md"));
        assert!(std::fs::read_to_string(global)
            .unwrap()
            .contains("## Preferences"));
        assert!(std::fs::read_to_string(project)
            .unwrap()
            .contains("## Architecture"));
    }

    #[test]
    fn project_memory_path_sanitizes_project_id() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();

        let path = storage.project_memory_path("../bad/project");

        assert_eq!(
            path,
            tmp.path().join("data/projects/___bad_project/memory.md")
        );
    }

    #[test]
    fn group_metadata_migrates_to_project_meta() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();
        std::fs::create_dir_all(storage.groups_root()).unwrap();
        let group = GroupSummary {
            id: "g123".into(),
            name: "Agentd".into(),
            created_at: chrono::Utc::now(),
            position: 7,
            collapsed: true,
        };
        std::fs::write(
            storage.group_path(&group.id),
            serde_json::to_string_pretty(&group).unwrap(),
        )
        .unwrap();

        let loaded = storage.load_groups().unwrap();

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "g123");
        assert!(storage.project_meta_path("g123").exists());
        assert!(!storage.group_path("g123").exists());
    }

    #[test]
    fn removing_project_metadata_preserves_memory_file() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();
        let group = GroupSummary {
            id: "g123".into(),
            name: "Agentd".into(),
            created_at: chrono::Utc::now(),
            position: 7,
            collapsed: true,
        };
        storage.save_group(&group).unwrap();
        storage.ensure_project_memory(&group.id).unwrap();

        storage.remove_group(&group.id).unwrap();

        assert!(!storage.project_meta_path(&group.id).exists());
        assert!(storage.project_memory_path(&group.id).exists());
    }
}
