//! Program selection verbs (spec 0089): typed refinement actions on a
//! Program selection, loaded from markdown definition files. Built-ins ship
//! embedded here; user files under a `verbs/` config directory add to, or by
//! matching `name`, override them.

use construct_protocol::{ProgramVerb, ProgramVerbEffect, ProgramVerbInteraction};
use std::collections::BTreeMap;
use std::path::Path;

/// Built-in verb sources, adapted from the Contrarian, Simplifier, Seed
/// Architect, and Socratic Interviewer personas of Q00/ouroboros (MIT
/// licensed; see each file's body for attribution). Embedded so a fresh
/// install has a useful verb set with no configuration; each file is
/// otherwise an ordinary verb definition — a user file with the same `name`
/// replaces it.
const BUILT_INS: &[(&str, &str)] = &[
    (
        "challenge-assumptions",
        include_str!("program_verbs/challenge-assumptions.md"),
    ),
    ("simplify", include_str!("program_verbs/simplify.md")),
    ("crystallize", include_str!("program_verbs/crystallize.md")),
    ("interview", include_str!("program_verbs/interview.md")),
];

/// Parse one verb definition file (frontmatter + body) into a [`ProgramVerb`].
/// `None` means the file is missing a required field (`name`, `effect`, or
/// `interaction`) or has an unrecognized `effect`/`interaction` value — the
/// caller skips such files with a diagnostic rather than failing the whole
/// listing.
fn parse_verb_definition(raw: &str, built_in: bool) -> Option<ProgramVerb> {
    let (frontmatter, body) = split_frontmatter(raw);
    let fields = parse_frontmatter_fields(&frontmatter);
    let name = fields.get("name")?.clone();
    let effect = match fields.get("effect")?.as_str() {
        "annotate" => ProgramVerbEffect::Annotate,
        "rewrite" => ProgramVerbEffect::Rewrite,
        _ => return None,
    };
    let interaction = match fields.get("interaction")?.as_str() {
        "single-shot" => ProgramVerbInteraction::SingleShot,
        "interactive" => ProgramVerbInteraction::Interactive,
        _ => return None,
    };
    let label = fields.get("label").cloned().unwrap_or_else(|| name.clone());
    let order = fields
        .get("order")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);
    Some(ProgramVerb {
        name,
        label,
        description: fields.get("description").cloned(),
        effect,
        interaction,
        order,
        built_in,
        prompt: body.trim().to_string(),
    })
}

/// Strip a leading `---\n ... \n---\n` frontmatter block, returning its raw
/// text and the remaining body. No frontmatter block (or an unterminated
/// one) returns an empty frontmatter and the whole file as body. Mirrors the
/// daemon's widget-frontmatter parser (same hand-rolled `key: value` shape,
/// no YAML dependency).
fn split_frontmatter(raw: &str) -> (String, String) {
    let Some(rest) = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))
    else {
        return (String::new(), raw.to_string());
    };
    let mut byte_offset = raw.len().saturating_sub(rest.len());
    let mut frontmatter = String::new();
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        byte_offset += line.len();
        if trimmed == "---" {
            return (frontmatter, raw[byte_offset..].to_string());
        }
        frontmatter.push_str(line);
    }
    (String::new(), raw.to_string())
}

fn parse_frontmatter_fields(frontmatter: &str) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();
    for line in frontmatter.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().trim_matches(['"', '\'']);
        if !value.is_empty() {
            fields.insert(key.trim().to_string(), value.to_string());
        }
    }
    fields
}

/// Load every verb: built-ins first, then every `*.md` file in `dir` (if it
/// exists), with a user file's `name` replacing a built-in of the same name.
/// Malformed or unreadable files are skipped with a `tracing::warn!`, never a
/// hard failure — one broken user file must not take down the whole list.
pub fn load_verbs(dir: &Path) -> Vec<ProgramVerb> {
    let mut by_name: BTreeMap<String, ProgramVerb> = BTreeMap::new();
    for (name, raw) in BUILT_INS {
        match parse_verb_definition(raw, true) {
            Some(verb) => {
                by_name.insert(verb.name.clone(), verb);
            }
            None => tracing::warn!(verb = %name, "built-in program verb failed to parse"),
        }
    }
    if dir.exists() {
        match std::fs::read_dir(dir) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("md") {
                        continue;
                    }
                    let raw = match std::fs::read_to_string(&path) {
                        Ok(raw) => raw,
                        Err(e) => {
                            tracing::warn!(path = %path.display(), error = ?e, "skip unreadable program verb");
                            continue;
                        }
                    };
                    match parse_verb_definition(&raw, false) {
                        Some(verb) => {
                            by_name.insert(verb.name.clone(), verb);
                        }
                        None => {
                            tracing::warn!(path = %path.display(), "skip malformed program verb (missing name/effect/interaction)")
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(dir = %dir.display(), error = ?e, "read program verbs dir failed")
            }
        }
    }
    let mut verbs: Vec<ProgramVerb> = by_name.into_values().collect();
    verbs.sort_by(|a, b| a.order.cmp(&b.order).then_with(|| a.label.cmp(&b.label)));
    verbs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_ins_all_parse() {
        let verbs = load_verbs(Path::new("/nonexistent/verbs/dir/for/test"));
        let names: Vec<_> = verbs.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "challenge-assumptions",
                "simplify",
                "crystallize",
                "interview"
            ]
        );
        assert!(verbs.iter().all(|v| v.built_in));
        assert!(verbs.iter().all(|v| !v.prompt.trim().is_empty()));
    }

    #[test]
    fn user_file_overrides_built_in_by_name() {
        let dir = std::env::temp_dir().join(format!("agentd-verb-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("simplify.md"),
            "---\nname: simplify\nlabel: My Simplify\neffect: rewrite\ninteraction: single-shot\norder: 1\n---\n\nCustom body.\n",
        )
        .unwrap();
        let verbs = load_verbs(&dir);
        let simplify = verbs.iter().find(|v| v.name == "simplify").unwrap();
        assert_eq!(simplify.label, "My Simplify");
        assert!(!simplify.built_in);
        assert_eq!(verbs.len(), 4, "override replaces, does not duplicate");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn malformed_user_file_is_skipped_not_fatal() {
        let dir = std::env::temp_dir().join(format!("agentd-verb-test-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("broken.md"), "no frontmatter at all").unwrap();
        let verbs = load_verbs(&dir);
        assert_eq!(verbs.len(), 4, "broken file skipped, built-ins survive");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn new_user_verb_is_added_alongside_built_ins() {
        let dir = std::env::temp_dir().join(format!("agentd-verb-test-new-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("threat-model.md"),
            "---\nname: threat-model\nlabel: Threat model\neffect: annotate\ninteraction: single-shot\n---\n\nList abuse cases.\n",
        )
        .unwrap();
        let verbs = load_verbs(&dir);
        assert_eq!(verbs.len(), 5);
        assert!(verbs
            .iter()
            .any(|v| v.name == "threat-model" && !v.built_in));
        std::fs::remove_dir_all(&dir).ok();
    }
}
