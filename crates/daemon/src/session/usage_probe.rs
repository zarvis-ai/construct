//! Harness usage-probe orchestration (spec 0086): `usage.query`'s
//! background half. See `crate::usage` for the cache types this module
//! populates.
//!
//! The shape deliberately mirrors the post-resume force-redraw wait
//! (`lifecycle::resume_redraw_ready` + its poll loop): a pure decision fn
//! polled on an interval against a session's `last_pty_at_ms`, capped by a
//! hard timeout. Everything that awaits (session create, submitting the
//! probe command, sleeps, pty_replay, delete) runs with no `usage_cache`
//! lock held — the lock is only ever taken for the tiny read/write
//! critical sections in `usage_query` and `refresh_usage`.

use super::*;
use std::path::Path;

/// Poll interval while waiting for a probe session's PTY to go quiet.
const USAGE_PROBE_POLL_INTERVAL: Duration = Duration::from_millis(200);
/// How long the PTY must stay quiet before the harness's startup draw is
/// stable enough to accept input. This deliberately stays short: several
/// interactive harnesses keep emitting periodic idle-screen updates, so a
/// long silence requirement prevents the probe command from ever being
/// sent. Command output has its own longer settle window below.
const USAGE_PROBE_STARTUP_SETTLE: Duration = Duration::from_millis(500);
/// How long the PTY must stay quiet before the usage command's response is
/// considered finished. This is intentionally much longer than the startup
/// threshold: grok's usage panel can take several seconds to replace its
/// initial draw with the real usage numbers.
const USAGE_PROBE_COMMAND_SETTLE: Duration = Duration::from_secs(10);
/// Hard cap on waiting for the harness to finish its own startup draw
/// before giving up on the whole probe — a hung/slow-starting harness must
/// not wedge the probe forever.
const USAGE_PROBE_STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
/// Hard cap on waiting for the usage/status command's response. Longer
/// than the startup cap since some harnesses' usage panels fetch live
/// account data over the network.
const USAGE_PROBE_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
/// Poll interval while waiting for the pasted probe command to echo back
/// in the PTY output before the submit Enter is sent.
const USAGE_PROBE_ECHO_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Hard cap on waiting for the paste echo. Generous relative to how fast a
/// responsive harness echoes (tens of ms) precisely because the case this
/// gate exists for is a harness still busy with its own startup work and
/// slow to drain stdin. On timeout the Enter is sent anyway — a missing
/// echo means the submission will likely fail validation and be retried,
/// not that sending Enter could make anything worse.
const USAGE_PROBE_ECHO_TIMEOUT: Duration = Duration::from_secs(5);
/// Fixed PTY size for probe sessions — generous, since there's no live
/// client to negotiate a real size against.
const USAGE_PROBE_COLS: u16 = 100;
const USAGE_PROBE_ROWS: u16 = 40;
/// Extra idle wait inserted before each retry beyond the first, scaled by
/// attempt number (2s before attempt 2, 4s before attempt 3, ...). See the
/// submit-retry loop's own doc comment in `run_usage_probe` for why real
/// additional elapsed time — not just re-trying — is what actually
/// recovers a harness whose startup interference is consistent rather than
/// occasional.
const USAGE_PROBE_RETRY_BACKOFF: Duration = Duration::from_secs(2);
/// How many times to (re-)submit the probe command in the same session
/// before giving up. Confirmed live this needs to be more than 1: a fresh
/// `SessionKind::UsageProbe` session cold-starts the harness from scratch,
/// so a harness whose own startup work consistently outlasts
/// `USAGE_PROBE_SETTLE` races identically on every single attempt — 402
/// consecutive failures, 0 successes, over 46 minutes, confirmed live
/// before this retry loop existed (spec 0086). Retrying in the *same*
/// session with backoff is what breaks that determinism.
const USAGE_PROBE_SUBMIT_ATTEMPTS: u32 = 3;
/// Last-resort ceiling on one probe attempt's total wall-clock time, in
/// [`SessionManager::refresh_usage`] — comfortably above the legitimate
/// worst case (adapter spawn's own 60s request timeout, the 20s startup
/// wait, up to `USAGE_PROBE_SUBMIT_ATTEMPTS` rounds of a 5s echo wait plus
/// a 30s response wait plus growing backoff between them, ≈191s) so a
/// harness can never be stuck showing "probing…" indefinitely even if
/// something not covered by those individual bounds goes wrong.
const PROBE_HARD_CEILING: Duration = Duration::from_secs(240);

impl SessionManager {
    /// `usage.query` (spec 0086). Read-mostly: never blocks on the probe
    /// itself. Returns the most recently cached snapshot for `harness`
    /// (regardless of freshness — the TTL only gates whether a new probe is
    /// warranted, not whether the last one is still returned) plus whether
    /// a refresh is in flight. When `allow_refresh` is set and the cache is
    /// stale-or-missing and nothing is already running for this harness, a
    /// background probe is spawned and this call still returns immediately.
    pub async fn usage_query(
        self: &Arc<Self>,
        harness: &str,
        allow_refresh: bool,
    ) -> construct_protocol::UsageQueryResult {
        let Some(command) = self
            .config
            .effective_usage_probe(harness)
            .map(str::to_string)
        else {
            return construct_protocol::UsageQueryResult {
                snapshot: None,
                refreshing: false,
                enabled: false,
            };
        };

        let (snapshot, mut refreshing) = {
            let cache = self
                .usage_cache
                .lock()
                .expect("usage_cache mutex poisoned");
            (cache.get(harness), cache.is_refreshing(harness))
        };
        let stale = snapshot.as_ref().map(|s| !s.is_fresh()).unwrap_or(true);
        if allow_refresh && stale && !refreshing {
            let began = {
                let mut cache = self
                    .usage_cache
                    .lock()
                    .expect("usage_cache mutex poisoned");
                cache.try_begin_refresh(harness)
            };
            if began {
                refreshing = true;
                let mgr = self.clone();
                let harness_owned = harness.to_string();
                tokio::spawn(async move {
                    mgr.refresh_usage(harness_owned, command).await;
                });
            }
        }

        construct_protocol::UsageQueryResult {
            snapshot: snapshot.map(|s| construct_protocol::UsageSnapshotInfo {
                bytes: base64::engine::general_purpose::STANDARD.encode(&s.bytes),
                cols: s.cols,
                rows: s.rows,
                captured_at_ms: s.captured_at_ms,
            }),
            refreshing,
            enabled: true,
        }
    }

