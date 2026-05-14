//! Zarvis — agentd's built-in multi-provider agent harness.
//!
//! Talks to OpenAI / Anthropic / Ollama directly (no vendor CLI required),
//! runs its own agent loop, and executes shell + filesystem +
//! agentd-control tools on the model's behalf. See README for the full
//! design.

use agentd_protocol::adapter::run;
use agentd_protocol::{Capabilities, InitializeResult};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let metadata = InitializeResult {
        name: "zarvis".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        capabilities: Capabilities {
            supports_input: true,
            supports_interrupt: true,
            supports_cost: true,
            ..Default::default()
        },
    };
    run(metadata, |_params, _ctx| async move {
        // Implementation lands in subsequent commits — provider plumbing,
        // tool registry, agent loop.
    })
    .await
}
