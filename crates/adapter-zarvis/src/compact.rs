//! Conversation compaction — replace older turns with an LLM-written
//! summary instead of silently dropping them when the rolling window
//! prunes.
//!
//! The agent loop carries a `Vec<Message>` of every user / assistant /
//! tool turn. The vanilla [`context::prune_to_budget`] path discards
//! the oldest turn pairs when the estimate exceeds the model's input
//! budget — fast, but lossy. Compaction is the same shape with one
//! extra step: before dropping, ask the model to write a structured
//! summary of the history we're about to lose, then prepend a synthetic
//! `Content::Summary` user turn at the head so subsequent prompts still
//! have the context.
//!
//! Two trigger paths:
//!
//!   * **Manual** — the user runs `/compact [N]` in interactive mode.
//!     `N` defaults to [`DEFAULT_KEEP_PAIRS`] and is the number of
//!     recent turn pairs to preserve verbatim.
//!   * **Auto** — the agent loop calls [`maybe_auto_compact`] each step
//!     before [`context::prune_to_budget`]. It fires when the estimated
//!     token count crosses [`AUTO_COMPACT_RATIO`] of the effective cap.
//!
//! Both paths share [`compact`] as the actual primitive. Failures
//! (provider error, no cut-point, summary itself too big after
//! halving) leave `messages` unchanged so the caller can fall through
//! to the existing prune path — compaction is strictly best-effort.
//!
//! ## Tool-call boundary
//!
//! The cut point *must* land at a User-role message boundary, never
//! between an `AssistantToolCalls` and its matching `ToolResult`(s).
//! Both Anthropic and OpenAI 400 on orphan `tool_call_id`s, so dropping
//! a half-finished tool exchange would break the next turn. Helper
//! [`cut_index_keeping_last_n_pairs`] enforces this invariant.

use crate::context;
use crate::provider::{Content, LlmProvider, Message, Role, TextSink, ToolSpec};
use anyhow::{anyhow, Result};

/// Env var to disable auto-compaction (manual `/compact` is always
/// available). Auto is on by default — set this to `0`, `false`, or
/// `off` to fall back to pure rolling-prune behavior. Mainly an escape
/// hatch for users hitting unexpected summarizer-call costs.
const ENV_AUTO_COMPACT: &str = "AGENTD_ZARVIS_AUTO_COMPACT";

/// Whether auto-compact should run this session. Default on.
pub fn auto_compact_enabled() -> bool {
    match std::env::var(ENV_AUTO_COMPACT).ok().as_deref() {
        Some("0") | Some("false") | Some("off") | Some("no") => false,
        _ => true,
    }
}

/// How many recent turn pairs `/compact` keeps verbatim when no `N`
/// arg is supplied. Empirically: 4 covers "the immediate task at hand
/// plus a beat of preceding context" without leaving so much that the
/// compact barely helps.
pub const DEFAULT_KEEP_PAIRS: usize = 4;

/// Auto-compact threshold as a fraction of the effective per-model
/// input-token cap. 0.95 keeps auto-compaction as a near-overflow
/// backstop: short conversations that briefly poke above the
/// rolling-prune utilization (0.7) don't pay the summarizer-call
/// cost; only conversations that have grown long enough to brush
/// against the actual model ceiling get summarized. The existing
/// `context::prune_to_budget` still runs each step against the 0.7
/// utilization, so we keep losing tail-end detail on long chats —
/// auto-compact just intervenes before the model itself starts
/// rejecting requests.
pub const AUTO_COMPACT_RATIO: f64 = 0.95;

/// Maximum recursion depth when summarizing a head that's itself too
/// large for one provider call. Each level halves the input. Depth 3
/// covers 8x the per-call budget, which is more than any realistic
/// session.
const MAX_SUMMARIZE_DEPTH: u32 = 3;

/// Hard cap on how many chars of head we feed the summarizer in a
/// single call before halving. Conservative — many providers can take
/// more, but going low keeps the summarizer call itself cheap and
/// dodges per-provider input limits we haven't learned yet.
const SUMMARIZE_CHAR_BUDGET: usize = 60_000;

/// Prompt the summarizer model runs under. Kept terse — the model is
/// the same one driving the chat, which already knows its job. The
/// list shape is load-bearing: the synthesizing turns that read this
/// back rely on the section order to know where to look for "what was
/// the user trying to do."
const SUMMARIZER_SYSTEM: &str = r#"You are condensing an earlier portion of an agent conversation into a compact reference summary. The summary REPLACES the original messages in the rolling context — write it so the agent can resume cleanly from it alone.

