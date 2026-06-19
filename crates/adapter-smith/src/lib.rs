//! Smith — construct's built-in multi-provider agent harness.
//!
//! Talks to OpenAI / Anthropic / Gemini / Ollama directly (no vendor CLI required),
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
mod sandbox;
mod provider_watchdog;
mod skills;
mod tasks;
mod title_mode;
mod tools;

use agentd_protocol::adapter::run as adapter_run;
use agentd_protocol::{Capabilities, InitializeResult, SessionEvent, SessionStartParams};

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
        // TUI always does), else headless (so `construct new smith "..."`
        // from a non-TUI client gets the structured stream).
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
        let resolved = match agent::resolve_model(&params) {
            Ok(r) => r,
            Err(e) => {
                ctx.emit.emit(SessionEvent::Error {
                    message: format!(
                        "{e}\n\nsmith needs one of: CONSTRUCT_SMITH_MODEL set, \
                         ANTHROPIC_API_KEY set, OPENAI_API_KEY set, \
                         GEMINI_API_KEY set, or a local Ollama (set OLLAMA_HOST \
                         if not at localhost:11434)."
                    ),
                });
                ctx.emit.emit(SessionEvent::Done { exit_code: 2 });
                return;
            }
        };
        let mode = resolve_mode(&params);
        let result = match mode {
            Mode::Interactive => interactive::run(params, ctx, resolved).await,
            Mode::Headless => agent::run(params, ctx, resolved).await,
        };
        if let Err(e) = result {
            tracing::warn!(error = ?e, "smith agent loop returned with error");
        }
    })
    .await
}

pub async fn run_title_mode(prompt: &str) -> anyhow::Result<()> {
    match title_mode::suggest_title(prompt).await {
        Ok(title) => { println!("{title}"); Ok(()) }
        Err(e) => { eprintln!("title-mode failed: {e}"); std::process::exit(1); }
    }
}
