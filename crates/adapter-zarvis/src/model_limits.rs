//! Persisted per-model input-token limit table.
//!
//! Zarvis treats provider-reported limits as the ground truth: when
//! a request fails with a context-overflow error we (a) extract the
//! limit out of the error body if the provider includes it, (b) save
//! the new value here, and (c) reduce-then-retry. To detect a model
//! quietly raising its limit, occasional *probe* calls run with a
//! slightly more generous budget — if that probe doesn't overflow,
//! we bump the saved limit by however many tokens the provider
//! actually accepted.
//!
//! The table lives in `state_dir/zarvis-model-limits.json` so every
//! agentd session on the same machine shares the learning. The file
//! is JSON to stay forgiving: extra keys are ignored, a corrupt file
//! falls back to defaults instead of failing the launch.

use std::collections::HashMap;
use std::path::PathBuf;

use agentd_protocol::paths::Paths;
use serde::{Deserialize, Serialize};

/// Number of seconds between probe attempts for the same model.
/// We only *consider* probing when the conversation is already close
/// to the limit, so this is an upper-bound on how often a probe
/// actually fires.
pub const PROBE_INTERVAL_SECS: i64 = 7 * 24 * 60 * 60;

/// On overflow with no extracted limit, we don't know the real cap
/// — fall back to "the request that just failed was clearly above
/// it, so drop us to a safer fraction". 0.8 = drop 20%.
pub const FALLBACK_OVERFLOW_RATIO: f64 = 0.8;

/// Probe budget = learned_limit * PROBE_OVERFLOW_RATIO. 1.2 = try
/// 20% above the learned limit. The provider either accepts (we
/// bump) or rejects (we re-prune and retry — same retry path as a
/// normal overflow).
pub const PROBE_OVERFLOW_RATIO: f64 = 1.2;

/// Probe only fires when the conversation is naturally large enough
/// to actually exceed the old limit — otherwise the probe is a
/// no-op (success tells us nothing). 0.9 = "≥90% of learned limit".
pub const PROBE_TRIGGER_RATIO: f64 = 0.9;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    pub learned_input_tokens: u64,
    /// Unix-ms of the last probe attempt (success OR overflow).
    /// 0 = never probed. Used by `should_probe`.
    #[serde(default)]
    pub last_probed_at_ms: i64,
    /// Bookkeeping for telemetry / debugging. Not consulted by the
    /// probe trigger — that's purely time + context-fill driven.
    #[serde(default)]
    pub calls_since_probe: u64,
    /// Provider/model name as it was when this entry was recorded;
    /// kept so old entries left behind by renamed models stay
    /// debuggable.
    #[serde(default)]
    pub key: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ModelLimits {
    #[serde(default)]
    pub entries: HashMap<String, ModelEntry>,
}

fn key(provider: &str, model: &str) -> String {
    format!("{provider}:{model}")
}

fn store_path() -> PathBuf {
    Paths::discover().zarvis_model_limits_file()
}