Preserve, in this order:
1. The user's overall goal(s) and any constraints they stated.
2. Decisions or plans agreed upon (with reasons when given).
3. Files / paths touched, each with a one-line note on what changed or was learned.
4. Tool actions taken and their outcomes — group similar calls; note failures explicitly.
5. Open questions, blockers, or pending work the agent has not yet done.

Be terse. Use short headings (e.g. "Goal:", "Decisions:", "Files:", "Tools:", "Pending:"). No prose intro. No closing summary. Bullets over paragraphs. Do not invent facts not present in the source."#;

/// Outcome of a [`compact`] call. Returned so the caller can emit the
/// `ContextCompacted` event with concrete numbers.
#[derive(Debug, Clone)]
pub struct CompactOutcome {
    /// Number of turn pairs collapsed into the new summary.
    pub dropped_turn_pairs: u32,
    /// Number of turn pairs kept verbatim after the summary.
    pub kept_turn_pairs: u32,
    /// Token estimate before compaction.
    pub tokens_before: u64,
    /// Token estimate after compaction.
    pub tokens_after: u64,
    /// First ~160 chars of the summary text, for the TUI banner.
    pub summary_preview: String,
}

/// Run a compaction pass on `messages` in place. Returns
/// `Ok(Some(outcome))` when the conversation was actually compacted,
/// `Ok(None)` when there wasn't enough history to compact safely (no
/// cut point at the requested keep-count) — in that case `messages` is
/// untouched. Errors propagate from the provider call.
pub async fn compact(
    messages: &mut Vec<Message>,
    keep_pairs: usize,
    provider: &dyn LlmProvider,
    model: &str,
) -> Result<Option<CompactOutcome>> {
    let cut = match cut_index_keeping_last_n_pairs(messages, keep_pairs) {
        Some(i) => i,
        None => return Ok(None),
    };
    if cut == 0 {
        return Ok(None);
    }
    let tokens_before = context::estimate_tokens(messages) as u64;
    let dropped_turn_pairs = count_turn_pairs(&messages[..cut]);
    let kept_turn_pairs = count_turn_pairs(&messages[cut..]);
    if dropped_turn_pairs == 0 {
        return Ok(None);
    }

    let head: Vec<Message> = messages[..cut].to_vec();
    let summary_text = summarize_head(&head, provider, model, 0).await?;

    let summary_msg = Message {
        role: Role::User,
        content: Content::Summary {
            text: summary_text.clone(),
            dropped_turn_pairs: dropped_turn_pairs as u32,
        },
    };
    let mut new_messages: Vec<Message> = Vec::with_capacity(messages.len() - cut + 1);
    new_messages.push(summary_msg);
    new_messages.extend(messages.drain(cut..));
    *messages = new_messages;

    let tokens_after = context::estimate_tokens(messages) as u64;
    let summary_preview: String = summary_text.chars().take(160).collect();
    Ok(Some(CompactOutcome {
        dropped_turn_pairs: dropped_turn_pairs as u32,
        kept_turn_pairs: kept_turn_pairs as u32,
        tokens_before,
        tokens_after,
        summary_preview,
    }))
}

/// Auto-compact entrypoint used by the agent loops. Returns
/// `Ok(Some(outcome))` when a compaction ran, `Ok(None)` when it
/// wasn't needed (below threshold) or wasn't possible (not enough
/// history). Errors are non-fatal — the caller logs and proceeds to
/// the rolling-prune path.
///
/// `effective_cap` is the per-model input-token cap the caller is
/// using for budget math (learned limit or hardcoded default). The
/// trigger is `est_tokens > AUTO_COMPACT_RATIO * effective_cap`.
pub async fn maybe_auto_compact(
    messages: &mut Vec<Message>,
    effective_cap: u64,
    provider: &dyn LlmProvider,
    model: &str,
) -> Result<Option<CompactOutcome>> {
    let est = context::estimate_tokens(messages) as u64;
    let trigger = ((effective_cap as f64) * AUTO_COMPACT_RATIO) as u64;
    if est < trigger {
        return Ok(None);
    }
    compact(messages, DEFAULT_KEEP_PAIRS, provider, model).await
}