    /// Run one usage probe for `harness` and update the cache. Always
    /// clears the in-flight guard on the way out — including on any
    /// failure — so a later query just retries; no snapshot is stored on
    /// failure, which is exactly "not cached" from `usage_query`'s view.
    async fn refresh_usage(self: Arc<Self>, harness: String, command: String) {
        // `finish_refresh` (clearing the in-flight guard) MUST run no
        // matter what happens inside the probe — otherwise a harness gets
        // permanently stuck showing "probing…" until the next daemon
        // restart, since `usage_query` never spawns a second attempt while
        // the guard is still set. Two distinct failure modes are guarded
        // against here, neither of which the individual timeouts inside
        // `run_usage_probe` cover on their own:
        //
        // 1. A panic anywhere inside `run_usage_probe` (or a callee it
        //    `.await`s) would otherwise silently kill this async fn before
        //    it ever reaches the `finish_refresh` call below — a bare
        //    `.await` propagates a panic, it doesn't turn into an `Err`.
        //    Isolated the standard way: run the probe in its own spawned
        //    task and await the `JoinHandle`, which turns a panic into an
        //    `Err` instead of propagating it here.
        // 2. Every individual wait inside `run_usage_probe` has its own
        //    bound (10s/15s quiescence waits, 60s adapter request
        //    timeout), but nothing bounds their *sum*, and a bug not yet
        //    found could still block somewhere uncovered. `PROBE_HARD_CEILING`
        //    is a last-resort ceiling comfortably above the legitimate
        //    worst case (~90s) so a harness can never be stuck longer than
        //    this even if something above is wrong. On timeout the
        //    spawned task is left to finish (and clean up its ephemeral
        //    session) on its own rather than aborted mid-flight; its
        //    eventual result is simply not waited for.
        let mgr = self.clone();
        let harness_for_probe = harness.clone();
        let command_for_probe = command.clone();
        let probe = tokio::spawn(async move {
            mgr.run_usage_probe(&harness_for_probe, &command_for_probe)
                .await
        });
        let outcome = tokio::time::timeout(PROBE_HARD_CEILING, probe).await;
        let mut cache = self
            .usage_cache
            .lock()
            .expect("usage_cache mutex poisoned");
        match outcome {
            Ok(Ok(Some(snapshot))) => cache.store(&harness, snapshot),
            Ok(Ok(None)) => {}
            Ok(Err(join_err)) => {
                tracing::warn!(%harness, error = %join_err, "usage probe: task panicked; discarding");
            }
            Err(_) => {
                tracing::warn!(%harness, "usage probe: exceeded hard ceiling; discarding");
            }
        }
        cache.finish_refresh(&harness);
    }

    /// Spin up an ephemeral `SessionKind::UsageProbe` session, run
    /// `command` in it, capture what it renders, and tear the session (and
    /// the native transcript file it caused the harness CLI to create)
    /// back down. Returns `None` on any hard failure or empty capture —
    /// the caller treats that as "nothing to cache", not an error to
    /// surface.
    async fn run_usage_probe(
        self: &Arc<Self>,
        harness: &str,
        command: &str,
    ) -> Option<crate::usage::UsageSnapshot> {
        let create_params = construct_protocol::CreateSessionParams {
            harness: harness.to_string(),
            cwd: usage_probe_cwd(),
            prompt: None,
            model: None,
            title: Some("usage probe".to_string()),
            mode: Some("interactive".to_string()),
            pty_size: Some(construct_protocol::PtySize {
                cols: USAGE_PROBE_COLS,
                rows: USAGE_PROBE_ROWS,
            }),
            worktree: false,
            env: HashMap::new(),
            args: Vec::new(),
            kind: construct_protocol::SessionKind::UsageProbe,
            parent_session_id: None,
            group_id: None,
            position_after_session_id: None,
            forked_from: None,
        };
        let created_at_ms = Utc::now().timestamp_millis();
        let id = match self.create(create_params).await {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(%harness, error = %e, "usage probe: session create failed");
                return None;
            }
        };
        tracing::debug!(%harness, session = %id, "usage probe: session created");

        // Step 4: wait for the harness's own startup draw to settle before
        // sending the usage command — sending too early can land before
        // the harness has even wired up its slash-command handler. A
        // session that never settles (hung/slow-starting harness) aborts
        // the whole probe rather than sending input into a black box.
        // `since_ms` floors which `last_pty_at_ms` updates count: only
        // output from at-or-after session creation, so a stale timestamp
        // can never exist yet at this point anyway (this is the session's
        // first activity).
        if !self
            .wait_for_pty_settle(
                &id,
                created_at_ms,
                USAGE_PROBE_STARTUP_SETTLE,
                USAGE_PROBE_STARTUP_TIMEOUT,
            )
            .await
        {
            tracing::warn!(%harness, session = %id, "usage probe: startup timed out; aborting");
            self.cleanup_usage_probe_session(harness, &id).await;
            return None;
        }
        tracing::debug!(%harness, session = %id, "usage probe: startup settled");

