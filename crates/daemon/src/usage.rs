//! Daemon-side cache for harness usage-probe captures (spec 0086).
//!
//! One [`UsageSnapshot`] per harness: the raw PTY bytes the harness's own
//! usage/status slash command rendered, captured by a short-lived
//! `SessionKind::UsageProbe` session (see `session::usage_probe`) and kept
//! around for [`USAGE_CACHE_TTL`]. In-memory only — losing it on daemon
//! restart is fine, a later query just re-probes.
//!
//! Structured the same way as [`crate::availability::AvailabilityCache`]:
//! a plain, non-async struct behind a `std::sync::Mutex` on
//! `SessionManager`, so every critical section is a tiny read/write never
//! held across an `.await`.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

pub const USAGE_CACHE_TTL: Duration = Duration::from_secs(300);

/// One cached usage-probe capture for a harness.
#[derive(Debug, Clone)]
pub struct UsageSnapshot {
    /// Raw PTY bytes the harness's usage/status command rendered.
    /// Deliberately unparsed — see spec 0086's "redisplay verbatim, never
    /// parse" decision.
    pub bytes: Vec<u8>,
    pub cols: u16,
    pub rows: u16,
    /// Monotonic capture time, used for the TTL check ([`Self::is_fresh`]).
    pub captured_at: Instant,
    /// Wall-clock capture time (Unix epoch ms), sent to clients so they can
    /// display "captured N minutes ago". Not used for TTL logic — `Instant`
    /// is immune to wall-clock adjustments.
    pub captured_at_ms: i64,
}

impl UsageSnapshot {
    pub fn is_fresh(&self) -> bool {
        self.captured_at.elapsed() < USAGE_CACHE_TTL
    }
}

/// Per-harness cache of the most recent usage-probe capture, plus an
/// in-flight guard so concurrent triggers (e.g. two rapid hovers) for the
/// same harness dedupe into a single probe.
#[derive(Default)]
pub struct UsageCache {
    snapshots: HashMap<String, UsageSnapshot>,
    refreshing: HashSet<String>,
}

impl UsageCache {
    /// The cached snapshot for `harness`, if any — regardless of
    /// freshness. Callers decide whether a stale-but-present snapshot is
    /// still worth returning while a refresh is in flight.
    pub fn get(&self, harness: &str) -> Option<UsageSnapshot> {
        self.snapshots.get(harness).cloned()
    }

    pub fn is_refreshing(&self, harness: &str) -> bool {
        self.refreshing.contains(harness)
    }

    /// Claim the in-flight slot for `harness`. Returns `true` if this call
    /// claimed it (caller should proceed to probe), `false` if a refresh
    /// was already in flight (caller should not spawn another one).
    pub fn try_begin_refresh(&mut self, harness: &str) -> bool {
        self.refreshing.insert(harness.to_string())
    }

    pub fn finish_refresh(&mut self, harness: &str) {
        self.refreshing.remove(harness);
    }

    pub fn store(&mut self, harness: &str, snapshot: UsageSnapshot) {
        self.snapshots.insert(harness.to_string(), snapshot);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_begin_refresh_dedupes_concurrent_triggers() {
        let mut cache = UsageCache::default();
        assert!(cache.try_begin_refresh("claude"));
        assert!(!cache.try_begin_refresh("claude"), "second trigger must not claim the slot too");
        assert!(cache.is_refreshing("claude"));
        cache.finish_refresh("claude");
        assert!(!cache.is_refreshing("claude"));
        assert!(cache.try_begin_refresh("claude"), "slot is claimable again after finishing");
    }

    #[test]
    fn store_and_get_roundtrip() {
        let mut cache = UsageCache::default();
        assert!(cache.get("codex").is_none());
        cache.store(
            "codex",
            UsageSnapshot {
                bytes: b"hello".to_vec(),
                cols: 80,
                rows: 24,
                captured_at: Instant::now(),
                captured_at_ms: 0,
            },
        );
        let snap = cache.get("codex").expect("stored snapshot");
        assert_eq!(snap.bytes, b"hello");
        assert!(snap.is_fresh());
    }
}
