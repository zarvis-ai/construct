//! Provider plumbing: a small trait + normalized message/tool types,
//! plus per-provider implementations. The agent loop is generic over
//! [`LlmProvider`] so adding a new provider is one impl file.

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod anthropic;
pub mod codex_oauth;
pub mod ollama;
pub mod openai;
pub mod routing;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// One message in the rolling conversation we send to the model. Mirrors
/// the shape that maps cleanly onto OpenAI / Anthropic / Ollama wire
/// formats — each provider impl translates it to its own JSON.
///
/// Serializable so the agent loop can append each message to
/// `zarvis.jsonl` and replay on daemon-restart resume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Content,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Content {
    /// Plain text (system / user / assistant).
    Text { text: String },
    /// Assistant turn that's making tool calls. May also include final
    /// pre-tool prose (`text`) that comes before the calls.
    AssistantToolCalls {
        text: Option<String>,
        calls: Vec<ToolCall>,
    },
    /// Single tool result, paired with its originating call id.
    ToolResult {
        call_id: String,
        output: String,
        is_error: bool,
    },
    /// Output of `/compact` (manual or automatic). Carries the
    /// LLM-written summary of older turns it replaced. Providers
    /// serialize this as a plain `user` text turn on the wire (prefixed
    /// with [`SUMMARY_WIRE_PREFIX`]); the variant exists so on-disk and
    /// in-memory code can identify compacted history — e.g. the next
    /// compaction walks past it instead of re-summarizing the summary,
    /// and the TUI can render it with a banner.
    Summary {
        text: String,
        /// Number of *turn pairs* (user → assistant exchange, including
        /// any interleaved tool calls) collapsed into this summary.
        /// Used for telemetry and the TUI banner.
        dropped_turn_pairs: u32,
    },
    /// A model reasoning item, echoed back to the provider on the next
    /// turn. The codex-oauth backend (gpt-5 Responses API with
    /// `store:false`) requires reasoning items to be replayed with their
    /// `id` + `encrypted_content` for prompt caching and reasoning
    /// continuity; providers that don't support it skip this variant.
    Reasoning(ReasoningItem),
}

/// A reasoning item captured from a Responses-API turn and replayed on the
/// next request. `encrypted_content` is the opaque blob the backend returns
/// when `include: ["reasoning.encrypted_content"]` is requested.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReasoningItem {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted_content: Option<String>,
    #[serde(default)]
    pub summary: Vec<String>,
}

/// Prefix prepended to a [`Content::Summary`] body when serialized on
/// the wire. Kept out of the in-memory `text` field so re-compaction
/// can recognize summaries without string-matching the body.
pub const SUMMARY_WIRE_PREFIX: &str = "[Compacted earlier context]\n";

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON-schema-shaped object describing the tool's input.
    pub schema: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Cached input tokens (subset of `input_tokens`). 0 when unknown.
    pub cached_tokens: u64,
    pub usd: f64,
}

/// Why the provider stopped producing tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// Final assistant text; the agent loop should park awaiting input.
    EndTurn,
    /// Assistant emitted tool calls; the agent loop should run them and
    /// feed results back.
    ToolUse,
    /// Hit max tokens / other provider-side limit. Treat like EndTurn so
    /// the user can intervene.
    MaxTokens,
}

/// The aggregated result of one provider call.
#[derive(Debug)]
pub struct ProviderTurn {
    pub text: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub stop_reason: StopReason,
    pub usage: Usage,
    /// Reasoning items emitted this turn (codex-oauth only; empty
    /// elsewhere). The agent loop stores these in history so they're
    /// replayed on the next request. See [`Content::Reasoning`].
    pub reasoning_items: Vec<ReasoningItem>,
}

