//! Per-session recurring-prompt loops.
//!
//! A [`Loop`] is a stored schedule (spec + prompt + optional
//! expiry) attached to a single session. The daemon's
//! [`LoopRegistry`] is one in-memory map + per-session
//! `sessions/<id>/loops.json` on disk. A single tokio task
//! ([`Scheduler::run`]) wakes every second, scans for due loops,
//! and calls `SessionManager::send_input` with the prompt — the
//! adapter just sees a regular user-typed input.
//!
//! ## Lifecycle
//! - Created via `loop.create` IPC / `agentd_loop_create` tool.
//! - Listed / updated / removed via the matching IPC methods.
//! - Session deletion cascades: the per-session loops.json lives
//!   inside `sessions/<id>/` so [`crate::storage::Storage::remove_session`]
//!   takes the loops with it. The in-memory registry is purged on
//!   delete via [`LoopRegistry::drop_session`].
//!
//! ## When the session is busy
//! Loops fire by calling `send_input`, which queues into the
//! adapter's inbox channel; the adapter processes it at the next
//! turn boundary. Loops don't introspect session state.
//!
//! ## Terminal-state sessions
//! Sessions in `Done` / `Errored` skip firing (the input would
//! never be processed) but the loop stays — if the session
//! resumes, firing resumes.
//!
//! ## Expiration
//! When `now > expires_at_ms`, the scheduler removes the loop on
//! its next pass.

use construct_protocol::{Loop, LoopSpec};
use anyhow::{Context, Result};
use chrono::Utc;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

/// Minimum interval; below this is just spam. Configurable for
/// tests via `CONSTRUCT_LOOP_MIN_SECS`.
pub fn min_interval_secs() -> u64 {
    std::env::var("CONSTRUCT_LOOP_MIN_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30)
}

/// Maximum interval. Configurable via `CONSTRUCT_LOOP_MAX_SECS`.
pub fn max_interval_secs() -> u64 {
    std::env::var("CONSTRUCT_LOOP_MAX_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(24 * 3600)
}

/// Clamp a desired interval to the configured bounds. Returns the
/// resulting interval and a bool indicating whether it was
/// clamped (caller surfaces the change in the tool result).
pub fn clamp_interval(secs: u64) -> (u64, bool) {
    let min = min_interval_secs();
    let max = max_interval_secs();
    if secs < min {
        (min, true)
    } else if secs > max {
        (max, true)
    } else {
        (secs, false)
    }
}

pub struct LoopRegistry {
    data_dir: PathBuf,
    /// Loop id → loop. Single global map; lookups by session
    /// scan-and-filter, which is fine at the scale we expect
    /// (tens of loops per host).
    inner: RwLock<HashMap<String, Loop>>,
}

impl LoopRegistry {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            inner: RwLock::new(HashMap::new()),
        }
    }

    /// Walk every persisted session directory and load its
    /// `loops.json` into the in-memory map. Called once at daemon
    /// startup. Missing files are silently skipped — many sessions
    /// will never have a loop.
    pub async fn hydrate_from_disk(&self, session_ids: &[String]) {
        let mut g = self.inner.write().await;
        for sid in session_ids {
            let path = self.path_for(sid);
            if !path.exists() {
                continue;
            }
            match std::fs::read(&path) {
                Ok(bytes) => match serde_json::from_slice::<Vec<Loop>>(&bytes) {
                    Ok(loops) => {
                        for l in loops {
                            g.insert(l.id.clone(), l);
                        }
                    }
                    Err(e) => tracing::warn!(
                        session = %sid,
                        error = %e,
                        "loops.json parse failed; skipping",
                    ),
                },
                Err(e) => tracing::warn!(
                    session = %sid,
                    error = %e,
                    "loops.json read failed; skipping",
                ),
            }
        }
    }

    fn path_for(&self, session_id: &str) -> PathBuf {
        self.data_dir
            .join("sessions")
            .join(session_id)
            .join("loops.json")
    }

    /// Atomic rewrite of the session's loops.json from in-memory
    /// state. Called after every mutation.
    async fn persist_session(&self, session_id: &str) -> Result<()> {
        let g = self.inner.read().await;
        let session_loops: Vec<&Loop> = g.values().filter(|l| l.session_id == session_id).collect();
        let path = self.path_for(session_id);
        let parent = path.parent().context("loops.json parent")?;
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(&session_loops)?;
        std::fs::write(&tmp, json).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("rename {}", path.display()))?;
        Ok(())
    }

    pub async fn create(&self, mut loop_: Loop) -> Result<Loop> {
        if loop_.id.is_empty() {
            loop_.id = format!("L{}", uuid::Uuid::new_v4().simple());
        }
        let session_id = loop_.session_id.clone();
        {
            let mut g = self.inner.write().await;
            g.insert(loop_.id.clone(), loop_.clone());
        }
        self.persist_session(&session_id).await?;
        Ok(loop_)
    }

    pub async fn list(&self, session_id: Option<&str>) -> Vec<Loop> {
        let g = self.inner.read().await;
        match session_id {
            Some(sid) => g
                .values()
                .filter(|l| l.session_id == sid)
                .cloned()
                .collect(),
            None => g.values().cloned().collect(),
        }
    }

    pub async fn update(
        &self,
        loop_id: &str,
        spec: Option<LoopSpec>,
        prompt: Option<String>,
        expires_at_ms: Option<i64>,
    ) -> Result<Loop> {
        let session_id;
        let updated;
        {
            let mut g = self.inner.write().await;
            let entry = g.get_mut(loop_id).context("loop not found")?;
            if let Some(s) = spec {
                entry.spec = s;
                // Re-compute next_fire_at from now + new interval.
                let now_ms = Utc::now().timestamp_millis();
                entry.next_fire_at_ms = next_fire_after_ms(&entry.spec, now_ms);
            }
            if let Some(p) = prompt {
                entry.prompt = p;
            }
            if let Some(e) = expires_at_ms {
                entry.expires_at_ms = Some(e);
            }
            session_id = entry.session_id.clone();
            updated = entry.clone();
        }
        self.persist_session(&session_id).await?;
        Ok(updated)
    }

    pub async fn remove(&self, loop_id: &str) -> Result<()> {
        let session_id;
        {
            let mut g = self.inner.write().await;
            let entry = g.remove(loop_id).context("loop not found")?;
            session_id = entry.session_id;
        }
        self.persist_session(&session_id).await?;
        Ok(())
    }

    /// Drop every loop attached to a session (called when the
    /// session itself is deleted). The on-disk loops.json goes
    /// away with the session directory; we just clear in-memory.
    pub async fn drop_session(&self, session_id: &str) {
        let mut g = self.inner.write().await;
        g.retain(|_, l| l.session_id != session_id);
    }

    /// Return loops that should fire now (next_fire_at_ms <= now)
    /// plus loops that have expired. The scheduler post-processes
    /// each: fire-and-advance for due, drop for expired.
    pub async fn due_and_expired(&self, now_ms: i64) -> (Vec<Loop>, Vec<Loop>) {
        let g = self.inner.read().await;
        let mut due = Vec::new();
        let mut expired = Vec::new();
        for l in g.values() {
            if let Some(exp) = l.expires_at_ms {
                if now_ms > exp {
                    expired.push(l.clone());
                    continue;
                }
            }
            if l.next_fire_at_ms <= now_ms {
                due.push(l.clone());
            }
        }
        (due, expired)
    }

    /// Advance the loop's next_fire / counters after a successful
    /// fire. Persisted atomically.
    pub async fn mark_fired(&self, loop_id: &str, fired_at_ms: i64) -> Result<()> {
        let session_id;
        {
            let mut g = self.inner.write().await;
            let entry = g.get_mut(loop_id).context("loop not found")?;
            entry.last_fired_at_ms = Some(fired_at_ms);
            entry.fire_count += 1;
            entry.next_fire_at_ms = next_fire_after_ms(&entry.spec, fired_at_ms);
            session_id = entry.session_id.clone();
        }
        self.persist_session(&session_id).await
    }
}

