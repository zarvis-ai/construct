//! Rolling-window context manager.
//!
//! Estimates token count with a coarse `chars / 3.5` heuristic + a
//! safety margin, then prunes complete turn pairs (user → assistant →
//! tool exchanges between them) from the oldest end when the budget
//! is exceeded. The system prompt is owned by the caller and not
//! included here; we always keep the most-recent N turns.
//!
//! Approximate by design. v2 can swap in a real tokenizer (`tiktoken`,
//! `tokenizers`) and provider-native prompt caching.

use crate::provider::{Content, Message, Role};

/// Token budget per provider/model. Returned as a soft cap — we prune
/// when the estimated total exceeds `cap * UTILIZATION`. Numbers are
/// the *input* context windows the providers advertise at the API
/// tier, updated for the 2026 model line-ups.
///
/// Notes:
///   * OpenAI gpt-5 family: 400K input tokens (output is a separate
///     128K budget, not counted here).
///   * OpenAI o-series (o1/o3/o4): 200K input.
///   * Anthropic Claude 4.x Sonnet has a 1M-context tier available
///     *only* with the `anthropic-beta: context-1m-2025-08-07`
///     header. Without that header it's 200K — and the current
///     `provider/anthropic.rs` does not send the header. So the
///     200K value here matches what the wire actually allows.
///     Opus and Haiku stay at 200K regardless.
pub fn context_window_tokens(provider: &str, model: &str) -> usize {
    match (provider, model) {
        ("openai", m) if m.starts_with("gpt-5") => 400_000,
        ("openai", m) if m.starts_with("o") => 200_000,
        ("openai", _) => 32_000,
        ("anthropic", _) => 200_000,
        ("ollama", _) => 8_000,
        // xAI Grok models currently expose large context windows on the
        // cloud endpoint (order-of-magnitude in the same class as recent
        // OpenAI models); this is a safe conservative starting value.
        ("grok", _) => 100_000,
        // ChatGPT-subscription Codex backend. Same gpt-5* family,
        // same advertised context window as the platform API — the
        // billing pipe is what differs, not the model. Starting
        // value; the runtime overflow-learn path in `model_limits.rs`
        // will tighten if the subscription tier enforces something
        // lower in practice.
        ("codex-oauth", _) => 400_000,
        // Claude Code OAuth hits the Anthropic API directly with the
        // subscription token; same 200k context as the `anthropic:` path.
        ("claude-oauth", _) => 200_000,
        _ => 8_000,
    }
}

pub const UTILIZATION: f64 = 0.7;
const MIN_KEEP_TURNS: usize = 2;

/// Rough token estimate (chars / 3.5). Safe to overestimate.
pub fn estimate_tokens(messages: &[Message]) -> usize {
    let mut chars = 0usize;
    for m in messages {
        match &m.content {
            Content::Text { text: t } => chars += t.len(),
            Content::AssistantToolCalls { text, calls } => {
                if let Some(t) = text {
                    chars += t.len();
                }
                for c in calls {
                    chars += c.name.len();
                    chars += serde_json::to_string(&c.input)
                        .map(|s| s.len())
                        .unwrap_or(0);
                }
            }
            Content::ToolResult { output, .. } => chars += output.len(),
            Content::Summary { text, .. } => {
                chars += text.len() + crate::provider::SUMMARY_WIRE_PREFIX.len();
            }
            Content::Reasoning(item) => {
                chars += item.encrypted_content.as_deref().map(str::len).unwrap_or(0)
                    + item.summary.iter().map(String::len).sum::<usize>();
            }
        }
    }
    (chars as f64 / 3.5) as usize
}

/// Prune oldest turn pairs until the estimate is under budget. A turn
/// pair is a User message + everything until the next User (or end).
/// Returns the number of pruned turns for logging.
pub fn prune(messages: &mut Vec<Message>, provider: &str, model: &str) -> usize {
    let cap = (context_window_tokens(provider, model) as f64 * UTILIZATION) as usize;
    prune_to_budget(messages, cap)
}

/// Variant of `prune` that takes an explicit token budget instead
/// of looking up the hardcoded table. Used by the learned-limit /
/// probe path in `agent.rs` so the budget reflects the per-model
/// runtime knowledge.
pub fn prune_to_budget(messages: &mut Vec<Message>, cap: usize) -> usize {
    let mut pruned = 0;
    while estimate_tokens(messages) > cap {
        // Find next User-message boundary; everything before it is one
        // (or zero) full turn-pair we can drop.
        let mut second_user_idx = None;
        let mut user_seen = 0;
        for (i, m) in messages.iter().enumerate() {
            if matches!(m.role, Role::User) {
                user_seen += 1;
                if user_seen == MIN_KEEP_TURNS + 1 {
                    second_user_idx = Some(i);
                    break;
                }
            }
        }
        // If we don't have at least MIN_KEEP_TURNS+1 user messages, we
        // can't prune anything without dropping too much.
        let cut = match second_user_idx {
            Some(_) => find_first_user_run_end(messages),
            None => break,
        };
        if cut == 0 {
            break;
        }
        messages.drain(..cut);
        pruned += 1;
    }
    pruned
}

