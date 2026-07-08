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
    GroupSummary, ProgramBlockView, ProgramDocument, ProgramEdit, ProgramRevision, ProgramTemplate,
    ProgramUpdateActor, SearchHit, SearchParams, SearchResult, SearchScope, SessionSummary,
    TimestampedEvent, TranscriptResult, UiPanel, UiPlacement,
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
/// Per-session cap on transcript bytes read from the tail while searching
/// (spec 0076), mirroring `PTY_REPLAY_CAP`'s "bound the worst case, not the
/// common case" role for `session.pty_replay`.
const PER_SESSION_TRANSCRIPT_SCAN_CAP: u64 = 8 * 1024 * 1024;
/// Global cap on transcript bytes read across every session in one
/// `session.search` call, so a fleet of huge transcripts can't turn a
/// search into a multi-GB scan.
const GLOBAL_TRANSCRIPT_SCAN_CAP: u64 = 64 * 1024 * 1024;
/// Search hit snippets are trimmed to roughly this many characters,
/// centered on the match.
const SEARCH_SNIPPET_MAX_CHARS: usize = 200;
const BLANK_PROGRAM: &str = "";
const TASKS_PROGRAM: &str = concat!(
    "# Rule\n",
    "\n",
    "Todo -> In progress (dispatched) -> Done (merged). I'm pre-approving all ",
    "tasks pr to be merged, so you don't need to ask me. Each item, create ",
    "claude subagent session to resolve. archive subagent sessions after done.\n",
    "\n",
    "## Todo\n",
    "\n",
    "## In Progress\n",
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
    #[serde(default)]
    next_block_ordinal: u64,
    #[serde(default)]
    blocks: Vec<ProgramBlockIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct ProgramBlockIdentity {
    block_id: String,
    content_epoch: u64,
    content_id: String,
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
        let mut meta = self.read_program_meta(id).unwrap_or_default();
        let changed = self.reconcile_program_block_identities(&mut meta, &markdown);
        if changed {
            self.save_program_meta_struct(id, &meta)?;
        }
        Ok(ProgramDocument {
            session_id: id.to_string(),
            markdown,
            version: meta.version,
            updated_at_ms: meta.updated_at_ms,
            template_id: meta.template_id,
        })
    }

    pub fn read_program_with_blocks(
        &self,
        id: &str,
    ) -> Result<(ProgramDocument, Vec<ProgramBlockView>)> {
        let program = self.read_program(id)?;
        let mut meta = self.read_program_meta(id).unwrap_or_default();
        let changed = self.reconcile_program_block_identities(&mut meta, &program.markdown);
        if changed {
            self.save_program_meta_struct(id, &meta)?;
        }
        let blocks = self.program_block_views_from_meta(&meta, &program.markdown);
        Ok((program, blocks))
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
        let mut meta = self
            .read_program_meta(&program.session_id)
            .unwrap_or_default();
        meta.version = program.version;
        meta.updated_at_ms = program.updated_at_ms;
        meta.template_id = program.template_id.clone();
        self.reconcile_program_block_identities(&mut meta, &program.markdown);
        self.save_program_meta_struct(&program.session_id, &meta)
    }

    fn save_program_meta_struct(&self, id: &str, meta: &ProgramMeta) -> Result<()> {
        self.ensure_session_dir(id)?;
        let path = self.program_meta_path(id);
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(&meta)?;
        std::fs::write(&tmp, json).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("rename {}", path.display()))?;
        Ok(())
    }

    fn next_program_block_id(meta: &mut ProgramMeta) -> String {
        let id = format!("pb_{:016x}", meta.next_block_ordinal);
        meta.next_block_ordinal = meta.next_block_ordinal.saturating_add(1);
        id
    }

    /// Reconcile stable block identities (spec 0053) against the current
    /// Markdown in two passes so an edit anywhere in the document cannot
    /// misattribute — and thereby drop the shimmer of — a sibling block whose
    /// content did not change.
    ///
    /// Pass 1 matches by content, order-independent: any span whose content
    /// exactly matches a not-yet-used prior block keeps that block's id and
    /// epoch, no matter where either sits in the document. This alone handles
    /// inserts, deletes, and reorders/moves of untouched blocks.
    ///
    /// Pass 2 handles genuine semantic edits. Only the spans and prior records
    /// left over from pass 1 are considered, and they are paired by their
    /// relative order *within that leftover subsequence* (not by raw document
    /// index) — so an insertion or deletion elsewhere no longer shifts which
    /// prior record an edited block's continuity is attributed to. A leftover
    /// span beyond the leftover records' count is a brand-new block.
    fn reconcile_program_block_identities(&self, meta: &mut ProgramMeta, markdown: &str) -> bool {
        let spans = agentd_protocol::program_block_spans(markdown);
        let old = meta.blocks.clone();
        let mut used = vec![false; old.len()];
        let mut next: Vec<Option<ProgramBlockIdentity>> = vec![None; spans.len()];
        let mut changed = old.len() != spans.len();

        for (i, span) in spans.iter().enumerate() {
            if let Some(j) = old
                .iter()
                .enumerate()
                .find(|(j, rec)| !used[*j] && rec.content_id == span.id)
                .map(|(j, _)| j)
            {
                used[j] = true;
                next[i] = Some(old[j].clone());
            }
        }

        let leftover_old: Vec<usize> = (0..old.len()).filter(|&j| !used[j]).collect();
        let leftover_new: Vec<usize> = (0..spans.len()).filter(|&i| next[i].is_none()).collect();
        for (k, &i) in leftover_new.iter().enumerate() {
            changed = true;
            let rec = if let Some(&j) = leftover_old.get(k) {
                let mut rec = old[j].clone();
                rec.content_epoch = rec.content_epoch.saturating_add(1);
                rec.content_id = spans[i].id.clone();
                rec
            } else {
                ProgramBlockIdentity {
                    block_id: Self::next_program_block_id(meta),
                    content_epoch: 0,
                    content_id: spans[i].id.clone(),
                }
            };
            next[i] = Some(rec);
        }

        // Every span was filled by an exact match (pass 1) or a leftover
        // pairing (pass 2), so this never panics.
        let next: Vec<ProgramBlockIdentity> = next.into_iter().map(Option::unwrap).collect();
        if meta.blocks != next {
            changed = true;
            meta.blocks = next;
        }
        changed
    }

    fn program_block_views_from_meta(
        &self,
        meta: &ProgramMeta,
        markdown: &str,
    ) -> Vec<ProgramBlockView> {
        agentd_protocol::program_block_spans(markdown)
            .into_iter()
            .zip(meta.blocks.iter())
            .map(|(span, rec)| {
                let block_ref = format!("{}:{}", rec.block_id, rec.content_epoch);
                ProgramBlockView {
                    id: block_ref.clone(),
                    block_id: rec.block_id.clone(),
                    content_epoch: rec.content_epoch,
                    block_ref,
                    content_id: rec.content_id.clone(),
                    start_line: span.start_line,
                    end_line: span.end_line,
                    text: span.text,
                    shimmer: false,
                    tooltip: None,
                }
            })
            .collect()
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

    /// Substring search across session name/metadata, stored `program.md`
    /// contents, and transcript history (spec 0076). The file-scanning core
    /// of `session.search`: `SessionManager::search` is a thin async
    /// wrapper around this.
    /// `sessions` is the caller's current session list (`SessionManager`
    /// passes its live, in-memory summaries — this method doesn't re-read
    /// `meta.json` itself, so it always sees the up-to-the-millisecond
    /// state, including sessions whose latest activity hasn't been synced
    /// to disk yet). This method re-orders and filters that list itself
    /// (by recency and `params.session_ids`) and only touches disk for
    /// each session's `program.md` / `transcript.jsonl`.
    pub fn search(
        &self,
        sessions: &[SessionSummary],
        params: &SearchParams,
    ) -> Result<SearchResult> {
        self.search_with_budgets(
            sessions,
            params,
            PER_SESSION_TRANSCRIPT_SCAN_CAP,
            GLOBAL_TRANSCRIPT_SCAN_CAP,
        )
    }

    /// `search` with the transcript byte budgets as explicit parameters, so
    /// tests can exercise truncation without writing tens of megabytes of
    /// fixture data.
    fn search_with_budgets(
        &self,
        sessions: &[SessionSummary],
        params: &SearchParams,
        per_session_cap: u64,
        global_cap: u64,
    ) -> Result<SearchResult> {
        let query = params.query.trim();
        if query.is_empty() {
            return Ok(SearchResult {
                hits: Vec::new(),
                truncated: false,
                sessions_scanned: 0,
            });
        }
        let query_lower = query.to_lowercase();
        let scopes: Vec<SearchScope> = params.scopes.clone().unwrap_or_else(|| {
            vec![
                SearchScope::Name,
                SearchScope::Program,
                SearchScope::Transcript,
            ]
        });
        let limit = params.limit.unwrap_or(50);
        let per_session_limit = params.per_session_limit.unwrap_or(5);

        let mut sessions: Vec<&SessionSummary> = match &params.session_ids {
            Some(ids) => {
                let allow: std::collections::HashSet<&str> =
                    ids.iter().map(String::as_str).collect();
                sessions
                    .iter()
                    .filter(|s| allow.contains(s.id.as_str()))
                    .collect()
            }
            None => sessions.iter().collect(),
        };
        // Most-recent-activity first, not the user-controlled list-view
        // `position` order — a search wants the sessions most likely to be
        // relevant right now, which is recency of activity.
        sessions.sort_by(|a, b| {
            let ka = a.last_event_at.unwrap_or(a.created_at);
            let kb = b.last_event_at.unwrap_or(b.created_at);
            kb.cmp(&ka)
        });

        let mut hits: Vec<SearchHit> = Vec::new();
        let mut truncated = false;
        let mut sessions_scanned = 0usize;
        let mut transcript_budget = global_cap;

        'sessions: for summary in sessions.iter().copied() {
            if hits.len() >= limit {
                truncated = true;
                break;
            }
            sessions_scanned += 1;

            if scopes.contains(&SearchScope::Name) {
                if let Some(hit) = search_name(summary, &query_lower) {
                    hits.push(hit);
                    if hits.len() >= limit {
                        truncated = true;
                        break 'sessions;
                    }
                }
            }
            if scopes.contains(&SearchScope::Program) {
                let (program_hits, program_truncated) =
                    self.search_program(summary, &query_lower, per_session_limit)?;
                truncated |= program_truncated;
                for hit in program_hits {
                    hits.push(hit);
                    if hits.len() >= limit {
                        truncated = true;
                        break 'sessions;
                    }
                }
            }
            if scopes.contains(&SearchScope::Transcript) {
                if transcript_budget == 0 {
                    truncated = true;
                    continue;
                }
                let session_cap = per_session_cap.min(transcript_budget);
                let (transcript_hits, bytes_read, session_truncated) =
                    self.search_transcript(summary, &query_lower, per_session_limit, session_cap)?;
                transcript_budget = transcript_budget.saturating_sub(bytes_read);
                truncated |= session_truncated;
                for hit in transcript_hits {
                    hits.push(hit);
                    if hits.len() >= limit {
                        truncated = true;
                        break 'sessions;
                    }
                }
            }
        }
        if sessions_scanned < sessions.len() {
            truncated = true;
        }

        Ok(SearchResult {
            hits,
            truncated,
            sessions_scanned,
        })
    }

    /// Scope `program`: line-by-line substring search over the session's
    /// `program.md` (via [`Self::read_program`], which also runs the
    /// canvas→program legacy migration). Returns up to `per_session_limit`
    /// hits and whether that limit cut the scan short.
    fn search_program(
        &self,
        summary: &SessionSummary,
        query_lower: &str,
        per_session_limit: usize,
    ) -> Result<(Vec<SearchHit>, bool)> {
        let doc = self.read_program(&summary.id)?;
        let mut hits = Vec::new();
        let mut truncated = false;
        for line in doc.markdown.lines() {
            if hits.len() >= per_session_limit {
                truncated = true;
                break;
            }
            if line.trim().is_empty() {
                continue;
            }
            if let Some((snippet, match_start, match_end)) =
                build_snippet(line, query_lower, SEARCH_SNIPPET_MAX_CHARS)
            {
                hits.push(SearchHit {
                    session_id: summary.id.clone(),
                    title: session_label(summary),
                    harness: summary.harness.clone(),
                    scope: SearchScope::Program,
                    seq: None,
                    at: None,
                    snippet,
                    match_start,
                    match_end,
                });
            }
        }
        Ok((hits, truncated))
    }

    /// Scope `transcript`: scan `transcript.jsonl` backward from the tail
    /// (newest first) via [`BackwardLineReader`], stopping at
    /// `per_session_limit` hits or `byte_budget` bytes read, whichever
    /// comes first. Returns the hits, the bytes actually read (for the
    /// caller's global budget), and whether a cap cut the scan short.
    fn search_transcript(
        &self,
        summary: &SessionSummary,
        query_lower: &str,
        per_session_limit: usize,
        byte_budget: u64,
    ) -> Result<(Vec<SearchHit>, u64, bool)> {
        let path = self.transcript_path(&summary.id);
        let mut reader = match BackwardLineReader::open(&path)? {
            Some(r) => r,
            None => return Ok((Vec::new(), 0, false)),
        };
        if byte_budget == 0 {
            return Ok((Vec::new(), 0, true));
        }
        let mut hits = Vec::new();
        let mut truncated = false;
        loop {
            if hits.len() >= per_session_limit {
                truncated = true;
                break;
            }
            if reader.bytes_read() >= byte_budget {
                truncated = true;
                break;
            }
            let line = match reader.next_line()? {
                Some(l) => l,
                None => break,
            };
            if line.trim().is_empty() {
                continue;
            }
            let ev: TimestampedEvent = match serde_json::from_str(&line) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(session = %summary.id, error = %e, "skip bad transcript line during search");
                    continue;
                }
            };
            if agentd_protocol::slash::is_model_hidden(&ev.event) {
                continue;
            }
            let Some(text) = searchable_event_text(&ev.event) else {
                continue;
            };
            let Some((snippet, match_start, match_end)) =
                build_snippet(&text, query_lower, SEARCH_SNIPPET_MAX_CHARS)
            else {
                continue;
            };
            hits.push(SearchHit {
                session_id: summary.id.clone(),
                title: session_label(summary),
                harness: summary.harness.clone(),
                scope: SearchScope::Transcript,
                seq: Some(ev.seq),
                at: Some(ev.at),
                snippet,
                match_start,
                match_end,
            });
        }
        Ok((hits, reader.bytes_read(), truncated))
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

