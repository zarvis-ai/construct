//! Smith — construct's built-in multi-provider agent harness.
//!
//! Talks to OpenAI / Anthropic / Gemini / Meta / Ollama directly (no vendor CLI required),
//! runs its own agent loop, and executes shell + filesystem +
//! agentd-control tools on the model's behalf. See README for the full
//! design.

mod agent;
mod compact;
mod context;
mod hooks;
mod interactive;
mod interval_suggest;
mod model_limits;
mod observe;
mod persist;
mod project_guide;
mod provider;
mod provider_watchdog;
mod sandbox;
mod skills;
mod tasks;
mod title_mode;
mod tools;

use construct_protocol::adapter::run as adapter_run;
use construct_protocol::{Capabilities, InitializeResult, SessionEvent, SessionStartParams};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Interactive,
    Headless,
}

fn resolve_mode(params: &SessionStartParams) -> Mode {
    if let Ok(m) = std::env::var("CONSTRUCT_SMITH_MODE") {
        match m.as_str() {
            "interactive" => return Mode::Interactive,
            "headless" => return Mode::Headless,
            _ => {}
        }
    }
    match params.mode.as_deref() {
        Some("interactive") => Mode::Interactive,
        Some("headless") => Mode::Headless,
        // Default: interactive when the client supplied a PTY size (the
        // TUI always does), else headless. The `construct new` CLI passes an
        // explicit mode, so this fallback is for other IPC clients.
        _ if params.pty_size.is_some() => Mode::Interactive,
        _ => Mode::Headless,
    }
}

pub async fn run() -> anyhow::Result<()> {
    let metadata = InitializeResult {
        name: "smith".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        capabilities: Capabilities {
            supports_input: true,
            supports_interrupt: true,
            supports_cost: true,
            supports_pty: true,
            supports_silent_resume: true,
            ..Default::default()
        },
    };
    adapter_run(metadata, |params, ctx| async move {
        let mode = resolve_mode(&params);
        let is_orchestrator =
            std::env::var("CONSTRUCT_SESSION_KIND").as_deref() == Ok("orchestrator");
        let resolved = agent::resolve_model(&params);

        // Orchestrator exception (spec 0071): the minibuffer's smith session
        // must keep serving slash commands — which never touch the provider
        // (`/construct restart` is the case this exists for) — even when no
        // model resolves. Hard-exiting here the way an ordinary session does
        // would take the whole fleet-dispatch surface down on a keyless
        // machine. `interactive::run` re-attempts resolution lazily on the
        // first turn that actually needs a model.
        if is_orchestrator && matches!(mode, Mode::Interactive) {
            let resolved =
                resolved.map_err(|e| model_startup_error_message(&params, &e.to_string()));
            if let Err(message) = &resolved {
                ctx.emit.emit(SessionEvent::Error {
                    message: message.clone(),
                });
            }
            if let Err(e) = interactive::run(params, ctx, resolved).await {
                tracing::warn!(error = ?e, "smith agent loop returned with error");
            }
            return;
        }

        let resolved = match resolved {
            Ok(r) => r,
            Err(e) => {
                ctx.emit.emit(SessionEvent::Error {
                    message: model_startup_error_message(&params, &e.to_string()),
                });
                ctx.emit.emit(SessionEvent::Done { exit_code: 2 });
                return;
            }
        };
        let result = match mode {
            Mode::Interactive => interactive::run(params, ctx, Ok(resolved)).await,
            Mode::Headless => agent::run(params, ctx, resolved).await,
        };
        if let Err(e) = result {
            tracing::warn!(error = ?e, "smith agent loop returned with error");
        }
    })
    .await
}

