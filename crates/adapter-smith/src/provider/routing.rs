//! Translate a model spec string into a (provider, bare model name).
//!
//! Explicit prefixes (`openai:`, `anthropic:`, `gemini:`, `meta:`, `ollama:`,
//! `grok:`, `grok-oauth:`, `codex-oauth:`, `claude-oauth:`, `claude-code-oauth:`) always win.
//! Otherwise we sniff the bare name:
//!   - starts with `gpt-` or `o[1-5]` → OpenAI
//!   - starts with `claude-` → Anthropic
//!   - starts with `gemini-` → Gemini
//!   - anything else → Ollama (local fallback)
//!
//! Returning an enum keeps the dispatch table small and testable.
//!
//! `codex-oauth:` is the subscription-billed Codex backend
//! (`chatgpt.com/backend-api/codex/responses`), distinct from `openai:`
//! which hits the public platform API at `api.openai.com`. There is no
//! bare-name fallback for it — users must opt in explicitly because the
//! billing path is different.
//!
//! `claude-oauth:` uses the user's Claude Code subscription: it reads the
//! Claude Code login credentials and calls the Anthropic API directly with the
//! subscription OAuth token (see spec 0031). It is distinct from `anthropic:`,
//! which uses `ANTHROPIC_API_KEY`.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    OpenAI,
    Anthropic,
    Gemini,
    /// Meta Model API Responses surface.
    Meta,
    Ollama,
    /// xAI Grok API surface.
    Grok,
    /// OAuth-backed Grok access path.
    GrokOauth,
    /// OAuth-backed Codex backend; reads `~/.codex/auth.json`, bills
    /// against the user's ChatGPT subscription.
    CodexOauth,
    /// Claude Code subscription path; reads the Claude Code OAuth
    /// credentials and calls the Anthropic API directly (spec 0031).
    ClaudeOauth,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSpec {
    pub provider: Provider,
    pub model: String,
}

pub fn parse_model_spec(s: &str) -> Result<ModelSpec, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty model spec".into());
    }
    if let Some(rest) = s.strip_prefix("openai:") {
        return Ok(ModelSpec {
            provider: Provider::OpenAI,
            model: rest.to_string(),
        });
    }
    if let Some(rest) = s.strip_prefix("anthropic:") {
        return Ok(ModelSpec {
            provider: Provider::Anthropic,
            model: rest.to_string(),
        });
    }
    if let Some(rest) = s.strip_prefix("gemini:") {
        return Ok(ModelSpec {
            provider: Provider::Gemini,
            model: rest.to_string(),
        });
    }
    if let Some(rest) = s.strip_prefix("meta:") {
        return Ok(ModelSpec {
            provider: Provider::Meta,
            model: rest.to_string(),
        });
    }
    if let Some(rest) = s.strip_prefix("ollama:") {
        return Ok(ModelSpec {
            provider: Provider::Ollama,
            model: rest.to_string(),
        });
    }
    if let Some(rest) = s.strip_prefix("grok:") {
        return Ok(ModelSpec {
            provider: Provider::Grok,
            model: rest.to_string(),
        });
    }
    if let Some(rest) = s.strip_prefix("grok-oauth:") {
        return Ok(ModelSpec {
            provider: Provider::GrokOauth,
            model: rest.to_string(),
        });
    }
    if let Some(rest) = s.strip_prefix("codex-oauth:") {
        return Ok(ModelSpec {
            provider: Provider::CodexOauth,
            model: rest.to_string(),
        });
    }
    if let Some(rest) = s
        .strip_prefix("claude-oauth:")
        .or_else(|| s.strip_prefix("claude-code-oauth:"))
    {
        return Ok(ModelSpec {
            provider: Provider::ClaudeOauth,
            model: rest.to_string(),
        });
    }
    if let Some(prefix) = s.split(':').next() {
        // Reject unknown explicit prefixes so typos don't silently fall through.
        if s.contains(':')
            && !matches!(
                prefix,
                "openai"
                    | "anthropic"
                    | "gemini"
                    | "meta"
                    | "ollama"
                    | "grok"
                    | "grok-oauth"
                    | "codex-oauth"
                    | "claude-oauth"
                    | "claude-code-oauth"
            )
        {
            return Err(format!(
                "unknown provider prefix `{prefix}:` (expected one of \
                 openai:, anthropic:, gemini:, meta:, ollama:, grok:, grok-oauth:, codex-oauth:, claude-oauth:)"
            ));
        }
    }
    let provider = if s.starts_with("gpt-") || is_o_series(s) {
        Provider::OpenAI
    } else if s.starts_with("claude-") {
        Provider::Anthropic
    } else if s.starts_with("gemini-") {
        Provider::Gemini
    } else if s.starts_with("grok") {
        Provider::Grok
    } else {
        Provider::Ollama
    };
    Ok(ModelSpec {
        provider,
        model: s.to_string(),
    })
}