/// Return the slice-index `cut` such that `messages[cut..]` contains
/// the last `keep_pairs` user-led turn pairs and starts on a User
/// message. Returns `None` when there aren't that many user messages
/// (or the head leading up to the cut is empty / nothing-to-compact).
///
/// "Turn pair" = one User message + everything until the next User
/// message (or end of vec). A Summary at the head counts toward the
/// kept tail since it's already a compacted user turn — re-summarizing
/// it would be a waste.
pub fn cut_index_keeping_last_n_pairs(messages: &[Message], keep_pairs: usize) -> Option<usize> {
    if keep_pairs == 0 {
        return Some(messages.len());
    }
    let mut user_idx: Vec<usize> = Vec::new();
    for (i, m) in messages.iter().enumerate() {
        if matches!(m.role, Role::User) {
            user_idx.push(i);
        }
    }
    if user_idx.len() <= keep_pairs {
        return None;
    }
    let cut = user_idx[user_idx.len() - keep_pairs];
    if cut == 0 {
        return None;
    }
    // If everything before the cut is already a single Summary, don't
    // re-compact — degenerate "compact the compact" cycle.
    if cut == 1 {
        if let Some(first) = messages.first() {
            if matches!(first.content, Content::Summary { .. }) {
                return None;
            }
        }
    }
    Some(cut)
}

/// Count user-led turn pairs in `slice`. A leading Summary counts as
/// one pair (it stands in for prior pairs).
fn count_turn_pairs(slice: &[Message]) -> usize {
    slice
        .iter()
        .filter(|m| matches!(m.role, Role::User))
        .count()
}

/// Render the head into a single user-prompt blob the summarizer model
/// consumes. We don't replay the head as actual chat turns because we
/// don't want the model to think it's still in that conversation — we
/// want it to *summarize*.
fn render_head_for_summarizer(head: &[Message]) -> String {
    let mut out = String::new();
    out.push_str(
        "Here is the conversation segment to condense. Each entry is tagged with its role.\n\n",
    );
    for m in head {
        match (&m.role, &m.content) {
            (Role::User, Content::Text { text }) => {
                out.push_str("USER: ");
                out.push_str(text.trim());
                out.push_str("\n\n");
            }
            (Role::Assistant, Content::Text { text }) => {
                out.push_str("ASSISTANT: ");
                out.push_str(text.trim());
                out.push_str("\n\n");
            }
            (_, Content::AssistantToolCalls { text, calls }) => {
                if let Some(t) = text {
                    if !t.is_empty() {
                        out.push_str("ASSISTANT: ");
                        out.push_str(t.trim());
                        out.push_str("\n");
                    }
                }
                for c in calls {
                    let args_str = serde_json::to_string(&c.input).unwrap_or_else(|_| "{}".into());
                    // Truncate args summary aggressively — the
                    // summarizer doesn't need full JSON, just the gist.
                    let args_short: String = args_str.chars().take(240).collect();
                    out.push_str(&format!(
                        "ASSISTANT_TOOL_CALL {} args={}\n",
                        c.name, args_short
                    ));
                }
                out.push('\n');
            }
            (
                _,
                Content::ToolResult {
                    output, is_error, ..
                },
            ) => {
                let preview: String = output.chars().take(400).collect();
                let label = if *is_error {
                    "TOOL_RESULT(error)"
                } else {
                    "TOOL_RESULT"
                };
                out.push_str(&format!("{}: {}\n\n", label, preview));
            }
            (_, Content::Summary { text, .. }) => {
                // A pre-existing summary in the head means the user
                // compacted earlier — fold its content in by passing
                // the text through verbatim so the new summary inherits
                // the older context.
                out.push_str("PRIOR_SUMMARY: ");
                out.push_str(text.trim());
                out.push_str("\n\n");
            }
            // System/Tool Text messages are not expected in practice
            // (System lives in the system prompt, Tool only carries
            // ToolResult), but the type system needs all combinations.
            (Role::System | Role::Tool, Content::Text { text }) => {
                out.push_str("OTHER: ");
                out.push_str(text.trim());
                out.push_str("\n\n");
            }
        }
    }
    out
}

