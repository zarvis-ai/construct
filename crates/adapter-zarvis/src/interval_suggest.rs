//! One-shot LLM helper: pick a polling interval (in seconds) for a
//! `/loop <prompt>` invocation that didn't include an explicit
//! cadence. Reuses the session's already-resolved provider/model
//! so we don't spin up a fresh API client.
//!
//! Returns the LLM's suggestion clamped to
//! `[min_interval_secs(), max_interval_secs()]`. The caller surfaces
//! the clamp + the suggestion source in the loop's tool result so
//! the user knows what they got.
//!
//! Failures (network, parse) fall back to the *minimum* interval
//! rather than refusing — slash commands feel less broken if they
//! over-fire than if they silently produce nothing.

use crate::provider::{Content, LlmProvider, Message, Role, TextSink, ToolSpec};
use anyhow::Result;

const SYSTEM_PROMPT: &str = r#"You are deciding the polling interval (in seconds) for a recurring user prompt that fires on a schedule. The user will give you the prompt they want to run. Reply with ONLY an integer number of seconds — nothing else, no units, no commentary, no markdown. Pick the interval that best fits the prompt's natural cadence:

- "Check if X is done" or "is the build still running" → tens of seconds (30-120s).
- "Summarize today's work" → ~3600s (hourly).
- "Review my PRs" / "any new emails" → ~600s (10min).
- "Daily standup notes" → 86400s.

Default to 300s (5 minutes) if you genuinely can't tell. Stay within [30, 86400]."#;

/// Min/max bounds for the suggestion. Daemon enforces the same
/// bounds, but clamping at the adapter level gives a cleaner
/// error path + lets us mention the clamp in the tool result.
fn bounds() -> (u64, u64) {
    let min = std::env::var("AGENTD_LOOP_MIN_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30u64);
    let max = std::env::var("AGENTD_LOOP_MAX_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(24 * 3600u64);
    (min, max)
}

fn clamp(secs: u64) -> u64 {
    let (min, max) = bounds();
    secs.clamp(min, max)
}

/// Sink that captures streamed deltas into a single String.
#[derive(Default)]
struct CaptureSink {
    text: String,
}
impl TextSink for CaptureSink {
    fn delta(&mut self, text: &str) {
        self.text.push_str(text);
    }
}

/// Ask the model for an interval in seconds for this loop's prompt.
/// Returns a clamped value; failures fall back to the lower bound.
pub async fn suggest(
    provider: &dyn LlmProvider,
    model: &str,
    user_prompt: &str,
) -> Result<u64> {
    let messages = vec![Message {
        role: Role::User,
        content: Content::Text {
            text: format!("Loop prompt: {user_prompt}"),
        },
    }];
    let tools: Vec<ToolSpec> = Vec::new();
    let mut sink = CaptureSink::default();
    let _turn = provider
        .complete(model, SYSTEM_PROMPT, &messages, &tools, &mut sink)
        .await?;
    Ok(parse_secs(&sink.text).map(clamp).unwrap_or_else(|| bounds().0))
}

/// Extract the first run of digits in the response, parse it,
/// return `Some(seconds)`. Returns `None` only when no digits
/// appear at all.
fn parse_secs(s: &str) -> Option<u64> {
    let mut digits = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
        } else if !digits.is_empty() {
            break;
        }
    }
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic() {
        assert_eq!(parse_secs("300"), Some(300));
        assert_eq!(parse_secs("  60  "), Some(60));
        assert_eq!(parse_secs("60 seconds"), Some(60));
    }

    #[test]
    fn ignores_trailing_text() {
        assert_eq!(parse_secs("1800 # about 30min"), Some(1800));
    }

    #[test]
    fn none_for_no_digits() {
        assert_eq!(parse_secs("noop"), None);
    }

    #[test]
    fn clamp_respects_bounds() {
        std::env::set_var("AGENTD_LOOP_MIN_SECS", "30");
        std::env::set_var("AGENTD_LOOP_MAX_SECS", "3600");
        assert_eq!(clamp(5), 30);
        assert_eq!(clamp(5000), 3600);
        assert_eq!(clamp(60), 60);
        std::env::remove_var("AGENTD_LOOP_MIN_SECS");
        std::env::remove_var("AGENTD_LOOP_MAX_SECS");
    }
}
