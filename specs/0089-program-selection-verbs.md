# 0089-program-selection-verbs

Status: accepted
Date: 2026-07-12
Area: architecture
Scope: Typed refinement actions ("verbs") on a Program selection, executed as result-returning forked sessions and merged by the platform, with verb definitions loaded from markdown files.

## Decision

The Program selection context menu offers, alongside Run, a set of **verbs**: typed refinement actions that operate on the selected region of the Program document. Run executes a selection as orchestration; verbs improve the document itself. The two families share the selection/scoping machinery but have different effects, and future changes must keep that distinction legible in the UI.

### Verb execution model

A verb invocation follows one lifecycle:

1. **Spawn.** The daemon forks the Program-owning session into a new sibling session whose first message is the verb's purpose prompt. The forked session runs the same harness as the owning session; when that harness supports native fork-resume, the verb session inherits the owning session's actual conversation state (real model memory), not merely the Program document — see "Fork, not a fresh child" below. It does **not** receive Program write capability; its jurisdiction over the selection is enforced by construction, not by prompt.
2. **Interact (optional).** Verbs declare an interaction policy. A `single-shot` verb runs to completion unattended. An `interactive` verb may hold a dialogue with the user inside its own session; the user reaches it through the session clip annotated into the Program (see below). Awaiting-input status flows to the orchestrator through the existing fleet-observation channel.
3. **Return.** The verb session's deliverable is a structured result, not an edit: the intended effect (`annotate` or `rewrite`), an anchor identifying the selected region (block identity and/or the exact prior text), and the exact replacement or annotation markdown. The result is applied verbatim; downstream steps never paraphrase it.
4. **Merge.** Merging is tiered. When the returned anchor still matches the current document, the platform applies the edit mechanically — no model round-trip — under the Program's existing version-conflict protection. When the document has drifted (the user edited the selection while the verb ran), the merge escalates to the Program-owning session, which re-reads the document and applies the intent with an anchored edit. Merges are serialized through a single writer so concurrent verb results cannot race each other.
5. **Retire.** After a successful merge, or once an abandoned verb is recognized as such, the verb session is soft-archived: its transcript and session clip remain resolvable, but it leaves the active session list.

While a verb is in flight, the selection's blocks carry the same in-progress affordance as a selection Run (shimmer over the selected block identities), and the selection is annotated with the verb session's own session clip so provenance and interaction access live in the document.

### Fork, not a fresh child

A verb session is a **visible top-level sibling** of the Program-owning session, created the same way `construct_fork_session` creates any fork — not a hidden `Subagent`-kind child. This is a deliberate choice over the alternative of a bare, context-free spawn:

- When the owning session's harness has a native fork-resume primitive (as of this writing: claude, codex, grok), the verb session's underlying harness process resumes the owning session's actual conversation — the model's own memory, not a summary or a copy. Harnesses without a native primitive (smith, shell, antigravity, and any cross-harness case) get an ordinary fresh start; the verb's prompt always carries the full Program document as context regardless (bounded in size, with a live read tool as fallback for anything truncated or for a document that changed since), so there is no *hard* dependency on native fork for a verb to be useful.
- Either way, the verb's own purpose prompt is still the first new instruction the session receives: a resumed conversation remembers the past but still needs to be told what to do *now*.
- Forking (over a bare spawn with no lineage) was chosen specifically so a harness capable of native resume can give the verb everything the owning session already knows — prior decisions, files read, established conventions — not just the document text.
- Visibility is an accepted tradeoff: the verb session appears in the fleet like any other session while it runs, grouped alongside the Program-owning session. It is retired (soft-archived) the same as before once its result merges or it is abandoned, so it does not linger once its job is done.

### Optimistic and confirmed shimmer

Shimmer on a verb's selected block(s) requires an active `ProgramRunProgress` entry to exist for the Program-owning session before any per-block shimmer declaration can take effect — a declaration made against a session with no active run is silently dropped, not a fallback error. Verb execution must seed this the same way selection Run does, before applying its own shimmer declaration. Clients give the same optimistic, pre-round-trip shimmer treatment to a verb dispatch that they give to a selection Run: the client marks the affected block(s) pending locally the instant the verb is dispatched, before the daemon round-trip completes, unioned with (not clobbering) any run already active on that session.

### Verb effects

- **`annotate`** inserts new content adjacent to the selection (typically directly below it) and leaves the selected text untouched.
- **`rewrite`** replaces the selected markdown with the returned content.

Both effects must preserve session/harness clips that appear inside the selection unless removing them is the explicit purpose of the verb invocation; a rewrite that silently destroys dispatch provenance is a defect.

The free-text instruction field defined for selection Run composes with verbs: a verb plus a non-empty instruction appends the instruction to the verb's purpose prompt. It never replaces the verb.

### Verb definitions are data

A verb is defined entirely by a markdown file:

- **Frontmatter** declares the verb's identity and policies: a stable kebab-case `name`, a short human `label` for menu display, the `effect` (`annotate` | `rewrite`), the `interaction` policy (`single-shot` | `interactive`), a one-line `description`, and an optional `order` hint for menu sorting.
- **Body** is the purpose prompt handed to the verb session, together with the standing contract text (selection jurisdiction, clip preservation, structured-return format).

Built-in verbs ship embedded in the daemon. Users add or override verbs by placing definition files in a `verbs/` directory under the construct configuration directory; a user file with the same `name` as a built-in replaces it. Adding a verb requires no code change in any client: clients render the selection menu from the daemon's advertised verb list, so a new definition file appears in every client's menu. Malformed definition files are skipped with a diagnostic, never a crash.

Verbs render in every client that has a Program selection menu (TUI and web UI alike). The instruction *field* remains TUI-only per 0076; verb buttons are cross-client.

