//! `Manager` probing methods, split verbatim from `mod.rs`.

use super::*;

impl Manager {
    /// Record the health of account `idx`'s most recent probe. A failing probe
    /// stamps a visible status + message (never a silently-frozen bar), while an
    /// `Ok` probe clears any prior error.
    ///
    /// Also keeps the run of consecutive failures that bounds keep-warm's wait for
    /// quota evidence, and — on the sweep that crosses
    /// [`PROBE_FAILURES_BEFORE_WARMING_UNPROBED`] — says once, greppably, that
    /// keep-warm is giving up on waiting for this account.
    pub fn record_probe(&self, idx: usize, status: ProbeStatus, error: Option<String>) {
        let gave_up_waiting = {
            let mut accounts = self.accounts.write().expect("accounts lock poisoned");
            let Some(account) = accounts.get_mut(idx) else {
                return;
            };
            account.probe_status = status;
            account.probe_error = error;
            // "Running" is a transient in-flight marker; only a terminal outcome
            // updates the last-probe timestamp the TUI ages against.
            if status != ProbeStatus::Never {
                account.last_probe_ms = Some(crate::now_ms());
            }
            match status {
                // A read succeeded — `apply_usage` has already latched
                // `quota_known` on this path, and the run of failures is over.
                ProbeStatus::Ok => {
                    account.consecutive_probe_failures = 0;
                    false
                }
                // Not a terminal outcome; it says nothing either way.
                ProbeStatus::Never => false,
                ProbeStatus::Error | ProbeStatus::Timeout | ProbeStatus::RateLimited => {
                    account.consecutive_probe_failures =
                        account.consecutive_probe_failures.saturating_add(1);
                    // The CROSSING probe only, so a probe that stays broken logs
                    // once rather than every cadence. An account whose quota we did
                    // read has nothing to give up on.
                    !account.quota_known
                        && account.consecutive_probe_failures
                            == PROBE_FAILURES_BEFORE_WARMING_UNPROBED
                }
            }
        };
        if gave_up_waiting {
            let name = self.account_name(idx).unwrap_or_default();
            tracing::warn!(
                account = %name,
                index = idx,
                failed_probes = PROBE_FAILURES_BEFORE_WARMING_UNPROBED,
                "keep-warm: proceeding without quota evidence — this account's probe has failed every time and absence of evidence is not evidence of a live 5h window"
            );
            // Same edge-triggered wake `apply_usage` fires when the gate opens the
            // other way: without it the account waits out a whole `warmupSeconds`
            // after becoming eligible.
            self.warm_wake.notify_one();
        }
    }

