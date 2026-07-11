//! Shell adapter (PTY mode).
//!
//! Spawns a real PTY-attached shell so the right-hand pane in the TUI feels
//! like any other terminal: bash readline, prompts, vim, htop, ssh — they all
//! just work because they're actually running in a PTY.
//!
//! - Empty prompt → `$SHELL -il` (interactive login shell).
//! - Non-empty prompt → `$SHELL -lc <prompt>` (one-shot login shell).
//!
//! Honors `CONSTRUCT_SHELL_CMD` for a full command prefix, falling back to
//! `CONSTRUCT_SHELL_BIN`, then `$SHELL`, then `/bin/bash`.

use construct_protocol::adapter::pty::{run_session, PtySpec};
use construct_protocol::adapter::run as adapter_run;
use construct_protocol::{Capabilities, InitializeResult, PtySize, SessionState};
use std::path::PathBuf;

pub async fn run() -> anyhow::Result<()> {
    let metadata = InitializeResult {
        name: "shell".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        capabilities: Capabilities {
            supports_input: true,
            supports_interrupt: true,
            supports_pty: true,
            ..Default::default()
        },
    };
    adapter_run(metadata, |params, ctx| async move {
        let default_shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        let command = construct_protocol::adapter::resolve_command_override(
            "CONSTRUCT_SHELL_CMD",
            "CONSTRUCT_SHELL_BIN",
            &default_shell,
        );

        // On daemon-restart resume: ignore the original one-shot prompt
        // (it already ran in the previous incarnation). Re-spawn a fresh
        // interactive login shell in the same cwd so the user can keep
        // working.
        let resuming = std::env::var("CONSTRUCT_RESUME").as_deref() == Ok("1");
        let mut args: Vec<String> = command.args.clone();
        match params.prompt.as_deref() {
            Some(p) if !p.trim().is_empty() && !resuming => {
                args.extend(["-lc".to_string(), p.to_string()]);
            }
            _ => args.push("-il".to_string()),
        }

        let label = command.argv_preview();
        let bin = command.bin;
        let spec = PtySpec {
            bin,
            args,
            cwd: PathBuf::from(&params.cwd),
            env: params
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            size: params.pty_size.unwrap_or(PtySize {
                cols: 100,
                rows: 30,
            }),
            status_detail: Some(label),
            // Shell is line-oriented: detect the prompt via the foreground group.
            detect_prompt_via_pgroup: true,
        };
        let _ = SessionState::Running; // silence dead-import lint if any
        let _ = run_session(spec, ctx).await;
    })
    .await
}