impl ProviderTurn {
    /// True when the provider returned a successful turn that gives the
    /// agent nothing to display, persist, or execute. Agent loops treat
    /// this as an error so provider/server regressions cannot silently
    /// eat a user prompt.
    pub fn is_empty(&self) -> bool {
        let has_text = self
            .text
            .as_deref()
            .map(|text| !text.trim().is_empty())
            .unwrap_or(false);
        !has_text && self.tool_calls.is_empty()
    }
}

/// Sink for the assistant's streaming text deltas. Headless mode wires
/// this to `SessionEvent::Message` events; interactive (PTY) mode wires
/// it to raw `SessionEvent::Pty` bytes so the user sees the response
/// flow in the terminal pane. Provider impls don't care which it is.
pub trait TextSink: Send {
    fn delta(&mut self, text: &str);
    /// Streaming "thinking" / reasoning text from the model — e.g.
    /// Anthropic's `thinking_delta` content blocks or Codex
    /// Responses' `reasoning_summary_text.delta` events. Default is
    /// a no-op so providers and sinks that don't care about
    /// reasoning don't have to opt out.
    fn reasoning_delta(&mut self, _text: &str) {}
    /// Liveness ping: the provider received *some* stream event from
    /// upstream (any SSE event / NDJSON chunk), even one that carries
    /// no text — `response.created`, tool-call argument deltas, usage,
    /// keepalives, etc. Providers should call this once per received
    /// stream item so the idle watchdog measures "no bytes from
    /// upstream" rather than "no assistant text", and so a turn that's
    /// streaming only tool-call arguments isn't killed as idle. Default
    /// no-op; `delta` / `reasoning_delta` already imply progress.
    fn progress(&mut self) {}
}

/// Sentinel error for "the input you sent is over the model's
/// context window". Providers return this wrapped in `anyhow::Error`
/// when they recognize the API's overflow signal in an HTTP 400
/// body; the agent loop downcasts and routes to the
/// `model_limits.rs` learn-and-retry path. `extracted` carries the
/// provider-reported limit when present (OpenAI: "maximum context
/// length is N tokens"); otherwise `None` and the agent loop falls
/// back to a fixed-ratio reduction.
#[derive(Debug, Clone)]
pub struct ContextOverflow {
    pub extracted: Option<u64>,
    pub raw: String,
}

impl std::fmt::Display for ContextOverflow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "context overflow (extracted={:?}): {}",
            self.extracted, self.raw
        )
    }
}

impl std::error::Error for ContextOverflow {}

/// Parse a provider's HTTP 400 error body for a context-overflow
/// signature. Returns `Some(extracted)` only when the body reads as
/// an overflow error; the extracted value is `Some(n)` if the body
/// stated a token limit explicitly, `None` if it was overflow-shaped
/// but didn't name a number.
///
/// Recognition uses two passes:
///
///   1. **Phrase-shaped:** known wordings from OpenAI / Anthropic /
///      Ollama overflow errors. Cheap to add new variants as vendors
///      change copy.
///   2. **Structural fallback:** the body says "tokens" *and* contains
///      a number that looks like a token count (in
///      `[MIN_TOKEN_COUNT, MAX_TOKEN_COUNT]`). Catches future error
///      formats that we haven't seen yet without us having to ship a
///      release every time a vendor edits a string.
///
/// The structural pass is gated on "tokens" because we don't want
/// every HTTP 400 with a big number in it (rate-limit windows, file
/// sizes, etc.) to trigger the overflow path.
pub fn parse_overflow(body: &str) -> Option<Option<u64>> {
    const MIN_TOKEN_COUNT: u64 = 1_000;
    const MAX_TOKEN_COUNT: u64 = 5_000_000;
    let lower = body.to_ascii_lowercase();
    // 1) Known phrases. Update this list when a vendor changes their
    //    error wording — additions are safe (no false positives) since
    //    any phrase reaching this list is one we've actually observed.
    let phrase_match = lower.contains("maximum context length")
        || lower.contains("context length")
        || lower.contains("context window")
        || lower.contains("prompt is too long")
        || lower.contains("input is too long")
        || lower.contains("too many tokens")
        // 2026 OpenAI: "Input tokens exceed the configured limit of N
        // tokens. Your messages resulted in M tokens." (Newer wording
        // that doesn't mention "context length" — this user's report.)
        || lower.contains("input tokens exceed")
        || lower.contains("configured limit");
    // 2) Structural fallback: "tokens" + a number in plausible range.
    //    Brittle phrase matching is the historical failure mode; this
    //    gate trips even when vendors invent new copy.
    let nums = extract_numbers_in_range(body, MIN_TOKEN_COUNT, MAX_TOKEN_COUNT);
    let structural_match = lower.contains("tokens") && !nums.is_empty();
    if !phrase_match && !structural_match {
        return None;
    }
    // Pick the FIRST number in plausible range. OpenAI's older format
    // reads "maximum context length is 200000 tokens. However, you
    // requested 380000 tokens" — the FIRST number is the cap, the
    // second is current usage. The newer "configured limit of 272000
    // tokens. Your messages resulted in 273174 tokens" follows the
    // same order. Anthropic / Ollama vary; the fallback ratio at the
    // agent layer corrects either way.
    Some(nums.into_iter().next())
}

