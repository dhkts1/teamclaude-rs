//! `Manager` usage methods, split verbatim from `mod.rs`.

use super::*;

impl Manager {
    /// Fold a response's rate-limit headers into account `idx`'s quota. This runs
    /// on **every** upstream response (including 429s that trigger a rotation), so
    /// it deliberately does NOT touch the request counter — that would double-count
    /// a client request that was retried across accounts (bug #4). Request counting
    /// happens once, in [`Manager::record_served`].
    ///
    /// A response that CARRIED the unified 5h header also latches
    /// [`AccountRuntime::quota_known`], on the same terms and for the same reason
    /// [`Self::apply_usage`] does: those headers are a first-hand read of this
    /// account's session window, so an account serving live traffic must not go on
    /// reading as "we have never seen this account's quota" — it plainly is not
    /// true, and keep-warm's gate is asking exactly that question. A response
    /// WITHOUT the header latches nothing: an error page or a non-Anthropic
    /// upstream tells us nothing about the window, and silence must never be
    /// mistaken for evidence.
    ///
    /// `probe_status` is deliberately still untouched here — that field is about
    /// probe HEALTH, which a served response says nothing about.
    pub fn update_quota(&self, idx: usize, headers: &reqwest::header::HeaderMap) {
        let newly_known = {
            let mut accounts = self.accounts.write().expect("accounts lock poisoned");
            match accounts.get_mut(idx) {
                Some(account) => {
                    let read_five_hour = account.quota.update_from_headers(headers);
                    let flipped = read_five_hour && !account.quota_known;
                    account.quota_known |= read_five_hour;
                    flipped
                }
                None => false,
            }
        };
        // Edge-triggered, with the accounts lock RELEASED — identical to
        // `apply_usage`, whose comment explains both halves.
        if newly_known {
            self.warm_wake.notify_one();
        }
    }

    /// Add token usage to account `idx` (the true serving account — bug #3).
    /// `input_tokens` already includes `cache_creation_input_tokens` +
    /// `cache_read_input_tokens` (summed by the caller — bug #4), and remains the
    /// quota counter. The `cache_read` / `cache_creation` components are ALSO
    /// tracked separately (they are a SUBSET of `input_tokens`, not additional
    /// quota) so the prompt-cache hit ratio is visible per account without
    /// changing what counts against quota.
    pub fn update_usage(
        &self,
        idx: usize,
        input_tokens: u64,
        output_tokens: u64,
        cache_read: u64,
        cache_creation: u64,
    ) {
        let mut accounts = self.accounts.write().expect("accounts lock poisoned");
        if let Some(account) = accounts.get_mut(idx) {
            account.input_tokens += input_tokens;
            account.output_tokens += output_tokens;
            account.cache_read_tokens += cache_read;
            account.cache_creation_tokens += cache_creation;
        }
    }

    /// Fold a background probe's usage into account `idx`'s quota windows and latch
    /// [`AccountRuntime::quota_known`].
    ///
    /// This is the ONLY place the latch is set, and that is the whole point: it runs
    /// exclusively on the `Ok` arm of [`Manager::probe_account`], so every probe
    /// FAILURE — `Error`, `Timeout`, `RateLimited` — leaves the latch (and therefore
    /// the account's keep-warm eligibility) untouched. Note it latches even when the
    /// endpoint reported no 5h bucket at all: "we have read this account's quota" is
    /// about the READ, not about which windows came back.
    pub fn apply_usage(&self, idx: usize, usage: &Usage) {
        let newly_known = {
            let mut accounts = self.accounts.write().expect("accounts lock poisoned");
            match accounts.get_mut(idx) {
                Some(account) => {
                    account.quota.apply_usage(usage);
                    let flipped = !account.quota_known;
                    account.quota_known = true;
                    flipped
                }
                None => false,
            }
        };
        // Wake the keep-warm loop only on the false→true flip — at most once per
        // account per process, so the loop can never be spun by a steady-state
        // probe cadence. Signalled with the accounts lock RELEASED: the loop's
        // first act on waking is `warm_targets()`, which takes that same lock.
        if newly_known {
            self.warm_wake.notify_one();
        }
    }

    /// Hold account `idx` out of rotation for `seconds` (a 429 quota rejection).
    /// The hold is clamped to at most [`MAX_RATE_LIMIT_HOLD_SECONDS`] so a huge
    /// `retry-after` (Anthropic weekly caps report hours) can never pin a healthy
    /// account out for that long with no revalidation path: it is re-selected
    /// after the bounded hold, and either serves or is re-held. Durable
    /// exhaustion is separately kept out of rotation by the
    /// learned quota utilization, not by this short-term hold.
    pub fn mark_rate_limited(&self, idx: usize, seconds: i64) {
        let hold = seconds.clamp(0, MAX_RATE_LIMIT_HOLD_SECONDS);
        let until = crate::now_ms() + hold * 1000;
        let mut accounts = self.accounts.write().expect("accounts lock poisoned");
        if let Some(account) = accounts.get_mut(idx) {
            account.status = AccountStatus::Throttled;
            account.rate_limited_until_ms = Some(until);
            tracing::info!(account = %account.name, hold_seconds = hold, "rate limited");
        }
    }

    /// Clear a rate-limit hold after live proof it no longer binds (any non-429).
    pub fn clear_rate_limited(&self, idx: usize) {
        let mut accounts = self.accounts.write().expect("accounts lock poisoned");
        if let Some(account) = accounts.get_mut(idx) {
            if account.status == AccountStatus::Throttled {
                account.status = AccountStatus::Active;
                account.rate_limited_until_ms = None;
            }
        }
    }
}
