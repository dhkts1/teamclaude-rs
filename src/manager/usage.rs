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
    ///
    /// Returns whether THIS response's headers carried the 5h window — the same
    /// signal [`crate::quota::Quota::update_from_headers`] returns, passed through
    /// so a caller like [`Manager::warm_account`] can tell "evidence read" apart
    /// from "silence" on this specific response, not just the account's
    /// once-ever `quota_known` latch. Deliberately NOT `#[must_use]`: most
    /// callers (every served-response path in `proxy.rs`, most existing tests)
    /// only care about the side effect of folding the headers in, and forcing
    /// every one of them to consume the bool would be pure churn unrelated to
    /// this fix.
    pub fn update_quota(&self, idx: usize, headers: &reqwest::header::HeaderMap) -> bool {
        let (newly_known, read_five_hour) = {
            let mut accounts = self.accounts.write().expect("accounts lock poisoned");
            match accounts.get_mut(idx) {
                Some(account) => {
                    let read_five_hour = account.quota.update_from_headers(headers);
                    let flipped = read_five_hour && !account.quota_known;
                    account.quota_known |= read_five_hour;
                    // Any response that DOES carry the 5h window is proof this
                    // account is not stuck returning header-less responses —
                    // resets keep-warm's miss counter regardless of whether this
                    // particular update was the warm path or a served request.
                    if read_five_hour {
                        account.consecutive_warms_without_evidence = 0;
                    }
                    (flipped, read_five_hour)
                }
                None => (false, false),
            }
        };
        // Edge-triggered, with the accounts lock RELEASED — identical to
        // `apply_usage`, whose comment explains both halves.
        if newly_known {
            self.warm_wake.notify_one();
        }
        read_five_hour
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

    /// Record that account `idx` served a stream that failed to complete cleanly
    /// (`kind` is either an Anthropic `error.error.type`, e.g. `"overloaded_error"`,
    /// from an in-band SSE `error` event, or the fixed string `"truncated"` for a
    /// stream that hit EOF without Anthropic's `message_stop` terminator) — a
    /// turn the client saw as a 200 that must not read as a clean serve.
    /// OBSERVABILITY ONLY: this warns and updates a decayed counter and
    /// the account's last-error label; it deliberately does NOT call
    /// `mark_error` (that condemns terminally — see its doc comment on the 2026-07-17
    /// incident) or `mark_rate_limited` (an overloaded account is not over quota —
    /// see [`Self::mark_rate_limited`]'s doc comment), and nothing in
    /// `select.rs`'s `eligible`/`hard_ok`/`account_terminal_gate` reads the
    /// counter this writes — wiring it into routing is a separate, later,
    /// premortem'd change.
    pub fn record_stream_error(&self, idx: usize, kind: &str) {
        let now_ms = crate::now_ms();
        let mut accounts = self.accounts.write().expect("accounts lock poisoned");
        if let Some(account) = accounts.get_mut(idx) {
            tracing::warn!(
                account = %account.name,
                index = idx,
                error_type = kind,
                "stream failed to complete cleanly — truncated turn, not a clean serve"
            );
            account.stream_error_times_ms.push_back(now_ms);
            prune_stream_errors(&mut account.stream_error_times_ms, now_ms);
            account.last_stream_error = Some(kind.to_string());
        }
    }
}

/// Prune `times` to [`STREAM_ERROR_WINDOW_MS`] and hard-cap at
/// [`STREAM_ERROR_CAP`] entries, oldest first. The mutating half of pruning —
/// called on INSERT, where the caller already holds the accounts write lock.
fn prune_stream_errors(times: &mut VecDeque<i64>, now_ms: i64) {
    while times
        .front()
        .is_some_and(|&t| now_ms - t > STREAM_ERROR_WINDOW_MS)
    {
        times.pop_front();
    }
    while times.len() > STREAM_ERROR_CAP {
        times.pop_front();
    }
}

/// The account's DECAYED stream-error count as of `now_ms`: entries within
/// [`STREAM_ERROR_WINDOW_MS`]. The read-side half of pruning — [`Manager::snapshot`]
/// holds only the accounts READ lock, so it cannot pop stale entries the way
/// [`prune_stream_errors`] does on insert; this counts without mutating instead,
/// which is equivalent for display purposes (the next insert physically prunes).
pub(super) fn stream_error_count(times: &VecDeque<i64>, now_ms: i64) -> usize {
    times
        .iter()
        .filter(|&&t| now_ms - t <= STREAM_ERROR_WINDOW_MS)
        .count()
}