/// Summarize `head` via one (or, on overflow, several recursive)
/// provider calls. Returns the summary text. Bails with an error when
/// recursion exceeds [`MAX_SUMMARIZE_DEPTH`] without producing a
/// short-enough draft.
async fn summarize_head(
    head: &[Message],
    provider: &dyn LlmProvider,
    model: &str,
    depth: u32,
) -> Result<String> {
    if depth > MAX_SUMMARIZE_DEPTH {
        return Err(anyhow!(
            "compact: summarizer recursion exceeded {MAX_SUMMARIZE_DEPTH} levels"
        ));
    }
    let rendered = render_head_for_summarizer(head);
    if rendered.len() > SUMMARIZE_CHAR_BUDGET && head.len() > 1 {
        // Halve and recurse: summarize each half, then summarize the
        // two summaries. This keeps any single provider call under our
        // own budget, regardless of how big the head is.
        let mid = head.len() / 2;
        let (left, right) = head.split_at(mid);
        let s_left = Box::pin(summarize_head(left, provider, model, depth + 1)).await?;
        let s_right = Box::pin(summarize_head(right, provider, model, depth + 1)).await?;
        let combined = vec![
            Message {
                role: Role::User,
                content: Content::Summary {
                    text: s_left,
                    dropped_turn_pairs: count_turn_pairs(left) as u32,
                },
            },
            Message {
                role: Role::User,
                content: Content::Summary {
                    text: s_right,
                    dropped_turn_pairs: count_turn_pairs(right) as u32,
                },
            },
        ];
        return Box::pin(summarize_head(&combined, provider, model, depth + 1)).await;
    }

    let user_msg = Message {
        role: Role::User,
        content: Content::Text { text: rendered },
    };
    let mut sink = NoopSink;
    let turn = provider
        .complete(
            model,
            SUMMARIZER_SYSTEM,
            &[user_msg],
            &[] as &[ToolSpec],
            &mut sink,
        )
        .await?;
    let text = turn.text.unwrap_or_default();
    if text.trim().is_empty() {
        return Err(anyhow!("compact: summarizer returned empty text"));
    }
    Ok(text)
}

