//! Idle-timeout wrapper for provider turns.
//!
//! A provider stream that never reaches its terminal event leaves the
//! zarvis turn active forever. In interactive mode that also freezes
//! the adapter-owned prompt, because the TUI only paints editor updates
//! after the adapter emits `EditorState`. Bound every provider turn so
//! a hung upstream becomes a visible error and the outer loop can return
//! to input handling.

use crate::provider::{LlmProvider, Message, ProviderTurn, TextSink, ToolSpec};
use anyhow::{anyhow, Result};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::Duration;

const ENV_PROVIDER_IDLE_TIMEOUT_SECS: &str = "AGENTD_ZARVIS_PROVIDER_IDLE_TIMEOUT_SECS";
const DEFAULT_PROVIDER_IDLE_TIMEOUT_SECS: u64 = 600;

pub async fn complete(
    provider: &dyn LlmProvider,
    model: &str,
    system: &str,
    messages: &[Message],
    tools: &[ToolSpec],
    sink: &mut dyn TextSink,
) -> Result<ProviderTurn> {
    let timeout = provider_idle_timeout();
    complete_with_idle_timeout(provider, model, system, messages, tools, sink, timeout).await
}

async fn complete_with_idle_timeout(
    provider: &dyn LlmProvider,
    model: &str,
    system: &str,
    messages: &[Message],
    tools: &[ToolSpec],
    sink: &mut dyn TextSink,
    timeout: Option<Duration>,
) -> Result<ProviderTurn> {
    let Some(timeout) = timeout else {
        return provider
            .complete(model, system, messages, tools, sink)
            .await;
    };
    let last_activity_ms = Arc::new(AtomicU64::new(now_ms()));
    let mut watchdog_sink = WatchdogSink {
        inner: sink,
        last_activity_ms: last_activity_ms.clone(),
    };
    let fut = provider.complete(model, system, messages, tools, &mut watchdog_sink);
    tokio::pin!(fut);
    loop {
        tokio::select! {
            result = &mut fut => return result,
            _ = tokio::time::sleep(timeout) => {
                let idle_for = now_ms().saturating_sub(last_activity_ms.load(Ordering::Relaxed));
                if idle_for >= timeout.as_millis() as u64 {
                    return Err(anyhow!(
                        "{} provider turn idle timed out after {}s without completing",
                        provider.name(),
                        timeout.as_secs()
                    ));
                }
            }
        }
    }
}

fn provider_idle_timeout() -> Option<Duration> {
    match std::env::var(ENV_PROVIDER_IDLE_TIMEOUT_SECS).ok() {
        Some(raw) => match raw.trim().parse::<u64>() {
            Ok(0) => None,
            Ok(secs) => Some(Duration::from_secs(secs)),
            Err(_) => Some(Duration::from_secs(DEFAULT_PROVIDER_IDLE_TIMEOUT_SECS)),
        },
        None => Some(Duration::from_secs(DEFAULT_PROVIDER_IDLE_TIMEOUT_SECS)),
    }
}

struct WatchdogSink<'a> {
    inner: &'a mut dyn TextSink,
    last_activity_ms: Arc<AtomicU64>,
}

impl TextSink for WatchdogSink<'_> {
    fn delta(&mut self, text: &str) {
        self.last_activity_ms.store(now_ms(), Ordering::Relaxed);
        self.inner.delta(text);
    }

    fn reasoning_delta(&mut self, text: &str) {
        self.last_activity_ms.store(now_ms(), Ordering::Relaxed);
        self.inner.reasoning_delta(text);
    }
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Content, Role, StopReason, Usage};
    use async_trait::async_trait;

    struct HangingProvider;

    #[async_trait]
    impl LlmProvider for HangingProvider {
        fn name(&self) -> &str {
            "stub"
        }

        async fn complete(
            &self,
            _model: &str,
            _system: &str,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _sink: &mut dyn TextSink,
        ) -> Result<ProviderTurn> {
            std::future::pending::<Result<ProviderTurn>>().await
        }
    }

    struct ImmediateProvider;

    #[async_trait]
    impl LlmProvider for ImmediateProvider {
        fn name(&self) -> &str {
            "stub"
        }

        async fn complete(
            &self,
            _model: &str,
            _system: &str,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _sink: &mut dyn TextSink,
        ) -> Result<ProviderTurn> {
            Ok(ProviderTurn {
                text: Some("ok".into()),
                tool_calls: Vec::new(),
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            })
        }
    }

    struct NullSink;

    impl TextSink for NullSink {
        fn delta(&mut self, _text: &str) {}
    }

    fn messages() -> Vec<Message> {
        vec![Message {
            role: Role::User,
            content: Content::Text {
                text: "hello".into(),
            },
        }]
    }

    #[tokio::test]
    async fn hung_provider_turn_times_out() {
        let provider = HangingProvider;
        let mut sink = NullSink;
        let err = complete_with_idle_timeout(
            &provider,
            "model",
            "system",
            &messages(),
            &[],
            &mut sink,
            Some(Duration::from_millis(5)),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn disabled_timeout_allows_provider_to_complete() {
        let provider = ImmediateProvider;
        let mut sink = NullSink;
        let turn = complete_with_idle_timeout(
            &provider,
            "model",
            "system",
            &messages(),
            &[],
            &mut sink,
            None,
        )
        .await
        .unwrap();
        assert_eq!(turn.text.as_deref(), Some("ok"));
    }
}