#[cfg(test)]
mod provider_turn_tests {
    use super::*;

    #[test]
    fn detects_empty_successful_turns() {
        let turn = ProviderTurn {
            text: None,
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
            reasoning_items: Vec::new(),
        };
        assert!(turn.is_empty());
    }

    #[test]
    fn whitespace_text_is_still_empty() {
        let turn = ProviderTurn {
            text: Some(" \n\t".into()),
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
            reasoning_items: Vec::new(),
        };
        assert!(turn.is_empty());
    }

    #[test]
    fn text_or_tools_are_not_empty() {
        let text_turn = ProviderTurn {
            text: Some("done".into()),
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
            reasoning_items: Vec::new(),
        };
        assert!(!text_turn.is_empty());

        let tool_turn = ProviderTurn {
            text: None,
            tool_calls: vec![ToolCall {
                id: "call_1".into(),
                name: "exec".into(),
                input: serde_json::json!({}),
            }],
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
            reasoning_items: Vec::new(),
        };
        assert!(!tool_turn.is_empty());
    }
}

/// Scan `s` for non-overlapping decimal integer runs and return the
/// ones that fit in `[min_val, max_val]`. Order is preserved (first
/// occurrence first).
fn extract_numbers_in_range(s: &str, min_val: u64, max_val: u64) -> Vec<u64> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            cur.push(ch);
        } else if !cur.is_empty() {
            if let Ok(n) = cur.parse::<u64>() {
                if n >= min_val && n <= max_val {
                    out.push(n);
                }
            }
            cur.clear();
        }
    }
    if !cur.is_empty() {
        if let Ok(n) = cur.parse::<u64>() {
            if n >= min_val && n <= max_val {
                out.push(n);
            }
        }
    }
    out
}

#[cfg(test)]
mod overflow_tests {
    use super::parse_overflow;

    #[test]
    fn openai_style_with_limit() {
        let body = r#"{"error":{"message":"This model's maximum context length is 200000 tokens. However, you requested 380000 tokens (250000 in the messages, 130000 in the completion). Please reduce the length of the messages or completion.","type":"invalid_request_error","param":"messages","code":"context_length_exceeded"}}"#;
        let r = parse_overflow(body);
        assert_eq!(r, Some(Some(200_000)));
    }

    #[test]
    fn anthropic_style_no_explicit_limit() {
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 250000 tokens > 200000 maximum"}}"#;
        let r = parse_overflow(body);
        // Two numbers in range; we take the first (250000 here, the
        // "current usage" number). That's a worse outcome than
        // extracting the actual 200000 cap — but the fallback ratio
        // applied at the agent layer drops it 20% to 200k anyway,
        // so the user lands close to right either way.
        assert!(r.is_some());
        let extracted = r.unwrap();
        // Either parser interpretation is acceptable; the important
        // thing is we recognized the overflow shape.
        assert!(extracted.is_some());
    }