/// Given a fire timestamp + spec, compute the next fire time.
pub fn next_fire_after_ms(spec: &LoopSpec, after_ms: i64) -> i64 {
    match spec {
        LoopSpec::Interval { seconds } => after_ms + (*seconds as i64) * 1000,
    }
}

/// Tick interval for the scheduler task. Resolution is 1 second —
/// good enough for "every N seconds/minutes/hours". Future cron
/// support might want sub-second tick if expressions allow that.
pub const SCHEDULER_TICK_MS: u64 = 1000;

/// Scheduler task: wakes every [`SCHEDULER_TICK_MS`], fires due
/// loops via `SessionManager::send_input`, drops expired ones.
pub async fn run_scheduler(
    manager: Arc<crate::session::SessionManager>,
    registry: Arc<LoopRegistry>,
) {
    let mut interval = tokio::time::interval(Duration::from_millis(SCHEDULER_TICK_MS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        let now_ms = Utc::now().timestamp_millis();
        // Flip interactive TUI sessions gone quiet to AwaitingInput (spec 0054).
        manager.poll_pty_quiescence().await;
        let (due, expired) = registry.due_and_expired(now_ms).await;
        for l in expired {
            tracing::info!(
                loop_id = %l.id,
                session = %l.session_id,
                "loop expired; removing"
            );
            if let Err(e) = registry.remove(&l.id).await {
                tracing::warn!(loop_id = %l.id, error = %e, "loop remove failed");
            }
        }
        for l in due {
            // Skip terminal sessions — the input would never be
            // processed. The loop stays in the registry so a
            // resume picks it back up. Skip also if the session
            // has been removed (get_entry returns None).
            let skip = match manager.get_entry(&l.session_id).await {
                Some(entry) => entry.snapshot_state().await.is_terminal(),
                None => true,
            };
            if skip {
                // Don't advance next_fire either — leave it alone;
                // expire will eventually GC if set.
                continue;
            }
            // Fire: synthesize a user input. The adapter's inbox
            // queues it for the next turn boundary.
            match manager.send_input(&l.session_id, l.prompt.clone()).await {
                Ok(()) => {
                    if let Err(e) = registry.mark_fired(&l.id, now_ms).await {
                        tracing::warn!(
                            loop_id = %l.id,
                            error = %e,
                            "mark_fired failed"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        loop_id = %l.id,
                        session = %l.session_id,
                        error = %e,
                        "loop send_input failed; will retry next tick"
                    );
                }
            }
        }
    }
}

/// Parse a natural-language schedule spec for the `/loop` slash
/// command. Returns `(LoopSpec, Option<expires_at_ms>, rest_of_text)`
/// or `None` if no interval token was found at the start of the
/// input — caller falls back to LLM-suggested interval.
///
/// Recognized:
/// - `every? <num><unit>` where unit is `s|sec|secs|second|seconds|
///   m|min|mins|minute|minutes|h|hour|hours|d|day|days`.
/// - `for <num><unit>` → sets expires_at = now + duration.
///
/// Example input strings:
/// - `5m hello`               → 5min interval, no expiry, prompt="hello"
/// - `every 10s hi`           → 10s interval, no expiry
/// - `30s for 5min check`     → 30s interval, expires in 5min, prompt="check"
/// - `hello`                  → None (no interval token recognized)
pub fn parse_slash_spec(input: &str, now_ms: i64) -> Option<(LoopSpec, Option<i64>, String)> {
    let mut tokens = input.split_whitespace().peekable();
    // Strip optional "every"
    if matches!(tokens.peek().copied(), Some("every")) {
        tokens.next();
    }
    let first = tokens.peek().copied()?;
    let secs = parse_duration_secs(first)?;
    tokens.next();
    let mut expires_at_ms: Option<i64> = None;
    if matches!(tokens.peek().copied(), Some("for")) {
        tokens.next();
        if let Some(t) = tokens.peek().copied() {
            if let Some(d) = parse_duration_secs(t) {
                tokens.next();
                expires_at_ms = Some(now_ms + (d as i64) * 1000);
            }
        }
    }
    let rest: Vec<&str> = tokens.collect();
    let prompt = rest.join(" ");
    Some((LoopSpec::Interval { seconds: secs }, expires_at_ms, prompt))
}

/// Parse a single duration token like `5m` / `30sec` / `2 hours`.
/// Whitespace-stripped form only (the splitter handles spaces
/// between tokens; `2 hours` arrives here as two tokens). Returns
/// `None` if unrecognized.
fn parse_duration_secs(tok: &str) -> Option<u64> {
    // Find the boundary between digits and unit.
    let split_at = tok.find(|c: char| !c.is_ascii_digit())?;
    if split_at == 0 {
        return None;
    }
    let (num_s, unit_s) = tok.split_at(split_at);
    let num: u64 = num_s.parse().ok()?;
    let mult = match unit_s.to_ascii_lowercase().as_str() {
        "s" | "sec" | "secs" | "second" | "seconds" => 1,
        "m" | "min" | "mins" | "minute" | "minutes" => 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3600,
        "d" | "day" | "days" => 86400,
        _ => return None,
    };
    num.checked_mul(mult)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minute_interval() {
        let (spec, exp, prompt) = parse_slash_spec("5m hello world", 0).unwrap();
        assert!(matches!(spec, LoopSpec::Interval { seconds: 300 }));
        assert!(exp.is_none());
        assert_eq!(prompt, "hello world");
    }

    #[test]
    fn parses_every_prefix() {
        let (spec, _, prompt) = parse_slash_spec("every 10s ping", 0).unwrap();
        assert!(matches!(spec, LoopSpec::Interval { seconds: 10 }));
        assert_eq!(prompt, "ping");
    }

    #[test]
    fn parses_for_expiry() {
        let (spec, exp, prompt) = parse_slash_spec("30s for 5min check", 1000).unwrap();
        assert!(matches!(spec, LoopSpec::Interval { seconds: 30 }));
        assert_eq!(exp, Some(1000 + 5 * 60 * 1000));
        assert_eq!(prompt, "check");
    }

    #[test]
    fn returns_none_for_unparseable() {
        assert!(parse_slash_spec("hello world", 0).is_none());
    }

    #[test]
    fn parses_hour_unit() {
        let (spec, _, prompt) = parse_slash_spec("2hours summarize the day", 0).unwrap();
        assert!(matches!(spec, LoopSpec::Interval { seconds: 7200 }));
        assert_eq!(prompt, "summarize the day");
    }

    #[test]
    fn clamp_respects_min() {
        std::env::set_var("CONSTRUCT_LOOP_MIN_SECS", "30");
        let (v, clamped) = clamp_interval(5);
        assert_eq!(v, 30);
        assert!(clamped);
        std::env::remove_var("CONSTRUCT_LOOP_MIN_SECS");
    }
}