        // Steps 5-7: submit the command and capture the response, retrying
        // in the *same* still-live session (not a fresh one) if validation
        // fails. Empirically necessary, not just a nice-to-have: a fresh
        // `SessionKind::UsageProbe` session cold-starts the harness from
        // scratch every time, so if a harness's own async startup work
        // (confirmed live for claude — an MCP-server auth check) reliably
        // takes longer than `USAGE_PROBE_SETTLE` to finish in a given
        // environment, *every* fresh attempt races identically and no
        // amount of client-triggered retrying (a new probe every ~2s while
        // hovering) ever succeeds — confirmed live: 402 consecutive
        // attempts, 0 successes, over 46 minutes, before this loop existed
        // (see spec 0086). Reusing the session and just waiting longer
        // between attempts breaks that determinism: the harness's startup
        // work keeps progressing in real time regardless of how many times
        // we retry, so a later attempt in the same session has strictly
        // more real time behind it than the first one did.
        let mut bytes = Vec::new();
        for attempt in 0..USAGE_PROBE_SUBMIT_ATTEMPTS {
            if attempt > 0 {
                let backoff = USAGE_PROBE_RETRY_BACKOFF * attempt;
                tracing::warn!(
                    %harness, session = %id, attempt, backoff_ms = backoff.as_millis() as u64,
                    "usage probe: retrying command submission in the same session after extra settle wait",
                );
                tokio::time::sleep(backoff).await;
            }

            // Record the current PTY log offset, then send the probe
            // command as a bracketed paste + separate Enter — the same
            // delivery shape the program Run path uses for these exact
            // harnesses, but with the Enter gated on the paste's echo
            // rather than a fixed delay (see `submit_probe_command`). Plain
            // `send_input` (ahp `SESSION_INPUT`, "type it and append \n")
            // is NOT equivalent here: claude/codex/antigravity's rich
            // interactive TUIs only treat a real bracketed paste as one
            // atomic submission, so a bulk raw write lands the text in the
            // input box without ever submitting it — see
            // `program_submit_typed_prompt`'s own doc comment for the same
            // lesson learned once already for the program Run path.
            let before_offset = self.pty_log_len(&id);
            let sent_at_ms = Utc::now().timestamp_millis();
            if let Err(e) = self.submit_probe_command(&id, command, before_offset).await {
                tracing::warn!(%harness, session = %id, error = %e, "usage probe: submitting command failed");
                self.cleanup_usage_probe_session(harness, &id).await;
                return None;
            }
            tracing::debug!(%harness, session = %id, before_offset, command, "usage probe: command sent");

            // Wait for the response to settle. Unlike the startup wait, a
            // timeout here still proceeds to capture whatever was produced
            // — partial usage output beats nothing. `since_ms` is critical:
            // the startup wait (or a prior attempt in this same loop)
            // already left `last_pty_at_ms` sitting on an old,
            // already-quiet timestamp, so without a floor this would
            // immediately (and wrongly) read as "settled" before this
            // attempt's response ever arrives — only a PTY update at-or-
            // after this attempt's own submit counts as real evidence.
            let command_settled = self
                .wait_for_pty_settle(
                    &id,
                    sent_at_ms,
                    USAGE_PROBE_COMMAND_SETTLE,
                    USAGE_PROBE_COMMAND_TIMEOUT,
                )
                .await;
            tracing::debug!(%harness, session = %id, command_settled, attempt, "usage probe: command wait done");

            bytes = self.capture_pty_since(&id, before_offset).await;
            tracing::debug!(%harness, session = %id, captured_bytes = bytes.len(), attempt, "usage probe: captured");

            if !bytes.is_empty() && capture_shows_command_ran(&bytes, command) {
                break;
            }
            // Empirically observed race (see the loop's own doc comment
            // above): the startup-quiescence wait can declare "settled"
            // while the harness is still doing async startup work of its
            // own, so the probe command lands in a screen that's about to
            // redraw and never actually registers, and the harness's *own*
            // continued startup output gets mistaken for "the command's
            // response" — in practice, claude's plain idle welcome screen
            // with an empty prompt, captured as if it were real usage data.
            // See `capture_shows_command_ran`. Falls through to retry
            // (with backoff) unless this was the last attempt.
            tracing::warn!(
                %harness, session = %id, attempt, captured_bytes = bytes.len(),
                "usage probe: capture shows no trace of the probe command — \
                 likely raced the harness's own startup activity",
            );
        }

        // Steps 8-10: hard-kill the adapter, best-effort unlink the native
        // transcript file it caused to exist, delete construct's own
        // session record.
        self.cleanup_usage_probe_session(harness, &id).await;