/// `session_switch`-style label: title if set and non-blank, else the
/// short (first-10-char) id. Mirrors the TUI picker's own label so a
/// `session.search` hit reads the same as the row it corresponds to.
fn session_label(summary: &SessionSummary) -> String {
    summary
        .title
        .as_ref()
        .filter(|t| !t.trim().is_empty())
        .cloned()
        .unwrap_or_else(|| summary.id[..summary.id.len().min(10)].to_string())
}

/// Scope `name`: match against the same fields the TUI's `C-x b` picker
/// matches (label, full id, short id, harness). One hit per session at
/// most for this scope — there's only one name.
fn search_name(summary: &SessionSummary, query_lower: &str) -> Option<SearchHit> {
    let label = session_label(summary);
    let short_id = &summary.id[..summary.id.len().min(10)];
    let is_match = [
        label.as_str(),
        summary.id.as_str(),
        short_id,
        summary.harness.as_str(),
    ]
    .iter()
    .any(|field| field.to_lowercase().contains(query_lower));
    if !is_match {
        return None;
    }
    // Best-effort highlight: only the label is ever shown as the snippet,
    // so if the match actually landed in the id/harness instead, there's
    // nothing in the snippet to highlight.
    let (snippet, match_start, match_end) =
        build_snippet(&label, query_lower, SEARCH_SNIPPET_MAX_CHARS)
            .unwrap_or_else(|| (label.clone(), 0, 0));
    Some(SearchHit {
        session_id: summary.id.clone(),
        title: label,
        harness: summary.harness.clone(),
        scope: SearchScope::Name,
        seq: None,
        at: None,
        snippet,
        match_start,
        match_end,
    })
}

