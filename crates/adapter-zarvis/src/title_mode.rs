//! One-shot session-title generation.
//!
//! When the daemon detects a session's first user message and no
//! user-set title exists, it shells out to
//! `agentd-adapter-zarvis --title-mode "<prompt>"`. This module powers
//! that invocation: it picks a model the same way the regular adapter
//! does (AGENTD_ZARVIS_MODEL → API-key fallback), runs a single
//! tools-disabled completion with a short system prompt, and prints
//! the cleaned-up title to stdout.

use crate::provider::{
    self, routing::Provider, Content, LlmProvider, Message, Role, TextSink, ToolSpec,
};
use anyhow::{anyhow, Result};

/// Tight, opinionated system prompt so the model returns a clean title
/// and not a chatty preface. We further sanitize the output below.
const TITLE_SYSTEM_PROMPT: &str = "You generate short titles for conversation \
threads. Return ONLY a 3-5 word title that summarizes the user's request. \
Use Title Case. No quotes, no punctuation, no markdown, no preamble.";

/// Sink that captures streamed deltas in memory.
#[derive(Default)]
struct CaptureSink {
    text: String,
}
impl TextSink for CaptureSink {
    fn delta(&mut self, text: &str) {
        self.text.push_str(text);
    }
}

fn pick_default_spec_str() -> String {
    if let Ok(s) = std::env::var("AGENTD_ZARVIS_MODEL") {
        if !s.trim().is_empty() {
            return s;
        }
    }
    if std::env::var("ANTHROPIC_API_KEY").is_ok() {
        return "anthropic:claude-haiku-4-5".to_string();
    }
    if std::env::var("OPENAI_API_KEY").is_ok() {
        return "openai:gpt-5-mini".to_string();
    }
    if std::env::var("GEMINI_API_KEY").is_ok() || std::env::var("GOOGLE_API_KEY").is_ok() {
        return "gemini:gemini-2.5-flash".to_string();
    }
    "ollama:llama3.1".to_string()
}

fn provider_for(p: Provider) -> Result<Box<dyn LlmProvider>> {
    Ok(match p {
        Provider::OpenAI => Box::new(provider::openai::OpenAi::from_env()?),
        Provider::Anthropic => Box::new(provider::anthropic::Anthropic::from_env()?),
        Provider::Gemini => Box::new(provider::gemini::Gemini::from_env()?),
        Provider::Ollama => Box::new(provider::ollama::Ollama::from_env()?),
        // Title generation always uses one of the trio above; the
        // user never picks `codex-oauth:` for title-gen since the
        // selection comes from `pick_default_spec_str` which only
        // emits openai/anthropic/ollama. Bail loudly if we ever get
        // here so the contract doesn't drift silently.
        Provider::CodexOauth => {
            return Err(anyhow!(
                "title-gen does not support codex-oauth provider"
            ));
        }
    })
}

/// Run one title-generation completion and return the cleaned title.
/// Fails fast on missing API keys / network errors so the caller can
/// silently fall back to the session's default (hash-derived) name.
pub async fn suggest_title(user_prompt: &str) -> Result<String> {
    let spec = provider::routing::parse_model_spec(&pick_default_spec_str())
        .map_err(|e| anyhow!("model-spec parse: {e}"))?;
    let provider = provider_for(spec.provider)?;
    let messages = vec![Message {
        role: Role::User,
        content: Content::Text {
            text: user_prompt.to_string(),
        },
    }];
    let tools: Vec<ToolSpec> = Vec::new();
    let mut sink = CaptureSink::default();
    let _turn = provider
        .complete(&spec.model, TITLE_SYSTEM_PROMPT, &messages, &tools, &mut sink)
        .await?;
    Ok(sanitize_title(&sink.text))
}

/// Strip leading/trailing whitespace + quotes/markdown the model is
/// fond of adding, and cap the length so a misbehaving model can't
/// blow out the modeline. 5 words at ≤ 7 chars each ≈ 40 chars + a
/// safety buffer.
fn sanitize_title(raw: &str) -> String {
    let line = raw.lines().next().unwrap_or("");
    let mut s = line.trim().trim_matches(|c: char| {
        c == '"' || c == '\'' || c == '`' || c == '*' || c == '#'
    });
    // Strip a single pair of surrounding quotes after the trim-match above
    // catches the easy cases.
    while let (Some('"'), Some('"')) = (s.chars().next(), s.chars().last()) {
        s = &s[1..s.len() - 1];
        s = s.trim();
    }
    let truncated: String = s.chars().take(48).collect();
    truncated.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_quotes_and_markdown() {
        assert_eq!(sanitize_title("\"Refactor Adapter Spawning\""), "Refactor Adapter Spawning");
        assert_eq!(sanitize_title("`Add Pty Logging`"), "Add Pty Logging");
        assert_eq!(sanitize_title("**Plan The Refactor**"), "Plan The Refactor");
    }
    #[test]
    fn sanitize_first_line_only() {
        assert_eq!(sanitize_title("Title Here\nextra explanation"), "Title Here");
    }
    #[test]
    fn sanitize_caps_length() {
        let huge = "Word ".repeat(50);
        let out = sanitize_title(&huge);
        assert!(out.len() <= 48);
    }
}