        if bytes.is_empty() {
            tracing::warn!(%harness, session = %id, "usage probe: capture was empty after all attempts; not caching");
            return None;
        }
        if !capture_shows_command_ran(&bytes, command) {
            tracing::warn!(
                %harness, session = %id,
                "usage probe: still no trace of the probe command after {} attempts; discarding",
                USAGE_PROBE_SUBMIT_ATTEMPTS,
            );
            return None;
        }
        Some(crate::usage::UsageSnapshot {
            bytes,
            cols: USAGE_PROBE_COLS,
            rows: USAGE_PROBE_ROWS,
            captured_at: std::time::Instant::now(),
            captured_at_ms: Utc::now().timestamp_millis(),
        })
    }

    /// Deliver the probe command as a bracketed paste, wait for the
    /// harness to *echo* the pasted text back, then send the submit Enter.
    ///
    /// The echo gate replaces the fixed post-paste delay
    /// (`PROGRAM_EXTERNAL_PTY_SUBMIT_DELAY`) the shared
    /// `program_submit_typed_prompt` helper uses, because a fixed delay
    /// only separates the paste and the Enter at the *write* end.
    /// Empirically observed failure (spec 0086): a probe session
    /// cold-starts its harness, and a harness still busy with its own
    /// startup work (confirmed live for claude — an MCP-server auth check)
    /// doesn't drain stdin during the delay, so the paste and the Enter
    /// accumulate in the PTY buffer and arrive in ONE read. The Enter then
    /// sits directly after the `ESC[201~` paste-end marker in the same
    /// input batch and gets treated as part of the paste burst instead of
    /// a standalone submit keypress — the command lands in the input box,
    /// visibly typed with the slash-command menu open, and never runs.
    /// Waiting until the pasted text has observably been rendered proves
    /// the harness consumed the paste, so the Enter written afterwards
    /// necessarily arrives in a later read and is parsed as a real
    /// keypress.
    ///
    /// The gate reuses [`capture_shows_command_ran`] as its echo
    /// detector — the input-box echo of a pasted command is exactly "the
    /// command's token rendered at a word boundary". That check is only
    /// meaningful because the probe command is short: claude collapses
    /// *long* pastes into a `[Pasted text #N]` placeholder that never
    /// echoes the text itself, which is why this gate lives here and not
    /// in the shared program-Run delivery path.
    ///
    /// `before_offset` is the PTY-log offset recorded just before this
    /// call, so only output the paste itself produced counts as echo.
    /// A gate timeout still sends the Enter (matching the old fixed-delay
    /// behavior as the fallback): a missing echo most likely means the
    /// attempt will fail validation and be retried with backoff, and an
    /// Enter can't make that outcome worse.
    async fn submit_probe_command(
        &self,
        id: &str,
        command: &str,
        before_offset: u64,
    ) -> Result<()> {
        self.pty_input_without_capture(id, program_bracketed_paste_bytes(command))
            .await?;
        if !self.wait_for_paste_echo(id, before_offset, command).await {
            tracing::warn!(
                session = %id, command,
                "usage probe: paste echo never appeared within the gate window; sending Enter anyway",
            );
        }
        self.pty_input_without_capture(id, vec![b'\r']).await?;
        Ok(())
    }

    /// Poll the PTY bytes appended since `before_offset` until the pasted
    /// `command`'s echo shows up (`true`) or [`USAGE_PROBE_ECHO_TIMEOUT`]
    /// elapses (`false`). See [`Self::submit_probe_command`] for why the
    /// echo — not elapsed time — is the evidence that matters.
    async fn wait_for_paste_echo(&self, id: &str, before_offset: u64, command: &str) -> bool {
        let started = tokio::time::Instant::now();
        loop {
            let bytes = self.capture_pty_since(id, before_offset).await;
            if capture_shows_command_ran(&bytes, command) {
                return true;
            }
            if started.elapsed() >= USAGE_PROBE_ECHO_TIMEOUT {
                return false;
            }
            tokio::time::sleep(USAGE_PROBE_ECHO_POLL_INTERVAL).await;
        }
    }

    /// Poll `id`'s `last_pty_at_ms` until it has settled — a PTY update at
    /// or after `since_ms`, followed by `settle` of quiet (`true`) — or
    /// `max_wait` elapses (`false`: gave up). `since_ms` matters: this
    /// session's `last_pty_at_ms` can already hold an old, already-quiet
    /// timestamp from an earlier wait on the same session (step 4 vs. step
    /// 6), and only an update at-or-after `since_ms` counts as evidence
    /// that the thing this particular wait cares about actually happened.
    /// See [`usage_probe_wait_outcome`] for the pure decision step.
    async fn wait_for_pty_settle(
        &self,
        id: &str,
        since_ms: i64,
        settle: Duration,
        max_wait: Duration,
    ) -> bool {
        let started = tokio::time::Instant::now();
        loop {
            let last_pty_at_ms = match self.get_entry(id).await {
                Some(entry) => entry.summary.read().await.last_pty_at_ms,
                None => return false,
            };
            let now_ms = Utc::now().timestamp_millis();
            match usage_probe_wait_outcome(
                last_pty_at_ms,
                since_ms,
                now_ms,
                started.elapsed(),
                settle,
                max_wait,
            ) {
                Some(settled) => return settled,
                None => tokio::time::sleep(USAGE_PROBE_POLL_INTERVAL).await,
            }
        }
    }

    /// Current byte length of `id`'s on-disk `pty.log`, used to mark "the
    /// probe command hasn't been sent yet" before step 5 so step 7 can
    /// slice out only what the command produced.
    fn pty_log_len(&self, id: &str) -> u64 {
        std::fs::metadata(self.storage.pty_log_path(id))
            .map(|m| m.len())
            .unwrap_or(0)
    }

    /// Capture the bytes appended to `id`'s `pty.log` since `before_offset`.
    async fn capture_pty_since(&self, id: &str, before_offset: u64) -> Vec<u8> {
        match self.pty_replay_range(id, None, None).await {
            Ok(result) => {
                let all = base64::engine::general_purpose::STANDARD
                    .decode(&result.data)
                    .unwrap_or_default();
                if before_offset >= result.start_offset {
                    let skip = (before_offset - result.start_offset) as usize;
                    all.get(skip..).map(|s| s.to_vec()).unwrap_or_default()
                } else {
                    // The command produced more than PTY_REPLAY_CAP bytes
                    // (extremely unlikely for a usage panel) — the earliest
                    // new bytes already scrolled out of the read window.
                    // Returning the whole (tail) window is still a
                    // reasonable capture.
                    all
                }
            }
            Err(e) => {
                tracing::warn!(session = %id, error = %e, "usage probe: pty_replay_range failed");
                Vec::new()
            }
        }
    }

    /// Steps 8-10: kill the adapter, best-effort unlink the native
    /// transcript file it caused to exist, then delete construct's own
    /// session record.
    async fn cleanup_usage_probe_session(&self, harness: &str, id: &str) {
        // Hard-kill (SIGKILL) rather than a graceful stop — the native
        // transcript file is resolved and unlinked below only after the
        // process is confirmed dead, so there's no benefit to waiting for
        // a graceful exit first.
        if let Some(entry) = self.get_entry(id).await {
            if let Some(adapter) = entry.adapter.lock().await.take() {
                adapter.kill();
            }
        }

        self.unlink_usage_probe_native_transcript(harness, id)
            .await;

        // Worktree removal inside `delete` is a no-op: probe sessions are
        // always created with `worktree: false`.
        if let Err(e) = self.delete(id).await {
            tracing::warn!(%harness, session = %id, error = %e, "usage probe: session delete failed");
        }
    }

    /// Best-effort: resolve and remove the native transcript file (or, for
    /// harnesses that give each session its own directory, the whole
    /// directory — see [`Self::try_unlink_usage_probe_native_transcript`])
    /// this probe session caused its harness CLI to create, so a burst of
    /// probes never leaves stray entries in the harness's own native
    /// history (`claude --resume` picker, `~/.codex/sessions/`, ...).
    /// Never fails the probe — any error here is logged and swallowed.
    /// Only ever called for `SessionKind::UsageProbe` sessions; real user
    /// sessions' native transcripts are never touched.
    ///
    /// Retries a few times with a short delay between attempts: at the
    /// point this runs the adapter has just been SIGKILL'd, but two
    /// sources of native-side latency can still be in flight — (a) some
    /// harnesses (grok, codex, antigravity) capture their own native id
    /// via a background watcher that polls periodically rather than
    /// synchronously at spawn, so the id-file this reads may not exist
    /// yet, and (b) a harness's own write of its transcript file can still
    /// land on disk a moment after the process receives SIGKILL (syscalls
    /// already in flight complete even though the process can't run more
    /// code). Confirmed empirically: an immediate single-attempt check
    /// against a live grok probe reported "file not found" and then the
    /// file appeared on disk moments later, which would have left a real
    /// stray entry. A single-shot check is not reliable enough here.
    ///
    /// Often ends up a genuine no-op even after retrying: a usage/status
    /// slash command is a local UI query, not a real conversational turn,
    /// and several harnesses only persist a transcript file once an actual
    /// turn happens (confirmed for claude: `claude_session_id.txt` is
    /// written at process startup, well before any turn, but the
    /// corresponding `~/.claude/projects/.../*.jsonl` is never created for
    /// a probe that only ran `/usage`) — that case is expected and not
    /// logged as a failure.
    async fn unlink_usage_probe_native_transcript(&self, harness: &str, id: &str) {
        const ATTEMPTS: u32 = 4;
        const RETRY_DELAY: Duration = Duration::from_millis(300);
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(RETRY_DELAY).await;
            }
            match self.try_unlink_usage_probe_native_transcript(harness, id) {
                UnlinkOutcome::Removed(path) => {
                    tracing::debug!(
                        %harness, session = %id, path = %path.display(), attempt,
                        "usage probe: removed native transcript",
                    );
                    return;
                }
                UnlinkOutcome::Error(path, e) => {
                    // A real error (permission denied, etc.) — not a
                    // timing issue retrying would fix.
                    tracing::warn!(
                        %harness, session = %id, path = %path.display(), error = %e,
                        "usage probe: failed to remove native transcript",
                    );
                    return;
                }
                UnlinkOutcome::NothingYet if attempt + 1 < ATTEMPTS => continue,
                UnlinkOutcome::NothingYet => {
                    tracing::debug!(
                        %harness, session = %id, attempts = ATTEMPTS,
                        "usage probe: no native transcript found after retrying — likely no real turn happened",
                    );
                    return;
                }
            }
        }
    }

    /// One attempt at resolving + removing the native transcript. See
    /// [`Self::unlink_usage_probe_native_transcript`] for why this is
    /// retried rather than a single check.
    ///
    /// claude and codex persist a single flat file per session inside a
    /// directory *shared* with other sessions
    /// (`<home>/projects/<slug>/*.jsonl`, `<home>/sessions/**/*.jsonl`), so
    /// only that one file is removed. grok and antigravity instead give
    /// each session/conversation its own *exclusive* directory containing
    /// several sibling files (grok: `summary.json`, `prompt_context.json`,
    /// `system_prompt.txt`, ... alongside `chat_history.jsonl`;
    /// antigravity: a full `.git` history, task logs, uploads alongside
    /// `.system_generated/logs/transcript.jsonl`) — removing only the
    /// transcript file there would still leave a real entry in the
    /// harness's own session picker, so the whole directory is removed
    /// instead. Verified against a real antigravity conversation directory
    /// during manual testing (spec 0086): far more lives there than just
    /// the transcript file this module reads to mirror chat history.
    fn try_unlink_usage_probe_native_transcript(&self, harness: &str, id: &str) -> UnlinkOutcome {
        let session_dir = self.storage.session_dir(id);
        let env = self
            .config
            .adapters
            .get(harness)
            .map(|c| c.env.clone())
            .unwrap_or_default();
        let cwd = PathBuf::from(usage_probe_cwd());
        match harness {
            "claude" => remove_file_outcome(
                read_native_id_file(&session_dir.join("claude_session_id.txt")).and_then(
                    |native_id| {
                        construct_adapter_common::claude_transcript_path(&cwd, &native_id, &env)
                    },
                ),
            ),
            "codex" => remove_file_outcome(
                read_native_id_file(&session_dir.join("codex_session_id.txt")).and_then(
                    |native_id| construct_adapter_common::codex_transcript_path(&env, &native_id),
                ),
            ),
            "grok" => remove_dir_outcome(
                read_native_id_file(&session_dir.join("grok_session_id.txt")).and_then(
                    |native_id| construct_adapter_common::grok_session_dir(&cwd, &native_id, &env),
                ),
            ),
            "agy" => remove_dir_outcome(
                read_native_id_file(&session_dir.join("agy_conversation_id.txt")).and_then(
                    |native_id| construct_adapter_common::antigravity_conversation_dir(&native_id, &env),
                ),
            ),
            _ => UnlinkOutcome::NothingYet,
        }
    }
}