/// Extract the searchable text from a transcript event. Only
/// message/reasoning/tool_use/tool_result events carry text worth
/// searching (spec 0076) — everything else (status, PTY bytes, UI panels,
/// approval/lifecycle plumbing) is noise for this purpose. Callers are
/// expected to have already filtered out `is_model_hidden` events (which
/// would otherwise let the legacy `tui` dispatch `ToolUse` leak in).
fn searchable_event_text(ev: &agentd_protocol::SessionEvent) -> Option<String> {
    use agentd_protocol::SessionEvent;
    match ev {
        SessionEvent::Message { text, .. } => Some(text.clone()),
        SessionEvent::Reasoning { text } => Some(text.clone()),
        SessionEvent::ToolUse { tool, args, .. } => {
            let args_text = if args.is_null() {
                String::new()
            } else {
                args.to_string()
            };
            Some(format!("{tool} {args_text}"))
        }
        SessionEvent::ToolResult { tool, output, .. } => Some(format!("{tool} {output}")),
        _ => None,
    }
}

/// Case-insensitive substring search. Returns the byte range of the first
/// match within `haystack`, in `haystack`'s own byte indexing. `needle_lower`
/// must already be lowercased and non-empty.
///
/// Assumes lowercasing doesn't change a string's byte length, which holds
/// for ASCII (the overwhelmingly common case for session titles/ids and
/// transcript text); exotic Unicode casing (Turkish İ, German ß, …) may
/// shift the returned offsets slightly. Acceptable for a substring-match
/// search feature with no regex/whole-word ambitions.
fn find_ci(haystack: &str, needle_lower: &str) -> Option<(usize, usize)> {
    if needle_lower.is_empty() {
        return None;
    }
    let hay_lower = haystack.to_lowercase();
    let start = hay_lower.find(needle_lower)?;
    Some((start, start + needle_lower.len()))
}