    /// The OAuth account indices eligible for a usage probe: an OAuth account with a
    /// refresh token that is not disabled, plus any `Error` row whose RECOVERY
    /// backoff has elapsed.
    ///
    /// Errored rows were previously excluded outright, on the reasoning that "a probe's
    /// successful refresh would flip an `Error` row back to `Active`, silently
    /// re-inserting it into rotation". That reasoning no longer holds, and its premise
    /// has since been removed on both sides:
    /// - `Error` now arises ONLY from a REJECTED refresh (the `AuthRejected` arm). A
    ///   request-level 401 no longer condemns a row (that was rotation churn falsely
    ///   sidelining healthy accounts — fixed 2026-07-17), and the port-takeover
    ///   singleton removed token-wars, the main source of *transient* rejections.
    /// - Recovery requires a **successful refresh**, which a genuinely dead
    ///   credential cannot produce — so a re-probe can only ever revive a row that was
    ///   never actually dead. The feared "silently re-insert a dead account" is
    ///   unreachable: a dead token re-rejects and the backoff doubles.
    ///
    /// Excluding them outright made `Error` a life sentence: nothing probes or selects
    /// an errored row, and only a refresh clears it — so one transient rejection
    /// sidelined a healthy account until restart (observed live 2026-07-17).
    pub fn probeable_indices(&self) -> Vec<usize> {
        let now_ms = crate::now_ms();
        let accounts = self.accounts.read().expect("accounts lock poisoned");
        accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| {
                a.account_type == "oauth"
                    && a.refresh_token.is_some()
                    && !a.disabled
                    && (a.status != AccountStatus::Error
                        || a.error_retry_after_ms.is_some_and(|until| now_ms >= until))
            })
            .map(|(idx, _)| idx)
            .collect()
    }

    /// Probe **every** OAuth account's quota once, sequentially, spaced
    /// [`crate::probe::PROBE_SPACING`] apart.
    ///
    /// This is the ONE-SHOT "refresh everything now" sweep, and it is still what
    /// `tcr accounts --probe` / `tcr status`'s offline fallback need: a caller
    /// with no server behind it wants every row filled before it renders, not a
    /// schedule. The *background* refresh no longer goes through here — since
    /// every account fired inside one `N * 350ms` window on an exact period,
    /// which is the fleet synchronization [`crate::schedule`] exists to remove.
    /// The server drives [`Self::probe_account`] per account on independent
    /// random schedules instead.
    pub async fn probe_all(&self) {
        let idxs = self.probeable_indices();
        let last = idxs.len().saturating_sub(1);
        for (i, idx) in idxs.into_iter().enumerate() {
            self.probe_account(idx).await;
            // Space the calls so a fleet sweep never bursts the usage endpoint
            // into its own 429 — the burst was what painted a false "error" on
            // every row. Sequential + spaced stays well inside the cadence.
            if i < last {
                tokio::time::sleep(crate::probe::PROBE_SPACING).await;
            }
        }
    }

    /// Probe one account: ensure its token is fresh, read usage, and on a `401`
    /// force-refresh and retry exactly once. Success folds the usage into the
    /// account's windows; any failure is recorded as visible probe health.
    pub async fn probe_account(&self, idx: usize) {
        // An `Error` row only reaches here once its recovery backoff elapsed (see
        // `probeable_indices`). `Error` means "the refresh token was rejected", so the
        // only meaningful recovery test is to attempt a refresh: success runs
        // `apply_refresh`, which clears Error→Active and the backoff — proving the row
        // was never dead. Anything else leaves it errored; a rejection already grew the
        // backoff (a transient failure deliberately did not), so just stand down.
        // Short-circuit: a healthy row never pays for the forced refresh.
        if self.account_status(idx) == Some(AccountStatus::Error)
            && !self.ensure_fresh_force(idx).await
        {
            return;
        }
        self.ensure_fresh(idx).await;
        let Some(token) = self.access_token(idx) else {
            return;
        };

        let result = self.prober.probe(token).await;
        let result = match result {
            Err(err) if err.status == Some(401) => {
                // Token rejected: force a refresh and retry once with the new one.
                self.ensure_fresh_force(idx).await;
                match self.access_token(idx) {
                    Some(fresh) => self.prober.probe(fresh).await,
                    None => Err(err),
                }
            }
            other => other,
        };

        match result {
            Ok(usage) => {
                self.apply_usage(idx, &usage);
                self.record_probe(idx, ProbeStatus::Ok, None);
            }
            Err(err) => {
                let msg = err.message.to_lowercase();
                let is_timeout = msg.contains("timed out") || msg.contains("timeout");
                // A failing probe never blanks the last-learned bar (no apply_usage
                // on this path), so the only question is how loud to be. Soften only
                // ENDPOINT-side failures where the credential is provably fine: a 429
                // (the usage endpoint throttling the probe itself) or a transient 5xx
                // read as a benign, self-clearing `RateLimited`, never the false
                // fleet-wide "error" a bursted sweep used to paint. But a TRANSPORT
                // failure (no HTTP status, not a timeout — connection refused, DNS/TLS,
                // a proxy-env regression) stays a visible `Error`: a persistent one is
                // a real connectivity problem and must NOT hide behind a benign label
                // (probe health is first-class — masking it defeats the point).
                let status = match err.status {
                    Some(429) => ProbeStatus::RateLimited,
                    Some(s) if (500..=599).contains(&s) => ProbeStatus::RateLimited,
                    Some(_) => ProbeStatus::Error,
                    None if is_timeout => ProbeStatus::Timeout,
                    None => ProbeStatus::Error,
                };
                self.record_probe(idx, status, Some(err.message));
            }
        }
    }
}