/// Outcome of one [`SessionManager::try_unlink_usage_probe_native_transcript`]
/// attempt.
enum UnlinkOutcome {
    /// Successfully removed the file or directory at this path.
    Removed(PathBuf),
    /// Either no native id-file exists yet, or it exists but the harness
    /// hasn't created anything at the resolved path yet (or ever will, for
    /// a probe that never triggered a real turn) — retry, then treat as
    /// "nothing to unlink" once attempts are exhausted.
    NothingYet,
    /// Resolved a path and found something there, but removing it failed
    /// for a real reason (permission, etc.) — not worth retrying.
    Error(PathBuf, std::io::Error),
}

fn remove_file_outcome(path: Option<PathBuf>) -> UnlinkOutcome {
    let Some(path) = path else {
        return UnlinkOutcome::NothingYet;
    };
    match std::fs::remove_file(&path) {
        Ok(()) => UnlinkOutcome::Removed(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => UnlinkOutcome::NothingYet,
        Err(e) => UnlinkOutcome::Error(path, e),
    }
}

fn remove_dir_outcome(path: Option<PathBuf>) -> UnlinkOutcome {
    let Some(path) = path else {
        return UnlinkOutcome::NothingYet;
    };
    match std::fs::remove_dir_all(&path) {
        Ok(()) => UnlinkOutcome::Removed(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => UnlinkOutcome::NothingYet,
        Err(e) => UnlinkOutcome::Error(path, e),
    }
}

/// cwd for an ephemeral probe session. `worktree: false` means no git repo
/// is required, so a project-less default is fine — the harness only needs
/// *a* writable directory to start in, not any particular project.
///
/// Deliberately the daemon's own process cwd (same choice
/// `ensure_orchestrator` makes for the minibuffer session), NOT the user's
/// home directory: several wrapper harnesses gate a directory they haven't
/// seen before behind a first-run interactive trust prompt (confirmed for
/// claude — `$HOME` is very often untrusted since users rarely start a real
/// claude session directly in their home directory), and that prompt
/// consumes the probe's only turn, producing no usage output at all. The
/// daemon's own cwd is where the user chose to start `construct daemon
/// run` — typically inside a project they already work in and have likely
/// already trusted with every harness — so it is far more likely to be
/// pre-trusted than an arbitrary fixed path. This is a best-effort
/// mitigation, not a guarantee: a harness can still show its trust prompt
/// for a directory it has truly never seen, in which case that prompt is
/// exactly what gets captured (see the "redisplay verbatim" decision) and
/// the next probe (after the user trusts it, e.g. by using that harness
/// normally) succeeds.
fn usage_probe_cwd() -> String {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| std::env::var("HOME").unwrap_or_else(|_| "/".to_string()))
}

/// Trim a native id-file's contents, treating whitespace-only as absent.
fn read_native_id_file(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// First whitespace-delimited token of a probe `command` (e.g. `"/usage"`
/// from `"/usage show"` or from an operator override like `"/usage
/// --verbose-test-override"`). [`capture_shows_command_ran`] checks for
/// only this token, not the whole command string, because a harness
/// commonly renders a command keyword and its trailing arguments as
/// separately styled spans — an ANSI color-change/cursor-move escape sits
/// between them (confirmed live for grok's `/usage --arg` rendering) — so
/// the full command string is not reliably one contiguous run in the raw
/// capture even when everything worked correctly.
fn command_first_token(command: &str) -> &str {
    command.split_whitespace().next().unwrap_or(command)
}

/// Whether `bytes` (the newly-captured PTY output) shows any trace of
/// `command` having actually been typed/rendered at some point during the
/// capture window. Guards against a real, empirically observed race: the
/// startup-quiescence wait (step 4) can declare a harness "settled" while
/// it's still doing async startup work of its own — confirmed live for
/// claude, where an MCP-server auth check was still in flight after the
/// PTY had already gone quiet for the settle window. The probe command
/// then gets sent into a screen that's about to redraw and never actually
/// lands, and the harness's *own* continued startup output during the
/// response wait (step 6) gets mistaken for "the command's response" —
/// captured and cached as if it were real usage data, when it's actually
/// just the harness's idle welcome screen. If the command's own text never
/// appears anywhere in the capture, nothing legitimate could have rendered
/// in response to it.
fn capture_shows_command_ran(bytes: &[u8], command: &str) -> bool {
    let token = command_first_token(command);
    if token.is_empty() {
        return true; // nothing meaningful to check against
    }
    let text = String::from_utf8_lossy(bytes);
    // A bare substring search isn't enough: a probe command starting with
    // "/" (every real one does) can coincidentally appear inside an
    // unrelated path — this repo's own worktree happens to be named
    // "usage-probe-backend", so ".../worktrees/usage-probe-backend"
    // contains the literal substring "/usage" without the command ever
    // having run (caught by this exact regression test against a real
    // captured failure). Require the character immediately after the
    // matched token to be neither alphanumeric nor `-`, so a genuine
    // command echo (followed by whitespace, a newline, or another ANSI
    // escape byte) still matches, but a path continuing into
    // "-probe-backend" does not.
    text.match_indices(token).any(|(idx, matched)| {
        match text[idx + matched.len()..].chars().next() {
            None => true,
            Some(c) => !c.is_alphanumeric() && c != '-',
        }
    })
}

/// Pure decision step for [`SessionManager::wait_for_pty_settle`]'s poll
/// loop, mirroring `lifecycle::resume_redraw_ready`'s shape (checked on an
/// interval against a session's `last_pty_at_ms`) but distinguishing
/// "settled" (`Some(true)`: a PTY update at or after `since_ms` happened,
/// then went quiet for `settle`) from "gave up after `max_wait` with
/// nothing to show for it" (`Some(false)`). `None` means keep polling.
///
/// `since_ms` exists because the same session is waited on twice in a row
/// (step 4's startup wait, then step 6's post-command wait) sharing one
/// `last_pty_at_ms` field: without a floor, step 6 would immediately read
/// step 4's already-old, already-quiet timestamp as "settled" and return
/// before the command's response ever arrived. A `last_pty_at_ms` older
/// than `since_ms` is stale evidence from a previous wait and must not
/// count.
///
/// The caller treats the two `Some` outcomes differently: a startup wait
/// that never settles aborts the whole probe, while a post-command wait
/// that never settles still proceeds to capture whatever was produced.
fn usage_probe_wait_outcome(
    last_pty_at_ms: Option<i64>,
    since_ms: i64,
    now_ms: i64,
    elapsed: Duration,
    settle: Duration,
    max_wait: Duration,
) -> Option<bool> {
    if let Some(t) = last_pty_at_ms {
        if t >= since_ms && now_ms.saturating_sub(t) >= settle.as_millis() as i64 {
            return Some(true);
        }
    }
    if elapsed >= max_wait {
        return Some(false);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Proves the exact `tokio::spawn` + `timeout` + `JoinHandle` pattern
    /// `refresh_usage` relies on: a panic inside the spawned probe task
    /// must turn into an `Err` when the handle is awaited, not propagate
    /// and kill the awaiting task — otherwise `finish_refresh` (clearing
    /// the in-flight guard) would never run, and the harness would be
    /// stuck showing "probing…" until the next daemon restart. This is a
    /// real bug found live: a race can leave a probe task in an unexpected
    /// state, and without this isolation any panic there — not just this
    /// specific one — would wedge the harness forever.
    #[tokio::test]
    async fn spawned_panic_becomes_a_join_error_not_a_propagated_panic() {
        let handle = tokio::spawn(async { panic!("simulated probe panic") });
        let outcome = tokio::time::timeout(Duration::from_secs(5), handle).await;
        assert!(
            matches!(outcome, Ok(Err(_))),
            "a panic in the spawned task must surface as Ok(Err(JoinError)), \
             not hang or propagate: {outcome:?}"
        );
    }

    /// The settle gate: keep polling while the probe session is still
    /// drawing (recent output) or hasn't drawn at all, settle once it goes
    /// quiet, and give up once the hard cap passes with nothing observed.
    #[test]
    fn usage_probe_wait_outcome_settle_gate() {
        let now = 1_000_000i64;
        let since = 0i64; // no floor for this test
        let settle = USAGE_PROBE_COMMAND_SETTLE;
        // Deliberately derived from `settle`, not a bare literal: this wait
        // can only ever report "settled" if a full `settle` window fits
        // before `max_wait`, so
        // a hardcoded `max_wait` would silently stop exercising that once
        // `settle` grew to meet or exceed it.
        let max_wait = settle + Duration::from_secs(5);

        // Nothing drawn yet, well under the cap -> keep polling.
        assert_eq!(
            usage_probe_wait_outcome(None, since, now, Duration::from_millis(0), settle, max_wait),
            None
        );
        // Output 50ms ago (< settle) -> still drawing, keep polling.
        assert_eq!(
            usage_probe_wait_outcome(
                Some(now - 50),
                since,
                now,
                Duration::from_secs(1),
                settle,
                max_wait
            ),
            None
        );
        // Quiet for exactly the settle window -> settled.
        assert_eq!(
            usage_probe_wait_outcome(
                Some(now - settle.as_millis() as i64),
                since,
                now,
                Duration::from_secs(1),
                settle,
                max_wait
            ),
            Some(true)
        );
        // Quiet well past settle -> settled.
        assert_eq!(
            usage_probe_wait_outcome(
                Some(now - settle.as_millis() as i64 - 5_000),
                since,
                now,
                Duration::from_secs(1),
                settle,
                max_wait
            ),
            Some(true)
        );
        // Never settles (recent output) but hit the hard cap -> gave up.
        assert_eq!(
            usage_probe_wait_outcome(Some(now), since, now, max_wait, settle, max_wait),
            Some(false)
        );
        // Never drew anything, but hit the hard cap -> gave up.
        assert_eq!(
            usage_probe_wait_outcome(None, since, now, max_wait, settle, max_wait),
            Some(false)
        );
    }

    /// Regression: idle-screen repaints from claude, codex, and grok can
    /// arrive more often than the command-response settle window. Startup
    /// must still become ready during a shorter quiet interval, while the
    /// same interval remains insufficient to finish a response capture.
    #[test]
    fn startup_and_command_use_distinct_settle_windows() {
        let now = 1_000_000i64;
        let last_output = Some(now - 1_000);
        let elapsed = Duration::from_secs(1);

        assert_eq!(
            usage_probe_wait_outcome(
                last_output,
                0,
                now,
                elapsed,
                USAGE_PROBE_STARTUP_SETTLE,
                USAGE_PROBE_STARTUP_TIMEOUT,
            ),
            Some(true),
            "one second of quiet is enough to send the probe command",
        );
        assert_eq!(
            usage_probe_wait_outcome(
                last_output,
                0,
                now,
                elapsed,
                USAGE_PROBE_COMMAND_SETTLE,
                USAGE_PROBE_COMMAND_TIMEOUT,
            ),
            None,
            "one second of quiet is not enough to truncate a slow usage panel",
        );
    }

    /// The `since_ms` floor: a stale, already-quiet timestamp from *before*
    /// `since_ms` must not read as settled — this is exactly the step-4-vs-
    /// step-6 reuse bug the floor exists to prevent. Only a PTY update at
    /// or after `since_ms` counts as real evidence for this particular wait.
    #[test]
    fn usage_probe_wait_outcome_ignores_stale_timestamp_before_since() {
        let now = 1_000_000i64;
        let since = now; // command was just sent "now"
        let settle = USAGE_PROBE_COMMAND_SETTLE;
        let max_wait = settle + Duration::from_secs(5);

        // last_pty_at_ms is from well before `since` and long "quiet" by
        // wall-clock terms, but it predates the thing we're waiting for ->
        // must NOT read as settled.
        assert_eq!(
            usage_probe_wait_outcome(
                Some(since - 10_000),
                since,
                now,
                Duration::from_millis(0),
                settle,
                max_wait
            ),
            None
        );
        // A fresh update at/after `since`, settled -> settles normally.
        assert_eq!(
            usage_probe_wait_outcome(
                Some(since + 10),
                since,
                since + 10 + settle.as_millis() as i64,
                Duration::from_secs(1),
                settle,
                max_wait
            ),
            Some(true)
        );
    }

    #[test]
    fn read_native_id_file_trims_and_treats_blank_as_absent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join("id.txt");
        std::fs::write(&file, "  abc-123  \n").expect("write");
        assert_eq!(read_native_id_file(&file), Some("abc-123".to_string()));

        let blank = tmp.path().join("blank.txt");
        std::fs::write(&blank, "   \n").expect("write");
        assert_eq!(read_native_id_file(&blank), None);

        assert_eq!(read_native_id_file(&tmp.path().join("missing.txt")), None);
    }

    // -- `capture_shows_command_ran` (spec 0086): regression coverage for
    // the live-observed startup-quiescence race where a probe's command
    // never actually landed, and the harness's own idle welcome screen got
    // captured and cached as if it were a real response.

    #[test]
    fn command_first_token_strips_trailing_arguments() {
        assert_eq!(command_first_token("/usage show"), "/usage");
        assert_eq!(command_first_token("/usage --verbose-test-override"), "/usage");
        assert_eq!(command_first_token("/status"), "/status");
    }

    #[test]
    fn capture_shows_command_ran_true_when_token_present() {
        // Shape of a real successful claude capture: the command appears
        // in an autocomplete-dropdown help line before the real panel.
        let bytes = b"\x1b[38;2;177;185;249m/usage    Show session cost, plan usage stats";
        assert!(capture_shows_command_ran(bytes, "/usage"));
    }

    /// The paste-echo gate (`submit_probe_command`) reuses
    /// `capture_shows_command_ran` as its echo detector: the input-box
    /// echo of a freshly pasted command — the token followed by an ANSI
    /// escape or cursor sequence, before any Enter was sent — must count
    /// as "echo seen", while pre-echo startup noise must not (covered by
    /// `capture_shows_command_ran_false_on_raced_startup_screen` below:
    /// that exact screen shape is what the gate keeps polling through).
    #[test]
    fn paste_echo_gate_fires_on_input_box_echo() {
        // Shape of claude's input box right after a paste: prompt marker,
        // the pasted command, a styled cursor cell — no submission yet.
        let bytes = b"\x1b[38;5;250m\xe2\x9d\xaf \x1b[0m/usage\x1b[7m \x1b[0m";
        assert!(capture_shows_command_ran(bytes, "/usage"));
    }

    #[test]
    fn capture_shows_command_ran_false_on_raced_startup_screen() {
        // Shape of the real observed failure: claude's idle welcome screen
        // and cwd banner, with the probe command never actually typed —
        // the only "usage" substring is the unrelated directory name.
        let bytes = b"Claude Code v2.1.207\n~/agentd/.claude/worktrees/usage-probe-backend\n1 MCP server needs authentication";
        assert!(!capture_shows_command_ran(bytes, "/usage"));
    }

    #[test]
    fn capture_shows_command_ran_finds_first_token_even_with_styled_arguments() {
        // Shape of a real grok override capture: "/usage" and its argument
        // render as separately styled spans with an ANSI escape between
        // them, so the full command string isn't one contiguous run even
        // on success — only the first token is checked.
        let bytes = b"\x1b[1m/usage\x1b[14G\x1b[2m--verify-override-test";
        assert!(capture_shows_command_ran(bytes, "/usage --verify-override-test"));
    }
}