/// Return the index where the first user-led "turn pair" ends — i.e.
/// the index of the second User message (or messages.len() if there's
/// only one).
fn find_first_user_run_end(messages: &[Message]) -> usize {
    let mut seen_user = false;
    for (i, m) in messages.iter().enumerate() {
        if matches!(m.role, Role::User) {
            if seen_user {
                return i;
            }
            seen_user = true;
        }
    }
    messages.len()
}

/// Placeholder a synthetic [`Content::ToolResult`] carries when
/// [`sanitize_tool_pairing`] back-fills an orphaned tool call. Worded so
/// the model understands the result is missing (not empty) and shouldn't
/// treat the placeholder as real output.
pub const ORPHAN_TOOL_RESULT_PLACEHOLDER: &str =
    "(no tool result was recorded — the previous turn was interrupted before this tool finished)";

/// Cheap scan: does `messages` contain an orphaned tool call (an
/// `AssistantToolCalls` whose call id has no matching `ToolResult`) or a
/// stray result (a `ToolResult` with no issuing call)? Lets hot paths
/// skip the allocating repair when the history is already well-formed.
pub fn needs_tool_pairing_repair(messages: &[Message]) -> bool {
    let mut call_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut result_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for m in messages {
        match &m.content {
            Content::AssistantToolCalls { calls, .. } => {
                for c in calls {
                    call_ids.insert(c.id.as_str());
                }
            }
            Content::ToolResult { call_id, .. } => {
                result_ids.insert(call_id.as_str());
            }
            _ => {}
        }
    }
    call_ids.iter().any(|id| !result_ids.contains(id))
        || result_ids.iter().any(|id| !call_ids.contains(id))
}