fn is_o_series(s: &str) -> bool {
    // o1, o3, o4, o5, plus their dashed variants (o1-mini, o3-pro, …).
    let mut chars = s.chars();
    let Some(c0) = chars.next() else { return false };
    if c0 != 'o' {
        return false;
    }
    let Some(c1) = chars.next() else { return false };
    if !matches!(c1, '1' | '3' | '4' | '5') {
        return false;
    }
    // Boundary: end-of-string, dash, dot, or digit cont.
    match chars.next() {
        None | Some('-') | Some('.') => true,
        Some(c) if c.is_ascii_digit() => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> ModelSpec {
        parse_model_spec(s).unwrap()
    }

    #[test]
    fn gpt_4o_is_openai() {
        assert_eq!(parse("gpt-4o").provider, Provider::OpenAI);
        assert_eq!(parse("gpt-4o").model, "gpt-4o");
    }

    #[test]
    fn o_series_is_openai() {
        assert_eq!(parse("o1").provider, Provider::OpenAI);
        assert_eq!(parse("o3-pro").provider, Provider::OpenAI);
        assert_eq!(parse("o5").provider, Provider::OpenAI);
    }

    #[test]
    fn claude_haiku_is_anthropic() {
        assert_eq!(parse("claude-haiku-4-5").provider, Provider::Anthropic);
        assert_eq!(parse("claude-haiku-4-5").model, "claude-haiku-4-5");
    }

    #[test]
    fn gemini_bare_and_prefixed_route_to_gemini() {
        assert_eq!(parse("gemini-2.5-pro").provider, Provider::Gemini);
        assert_eq!(parse("gemini-2.5-pro").model, "gemini-2.5-pro");

        let s = parse("gemini:gemini-2.5-flash");
        assert_eq!(s.provider, Provider::Gemini);
        assert_eq!(s.model, "gemini-2.5-flash");

        // `gemma` (Google's open weights) is not the Gemini API — it stays a
        // bare-name Ollama fallback.
        assert_eq!(parse("gemma2").provider, Provider::Ollama);
    }

    #[test]
    fn explicit_prefix_overrides_heuristic() {
        let s = parse("anthropic:something-new");
        assert_eq!(s.provider, Provider::Anthropic);
        assert_eq!(s.model, "something-new");

        let s = parse("ollama:llama3.1");
        assert_eq!(s.provider, Provider::Ollama);
        assert_eq!(s.model, "llama3.1");

        let s = parse("openai:gpt-5-mini");
        assert_eq!(s.provider, Provider::OpenAI);
        assert_eq!(s.model, "gpt-5-mini");
    }

    #[test]
    fn meta_prefix_is_recognized() {
        let s = parse("meta:muse-spark-1.1");
        assert_eq!(s.provider, Provider::Meta);
        assert_eq!(s.model, "muse-spark-1.1");
    }

    #[test]
    fn bare_unknown_falls_back_to_ollama() {
        let s = parse("llama3.1");
        assert_eq!(s.provider, Provider::Ollama);
        assert_eq!(s.model, "llama3.1");

        let s = parse("mistral");
        assert_eq!(s.provider, Provider::Ollama);
    }

    #[test]
    fn unknown_prefix_errors() {
        assert!(parse_model_spec("bogus:foo").is_err());
    }

    #[test]
    fn empty_errors() {
        assert!(parse_model_spec("").is_err());
        assert!(parse_model_spec("   ").is_err());
    }

    /// `codex-oauth:` routes to the OAuth-backed Codex backend
    /// (`chatgpt.com/backend-api/codex/responses`). Distinct from
    /// `openai:` to keep billing paths from getting silently swapped —
    /// `openai:gpt-5` stays on platform pay-as-you-go, only
    /// `codex-oauth:gpt-5` draws against ChatGPT subscription.
    #[test]
    fn codex_oauth_prefix_is_recognized() {
        let s = parse("codex-oauth:gpt-5-codex");
        assert_eq!(s.provider, Provider::CodexOauth);
        assert_eq!(s.model, "gpt-5-codex");

        let s = parse("codex-oauth:gpt-5");
        assert_eq!(s.provider, Provider::CodexOauth);
        assert_eq!(s.model, "gpt-5");
    }

    /// No bare-name fallback for codex-oauth: opting into a different
    /// billing path requires explicit prefix, even for the
    /// codex-specific model strings.
    #[test]
    fn gpt_5_codex_bare_does_not_route_to_codex_oauth() {
        // The platform OpenAI API doesn't serve `gpt-5-codex`, but the
        // router treats it as OpenAI based on the `gpt-` prefix; users
        // must type the `codex-oauth:` prefix explicitly.
        assert_eq!(parse("gpt-5-codex").provider, Provider::OpenAI);
    }

    #[test]
    fn grok_prefix_is_recognized() {
        let s = parse("grok:grok-2-1212");
        assert_eq!(s.provider, Provider::Grok);
        assert_eq!(s.model, "grok-2-1212");
    }

    #[test]
    fn grok_oauth_prefix_is_recognized() {
        let s = parse("grok-oauth:grok-2-1212");
        assert_eq!(s.provider, Provider::GrokOauth);
        assert_eq!(s.model, "grok-2-1212");
    }

    #[test]
    fn bare_grok_like_model_routes_to_grok() {
        let s = parse("grok-2-1212");
        assert_eq!(s.provider, Provider::Grok);
        assert_eq!(s.model, "grok-2-1212");
    }

    #[test]
    fn claude_oauth_prefixes_are_recognized() {
        let s = parse("claude-oauth:sonnet");
        assert_eq!(s.provider, Provider::ClaudeOauth);
        assert_eq!(s.model, "sonnet");

        let s = parse("claude-code-oauth:claude-sonnet-4-6");
        assert_eq!(s.provider, Provider::ClaudeOauth);
        assert_eq!(s.model, "claude-sonnet-4-6");
    }

    #[test]
    fn claude_bare_still_routes_to_anthropic_api() {
        assert_eq!(parse("claude-sonnet-4-6").provider, Provider::Anthropic);
    }
}