fn model_startup_error_message(params: &SessionStartParams, error: &str) -> String {
    let mut msg = String::new();
    msg.push_str("smith could not start because the configured model is not usable.");
    msg.push_str("\n\nTried model: ");
    msg.push_str(&model_hint(params));
    msg.push_str("\n\nProvider error:\n");
    msg.push_str(error);

    let lower = error.to_lowercase();
    if lower.contains("no auto-detected smith credential") {
        msg.push_str(
            "\n\nAction: run `/configure` in the construct TUI (or `M-x configure`) to see \
             every auth method smith supports, its live status, and how to set it up — or set \
             `CONSTRUCT_SMITH_MODEL` / `--model` explicitly.",
        );
    } else if lower.contains("grok auth token") && lower.contains("expired") {
        msg.push_str(
            "\n\nAction: run `grok login`, then restart this session again. \
             Alternatively set `GROK_API_KEY` or `XAI_API_KEY` before restarting.",
        );
    } else if lower.contains("grok provider requires grok_api_key or xai_api_key") {
        msg.push_str(
            "\n\nAction: set `GROK_API_KEY` or `XAI_API_KEY`, or use `grok-oauth:<model>` \
             after running `grok login`.",
        );
    } else if lower.contains("anthropic_api_key") {
        msg.push_str("\n\nAction: set `ANTHROPIC_API_KEY` or switch smith to another model.");
    } else if lower.contains("openai_api_key") {
        msg.push_str("\n\nAction: set `OPENAI_API_KEY` or switch smith to another model.");
    } else if lower.contains("gemini_api_key") || lower.contains("google_api_key") {
        msg.push_str(
            "\n\nAction: set `GEMINI_API_KEY` or `GOOGLE_API_KEY`, or switch smith to another model.",
        );
    } else if lower.contains("meta_api_key") || lower.contains("model_api_key") {
        msg.push_str(
            "\n\nAction: set `META_API_KEY` or `MODEL_API_KEY`, or switch smith to another model.",
        );
    } else if lower.contains("ollama") {
        msg.push_str("\n\nAction: start Ollama or set `OLLAMA_HOST` to the running Ollama server.");
    } else {
        msg.push_str(
            "\n\nAction: fix credentials for the tried model, or start/fork a smith session \
             with a different model.",
        );
    }

    msg.push_str(
        "\n\nsmith needs one of: `CONSTRUCT_SMITH_MODEL`, `ANTHROPIC_API_KEY`, \
         `OPENAI_API_KEY`, `GEMINI_API_KEY`, `META_API_KEY`/`MODEL_API_KEY`, `GROK_API_KEY`/`XAI_API_KEY`, \
         a valid Grok OAuth login, or a local Ollama. Run `/configure` in the construct TUI \
         (or `M-x configure`) to check status and pick one.",
    );
    msg
}

fn model_hint(params: &SessionStartParams) -> String {
    params
        .model
        .clone()
        .or_else(|| params.env.get("CONSTRUCT_SMITH_MODEL").cloned())
        .or_else(|| std::env::var("CONSTRUCT_SMITH_MODEL").ok())
        .unwrap_or_else(|| "(auto-detect from available credentials)".to_string())
}

pub async fn run_title_mode(prompt: &str) -> anyhow::Result<()> {
    match title_mode::suggest_title(prompt).await {
        Ok(title) => {
            println!("{title}");
            Ok(())
        }
        Err(e) => {
            eprintln!("title-mode failed: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn params_with_model(model: &str) -> SessionStartParams {
        SessionStartParams {
            session_id: "s1".into(),
            cwd: "/tmp".into(),
            prompt: None,
            model: Some(model.into()),
            mode: None,
            pty_size: None,
            env: HashMap::new(),
            args: Vec::new(),
        }
    }

    #[test]
    fn startup_error_mentions_expired_grok_recovery() {
        let msg = model_startup_error_message(
            &params_with_model("grok-oauth:grok-build-0.1"),
            "grok auth token in /Users/moon/.grok/auth.json is expired; run `grok login` or set GROK_API_KEY/XAI_API_KEY.",
        );

        assert!(msg.contains("Tried model: grok-oauth:grok-build-0.1"));
        assert!(msg.contains("run `grok login`"));
        assert!(msg.contains("restart this session again"));
        assert!(msg.contains("GROK_API_KEY"));
        assert!(msg.contains("XAI_API_KEY"));
    }
}