### Built-in v1 verbs

Four built-ins ship, adapted from the persona prompts of the MIT-licensed Ouroboros project (Q00/ouroboros), with attribution in the definition files:

- **`challenge-assumptions`** (annotate, single-shot) — the Contrarian: surface the 2–3 most load-bearing implicit assumptions in the selection, state what breaks if each is wrong, question whether the right problem is being solved, and present "do nothing" as a considered alternative.
- **`simplify`** (rewrite, single-shot) — the Simplifier: catalog what the selection commits to, challenge each element ("what breaks if removed?"), and rewrite to the minimum that preserves the core intent.
- **`crystallize`** (rewrite, single-shot) — the Seed Architect: rewrite loose prose into a structured section — goal, constraints, and 3–7 acceptance criteria, each an independently valuable user-visible outcome (never an implementation sub-step of a sibling), with a one-line verification command where one exists.
- **`interview`** (annotate, interactive) — the Socratic Interviewer: questions only, one focused question per turn, targeting the largest unresolved ambiguity across goal, constraints, and success criteria; keeps breadth across multiple ambiguity tracks; ends when scope, non-goals, and verification expectations are explicit or the user signals enough. Its returned annotation is a digest of decisions extracted from the dialogue, suitable as input to a subsequent `crystallize`.

## Reason

The Program document is orchestration state; its quality gates everything dispatched from it. Ouroboros demonstrates that spec-refinement moves (challenge, simplify, crystallize, interview) are effective when encoded as small single-purpose persona prompts. Mapping each verb to a result-returning session fits this system's existing grain: sessions already give interactive verbs a place to hold a dialogue, the document already annotates dispatched work with session clips, and forking is the system's existing primitive for "continue this elsewhere with everything you already know."

Keeping write capability out of the verb session makes selection jurisdiction a guarantee instead of a convention, prevents a confused agent from clobbering concurrent human edits, and collapses the concurrent-verbs problem into single-writer merge serialization. The tiered merge keeps the common case (document unchanged) free of any model round-trip, consistent with the mechanical-fast-path philosophy of 0066.

Verb definitions as markdown files keep the surface extensible without client releases: the useful verb set is expected to grow and to be personal, and each verb is precisely a prompt plus a small policy tuple — data, not code.

## Consequences

- Clients must render the verb menu from the daemon's advertised list rather than hardcoding entries; adding a definition file must be sufficient to surface a verb everywhere.
- The structured-return contract (effect, anchor, exact content) is a compatibility surface between verb sessions and the merge path; changes to it must keep existing verb definitions working.
- Verb results are applied verbatim. Any future step that summarizes, reformats, or "improves" a returned rewrite before merging violates this spec.
- Mechanical merge must remain subordinate to the Program's version-conflict protection; escalation to the Program-owning session is the only sanctioned response to anchor drift, and merges stay single-writer.
- Overlapping in-flight verbs on intersecting selections are permitted but must be surfaced to the user (at minimum a warning at spawn time); later results merge against the post-merge document, and escalation handles the drift.
- Verb sessions that die or are cancelled before returning must clear the in-progress affordance and leave the document untouched.
- Soft-archived verb sessions must keep their clips resolvable, since the clip in the document is the durable record of the interaction.
- A verb session is `SessionKind::User` with `parent_session_id: None`, matching every other fork — never `SessionKind::Subagent` with `forked_from` also set. That combination is deliberately avoided: the lineage tree and the session-reorder logic disagree on which relationship takes precedence when both are set on the same session, so it is not a supported shape.

## Non-Goals

- Hard, capability-level enforcement that a merge touches only the selected blocks (v1 relies on the structured return plus mechanical anchoring).
- A quantitative ambiguity-scoring pipeline or score-gated Run (a possible later layer; the interview verb self-assesses informally).
- Multi-persona debate panels (Ouroboros "unstuck") and evaluation/QA verbs over Run results — future verb candidates, not v1.
- Changing Run itself; the execute family and 0066 fast-path semantics are untouched.
- Web UI parity for the free-text instruction field (unchanged from 0076).
- A remote marketplace or packaging format for verb definitions beyond local files.

## Examples

A user selects a loose paragraph describing a deploy plan and picks **Crystallize**. The blocks shimmer immediately (client-optimistic, then daemon-confirmed); a new visible session — forked from the Program-owning session, with its native conversation if the harness supports it — appears with a session clip by the selection. It returns a rewrite containing a goal line, two constraints, and four acceptance criteria; the document hasn't changed meanwhile, so the daemon applies it mechanically and the verb session is archived. The clip remains as provenance.

A user picks **Interview** on a vague feature section, then keeps editing elsewhere. The interview session asks one question at a time; the user enters it through the clip and answers over ten minutes. The returned decision digest is annotated under the (meanwhile edited) section — the anchor drifted, so the Program-owning session places it with an anchored edit rather than the mechanical path.

A user drops `verbs/threat-model.md` in the construct config directory with `effect: annotate`, `interaction: single-shot`, and a purpose prompt asking for abuse cases. On the next selection, every client's menu shows **Threat model** with no client or daemon code change.

An example definition file:

```markdown
---
name: challenge-assumptions
label: Challenge assumptions
description: Surface load-bearing assumptions in the selection and what breaks if they're wrong.
effect: annotate
interaction: single-shot
order: 10
---

You are the Contrarian. The selected region of a planning document is your
entire jurisdiction. List the implicit assumptions it rests on; pick the 2–3
most load-bearing; for each, state what breaks if it is wrong. Question
whether the right problem is being solved, and consider "do nothing" as a
real alternative. Be respectful but relentless, and keep the annotation
short enough to read in one sitting.
```
