//! `Manager` OAuth token-refresh methods, split verbatim from `mod.rs`.

use super::*;

impl Manager {
    /// Refresh account `idx`'s token if it is hard-expired OR within the proactive
    /// [`oauth::EXPIRING_SOON_MS`] buffer of expiry, coalescing concurrent callers
    /// into a single upstream refresh. Refreshing ahead of the hard boundary keeps
    /// a batch of in-flight requests from all carrying a just-dead token into an
    /// upstream 401 storm.
    pub async fn ensure_fresh(&self, idx: usize) {
        self.ensure_fresh_inner(idx, false).await;
    }

    /// Force a refresh regardless of expiry (used after a 401), still coalesced.
    /// Returns `true` iff a new token was actually applied — a `false` return means
    /// the force was coalesced, cooldown-suppressed, or the upstream refresh failed,
    /// so the caller (the proxy 401 arm) rotates instead of retrying a dead token.
    pub async fn ensure_fresh_force(&self, idx: usize) -> bool {
        self.ensure_fresh_inner(idx, true).await
    }

    /// Returns `true` iff [`Self::apply_refresh`] ran (a new token was applied).
    async fn ensure_fresh_inner(&self, idx: usize, force: bool) -> bool {
        // Decide whether to refresh and snapshot the access token we intend to
        // replace — all before taking the (async) coalescing lock.
        let Some((refresh_token, access_before)) = self.refresh_plan(idx, force, crate::now_ms())
        else {
            return false;
        };

        let lock = match self
            .accounts
            .read()
            .expect("accounts lock poisoned")
            .get(idx)
        {
            Some(account) => account.refresh_lock.clone(),
            None => return false,
        };
        let _guard = lock.lock().await;

        // Coalesce ALL concurrent callers, the force path included (bug #10): a
        // leader that already refreshed while we queued on the lock changed the
        // access token, so a follower sees a different token here and skips the
        // upstream refresh entirely — and so never sends the now-rotated stale
        // refresh token (which would 401 and wrongly sideline a healthy account).
        if self.access_token(idx).as_deref() != Some(access_before.as_str()) {
            return false;
        }
        // A *transient* leader failure leaves the access token unchanged, so the
        // guard above can't catch us — but the leader stamped a short cooldown
        // before releasing the lock. Honor it: skip re-POSTing the same
        // single-use refresh token while the cooldown is still live. It
        // self-clears by time, so a later sequential retry is not suppressed.
        {
            let accounts = self.accounts.read().expect("accounts lock poisoned");
            if let Some(until) = accounts.get(idx).and_then(|a| a.refresh_retry_after_ms) {
                if crate::now_ms() < until {
                    return false;
                }
            }
        }
        let (name, account_uuid, org_uuid, org_name) = {
            let accounts = self.accounts.read().expect("accounts lock poisoned");
            match accounts.get(idx) {
                Some(a) => (
                    a.name.clone(),
                    a.account_uuid.clone(),
                    a.org_uuid.clone(),
                    a.org_name.clone(),
                ),
                None => return false,
            }
        };

        tracing::info!(account = %name, "refreshing OAuth token");
        match self.refresher.refresh(refresh_token).await {
            Ok(tokens) => {
                // A *forced* success re-arms the refresh cooldown, collapsing a herd
                // of concurrent 401-driven force-refreshes to ONE rotation instead of
                // N back-to-back. Passing the deadline INTO apply_refresh folds the
                // re-arm and the success-path clear into one write, so the throttle
                // can never be clobbered by an ordering slip between two lock
                // sections — the policy (whether/what) stays here, the single atomic
                // write lives in apply_refresh.
                let cooldown_after = force.then(Self::cooldown_deadline);
                self.apply_refresh(idx, &tokens, cooldown_after);
                self.persist_tokens(&name, account_uuid, org_uuid, org_name, &tokens);
                tracing::info!(account = %name, "token refreshed");
                true
            }
            Err(oauth::OAuthError::AuthRejected { status }) => {
                tracing::error!(account = %name, status, "refresh token rejected — re-login needed");
                if let Some(a) = self
                    .accounts
                    .write()
                    .expect("accounts lock poisoned")
                    .get_mut(idx)
                {
                    a.status = AccountStatus::Error;
                }
                // A REJECTED refresh is the one signal that this credential may truly
                // be dead — back off before the next recovery attempt (first rejection
                // arms the base delay; each further one doubles it). A *transient*
                // failure deliberately does NOT back off: it proves nothing, so the
                // next probe tick may retry promptly.
                self.grow_error_backoff(idx);
                false
            }
            Err(oauth::OAuthError::Transient(msg)) => {
                // Keep the current token; this one request fails over instead.
                // The token is UNCHANGED, so the access-token guard can't stop a
                // follower already queued on the lock from re-POSTing the same
                // single-use refresh token. Stamp a short self-clearing cooldown
                // (still under this lock, so queued followers see it) to coalesce
                // the concurrent batch; a later sequential retry still refreshes.
                if let Some(a) = self
                    .accounts
                    .write()
                    .expect("accounts lock poisoned")
                    .get_mut(idx)
                {
                    a.refresh_retry_after_ms = Some(Self::cooldown_deadline());
                }
                tracing::warn!(account = %name, error = %msg, "token refresh transient failure");
                false
            }
        }
    }

