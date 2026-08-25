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
                    // Also clears the cooldown timestamp so no stale retry-after
                    // lingers on a row that just proved it does not need one.
                    if read_five_hour {
                        account.consecutive_warms_without_evidence = 0;
                        account.warm_evidence_retry_after_ms = None;
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
        // The caller of this shape only has the FLAT cache-creation count, so
        // the whole of it is booked as a 5-minute write. `input_tokens` is kept
        // VERBATIM as the quota figure — `account.input_tokens` grows by
        // exactly what was passed, as it always has — and only the base input
        // used for PRICING is derived from it; see
        // [`crate::usage::UsageRecord::from_quota_input`] for what happens when
        // a caller's components exceed its own total.
        self.record_usage(
            idx,
            crate::usage::UsageRecord::from_quota_input(
                crate::now_ms(),
                None,
                None,
                input_tokens,
                cache_creation,
                0,
                cache_read,
                output_tokens,
            ),
        );
    }

    /// **The single entry point for a served request's usage.** Every token this
    /// fleet spends passes through here exactly once, and nothing else may
    /// increment the four cumulative counters.
    ///
    /// It does two separable things, in this order:
    ///
    /// 1. Adds to the cumulative per-account counters, whose meanings are
    ///    UNCHANGED by this function existing — `input_tokens` is still the
    ///    quota counter with cache creation and cache reads folded in
    ///    ([`crate::usage::UsageRecord::input_total`] reproduces it exactly),
    ///    and `cache_creation_tokens` is still both TTLs together. `status.rs`
    ///    explains why an existing wire field may never quietly change meaning.
    /// 2. Records the same request in the usage tracker, which is where the
    ///    model, the 5m/1h cache split, the cost and the time buckets live.
    ///
    /// The accounts write lock is RELEASED before step 2 — the two locks are
    /// never held together, the same discipline `update_quota` follows for the
    /// warm-wake notify.
    ///
    /// Deliberately does NOT touch the request counter: a client request retried
    /// across accounts reaches this once per upstream RESPONSE, and counting
    /// requests here is bug #4. That count happens in [`Manager::record_served`].
    pub fn record_usage(&self, idx: usize, record: crate::usage::UsageRecord) {
        let (name, org) = {
            let mut accounts = self.accounts.write().expect("accounts lock poisoned");
            match accounts.get_mut(idx) {
                Some(account) => {
                    account.input_tokens += record.input_total();
                    account.output_tokens += record.output;
                    account.cache_read_tokens += record.cache_read;
                    account.cache_creation_tokens += record.cache_creation();
                    (
                        account.name.clone(),
                        crate::identity::org_key_of(
                            account.org_uuid.as_deref(),
                            account.org_name.as_deref(),
                        )
                        .map(str::to_string),
                    )
                }
                // An index with no account is not this function's to complain
                // about — the caller's rotation loop already resolved it, and a
                // configuration reloaded underneath is not a reason to record
                // the tokens against somebody else.
                None => return,
            }
        };
        self.usage.record(idx, &record, &name, org.as_deref());
    }

    /// Attach `dir` as the usage ledger: replay today's and yesterday's files
    /// back through the recording path so a restart does not lose the day, prune
    /// day files past `retention_days`, then start appending.
    ///
    /// Only the SERVING process calls this. Returns what the replay found so the
    /// caller can log whether the day was restored or genuinely started at zero.
    pub fn attach_usage_ledger(
        &self,
        dir: std::path::PathBuf,
        retention_days: u32,
    ) -> crate::usage::ReplayReport {
        // Name AND org: the ledger resolves a line by identity, because the same
        // email in two orgs is two accounts (`identity.rs`) and a name alone
        // cannot tell them apart.
        let accounts: Vec<crate::usage::LedgerAccount> = self
            .accounts
            .read()
            .expect("accounts lock poisoned")
            .iter()
            .map(|a| crate::usage::LedgerAccount {
                name: a.name.clone(),
                org: crate::identity::org_key_of(a.org_uuid.as_deref(), a.org_name.as_deref())
                    .map(str::to_string),
            })
            .collect();
        self.usage
            .attach_ledger(dir, retention_days, &accounts, crate::now_ms())
    }

    /// This account's usage row as of `now_ms` — see
    /// [`crate::usage::UsageTracker::row`].
    pub(super) fn usage_row(
        &self,
        idx: usize,
        now_ms: i64,
        five_hour_reset_ms: Option<i64>,
    ) -> tcr_status_wire::UsageRow {
        self.usage.row(idx, now_ms, five_hour_reset_ms)
    }

    /// The configured usage-ledger retention, in days
    /// ([`crate::config::Config::usage_retention_days`]). Read from the config
    /// this manager booted with, like every other boot-time knob.
    pub fn usage_retention_days(&self) -> u32 {
        self.config
            .lock()
            .expect("config lock poisoned")
            .usage_retention_days
    }

    /// Whether a usage ledger is attached AND writing — i.e. whether today's
    /// totals will survive a restart. False on every offline/test manager, and
    /// false again the moment the ledger's writes start failing.
    pub fn usage_is_persisting(&self) -> bool {
        self.usage.is_persisting()
    }

    /// Ledger lines dropped for a full writer queue — 0 unless the volume
    /// holding `~/.cache` stalled long enough to back the writer up.
    pub fn usage_dropped_lines(&self) -> u64 {
        self.usage.dropped_lines()
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
    ///
    /// Deliberately does NOT reset [`AccountRuntime::consecutive_warms_without_evidence`],
    /// even though this can latch [`AccountRuntime::quota_known`] just like a
    /// header-bearing served response does. A first cut of this fix reset it
    /// here unconditionally and reopened the exact loop the counter exists to
    /// close: `probeable_indices` (unlike [`Self::warm_targets`]) does not check
    /// this counter, so the background prober keeps visiting an excluded account
    /// on its OWN schedule — reset it on every successful probe and a
    /// persistently header-less account gets re-warmed once per probe cycle
    /// forever, worse than the original unbounded-warm bug this counter fixed.
    /// A probe finding no 5h bucket also says nothing about whether the next
    /// WARM response would carry one — it is a different, zero-spend endpoint,
    /// not a rehearsal of the warm request the counter is bounding. Recovery
    /// from the exclusion is [`Manager::record_warm_without_evidence`]'s
    /// [`AccountRuntime::warm_evidence_retry_after_ms`] cooldown instead — a
    /// flat wall-clock wait, decoupled from probe cadence, so the retry rate is
    /// bounded by policy rather than by how often the fleet happens to probe.
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
