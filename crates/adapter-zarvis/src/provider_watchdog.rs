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
const ENV_PROVIDER_RETRY_ATTEMPTS: &str = "AGENTD_ZARVIS_PROVIDER_RETRY_ATTEMPTS";
/// Idle = no stream activity from upstream (see `WatchdogSink`: any
/// `delta` / `reasoning_delta` / `progress` ping resets it). 90s leaves
/// headroom for the gap between the first event and the first content on
/// a large context under load (and cold local-model loads) while still
/// surfacing a stalled connection in ~1.5 min instead of freezing the
/// turn for minutes. Override with the env var (`0` disables).
const DEFAULT_PROVIDER_IDLE_TIMEOUT_SECS: u64 = 90;
const DEFAULT_PROVIDER_RETRY_ATTEMPTS: usize = 3;
const PROVIDER_RETRY_BASE_DELAY_MS: u64 = 500;
const PROVIDER_RETRY_MAX_DELAY_MS: u64 = 4_000;

pub async fn complete(
    provider: &dyn LlmProvider,
    model: &str,
    system: &str,
    messages: &[Message],
    tools: &[ToolSpec],
    sink: &mut dyn TextSink,
) -> Result<ProviderTurn> {
    let timeout = provider_idle_timeout();
    let retry = RetryConfig {
        max_attempts: provider_retry_attempts(),
        base_delay: Duration::from_millis(PROVIDER_RETRY_BASE_DELAY_MS),
        max_delay: Duration::from_millis(PROVIDER_RETRY_MAX_DELAY_MS),
    };
    complete_with_retries(
        provider, model, system, messages, tools, sink, timeout, retry,
    )
    .await
}

#[derive(Clone, Copy)]
struct RetryConfig {
    max_attempts: usize,
    base_delay: Duration,
    max_delay: Duration,
}