    /// If account `idx` should be refreshed now, return `(refresh_token,
    /// access_before)` — the refresh token to send and the access token we mean
    /// to replace. `access_before` lets [`Self::ensure_fresh_inner`] detect, under
    /// the coalescing guard, that another caller already refreshed (the access
    /// token changed), coalescing even the force path (bug #10).
    pub(super) fn refresh_plan(
        &self,
        idx: usize,
        force: bool,
        now_ms: i64,
    ) -> Option<(String, String)> {
        let accounts = self.accounts.read().expect("accounts lock poisoned");
        let account = accounts.get(idx)?;
        if account.account_type != "oauth" {
            return None;
        }
        let refresh_token = account.refresh_token.clone()?;
        let is_expired = oauth::is_expired(account.expires_at_ms, now_ms);
        // Proactive refresh: a token within the EXPIRING_SOON buffer is refreshed
        // before it hard-expires, so a batch of in-flight requests at the boundary
        // never all carry a just-dead token into an upstream 401 storm.
        let is_expiring_soon = oauth::is_expiring_soon(account.expires_at_ms, now_ms);
        if !(force || is_expired || is_expiring_soon) {
            return None;
        }
        // A transient failure stamped a short cooldown to coalesce the batch —
        // don't START a new refresh during it. A hard-expired token under
        // `force` still overrides, so a dead-in-hand token is never pinned
        // un-refreshable by the cooldown (the cooldown also self-clears by time).
        let in_cooldown = account
            .refresh_retry_after_ms
            .is_some_and(|until| now_ms < until);
        if in_cooldown && !(force && is_expired) {
            return None;
        }
        Some((refresh_token, account.access_token.clone()))
    }

    /// The wall-clock deadline before which no new refresh of an account may
    /// START. Single source of the cooldown formula so the transient-failure arm
    /// and the forced-success re-arm can never drift apart.
    fn cooldown_deadline() -> i64 {
        crate::now_ms() + REFRESH_RETRY_COOLDOWN_MS
    }

    /// Install a freshly refreshed token pair. `cooldown_after` is the refresh
    /// floor set atomically with the install: `None` clears any transient-failure
    /// cooldown (a good refresh ends it); `Some(deadline)` re-arms it for a
    /// *forced* success so a herd of concurrent 401 force-refreshes collapses to
    /// one rotation. Folding it in here makes the clear-vs-re-arm a single write —
    /// no ordering hazard between two separate lock sections.
    fn apply_refresh(&self, idx: usize, tokens: &Tokens, cooldown_after: Option<i64>) {
        let mut accounts = self.accounts.write().expect("accounts lock poisoned");
        if let Some(account) = accounts.get_mut(idx) {
            account.access_token = tokens.access_token.clone();
            // THE TRAP: `tokens.refresh_token` must be `Some(...)` on every
            // reachable refresh success — see [`Tokens::refresh_token`]'s doc
            // comment. Writing a bare `None` here would silently make a
            // healthy account unrefreshable with no error and no log line.
            account.refresh_token = tokens.refresh_token.clone();
            account.expires_at_ms = Some(tokens.expires_at_ms);
            account.refresh_retry_after_ms = cooldown_after;
            if account.status == AccountStatus::Error {
                // RECOVERY: a refresh just succeeded, which a genuinely dead
                // credential could never do — so this row was never dead, and it is
                // now holding a fresh valid token. Clear the error backoff with it.
                account.status = AccountStatus::Active;
            }
            account.error_retry_after_ms = None;
            account.error_backoff_ms = 0;
        }
    }

    /// Arm (or double) the recovery backoff for an errored row. First rejection
    /// arms [`ERROR_REPROBE_BASE_MS`]; each subsequent one doubles up to
    /// [`ERROR_REPROBE_CAP_MS`], so a permanently dead credential is retried at most
    /// ~twice an hour instead of hammering the OAuth endpoint (which would risk
    /// rate-limiting the whole fleet).
    pub(super) fn grow_error_backoff(&self, idx: usize) {
        let mut accounts = self.accounts.write().expect("accounts lock poisoned");
        if let Some(account) = accounts.get_mut(idx) {
            account.error_backoff_ms = if account.error_backoff_ms == 0 {
                ERROR_REPROBE_BASE_MS
            } else {
                account
                    .error_backoff_ms
                    .saturating_mul(2)
                    .min(ERROR_REPROBE_CAP_MS)
            };
            account.error_retry_after_ms = Some(crate::now_ms() + account.error_backoff_ms);
        }
    }
}