/// Discards streamed text — we want the final summary as one blob, not
/// streamed to the user.
struct NoopSink;
impl TextSink for NoopSink {
    fn delta(&mut self, _text: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{
        Content, LlmProvider, Message, ProviderTurn, Role, StopReason, ToolCall, ToolSpec, Usage,
    };
    use async_trait::async_trait;
    use std::sync::Mutex;

    fn user(s: &str) -> Message {
        Message {
            role: Role::User,
            content: Content::Text { text: s.into() },
        }
    }
    fn asst(s: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: Content::Text { text: s.into() },
        }
    }
    fn tool_calls(call_id: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: Content::AssistantToolCalls {
                text: None,
                calls: vec![ToolCall {
                    id: call_id.into(),
                    name: "shell".into(),
                    input: serde_json::json!({}),
                }],
            },
        }
    }
    fn tool_result(call_id: &str, output: &str) -> Message {
        Message {
            role: Role::Tool,
            content: Content::ToolResult {
                call_id: call_id.into(),
                output: output.into(),
                is_error: false,
            },
        }
    }

    #[test]
    fn cut_never_lands_inside_tool_pair() {
        let messages = vec![
            user("hi"),
            asst("hello"),
            user("run a tool"),
            tool_calls("c1"),
            tool_result("c1", "result1"),
            asst("did it"),
            user("recent"),
            asst("ok"),
        ];
        // keep_pairs = 1 → keep just the last User pair ("recent" + "ok").
        let cut = cut_index_keeping_last_n_pairs(&messages, 1).unwrap();
        // Must point at "recent" (index 6), not inside the tool exchange.
        assert!(matches!(messages[cut].role, Role::User));
        let kept: Vec<_> = messages[cut..].iter().collect();
        assert!(matches!(kept[0].content, Content::Text { ref text } if text == "recent"));
    }

    #[test]
    fn returns_none_when_not_enough_pairs() {
        let messages = vec![user("hi"), asst("hello")];
        assert!(cut_index_keeping_last_n_pairs(&messages, 4).is_none());
    }

    #[test]
    fn returns_none_when_only_summary_in_head() {
        let messages = vec![
            Message {
                role: Role::User,
                content: Content::Summary {
                    text: "prior".into(),
                    dropped_turn_pairs: 3,
                },
            },
            user("recent"),
            asst("ok"),
            user("more"),
            asst("ok2"),
        ];
        // Keep 2 → cut would be at index 1, which is the User "recent"
        // boundary. But everything before it is a single Summary — we
        // should refuse to re-compact.
        assert!(cut_index_keeping_last_n_pairs(&messages, 2).is_none());
    }

    #[test]
    fn empty_messages_returns_none() {
        let v: Vec<Message> = vec![];
        assert!(cut_index_keeping_last_n_pairs(&v, 1).is_none());
    }

    /// Stub provider that returns a fixed summary string. Records the
    /// number of completion calls so tests can assert the recursion
    /// path didn't run unexpectedly.
    struct StubProvider {
        summary: String,
        call_count: Mutex<usize>,
    }
    impl StubProvider {
        fn new(s: &str) -> Self {
            Self {
                summary: s.into(),
                call_count: Mutex::new(0),
            }
        }
    }
    #[async_trait]
    impl LlmProvider for StubProvider {
        fn name(&self) -> &str {
            "stub"
        }
        async fn complete(
            &self,
            _model: &str,
            _system: &str,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _sink: &mut dyn TextSink,
        ) -> anyhow::Result<ProviderTurn> {
            *self.call_count.lock().unwrap() += 1;
            Ok(ProviderTurn {
                text: Some(self.summary.clone()),
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            })
        }
    }

    #[tokio::test]
    async fn compact_collapses_head_into_summary() {
        // Use bulky messages so post-compact token estimate is
        // unambiguously smaller — fluff covers the synthetic
        // bookkeeping the summary adds (wire prefix + body).
        let bulk = "x".repeat(400);
        let mut messages = vec![
            user(&format!("first goal: {bulk}")),
            asst(&bulk),
            user(&format!("second step: {bulk}")),
            tool_calls("c1"),
            tool_result("c1", &bulk),
            asst(&bulk),
            user("third"),
            asst("ok"),
            user("recent"),
            asst("latest"),
        ];
        let provider = StubProvider::new("Goal: do things\nPending: nothing");
        let outcome = compact(&mut messages, 1, &provider, "stub-model")
            .await
            .unwrap()
            .expect("should compact");
        // After compact: messages = [Summary, user("recent"), asst("latest")].
        assert_eq!(messages.len(), 3);
        assert!(matches!(messages[0].content, Content::Summary { .. }));
        assert!(matches!(
            messages[1].content,
            Content::Text { ref text } if text == "recent"
        ));
        // Head before cut has 3 User messages (the 4th "recent" is the
        // kept boundary). Tool/Assistant rows don't count as pairs.
        assert_eq!(outcome.dropped_turn_pairs, 3);
        assert_eq!(outcome.kept_turn_pairs, 1);
        assert!(outcome.tokens_after < outcome.tokens_before);
        assert!(outcome.summary_preview.starts_with("Goal:"));
        // One provider call — head was small, no recursion.
        assert_eq!(*provider.call_count.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn compact_idempotent_on_already_compacted_minimal() {
        let mut messages = vec![
            Message {
                role: Role::User,
                content: Content::Summary {
                    text: "prior summary".into(),
                    dropped_turn_pairs: 3,
                },
            },
            user("recent"),
            asst("ok"),
        ];
        let provider = StubProvider::new("ignored");
        let outcome = compact(&mut messages, 1, &provider, "stub-model")
            .await
            .unwrap();
        assert!(outcome.is_none());
        // Provider never called.
        assert_eq!(*provider.call_count.lock().unwrap(), 0);
        assert_eq!(messages.len(), 3);
    }

    #[tokio::test]
    async fn recursive_halving_fires_on_huge_head() {
        // Build a head large enough to trip SUMMARIZE_CHAR_BUDGET so
        // we exercise the recursion path. 60K char threshold; each
        // entry adds ~10 chars of framing, so 8000 messages with
        // 20-char bodies → ~240K chars rendered. Plenty.
        let mut messages = Vec::new();
        for i in 0..8000 {
            messages.push(user(&format!("u{i}-xxxxxxxxxxxxx")));
            messages.push(asst(&format!("a{i}-xxxxxxxxxxxxx")));
        }
        messages.push(user("recent"));
        messages.push(asst("latest"));
        let provider = StubProvider::new("short");
        let outcome = compact(&mut messages, 1, &provider, "stub-model")
            .await
            .unwrap()
            .expect("should compact");
        assert!(outcome.dropped_turn_pairs >= 8000);
        // Recursion fired: > 1 provider call.
        assert!(*provider.call_count.lock().unwrap() > 1);
    }

    #[tokio::test]
    async fn auto_compact_skips_under_threshold() {
        let mut messages = vec![user("hi"), asst("hello"), user("again"), asst("ok")];
        let provider = StubProvider::new("unused");
        // cap = 100k tokens; conversation is ~10 tokens; way under
        // threshold.
        let outcome = maybe_auto_compact(&mut messages, 100_000, &provider, "stub")
            .await
            .unwrap();
        assert!(outcome.is_none());
        assert_eq!(*provider.call_count.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn auto_compact_fires_over_threshold() {
        // Build a conversation big enough to cross the threshold for a
        // small effective_cap. cap=1000 → trigger=750 tokens ≈ 2625
        // chars.
        let mut messages = Vec::new();
        let bulk = "x".repeat(500);
        for _ in 0..10 {
            messages.push(user(&bulk));
            messages.push(asst(&bulk));
        }
        messages.push(user("recent"));
        messages.push(asst("latest"));
        let provider = StubProvider::new("auto-summary");
        let outcome = maybe_auto_compact(&mut messages, 1000, &provider, "stub")
            .await
            .unwrap()
            .expect("should auto-compact");
        assert!(outcome.dropped_turn_pairs > 0);
    }
}
