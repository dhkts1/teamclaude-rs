//! `Manager` usage methods, split verbatim from `mod.rs`.

use super::*;

impl Manager {
    /// Fold a response's rate-limit headers into account `idx`'s quota. This runs
    /// on **every** upstream response (including 429s that trigger a rotation), so
    /// it deliberately does NOT touch the request counter — that would double-count
    /// a client request that was retried across accounts (bug #4). Request counting
    /// happens once, in [`Manager::record_served`].
    pub fn update_quota(&self, idx: usize, headers: &reqwest::header::HeaderMap) {
        let mut accounts = self.accounts.write().expect("accounts lock poisoned");
        if let Some(account) = accounts.get_mut(idx) {
            account.quota.update_from_headers(headers);
        }
    }

    /// Add token usage to account `idx` (the true serving account — bug #3).
    /// `input_tokens` already includes `cache_creation_input_tokens` +
    /// `cache_read_input_tokens` (summed by the caller — bug #4).
    pub fn update_usage(&self, idx: usize, input_tokens: u64, output_tokens: u64) {
        let mut accounts = self.accounts.write().expect("accounts lock poisoned");
        if let Some(account) = accounts.get_mut(idx) {
            account.input_tokens += input_tokens;
            account.output_tokens += output_tokens;
        }
    }

    /// Fold a background probe's usage into account `idx`'s quota windows.
    pub fn apply_usage(&self, idx: usize, usage: &Usage) {
        let mut accounts = self.accounts.write().expect("accounts lock poisoned");
        if let Some(account) = accounts.get_mut(idx) {
            account.quota.apply_usage(usage);
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