/// Build a match snippet: `text` trimmed to roughly `max_chars` centered on
/// the match, with the match's byte range re-expressed relative to the
/// snippet (for highlighting). Returns `None` when `text` doesn't contain
/// `needle_lower`. Window boundaries are snapped outward to char
/// boundaries so multi-byte UTF-8 is never sliced mid-codepoint.
fn build_snippet(
    text: &str,
    needle_lower: &str,
    max_chars: usize,
) -> Option<(String, usize, usize)> {
    let (match_start, match_end) = find_ci(text, needle_lower)?;
    if text.len() <= max_chars {
        return Some((text.to_string(), match_start, match_end));
    }
    let match_len = match_end - match_start;
    let context = max_chars.saturating_sub(match_len) / 2;
    let mut win_start = match_start.saturating_sub(context);
    let mut win_end = (match_end + context).min(text.len());
    while win_start > 0 && !text.is_char_boundary(win_start) {
        win_start -= 1;
    }
    while win_end < text.len() && !text.is_char_boundary(win_end) {
        win_end += 1;
    }
    let prefix = if win_start > 0 { "…" } else { "" };
    let suffix = if win_end < text.len() { "…" } else { "" };
    let snippet = format!("{prefix}{}{suffix}", &text[win_start..win_end]);
    let new_start = prefix.len() + (match_start - win_start);
    let new_end = prefix.len() + (match_end - win_start);
    Some((snippet, new_start, new_end))
}

