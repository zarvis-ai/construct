//! Shell adapter (PTY mode).
//!
//! Spawns a real PTY-attached shell so the right-hand pane in the TUI feels
//! like any other terminal: bash readline, prompts, vim, htop, ssh — they all
//! just work because they're actually running in a PTY.
//!
//! - Empty prompt → `$SHELL -il` (interactive login shell).
//! - Non-empty prompt → `$SHELL -lc <prompt>` (one-shot login shell).
//!
//! Honors `AGENTD_SHELL_BIN`, then `$SHELL`, falling back to `/bin/bash`.

use agentd_protocol::adapter::pty::{run_session, PtySpec};
use agentd_protocol::adapter::run;
use agentd_protocol::{Capabilities, InitializeResult, PtySize, SessionState};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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
    run(metadata, |params, ctx| async move {
        let shell = std::env::var("AGENTD_SHELL_BIN")
            .ok()
            .or_else(|| std::env::var("SHELL").ok())
            .unwrap_or_else(|| "/bin/bash".to_string());

        let args: Vec<String> = match params.prompt.as_deref() {
            Some(p) if !p.trim().is_empty() => {
                vec!["-lc".to_string(), p.to_string()]
            }
            _ => vec!["-il".to_string()],
        };

        let spec = PtySpec {
            bin: shell.clone(),
            args,
            cwd: PathBuf::from(&params.cwd),
            env: params.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            size: params.pty_size.unwrap_or(PtySize { cols: 100, rows: 30 }),
            status_detail: Some(shell),
        };
        let _ = SessionState::Running; // silence dead-import lint if any
        let _ = run_session(spec, ctx).await;
    })
    .await
}