impl ModelLimits {
    pub fn load() -> Self {
        let path = store_path();
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(_) => return Self::default(),
        };
        serde_json::from_slice(&bytes).unwrap_or_default()
    }

    pub fn save(&self) {
        let path = store_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            let _ = std::fs::write(&path, json);
        }
    }

    /// Learned limit for this model, or `None` if we've never seen
    /// it before. Callers fall back to the hardcoded
    /// `context::context_window_tokens` table on `None`.
    pub fn get(&self, provider: &str, model: &str) -> Option<u64> {
        self.entries
            .get(&key(provider, model))
            .map(|e| e.learned_input_tokens)
    }

    /// Should we run THIS call as a probe? Conditions: enough wall
    /// time has passed since the last probe, AND the estimated
    /// token count is already close to the learned limit so a
    /// probe would actually test the boundary. If either condition
    /// fails the call is a normal one.
    pub fn should_probe(
        &self,
        provider: &str,
        model: &str,
        estimated_tokens: u64,
        now_ms: i64,
    ) -> bool {
        let entry = match self.entries.get(&key(provider, model)) {
            Some(e) => e,
            None => return false, // No baseline yet — no point probing.
        };
        let limit = entry.learned_input_tokens as f64;
        let threshold = (limit * PROBE_TRIGGER_RATIO) as u64;
        if estimated_tokens < threshold {
            return false;
        }
        now_ms - entry.last_probed_at_ms
            >= PROBE_INTERVAL_SECS * 1_000
    }

    /// Called after a provider returns a context-overflow error.
    /// `extracted` is the limit number parsed out of the error body
    /// (when the provider includes one — OpenAI does, Anthropic
    /// usually doesn't). If `None`, we conservatively drop to
    /// `current * FALLBACK_OVERFLOW_RATIO`.
    ///
    /// Returns the new learned limit so the caller can re-prune
    /// before retrying.
    pub fn record_overflow(
        &mut self,
        provider: &str,
        model: &str,
        extracted: Option<u64>,
        fallback: u64,
        now_ms: i64,
    ) -> u64 {
        let k = key(provider, model);
        let entry = self.entries.entry(k.clone()).or_insert_with(|| {
            ModelEntry { key: k.clone(), ..Default::default() }
        });
        let new_limit = match extracted {
            Some(n) if n > 0 => n,
            _ => {
                let base = if entry.learned_input_tokens > 0 {
                    entry.learned_input_tokens as f64
                } else {
                    fallback as f64
                };
                (base * FALLBACK_OVERFLOW_RATIO) as u64
            }
        };
        entry.learned_input_tokens = new_limit;
        entry.last_probed_at_ms = now_ms;
        entry.calls_since_probe = 0;
        self.save();
        new_limit
    }

    /// Called after a successful provider call. `actual_input_tokens`
    /// is what the provider reported in its usage block; far more
    /// accurate than our chars/3.5 estimate. If this call was a
    /// probe AND the actual usage exceeded the prior learned limit,
    /// bump the learned limit to `actual + 5%` so subsequent calls
    /// can use the headroom.
    pub fn record_call(
        &mut self,
        provider: &str,
        model: &str,
        actual_input_tokens: u64,
        was_probe: bool,
        fallback: u64,
        now_ms: i64,
    ) {
        let k = key(provider, model);
        let entry = self.entries.entry(k.clone()).or_insert_with(|| {
            ModelEntry { key: k.clone(), ..Default::default() }
        });
        if entry.learned_input_tokens == 0 {
            entry.learned_input_tokens = fallback;
        }
        if was_probe {
            entry.last_probed_at_ms = now_ms;
            entry.calls_since_probe = 0;
            // Bump only if the probe actually pushed past the prior
            // limit — otherwise the probe didn't test anything.
            if actual_input_tokens > entry.learned_input_tokens {
                entry.learned_input_tokens =
                    ((actual_input_tokens as f64) * 1.05) as u64;
            }
        } else {
            entry.calls_since_probe = entry.calls_since_probe.saturating_add(1);
        }
        self.save();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_overflow_uses_extracted_when_available() {
        let mut s = ModelLimits::default();
        let new = s.record_overflow("openai", "gpt-5", Some(350_000), 400_000, 0);
        assert_eq!(new, 350_000);
        assert_eq!(s.get("openai", "gpt-5"), Some(350_000));
    }

    #[test]
    fn record_overflow_falls_back_to_ratio_without_extraction() {
        let mut s = ModelLimits::default();
        s.record_overflow("anthropic", "claude-sonnet-4-5", None, 200_000, 0);
        // No extracted, no prior entry → fallback * 0.8.
        assert_eq!(s.get("anthropic", "claude-sonnet-4-5"), Some(160_000));
    }

    #[test]
    fn probe_only_fires_near_limit_and_after_interval() {
        let mut s = ModelLimits::default();
        s.record_overflow("openai", "gpt-5", Some(400_000), 400_000, 0);
        // Just learned the limit at t=0; not enough time passed.
        let interval_ms = PROBE_INTERVAL_SECS * 1_000;
        assert!(!s.should_probe("openai", "gpt-5", 380_000, interval_ms / 2));
        // Enough time, but conversation is small (5% of limit).
        assert!(!s.should_probe("openai", "gpt-5", 20_000, interval_ms + 1));
        // Both conditions: enough time AND near the limit.
        assert!(s.should_probe("openai", "gpt-5", 380_000, interval_ms + 1));
    }

    #[test]
    fn record_call_bumps_only_when_probe_exceeds_prior_limit() {
        let mut s = ModelLimits::default();
        s.record_overflow("openai", "gpt-5", Some(400_000), 400_000, 0);
        // Probe that didn't actually push past 400K → no bump.
        s.record_call("openai", "gpt-5", 380_000, true, 400_000, 1_000);
        assert_eq!(s.get("openai", "gpt-5"), Some(400_000));
        // Probe that pushed to 450K → bump to 450K * 1.05.
        s.record_call("openai", "gpt-5", 450_000, true, 400_000, 2_000);
        assert_eq!(s.get("openai", "gpt-5"), Some((450_000.0 * 1.05) as u64));
    }

    /// Round-trip via the same JSON shape the daemon stores on disk.
    /// Catches accidental serde-rename or required-field churn that
    /// would silently break learned limits across daemon restarts.
    #[test]
    fn serde_round_trip_preserves_learned_limit() {
        let mut s = ModelLimits::default();
        s.record_overflow("openai", "gpt-5.5", Some(272_000), 400_000, 1_700_000_000_000);
        let json = serde_json::to_string(&s).expect("serialize");
        let restored: ModelLimits =
            serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.get("openai", "gpt-5.5"), Some(272_000));
        let e = restored
            .entries
            .get("openai:gpt-5.5")
            .expect("entry present");
        assert_eq!(e.learned_input_tokens, 272_000);
        assert_eq!(e.last_probed_at_ms, 1_700_000_000_000);
        assert_eq!(e.key, "openai:gpt-5.5");
    }

    /// Forward-compat: extra/unknown fields in the on-disk JSON must
    /// not break the load. `entries` is the only required field; the
    /// rest of `ModelEntry` is `#[serde(default)]`.
    #[test]
    fn load_tolerates_unknown_fields_and_missing_optionals() {
        let json = r#"{
            "entries": {
                "openai:gpt-5": {
                    "learned_input_tokens": 350000,
                    "future_field": "ignored"
                }
            },
            "future_top_level_field": 42
        }"#;
        let s: ModelLimits = serde_json::from_str(json).expect("forward-compat");
        assert_eq!(s.get("openai", "gpt-5"), Some(350_000));
    }

    /// Corrupt / empty file path: `load()` must NOT panic. Used by
    /// agent.rs and interactive.rs at session start; failure there
    /// would block every zarvis session from running.
    #[test]
    fn from_garbage_falls_back_to_default() {
        let s: ModelLimits = serde_json::from_str("not json").unwrap_or_default();
        assert!(s.entries.is_empty());
        let s: ModelLimits = serde_json::from_str("").unwrap_or_default();
        assert!(s.entries.is_empty());
    }
}