/// Backward, budget-capped line reader over an append-only line file
/// (`transcript.jsonl`). Generalizes [`Storage::read_transcript_tail`]'s
/// chunked seek-backward approach: instead of reading a fixed number of
/// trailing lines in one shot, it yields lines one at a time (newest
/// first) so a caller doing early-exit work (stop once N matches found)
/// never reads more of a multi-GB file than it needs to.
///
/// Reads 64 KiB chunks from the tail, prepending each to an internal
/// buffer. The buffer's leading line is uncertain (it may be a partial
/// line whose earlier bytes haven't been read yet) until the file start is
/// reached, so it's dropped from the confirmed set on every refill; the
/// confirmed suffix is stable across refills (prepending more history
/// can only complete that leading fragment into whole line(s), never
/// change the lines after it), which is what lets `yielded` keep counting
/// from the end without renumbering after each refill.
struct BackwardLineReader {
    file: std::fs::File,
    /// Bytes [0, cursor) of the file have not been read yet.
    cursor: u64,
    buf: Vec<u8>,
    /// How many lines (from the end of the current confirmed split of
    /// `buf`) have already been yielded.
    yielded: usize,
    bytes_read: u64,
}

impl BackwardLineReader {
    const CHUNK: u64 = 64 * 1024;

    fn open(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let file = std::fs::File::open(path)?;
        let len = file.metadata()?.len();
        if len == 0 {
            return Ok(None);
        }
        Ok(Some(Self {
            file,
            cursor: len,
            buf: Vec::new(),
            yielded: 0,
            bytes_read: 0,
        }))
    }

