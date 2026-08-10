//! `Manager` warm methods, split verbatim from `mod.rs`.

use super::*;

impl Manager {
    /// The account indices eligible for a keep-warm request. Starts from the same
    /// base as [`Self::probeable_indices`] (an OAuth account with a refresh token
    /// that is neither disabled nor errored) and additionally skips:
    ///  - a `Throttled` account — warming it would just 429;
    ///  - an account whose 5h window is **live** (a future reset) — already warm,
    ///    so warming again only burns quota for nothing;
    ///  - a near/over-threshold account — warming an exhausted account is pointless.
    ///
    /// A cold account (no 5h data) or one whose 5h reset has already passed IS a
    /// target — those are exactly the accounts whose window we want to (re)start.
    ///
    /// …with one exception, and it only bites at BOOT. `AccountRuntime::from_config`
    /// starts every account on `Quota::default()` and nothing restores the last
    /// known windows, so for the first probe cycle every account's 5h window reads
    /// blank. Blank is *unknown*, not *known-cold* — and the warm loop's ticker
    /// fires its first tick immediately, so without a gate the first sweep spends a
    /// real upstream request on every probeable account, including ones whose 5h
    /// window is genuinely live and ones sitting at 100% weekly.
    ///
    /// # The gate is about EVIDENCE, and has three states, not two
    ///
    /// "Safe to warm" means: we have evidence about this account's 5h window, and
    /// that evidence says no window is currently live. So:
    ///
    /// 1. **Evidence present** → open. [`AccountRuntime::quota_known`] is that
    ///    evidence, and it has TWO sources: a successful probe
    ///    ([`Manager::apply_usage`]) and a served response whose rate-limit headers
    ///    carried the unified 5h window ([`Manager::update_quota`]). Both are
    ///    first-hand reads of this account's window. Leaving the second one out is
    ///    why an account carrying live traffic used to read as "quota unknown",
    ///    which was simply false.
    /// 2. **No evidence yet, still expected** → wait. This is the boot case the
    ///    gate exists for, and it costs one probe cycle rather than one warm
    ///    interval: both latch sites signal [`Manager::warm_wake`] on the flip and
    ///    the warm loop `select!`s on it.
    /// 3. **Evidence unavailable** → proceed anyway, and say so once.
    ///    [`PROBE_FAILURES_BEFORE_WARMING_UNPROBED`] consecutive failed probes of
    ///    this account mean
    ///    the probe is not going to answer. Absence of evidence is not evidence of
    ///    a live window, and a permanently dark feature is worse than a bounded
    ///    warm on stale information — the wait must be BOUNDED, or gating on
    ///    `quota_known` is a silent kill switch whenever the probe stays broken.
    ///
    /// State 3 is self-limiting, which is what makes it safe: `warm_account` folds
    /// the warm response's own rate-limit headers back in, so one warm request per
    /// account produces the evidence the gate wanted and states 1/2 take over
    /// again. It cannot become a repeating burst.
    ///
    /// The predicate is NOT `probe_status`: `record_probe` stamps
    /// `Error`/`Timeout`/`RateLimited` on a FAILED probe, which leaves
    /// `probe_status != Never` while the quota is still `Quota::default()`, so a
    /// gate keyed on it lifts on blank quota after ONE failure and hands the boot
    /// burst straight back. `probing.rs` documents a real fleet-wide false-error
    /// sweep — exactly that shape, and exactly why state 3 needs several failures
    /// rather than one.
    ///
    /// The whole gate applies only while probing is actually enabled. With
    /// `quotaProbeSeconds == 0` no probe task is ever spawned, so there is nothing
    /// to wait for and no failure count to accrue either.
    pub fn warm_targets(&self) -> Vec<usize> {
        let now = OffsetDateTime::now_utc();
        // Read the probe cadence BEFORE taking the accounts lock: this touches the
        // config lock, and holding both at once here would invert the order
        // `set_disabled` takes them in (accounts, then config).
        let awaiting_first_probe = self.probe_interval_seconds() > 0;
        let accounts = self.accounts.read().expect("accounts lock poisoned");
        accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| {
                a.account_type == "oauth"
                    && a.refresh_token.is_some()
                    && !a.disabled
                    && a.status != AccountStatus::Error
                    && a.status != AccountStatus::Throttled
                    // Evidence about the 5h window, or a bounded wait for it. A
                    // probe that merely FAILED has read nothing — but once it has
                    // failed enough times in a row it is not going to read
                    // anything, and waiting forever is a kill switch, not caution.
                    && (!awaiting_first_probe
                        || a.quota_known
                        || a.consecutive_probe_failures >= PROBE_FAILURES_BEFORE_WARMING_UNPROBED)
                    // A live future 5h reset means the session window is already running.
                    && a.quota.five_hour.and_then(|w| w.live_reset(now)).is_none()
                    // Warming an at/over-threshold account is wasted spend.
                    && !a
                        .quota
                        .is_near(a.switch_threshold.unwrap_or(self.global_threshold), now)
            })
            .map(|(idx, _)| idx)
            .collect()
    }

    /// Warm one idle account: ensure its token is fresh, fire a minimal upstream
    /// request to (re)start its 5h window, and on success fold the response's
    /// rate-limit headers so the just-started window is immediately visible (which
    /// suppresses a re-warm on the next sweep). A warm FAILURE is non-fatal —
    /// logged at `warn` and stepped over, never a crash of the loop.
    pub async fn warm_account(&self, idx: usize) {
        self.ensure_fresh(idx).await;
        let Some(token) = self.access_token(idx) else {
            return;
        };
        let upstream = self.upstream.clone();
        let name = self.account_name(idx).unwrap_or_default();
        match self.warmer.warm(token, upstream).await {
            Ok(headers) => {
                // Fold the now-live 5h window into the account's quota so the next
                // sweep sees it as already warm (REQUIRED — suppresses re-warm).
                self.update_quota(idx, &headers);
                tracing::info!(account = %name, index = idx, "keep-warm: started 5h window");
            }
            Err(err) => {
                tracing::warn!(
                    account = %name,
                    index = idx,
                    error = %err,
                    "keep-warm request failed (non-fatal)"
                );
            }
        }
    }

    /// Warm ONE account, if it is still eligible at this instant.
    ///
    /// The per-account entry point [`crate::schedule`] drives, and the
    /// re-checking is the point: an account's due instant was drawn up to a whole
    /// cadence ago, and in between it may have started serving traffic (live 5h
    /// window), crossed its threshold, or been disabled — all of which
    /// [`Self::warm_targets`] excludes and all of which would otherwise be a
    /// wasted, quota-spending request. Takes the same [`Self::warm_in_flight`]
    /// guard [`Self::warm_all`] does, so a scheduled warm and a wake-driven sweep
    /// can never double-warm the same account.
    pub async fn warm_one(&self, idx: usize) {
        if !self.warm_targets().contains(&idx) {
            return;
        }
        if self.warm_in_flight.swap(true, Ordering::SeqCst) {
            return;
        }
        let _guard = WarmInFlightGuard(&self.warm_in_flight);
        self.warm_account(idx).await;
    }

    /// Warm every eligible idle account once, SEQUENTIALLY with [`crate::probe::PROBE_SPACING`]
    /// between calls (mirrors the spaced probe sweep). Overlapping sweeps are
    /// skipped via [`Self::warm_in_flight`] so two timers never double-warm.
    ///
    /// This stays the ONE-SHOT sweep, reached from the edge-triggered
    /// [`Manager::warm_wake`] signal (the whole fleet's eligibility just changed)
    /// rather than from a cadence — the periodic path is per-account and random,
    /// via [`Self::warm_one`].
    pub async fn warm_all(&self) {
        // Skip if a sweep is already running (mirrors the JS `_running` guard).
        if self.warm_in_flight.swap(true, Ordering::SeqCst) {
            return;
        }
        // Reset the flag even if a warm unexpectedly unwinds.
        let _guard = WarmInFlightGuard(&self.warm_in_flight);

        let idxs = self.warm_targets();
        let last = idxs.len().saturating_sub(1);
        for (i, idx) in idxs.into_iter().enumerate() {
            self.warm_account(idx).await;
            if i < last {
                tokio::time::sleep(crate::probe::PROBE_SPACING).await;
            }
        }
    }
}
