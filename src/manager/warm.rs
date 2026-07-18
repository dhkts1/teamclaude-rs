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
    pub fn warm_targets(&self) -> Vec<usize> {
        let now = OffsetDateTime::now_utc();
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