    /// Total bytes read from disk so far — the caller's signal for its own
    /// byte budget.
    fn bytes_read(&self) -> u64 {
        self.bytes_read
    }

    /// Read one more chunk from before `cursor`, prepending it to `buf`.
    /// Returns `false` when the start of the file has already been reached.
    fn refill(&mut self) -> Result<bool> {
        if self.cursor == 0 {
            return Ok(false);
        }
        use std::io::{Read, Seek, SeekFrom};
        let to_read = Self::CHUNK.min(self.cursor);
        self.cursor -= to_read;
        self.file.seek(SeekFrom::Start(self.cursor))?;
        let mut chunk = vec![0u8; to_read as usize];
        self.file.read_exact(&mut chunk)?;
        self.bytes_read += to_read;
        chunk.extend_from_slice(&self.buf);
        self.buf = chunk;
        Ok(true)
    }

    /// Pop the next line, newest first. `Ok(None)` once the start of the
    /// file has been reached and every line has been yielded.
    ///
    /// Recomputes the line split from `buf` on every call rather than
    /// caching it, so a chunk boundary that lands inside a multi-byte
    /// codepoint only ever corrupts the (always-dropped) leading line: any
    /// line at index >= 1 is built entirely from bytes that were already
    /// fully present in `buf` before this refill, so `from_utf8_lossy`
    /// decodes it correctly. Bounded by the caller's byte budget, so the
    /// repeated re-split is cheap in practice (at most `budget / CHUNK`
    /// refills, each re-splitting at most `budget` bytes).
    fn next_line(&mut self) -> Result<Option<String>> {
        loop {
            let text = String::from_utf8_lossy(&self.buf).into_owned();
            let mut lines: Vec<&str> = text.lines().collect();
            if self.cursor > 0 && !lines.is_empty() {
                // Leading line may still be partial — more file precedes it.
                lines.remove(0);
            }
            if self.yielded < lines.len() {
                let idx = lines.len() - 1 - self.yielded;
                self.yielded += 1;
                return Ok(Some(lines[idx].to_string()));
            }
            if !self.refill()? {
                return Ok(None);
            }
        }
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
        assert_eq!(
            apply_program_edits("", &[edit("", "first")]).unwrap(),
            "first"
        );
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

#[cfg(test)]
mod search_tests {
    use super::*;
    use agentd_protocol::{ApprovalMode, MessageRole, SessionEvent, SessionKind, SessionState};
    use chrono::{Duration as ChronoDuration, Utc};

    /// A minimal `SessionSummary` for search tests. `age_secs` controls
    /// both `created_at` and `last_event_at` (older == smaller `age_secs`
    /// is more recent), so callers can control recency ordering directly.
    fn make_summary(id: &str, title: Option<&str>, harness: &str, age_secs: i64) -> SessionSummary {
        let at = Utc::now() - ChronoDuration::seconds(age_secs);
        SessionSummary {
            id: id.to_string(),
            harness: harness.to_string(),
            cwd: "/tmp".into(),
            title: title.map(str::to_string),
            state: SessionState::Running,
            created_at: at,
            last_event_at: Some(at),
            cost_usd: None,
            model: None,
            worktree: None,
            pending_input: false,
            last_prompt: None,
            event_count: 0,
            has_pty: false,
            mode: None,
            pinned: false,
            position: 0,
            group_id: None,
            parent_session_id: None,
            last_pty_at_ms: None,
            approval_mode: ApprovalMode::Manual,
            kind: SessionKind::User,
            archived: false,
            operator_loop_disabled: false,
            needs_attention: false,
        }
    }

    fn message_event(seq: u64, text: &str) -> TimestampedEvent {
        TimestampedEvent {
            seq,
            at: Utc::now(),
            event: SessionEvent::Message {
                role: MessageRole::Assistant,
                text: text.to_string(),
            },
        }
    }

    fn search(storage: &Storage, sessions: &[SessionSummary], query: &str) -> SearchResult {
        storage
            .search(
                sessions,
                &SearchParams {
                    query: query.to_string(),
                    scopes: None,
                    session_ids: None,
                    limit: None,
                    per_session_limit: None,
                },
            )
            .unwrap()
    }

    fn search_scoped(
        storage: &Storage,
        sessions: &[SessionSummary],
        query: &str,
        scopes: Vec<SearchScope>,
    ) -> SearchResult {
        storage
            .search(
                sessions,
                &SearchParams {
                    query: query.to_string(),
                    scopes: Some(scopes),
                    session_ids: None,
                    limit: None,
                    per_session_limit: None,
                },
            )
            .unwrap()
    }

    #[test]
    fn empty_query_returns_empty_result() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();
        let sessions = vec![make_summary("s1", Some("hello world"), "shell", 0)];

        let result = search(&storage, &sessions, "   ");

        assert!(result.hits.is_empty());
        assert!(!result.truncated);
        assert_eq!(result.sessions_scanned, 0);
    }

    #[test]
    fn name_scope_matches_title_and_reports_offsets() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();
        let sessions = vec![
            make_summary("sabc1234567", Some("Fix the flaky test"), "claude", 0),
            make_summary("sxyz9999999", Some("unrelated session"), "shell", 10),
        ];

        let result = search_scoped(&storage, &sessions, "flaky", vec![SearchScope::Name]);

        assert_eq!(result.hits.len(), 1);
        let hit = &result.hits[0];
        assert_eq!(hit.session_id, "sabc1234567");
        assert_eq!(hit.scope, SearchScope::Name);
        assert_eq!(
            hit.snippet[hit.match_start..hit.match_end].to_lowercase(),
            "flaky"
        );
    }