async fn complete_with_retries(
    provider: &dyn LlmProvider,
    model: &str,
    system: &str,
    messages: &[Message],
    tools: &[ToolSpec],
    sink: &mut dyn TextSink,
    timeout: Option<Duration>,
    retry: RetryConfig,
) -> Result<ProviderTurn> {
    let max_attempts = retry.max_attempts.max(1);
    let mut attempt = 1usize;
    loop {
        let (result, visible_output) = {
            let mut retry_sink = RetryTrackingSink {
                inner: sink,
                visible_output: false,
            };
            let result = complete_with_idle_timeout(
                provider,
                model,
                system,
                messages,
                tools,
                &mut retry_sink,
                timeout,
            )
            .await;
            (result, retry_sink.visible_output)
        };

        match result {
            Ok(turn) => return Ok(turn),
            Err(err) => {
                if attempt >= max_attempts || visible_output || !is_retryable_provider_error(&err) {
                    return Err(err);
                }
                let delay = retry_delay(retry, attempt);
                tracing::warn!(
                    provider = provider.name(),
                    model,
                    attempt,
                    max_attempts,
                    delay_ms = delay.as_millis() as u64,
                    error = %format!("{err:#}"),
                    "retrying transient provider error"
                );
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
        }
    }
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

fn provider_retry_attempts() -> usize {
    match std::env::var(ENV_PROVIDER_RETRY_ATTEMPTS).ok() {
        Some(raw) => match raw.trim().parse::<usize>() {
            Ok(0 | 1) => 1,
            Ok(n) => n.min(8),
            Err(_) => DEFAULT_PROVIDER_RETRY_ATTEMPTS,
        },
        None => DEFAULT_PROVIDER_RETRY_ATTEMPTS,
    }
}

fn retry_delay(retry: RetryConfig, failed_attempt: usize) -> Duration {
    let multiplier = 1u32 << failed_attempt.saturating_sub(1).min(10);
    retry
        .base_delay
        .saturating_mul(multiplier)
        .min(retry.max_delay)
}

fn is_retryable_provider_error(err: &anyhow::Error) -> bool {
    if err
        .downcast_ref::<crate::provider::ContextOverflow>()
        .is_some()
    {
        return false;
    }
    let msg = format!("{err:#}").to_ascii_lowercase();
    if msg.contains("context window")
        || msg.contains("context overflow")
        || msg.contains("context length")
        || msg.contains("input exceeds")
        || msg.contains("input is too long")
        || msg.contains("prompt is too long")
        || msg.contains("too many tokens")
        || msg.contains("401")
        || msg.contains("unauthorized")
        || msg.contains("access token")
        || msg.contains("authentication")
    {
        return false;
    }
    msg.contains("provider turn idle timed out")
        || msg.contains("sse stream")
        || msg.contains("stream ended before")
        || msg.contains("transport error")
        || msg.contains("error decoding response body")
        || msg.contains("connection refused")
        || msg.contains("connection reset")
        || msg.contains("connection timeout")
        || msg.contains("timed out")
        || msg.contains("timeout")
        || msg.contains("too many requests")
        || msg.contains("rate limit")
        || msg.contains("rate_limit")
        || msg.contains("429")
        || msg.contains("500")
        || msg.contains("502")
        || msg.contains("503")
        || msg.contains("504")
        || msg.contains("service unavailable")
        || msg.contains("temporarily unavailable")
        || msg.contains("overloaded")
        || msg.contains("overload")
}

struct RetryTrackingSink<'a> {
    inner: &'a mut dyn TextSink,
    visible_output: bool,
}

impl TextSink for RetryTrackingSink<'_> {
    fn delta(&mut self, text: &str) {
        if !text.is_empty() {
            self.visible_output = true;
        }
        self.inner.delta(text);
    }

    fn reasoning_delta(&mut self, text: &str) {
        if !text.is_empty() {
            self.visible_output = true;
        }
        self.inner.reasoning_delta(text);
    }

    fn progress(&mut self) {
        self.inner.progress();
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

    fn progress(&mut self) {
        // Any stream event (even text-less ones) counts as liveness, so a
        // turn streaming only tool-call arguments — or just upstream
        // keepalives — isn't mistaken for a stall.
        self.last_activity_ms.store(now_ms(), Ordering::Relaxed);
        self.inner.progress();
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
                reasoning_items: Vec::new(),
            })
        }
    }

    struct FlakyProvider {
        failures_left: std::sync::atomic::AtomicUsize,
        message: &'static str,
        emits_before_failure: bool,
    }

    #[async_trait]
    impl LlmProvider for FlakyProvider {
        fn name(&self) -> &str {
            "stub"
        }

        async fn complete(
            &self,
            _model: &str,
            _system: &str,
            _messages: &[Message],
            _tools: &[ToolSpec],
            sink: &mut dyn TextSink,
        ) -> Result<ProviderTurn> {
            if self
                .failures_left
                .fetch_update(
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                    |n| n.checked_sub(1),
                )
                .is_ok()
            {
                if self.emits_before_failure {
                    sink.delta("partial");
                }
                return Err(anyhow!(self.message));
            }
            Ok(ProviderTurn {
                text: Some("ok".into()),
                tool_calls: Vec::new(),
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
                reasoning_items: Vec::new(),
            })
        }
    }

    /// Streams only `progress()` pings (no text/reasoning) `pings` times
    /// at `interval`, then completes — models a turn that's alive on the
    /// wire (e.g. streaming tool-call args / keepalives) but emits no text.
    struct ProgressOnlyProvider {
        pings: u32,
        interval: Duration,
    }

    #[async_trait]
    impl LlmProvider for ProgressOnlyProvider {
        fn name(&self) -> &str {
            "stub"
        }

        async fn complete(
            &self,
            _model: &str,
            _system: &str,
            _messages: &[Message],
            _tools: &[ToolSpec],
            sink: &mut dyn TextSink,
        ) -> Result<ProviderTurn> {
            for _ in 0..self.pings {
                tokio::time::sleep(self.interval).await;
                sink.progress();
            }
            Ok(ProviderTurn {
                text: Some("ok".into()),
                tool_calls: Vec::new(),
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
                reasoning_items: Vec::new(),
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

    #[tokio::test]
    async fn progress_pings_keep_a_text_less_stream_alive() {
        // Pings every 10ms for ~100ms — far past the 30ms idle timeout —
        // with no text/reasoning deltas. Because each gap is under the
        // timeout, the watchdog must treat the turn as live and let it
        // finish, not kill it as idle.
        let provider = ProgressOnlyProvider {
            pings: 10,
            interval: Duration::from_millis(10),
        };
        let mut sink = NullSink;
        let turn = complete_with_idle_timeout(
            &provider,
            "model",
            "system",
            &messages(),
            &[],
            &mut sink,
            Some(Duration::from_millis(30)),
        )
        .await
        .unwrap();
        assert_eq!(turn.text.as_deref(), Some("ok"));
    }

    #[tokio::test]
    async fn retries_transient_error_before_visible_output() {
        let provider = FlakyProvider {
            failures_left: std::sync::atomic::AtomicUsize::new(2),
            message: "codex-oauth: 503 Service Unavailable from chatgpt.com",
            emits_before_failure: false,
        };
        let mut sink = NullSink;
        let turn = complete_with_retries(
            &provider,
            "model",
            "system",
            &messages(),
            &[],
            &mut sink,
            Some(Duration::from_secs(1)),
            RetryConfig {
                max_attempts: 3,
                base_delay: Duration::ZERO,
                max_delay: Duration::ZERO,
            },
        )
        .await
        .unwrap();
        assert_eq!(turn.text.as_deref(), Some("ok"));
    }

    #[tokio::test]
    async fn does_not_retry_context_overflow() {
        let provider = FlakyProvider {
            failures_left: std::sync::atomic::AtomicUsize::new(1),
            message: "Your input exceeds the context window of this model",
            emits_before_failure: false,
        };
        let mut sink = NullSink;
        let err = complete_with_retries(
            &provider,
            "model",
            "system",
            &messages(),
            &[],
            &mut sink,
            Some(Duration::from_secs(1)),
            RetryConfig {
                max_attempts: 3,
                base_delay: Duration::ZERO,
                max_delay: Duration::ZERO,
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("context window"));
        assert_eq!(
            provider
                .failures_left
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    #[tokio::test]
    async fn does_not_retry_after_visible_output() {
        let provider = FlakyProvider {
            failures_left: std::sync::atomic::AtomicUsize::new(1),
            message: "codex-oauth SSE stream: Transport error",
            emits_before_failure: true,
        };
        let mut sink = NullSink;
        let err = complete_with_retries(
            &provider,
            "model",
            "system",
            &messages(),
            &[],
            &mut sink,
            Some(Duration::from_secs(1)),
            RetryConfig {
                max_attempts: 3,
                base_delay: Duration::ZERO,
                max_delay: Duration::ZERO,
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("SSE stream"));
        assert_eq!(
            provider
                .failures_left
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }
}
