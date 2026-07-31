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
    /// window is genuinely live and ones sitting at 100% weekly. A never-probed
    /// account is therefore not a target: we do not warm on quota we have not read.
    ///
    /// The gate applies ONLY while probing is actually enabled. With
    /// `quotaProbeSeconds == 0` no probe task is ever spawned, so `probe_status`
    /// would stay `Never` forever and gating on it unconditionally would make
    /// keep-warm structurally unable to fire — a dark feature that looks enabled.
    /// With the probe off there is nothing to wait for, so today's behaviour stands.
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
                    // Never probed while the probe is running: its blank quota is
                    // unknown, not known-cold. Wait for the first probe to speak.
                    && !(awaiting_first_probe && a.probe_status == ProbeStatus::Never)
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

    /// Warm every eligible idle account once, SEQUENTIALLY with [`crate::probe::PROBE_SPACING`]
    /// between calls (mirrors the spaced probe sweep). Overlapping sweeps are
    /// skipped via [`Self::warm_in_flight`] so two timers never double-warm.
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