    #[test]
    fn name_scope_matches_short_id_and_harness() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();
        let sessions = vec![make_summary("scodexsession12345", None, "codex", 0)];

        let by_id = search_scoped(&storage, &sessions, "codexsess", vec![SearchScope::Name]);
        assert_eq!(by_id.hits.len(), 1);

        let by_harness = search_scoped(&storage, &sessions, "codex", vec![SearchScope::Name]);
        assert_eq!(by_harness.hits.len(), 1);
    }

    #[test]
    fn session_ids_filter_restricts_the_scan() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();
        let sessions = vec![
            make_summary("s1", Some("needle here"), "shell", 0),
            make_summary("s2", Some("needle here too"), "shell", 5),
        ];

        let result = storage
            .search(
                &sessions,
                &SearchParams {
                    query: "needle".into(),
                    scopes: Some(vec![SearchScope::Name]),
                    session_ids: Some(vec!["s2".into()]),
                    limit: None,
                    per_session_limit: None,
                },
            )
            .unwrap();

        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].session_id, "s2");
        assert_eq!(result.sessions_scanned, 1);
    }

    #[test]
    fn program_scope_matches_lines_and_trims_long_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();
        let sessions = vec![make_summary("s1", None, "shell", 0)];
        storage.ensure_session_dir("s1").unwrap();
        let filler = "z".repeat(300);
        std::fs::write(
            storage.program_path("s1"),
            format!("# Title\n\n{filler} needle {filler}\n\nanother line\n"),
        )
        .unwrap();

        let result = search_scoped(&storage, &sessions, "needle", vec![SearchScope::Program]);

        assert_eq!(result.hits.len(), 1);
        let hit = &result.hits[0];
        assert_eq!(hit.scope, SearchScope::Program);
        assert!(hit.snippet.len() < filler.len());
        assert_eq!(
            hit.snippet[hit.match_start..hit.match_end].to_lowercase(),
            "needle"
        );
    }

    #[test]
    fn transcript_hits_are_newest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();
        let sessions = vec![make_summary("s1", None, "shell", 0)];
        // Fewer than the default per_session_limit (5) so this exercises
        // ordering without also tripping the limit-truncation path.
        for seq in 1..=3 {
            storage
                .append_event(
                    "s1",
                    &message_event(seq, &format!("needle occurrence {seq}")),
                )
                .unwrap();
        }

        let result = search_scoped(&storage, &sessions, "needle", vec![SearchScope::Transcript]);

        let seqs: Vec<u64> = result.hits.iter().map(|h| h.seq.unwrap()).collect();
        assert_eq!(seqs, vec![3, 2, 1]);
        assert!(!result.truncated);
    }

    #[test]
    fn transcript_per_session_limit_enforced_and_sets_truncated() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();
        let sessions = vec![make_summary("s1", None, "shell", 0)];
        for seq in 1..=10 {
            storage
                .append_event("s1", &message_event(seq, "needle"))
                .unwrap();
        }

        let result = storage
            .search(
                &sessions,
                &SearchParams {
                    query: "needle".into(),
                    scopes: Some(vec![SearchScope::Transcript]),
                    session_ids: None,
                    limit: None,
                    per_session_limit: Some(3),
                },
            )
            .unwrap();

        let seqs: Vec<u64> = result.hits.iter().map(|h| h.seq.unwrap()).collect();
        assert_eq!(seqs, vec![10, 9, 8]);
        assert!(result.truncated);
    }

    #[test]
    fn global_limit_enforced_and_sets_truncated() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();
        let sessions: Vec<SessionSummary> = (0..5)
            .map(|i| make_summary(&format!("s{i}"), Some("needle session"), "shell", i as i64))
            .collect();

        let result = storage
            .search(
                &sessions,
                &SearchParams {
                    query: "needle".into(),
                    scopes: Some(vec![SearchScope::Name]),
                    session_ids: None,
                    limit: Some(2),
                    per_session_limit: None,
                },
            )
            .unwrap();

        assert_eq!(result.hits.len(), 2);
        assert!(result.truncated);
        assert_eq!(result.sessions_scanned, 2);
    }

    #[test]
    fn transcript_byte_budget_truncates_long_transcript_and_sets_truncated() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();
        let sessions = vec![make_summary("s1", None, "shell", 0)];
        let filler = "x".repeat(2000);
        for seq in 1..=200u64 {
            storage
                .append_event("s1", &message_event(seq, &format!("{filler} needle {seq}")))
                .unwrap();
        }

        // A tiny per-session cap (well under the ~400 KiB transcript) forces
        // the scan to stop well before the start of the file.
        let result = storage
            .search_with_budgets(
                &sessions,
                &SearchParams {
                    query: "needle".into(),
                    scopes: Some(vec![SearchScope::Transcript]),
                    session_ids: None,
                    limit: None,
                    per_session_limit: Some(1000),
                },
                8 * 1024,
                64 * 1024 * 1024,
            )
            .unwrap();

        assert!(!result.hits.is_empty());
        assert!(result.hits.len() < 200);
        assert!(result.truncated);
        // Newest-first: the tail of the file is scanned first.
        assert_eq!(result.hits[0].seq, Some(200));
    }

    #[test]
    fn transcript_skips_hidden_and_non_searchable_events() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("data")).unwrap();
        let sessions = vec![make_summary("s1", None, "shell", 0)];
        // A raw PTY chunk mentioning "needle" must never surface as a hit —
        // `Pty` isn't a searchable event variant.
        storage
            .append_event(
                "s1",
                &TimestampedEvent {
                    seq: 1,
                    at: Utc::now(),
                    event: SessionEvent::Pty {
                        data: "needle".into(),
                    },
                },
            )
            .unwrap();
        // The legacy `tui` dispatch tool is `is_model_hidden` even though
        // `ToolUse` is otherwise a searchable variant.
        storage
            .append_event(
                "s1",
                &TimestampedEvent {
                    seq: 2,
                    at: Utc::now(),
                    event: SessionEvent::ToolUse {
                        tool: agentd_protocol::TUI_DISPATCH_TOOL.to_string(),
                        args: serde_json::json!({ "cmd": "needle" }),
                        call_id: None,
                    },
                },
            )
            .unwrap();
        storage
            .append_event("s1", &message_event(3, "needle for real"))
            .unwrap();

        let result = search_scoped(&storage, &sessions, "needle", vec![SearchScope::Transcript]);

        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].seq, Some(3));
    }

    #[test]
    fn build_snippet_centers_window_and_offsets_delimit_match() {
        let long = format!("{}{}{}", "a".repeat(300), "NEEDLE", "b".repeat(300));

        let (snippet, start, end) =
            build_snippet(&long, "needle", SEARCH_SNIPPET_MAX_CHARS).unwrap();

        assert!(snippet.starts_with('…'));
        assert!(snippet.ends_with('…'));
        assert_eq!(snippet[start..end].to_lowercase(), "needle");
    }

    #[test]
    fn build_snippet_returns_none_without_a_match() {
        assert!(build_snippet("no match here", "needle", SEARCH_SNIPPET_MAX_CHARS).is_none());
    }
}