/// Repair tool-call/result pairing in a (typically just-loaded) history.
///
/// Every `function_call` an `AssistantToolCalls` carries must have a
/// matching `function_call_output` (`ToolResult`), or the OpenAI / codex
/// Responses backend rejects the *entire* request with
/// `400 "No tool output found for function call ..."` (Anthropic 400s the
/// same way on an orphan `tool_use` id). The agent loop always appends a
/// result for every call, but it persists the `AssistantToolCalls` line
/// to `smith.jsonl` *before* running the tools — so a torn write (daemon
/// restart, SIGKILL on a turn timeout, or two adapters briefly sharing
/// one `smith.jsonl`) can leave an orphaned call on disk. Replayed
/// verbatim on resume, that one bad record wedges the session forever:
/// every subsequent turn rebuilds the same poisoned request and 400s.
///
/// This makes the history well-formed again by:
///   * back-filling a synthetic error `ToolResult` (immediately after the
///     issuing `AssistantToolCalls`) for any call with no result, and
///   * dropping any stray `ToolResult` whose call was lost.
///
/// Back-filling rather than deleting the call keeps partial parallel
/// tool batches intact — only the missing legs get a placeholder, the
/// ones that completed keep their real output. Returns the number of
/// repairs (synthetic results added + stray results dropped); `0` leaves
/// `messages` untouched.
pub fn sanitize_tool_pairing(messages: &mut Vec<Message>) -> usize {
    if !needs_tool_pairing_repair(messages) {
        return 0;
    }
    let mut call_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut result_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for m in messages.iter() {
        match &m.content {
            Content::AssistantToolCalls { calls, .. } => {
                for c in calls {
                    call_ids.insert(c.id.clone());
                }
            }
            Content::ToolResult { call_id, .. } => {
                result_ids.insert(call_id.clone());
            }
            _ => {}
        }
    }

    let mut repairs = 0usize;
    let mut out: Vec<Message> = Vec::with_capacity(messages.len() + 1);
    for m in messages.drain(..) {
        match &m.content {
            Content::AssistantToolCalls { calls, .. } => {
                // Snapshot which of this message's calls lack a result
                // before we move `m` into `out`.
                let orphans: Vec<String> = calls
                    .iter()
                    .filter(|c| !result_ids.contains(&c.id))
                    .map(|c| c.id.clone())
                    .collect();
                out.push(m);
                for id in orphans {
                    // Mark satisfied so a (pathological) duplicate call id
                    // later isn't back-filled twice.
                    result_ids.insert(id.clone());
                    out.push(Message {
                        role: Role::Tool,
                        content: Content::ToolResult {
                            call_id: id,
                            output: ORPHAN_TOOL_RESULT_PLACEHOLDER.to_string(),
                            is_error: true,
                        },
                    });
                    repairs += 1;
                }
            }
            Content::ToolResult { call_id, .. } => {
                if call_ids.contains(call_id) {
                    out.push(m);
                } else {
                    // Stray result with no issuing call — drop it.
                    repairs += 1;
                }
            }
            _ => out.push(m),
        }
    }
    *messages = out;
    repairs
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn no_prune_under_budget() {
        let mut ms = vec![user("hi"), asst("hello")];
        let pruned = prune(&mut ms, "openai", "gpt-5");
        assert_eq!(pruned, 0);
        assert_eq!(ms.len(), 2);
    }

    #[test]
    fn keeps_min_recent_turns() {
        // Tiny budget by using ollama default (8k tokens ≈ 28k chars).
        // Three turn pairs total; MIN_KEEP=2 means at least the most
        // recent two are preserved.
        let huge = "x".repeat(40_000);
        let mut ms = vec![
            user(&huge),
            asst(&huge),
            user("middle question"),
            asst("middle answer"),
            user("recent question"),
            asst("recent answer"),
        ];
        let pruned = prune(&mut ms, "ollama", "llama3.1");
        assert!(pruned >= 1);
        // Final messages should still contain the recent ones.
        assert!(matches!(ms.last().map(|m| m.role), Some(Role::Assistant)));
    }

    fn tool_calls(ids: &[&str]) -> Message {
        Message {
            role: Role::Assistant,
            content: Content::AssistantToolCalls {
                text: None,
                calls: ids
                    .iter()
                    .map(|id| crate::provider::ToolCall {
                        id: (*id).into(),
                        name: "shell".into(),
                        input: serde_json::json!({}),
                    })
                    .collect(),
            },
        }
    }
    fn tool_result(call_id: &str) -> Message {
        Message {
            role: Role::Tool,
            content: Content::ToolResult {
                call_id: call_id.into(),
                output: "ok".into(),
                is_error: false,
            },
        }
    }
    fn result_call_id(m: &Message) -> Option<&str> {
        match &m.content {
            Content::ToolResult { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        }
    }

    #[test]
    fn sanitize_noop_when_well_formed() {
        let mut ms = vec![
            user("go"),
            tool_calls(&["c1"]),
            tool_result("c1"),
            asst("done"),
        ];
        let before = ms.len();
        assert!(!needs_tool_pairing_repair(&ms));
        assert_eq!(sanitize_tool_pairing(&mut ms), 0);
        assert_eq!(ms.len(), before);
    }

    #[test]
    fn sanitize_backfills_orphaned_call() {
        // Mirrors the real wedge: an AssistantToolCalls persisted without
        // its ToolResult (a torn write), surrounded by valid pairs.
        let mut ms = vec![
            user("go"),
            tool_calls(&["c1"]),
            tool_result("c1"),
            tool_calls(&["orphan-155"]), // no result was ever persisted
            tool_calls(&["c2"]),
            tool_result("c2"),
        ];
        assert!(needs_tool_pairing_repair(&ms));
        assert_eq!(sanitize_tool_pairing(&mut ms), 1);
        assert!(!needs_tool_pairing_repair(&ms));
        // The synthetic result lands immediately after the orphaned call.
        let idx = ms
            .iter()
            .position(|m| matches!(&m.content,
                Content::AssistantToolCalls { calls, .. } if calls[0].id == "orphan-155"))
            .unwrap();
        match &ms[idx + 1].content {
            Content::ToolResult {
                call_id,
                is_error,
                output,
            } => {
                assert_eq!(call_id, "orphan-155");
                assert!(is_error);
                assert_eq!(output, ORPHAN_TOOL_RESULT_PLACEHOLDER);
            }
            other => panic!("expected synthetic ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn sanitize_backfills_only_missing_leg_of_parallel_batch() {
        // Parallel tool batch where one leg's result was lost: keep the
        // real result, back-fill only the orphan.
        let mut ms = vec![
            user("go"),
            tool_calls(&["a", "b"]),
            tool_result("a"), // b's result is missing
            asst("next"),
        ];
        assert_eq!(sanitize_tool_pairing(&mut ms), 1);
        assert!(!needs_tool_pairing_repair(&ms));
        let real = ms
            .iter()
            .find(|m| result_call_id(m) == Some("a"))
            .unwrap();
        assert!(matches!(&real.content, Content::ToolResult { is_error, .. } if !*is_error));
        let synth = ms
            .iter()
            .find(|m| result_call_id(m) == Some("b"))
            .unwrap();
        assert!(matches!(&synth.content, Content::ToolResult { is_error, .. } if *is_error));
    }

    #[test]
    fn sanitize_drops_stray_result() {
        let mut ms = vec![
            user("go"),
            tool_result("ghost"), // result with no issuing call
            asst("done"),
        ];
        assert!(needs_tool_pairing_repair(&ms));
        assert_eq!(sanitize_tool_pairing(&mut ms), 1);
        assert!(!needs_tool_pairing_repair(&ms));
        assert!(ms.iter().all(|m| result_call_id(m).is_none()));
        assert_eq!(ms.len(), 2);
    }
}