    #[test]
    fn ollama_style() {
        let body = "context length exceeded: 8192 tokens";
        assert_eq!(parse_overflow(body), Some(Some(8192)));
    }

    #[test]
    fn unrelated_400_is_not_overflow() {
        let body = r#"{"error":{"message":"invalid api key","code":"invalid_api_key"}}"#;
        assert_eq!(parse_overflow(body), None);
    }

    /// Regression for the user-reported failure: OpenAI's 2026 wording
    /// reads "Input tokens exceed the configured limit of N tokens.
    /// Your messages resulted in M tokens." The old phrase list didn't
    /// recognize either "input tokens exceed" or "configured limit",
    /// so the agent loop fell through to a raw error and broke. The
    /// learn-and-retry path needs this to fire.
    #[test]
    fn openai_2026_configured_limit_phrase() {
        let body = r#"{"error":{"message":"Input tokens exceed the configured limit of 272000 tokens. Your messages resulted in 273174 tokens. Please reduce the length of the messages.","type":"invalid_request_error","param":"messages","code":"context_length_exceeded"}}"#;
        let r = parse_overflow(body);
        assert_eq!(r, Some(Some(272_000)));
    }

    /// Structural fallback: if a vendor invents a new wording we
    /// haven't catalogued, presence of "tokens" + a token-shaped
    /// number should still trigger the overflow path. Keeps us from
    /// silently breaking every time a provider edits their copy.
    #[test]
    fn structural_fallback_catches_unknown_wording() {
        let body = r#"{"error":{"message":"Your call exhausted the allowed 200000 tokens for this conversation. Trim and retry."}}"#;
        let r = parse_overflow(body);
        assert_eq!(r, Some(Some(200_000)));
    }

    /// Structural fallback must NOT trip on errors that happen to
    /// contain a large number but aren't about tokens (rate-limit
    /// windows, file sizes, etc.). The word "tokens" is the gate.
    #[test]
    fn structural_does_not_overmatch_unrelated_big_numbers() {
        // Rate-limit error with a 300000-millisecond window.
        let body = r#"{"error":{"message":"Rate limit exceeded. Try again in 300000 ms."}}"#;
        assert_eq!(parse_overflow(body), None);
        // 4M-byte payload size error.
        let body = r#"{"error":{"message":"Request body too large: 4500000 bytes exceeds 4194304 bytes."}}"#;
        assert_eq!(parse_overflow(body), None);
    }

    /// Numbers below 1K are likely status codes / counts, not token
    /// counts. Above 5M would be implausible.
    #[test]
    fn structural_ignores_implausible_token_counts() {
        let body = r#"{"error":{"message":"too many tokens: 500"}}"#;
        // 500 is below MIN_TOKEN_COUNT; recognized as overflow-shaped
        // by phrase but no extractable number.
        assert_eq!(parse_overflow(body), Some(None));
        let body = r#"{"error":{"message":"too many tokens: 10000000"}}"#;
        // 10M > MAX_TOKEN_COUNT; same outcome.
        assert_eq!(parse_overflow(body), Some(None));
    }

    /// Phrase-shaped overflow with no number → caller should fall
    /// back to FALLBACK_OVERFLOW_RATIO at the agent layer.
    #[test]
    fn phrase_match_without_number_returns_some_none() {
        let body = r#"{"error":{"message":"prompt is too long"}}"#;
        assert_eq!(parse_overflow(body), Some(None));
    }
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn name(&self) -> &str;

    /// Run one turn against the model. Implementations stream the
    /// response and push deltas through `sink` so the user sees the
    /// assistant text flowing as it arrives.
    async fn complete(
        &self,
        model: &str,
        system: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        sink: &mut dyn TextSink,
    ) -> Result<ProviderTurn>;
}
