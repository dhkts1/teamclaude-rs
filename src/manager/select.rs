//! `Manager` account-selection and eligibility methods, split verbatim from `mod.rs`.

use super::*;

impl Manager {
    /// Pick the best eligible account not in `tried`, spreading load across the
    /// fleet, or `None` if all are exhausted/held/disabled.
    ///
    /// Within a priority tier we pick the **least-recently-selected** account
    /// (lowest `last_selected_seq`; a never-selected account sorts first) so
    /// consecutive requests fan out instead of hammering one account. Ordering by
    /// quota headroom was rejected deliberately: a single request barely moves a
    /// weekly bar, so "most headroom first" would deterministically pin one
    /// account until its bar caught up — the exact overload this fixes. The
    /// winner is stamped with the next monotonic tick *before returning*, so even
    /// a burst of concurrent selects rotates (each sees the previous stamp). The
    /// soonest weekly reset is the final cold-start tiebreak (all-unseen startup).
    ///
    /// This mutates rotation state (the stamp), so it takes the write lock.
    ///
    /// `model` is the request's target model (if known). When it names a Fable
    /// model, an account whose model-scoped weekly (`7d_oi`) bucket is exhausted
    /// is skipped — while that same account still serves every non-Fable model.
    ///
    /// `affinity` is the caller's session key (opt-in; `None` when the feature is
    /// off). With `None` this is byte-for-byte the pre-affinity behaviour. With
    /// `Some(key)`: if the session is already pinned to an account that is not in
    /// `tried` and still passes [`Self::eligible`], that pinned account is
    /// returned — still stamped with a fresh select tick so *other* sessions' LRU
    /// steers away from a busy pinned account. Otherwise a normal LRU/priority
    /// pick runs. Affinity never overrides priority — the pin is always a normal
    /// pick.
    ///
    /// **The invariant, and the whole point of a pin: a session's pin is re-keyed
    /// ONLY when the pinned account fails a HARD gate** ([`Self::hard_ok`] is the
    /// sole authority). Anthropic's prompt cache is per-account, so a re-key
    /// re-creates the entire conversation prefix on a cold account. Every SOFT
    /// failure — the utilization threshold, pacing, or a transient upstream blip
    /// that merely landed the pin in `tried` — DIVERTS this ONE request to the
    /// fall-through pick while the pin stays where it is (see `keep_pin`). Only
    /// disabled / [`AccountStatus::Error`] / a live hold / `rejected` /
    /// Fable-weekly-exhausted re-key durably. This is self-healing: a durable
    /// failure arms a hold or sets `rejected` on the very next attempt, and those
    /// fail `hard_ok`.
    pub fn select(
        &self,
        tried: &HashSet<usize>,
        now: OffsetDateTime,
        model: Option<&str>,
        affinity: Option<u64>,
    ) -> Option<usize> {
        // Hard account lock: pin ALL traffic to the configured account, bypassing
        // rotation/affinity/migration. `tried` still ends the rotation loop — once the
        // locked account has failed this request, return None (no failover to the pool).
        if let Some(li) = self.locked_idx {
            return if tried.contains(&li) { None } else { Some(li) };
        }

        let now_ms = odt_to_ms(now);
        // Compute the Fable classification ONCE, not per-account.
        let is_fable = model.is_some_and(crate::model::is_fable_model);

        // Set (to the OLD pin index) whenever the pin failed only SOFT gates, i.e. it
        // still clears every HARD gate (`Self::hard_ok`). A soft gate may DIVERT a
        // request; it may never RE-KEY a session — Anthropic's prompt cache is
        // per-account, so a re-pin re-creates the whole conversation prefix on the
        // next request. The fall-through re-pin at the bottom honours this and keeps
        // the old index.
        let mut keep_pin: Option<usize> = None;

        // Affinity fast-path: honour an existing pin when it is still usable. Read
        // the pin under the affinity lock, then DROP that lock before taking the
        // accounts lock (never nest the two — that is the documented deadlock).
        if let Some(key) = affinity {
            // (1) UNDER THE AFFINITY LOCK ONLY: read this session's pin `X` and, in
            // the same critical section, tally the per-account pinned-session counts
            // that the load-balancing decision needs. Drop this lock before taking
            // the accounts lock — the two are NEVER held simultaneously (the
            // documented deadlock).
            let (pinned, counts) = {
                let pins = self.affinity.lock().expect("affinity lock poisoned");
                let pinned = pins.get(&key).map(|&(idx, _)| idx);
                let mut counts: HashMap<usize, usize> = HashMap::new();
                for &(idx, _) in pins.values() {
                    *counts.entry(idx).or_insert(0) += 1;
                }
                (pinned, counts)
            };
            if let Some(idx) = pinned {
                if tried.contains(&idx) {
                    // The pin already failed THIS request upstream. That is NOT proof
                    // the account is gone — a transient blip (a dropped connection, a
                    // 5xx) leaves every HARD gate clear. So divert this request and
                    // keep the pin. Self-healing: a DURABLE failure arms a hold or
                    // sets `rejected`, both of which fail `hard_ok`, so the re-key
                    // still happens — one request later, on evidence.
                    let accounts = self.accounts.read().expect("accounts lock poisoned");
                    if accounts.get(idx).is_some_and(|a| {
                        Self::hard_ok(a, self.global_threshold, now, now_ms, is_fable)
                    }) {
                        keep_pin = Some(idx);
                    }
                } else {
                    let count_x = counts.get(&idx).copied().unwrap_or(0);
                    // (2) UNDER THE ACCOUNTS LOCK ONLY: confirm the pin `X` is still
                    // usable, then — and only when >=2 sessions stack on `X` — look
                    // for the least-loaded ELIGIBLE account `Y` that strictly improves
                    // balance (`count(Y)+1 < count(X)`). Stamp whichever we settle on.
                    // `None` means `X` is ineligible → fall through to the normal
                    // pick/re-pin path (which already handles a dead pin).
                    let decision: Option<(usize, Option<(String, String)>)> = {
                        let mut accounts = self.accounts.write().expect("accounts lock poisoned");
                        // Respect pacing on the pin: a saturated pinned account
                        // temporarily YIELDS here and the LRU pick below (with its
                        // soft fallback) steers to a cooler account, so a session's
                        // concurrent burst spreads instead of all slamming one pin.
                        // The yield is per-REQUEST only — `keep_pin` below holds the
                        // session on this account so the burst spreads without ever
                        // moving the pin.
                        let x_usable = accounts.get(idx).is_some_and(|a| {
                            Self::eligible(
                                a,
                                self.global_threshold,
                                &self.pacing,
                                true,
                                now,
                                now_ms,
                                is_fable,
                            )
                        });
                        if !x_usable {
                            // Re-test the SAME account against the HARD gates ALONE to
                            // separate a SOFT yield from a real block. Passing here
                            // means the pin cleared disabled/error/hold/rejected/
                            // Fable-weekly and only tripped a soft gate — the
                            // utilization threshold (`quota.is_near`) or the
                            // concurrency-cap / min-spacing pacing gate. Both are
                            // rotation hints: divert THIS request to the fall-through
                            // pick but keep the pin. Failing here is a genuine HARD
                            // block, which keeps today's durable re-pin (the session's
                            // account really is gone).
                            //
                            // The utilization case is the load-bearing one: an account
                            // crossing the soft threshold used to dump EVERY session
                            // pinned to it, simultaneously, cold-starting all of them.
                            if accounts.get(idx).is_some_and(|a| {
                                Self::hard_ok(a, self.global_threshold, now, now_ms, is_fable)
                            }) {
                                keep_pin = Some(idx);
                            }
                            None
                        } else {
                            // Default: honour the pin. When `count_x < 2` (a LONE
                            // session) `target` stays `idx` and nothing below runs, so
                            // this path is byte-identical to the pre-migration
                            // behaviour — a lone session's warm cache is never moved.
                            let mut target = idx;
                            let mut migrate_names: Option<(String, String)> = None;
                            if count_x >= 2 {
                                // Least-loaded eligible target, ordered by
                                // (pinned-session-count asc, in_flight asc,
                                // last_selected_seq asc / LRU). A candidate qualifies
                                // ONLY if it strictly improves balance — this guard
                                // prevents thrash and equal-swaps.
                                let mut best: Option<usize> = None;
                                let mut best_key: Option<(usize, u32, u64)> = None;
                                for (cand, account) in accounts.iter().enumerate() {
                                    if cand == idx {
                                        continue;
                                    }
                                    let count_y = counts.get(&cand).copied().unwrap_or(0);
                                    if count_y + 1 >= count_x {
                                        continue;
                                    }
                                    // Only migrate onto an ELIGIBLE account — the same
                                    // gate the pin honours (disabled/error/rate-limit/
                                    // quota/pacing), so a throttled `Y` is never chosen.
                                    if !Self::eligible(
                                        account,
                                        self.global_threshold,
                                        &self.pacing,
                                        true,
                                        now,
                                        now_ms,
                                        is_fable,
                                    ) {
                                        continue;
                                    }
                                    let cand_key =
                                        (count_y, account.in_flight, account.last_selected_seq);
                                    if best_key.is_none_or(|b| cand_key < b) {
                                        best = Some(cand);
                                        best_key = Some(cand_key);
                                    }
                                }
                                if let Some(y) = best {
                                    let x_name = accounts
                                        .get(idx)
                                        .map(|a| a.name.clone())
                                        .unwrap_or_default();
                                    let y_name =
                                        accounts.get(y).map(|a| a.name.clone()).unwrap_or_default();
                                    migrate_names = Some((x_name, y_name));
                                    target = y;
                                }
                            }
                            // Stamp the chosen account so a second session's LRU steers
                            // away from an account already busy under a pin.
                            let tick = self.select_seq.fetch_add(1, Ordering::Relaxed);
                            if let Some(account) = accounts.get_mut(target) {
                                account.last_selected_seq = tick;
                            }
                            Some((target, migrate_names))
                        }
                    };
                    if let Some((target, migrate_names)) = decision {
                        // (3) UNDER THE AFFINITY LOCK ONLY: commit the pin. Accounts
                        // lock already dropped — never nest.
                        //
                        // TOCTOU close: the `counts` driving the migration decision
                        // were read in section (1) and the lock was then dropped, so a
                        // concurrent select on another session stacked on the same X
                        // could have decided the SAME idle Y in parallel. Committing
                        // both blindly would OVER-migrate (Y over-stacks, X empties —
                        // the inverse of the goal, and it can oscillate). So for a
                        // MIGRATION we RE-VALIDATE against FRESH counts re-tallied from
                        // the live map under this same lock that mutates it: the pin
                        // must still be X AND `count(target)+1 < count(X)` must still
                        // hold. If not, ABORT the move and keep the existing pin. No
                        // accounts lock is taken here (the re-check needs only the
                        // affinity-map counts + the already-chosen target; the next
                        // select re-checks eligibility anyway).
                        let mut pins = self.affinity.lock().expect("affinity lock poisoned");
                        let mut committed = target;
                        if target != idx {
                            let still_pinned_x = pins.get(&key).map(|&(i, _)| i) == Some(idx);
                            let mut count_x_now = 0usize;
                            let mut count_t_now = 0usize;
                            for &(i, _) in pins.values() {
                                if i == idx {
                                    count_x_now += 1;
                                }
                                if i == target {
                                    count_t_now += 1;
                                }
                            }
                            // Strictly-improves-balance guard, re-checked on fresh state.
                            if still_pinned_x && count_t_now + 1 < count_x_now {
                                if let Some((x_name, y_name)) = migrate_names {
                                    tracing::info!(
                                        "affinity: migrate session off {} (n={}) -> {}",
                                        x_name,
                                        count_x_now,
                                        y_name
                                    );
                                }
                            } else {
                                // The decision went stale between sections (a sibling
                                // select already rebalanced): keep the current pin.
                                committed = idx;
                            }
                        }
                        // Re-pin the session (migration target, or the honoured/kept X)
                        // and refresh its last-touch for LRU eviction.
                        pins.insert(key, (committed, now_ms));
                        return Some(committed);
                    }
                }
            }
        }

        // Normal LRU/priority pick (identical to the pre-affinity path). The
        // accounts lock is scoped so it is released before we touch the affinity
        // lock again for the re-pin below.
        let best = {
            let mut accounts = self.accounts.write().expect("accounts lock poisoned");

            // First pass: honour pacing (skip accounts at the concurrency cap or
            // inside the min-spacing window). With pacing OFF this is byte-identical
            // to the pre-pacing pick.
            let mut best = self.pick_eligible(&accounts, tried, now, now_ms, is_fable, true);

            // Soft fallback (CRITICAL — pacing must never DROP a servable request):
            // if pacing gated EVERY account out but at least one is servable ignoring
            // pacing, serve the least-loaded (lowest in_flight, then the normal LRU
            // key). With pacing OFF the first pass and this pass use identical
            // eligibility, so a None first pass ⟹ None here too — default-OFF stays
            // byte-identical (no spurious fallback, no log).
            if best.is_none() {
                if let Some(idx) = self.pick_least_loaded(&accounts, tried, now, now_ms, is_fable) {
                    if let Some(account) = accounts.get(idx) {
                        tracing::info!(
                            account = %account.name,
                            in_flight = account.in_flight,
                            "pacing: all accounts paced, serving least-loaded"
                        );
                    }
                    best = Some(idx);
                }
            }

            // Stamp the chosen account so the next select prefers a different one.
            if let Some(idx) = best {
                let tick = self.select_seq.fetch_add(1, Ordering::Relaxed);
                if let Some(account) = accounts.get_mut(idx) {
                    account.last_selected_seq = tick;
                }
            }
            best
        };

        // Record the pick as this session's pin (initial pin, or re-pin on
        // migration). Skipped entirely when affinity is off, so the map stays
        // empty on the disabled path.
        if let (Some(key), Some(idx)) = (affinity, best) {
            // A SOFT-gated pin (over the utilization threshold, paced, or transiently
            // in `tried`) diverted THIS request only: re-insert the OLD index, not the
            // account we are about to serve. The insert still has to happen — this
            // `now_ms` is one of only two writers of the last-touch stamp that the
            // AFFINITY_CAP eviction below sorts on, so skipping it would make a
            // heavily-diverted session the eviction victim.
            let pin_idx = keep_pin.unwrap_or(idx);
            let moved_off = {
                let mut pins = self.affinity.lock().expect("affinity lock poisoned");
                let previous = pins.get(&key).map(|&(i, _)| i);
                pins.insert(key, (pin_idx, now_ms));
                // Bound the map by size + LRU-by-last-touch: once over AFFINITY_CAP, evict
                // the single oldest-last-touch entry (not the one we just inserted). Stable
                // pins survive reconnects, so this size cap — not a disconnect hook — is
                // what keeps a long-lived proxy from growing the map without limit.
                const AFFINITY_CAP: usize = 1024;
                if pins.len() > AFFINITY_CAP {
                    if let Some((&oldest, _)) = pins.iter().min_by_key(|(_, &(_, touch))| touch) {
                        if oldest != key {
                            pins.remove(&oldest);
                        }
                    }
                }
                // Only a real change is worth a line: `None` on an initial pin, and on
                // a soft divert (where `pin_idx` is the unchanged old index).
                previous.filter(|&p| p != pin_idx)
            };
            // The only durable re-key that happens here, and until now it logged
            // nothing — which is why a 39.4% live account-switch rate went unnoticed.
            // Reaching this line now means the pin failed a HARD gate, so the line
            // doubles as the audit trail for the invariant.
            // The accounts lock is taken ONLY after the affinity lock has dropped
            // above; the two are never held simultaneously.
            if let Some(previous) = moved_off {
                let accounts = self.accounts.read().expect("accounts lock poisoned");
                let old_name = accounts.get(previous).map_or("?", |a| a.name.as_str());
                let new_name = accounts.get(pin_idx).map_or("?", |a| a.name.as_str());
                tracing::info!(
                    "affinity: re-pin session {} off {} -> {} (pin failed a HARD gate)",
                    short_session_id(key),
                    old_name,
                    new_name
                );
            }
        }
        best
    }

    /// LAST-RESORT serve when normal [`Self::select`] found nothing (the whole
    /// fleet reads over the SOFT switch threshold). Serves an account that Anthropic
    /// still allows, ignoring the soft utilization/pacing gates but honoring the
    /// HARD blocks:
    ///   - `disabled` / [`AccountStatus::Error`] → skip;
    ///   - a live rate-limit hold (`rate_limited_until_ms` in the future) → skip;
    ///   - `quota.status == Some("rejected")` → skip (genuinely blocked; would 429);
    ///   - a Fable request whose model-scoped weekly (`7d_oi`) is a hard reject → skip
    ///     (that same account still serves every non-Fable model).
    ///
    /// TWO paths, mirroring [`Self::select`]'s never-nested lock discipline (read the
    /// pin under the affinity lock, DROP it before taking the accounts lock):
    ///  - **(A) PIN-HONOR** (cache-warm, the common case): when `affinity` is
    ///    `Some(key)` and the session's pinned account is not in `tried` and passes
    ///    the HARD gates, serve THAT pin — even over the soft threshold — to keep the
    ///    operator's prompt cache warm. NO revalidation throttle here (the global
    ///    egress throttle already paces); the pin is left as-is.
    ///  - **(B) FALLBACK** (no pin, or the pin is rejected/held/tried): the
    ///    least-utilized surviving account wins (lowest
    ///    [`crate::quota::Quota::max_utilization`]; ties: LRU `last_selected_seq`).
    ///    The anti-storm throttle applies HERE ONLY — at most one NEW-account
    ///    revalidation per [`REVALIDATION_MIN_SPACING_MS`]; inside that window return
    ///    `None` (the caller emits the honest 429). The window is spent only once a
    ///    servable candidate exists. On success the session is RE-PINNED to the
    ///    chosen account, mirroring `select()`'s re-pin.
    ///
    /// The winner's `last_selected_seq` is stamped and ONE `tracing::info!` names the
    /// path (pin-honor vs fallback), the account, and its utilization.
    pub fn select_revalidation(
        &self,
        tried: &HashSet<usize>,
        now: OffsetDateTime,
        model: Option<&str>,
        affinity: Option<u64>,
    ) -> Option<usize> {
        // Anti-storm spacing on the FALLBACK path only: the upstream 429→hold and the
        // global egress throttle are the real backstops; this just stops a
        // synchronized burst from slamming one over-threshold account when the fleet
        // saturates. The pin-honor path is never throttled here.
        const REVALIDATION_MIN_SPACING_MS: i64 = 2000;

        let now_ms = odt_to_ms(now);
        let is_fable = model.is_some_and(crate::model::is_fable_model);

        // HARD-gate predicate shared by both paths: the blocks that even a
        // revalidation serve must honor (soft utilization/pacing deliberately absent).
        let hard_ok = |account: &AccountRuntime| -> bool {
            Self::hard_ok(account, self.global_threshold, now, now_ms, is_fable)
        };

        // (A) PIN-HONOR — read the pin under the affinity lock, DROP it, then check
        // the pin's HARD gates under the accounts lock (never nested). No throttle.
        if let Some(key) = affinity {
            let pinned = {
                let pins = self.affinity.lock().expect("affinity lock poisoned");
                pins.get(&key).map(|&(idx, _)| idx)
            };
            if let Some(idx) = pinned {
                if !tried.contains(&idx) {
                    let mut accounts = self.accounts.write().expect("accounts lock poisoned");
                    if accounts.get(idx).is_some_and(&hard_ok) {
                        let tick = self.select_seq.fetch_add(1, Ordering::Relaxed);
                        let util = accounts
                            .get(idx)
                            .map(|a| a.quota.max_utilization(now, is_fable))
                            .unwrap_or_default();
                        if let Some(account) = accounts.get_mut(idx) {
                            account.last_selected_seq = tick;
                            tracing::info!(
                                account = %account.name,
                                utilization = util,
                                is_fable,
                                "revalidation-serve (pin-honor): serving session's pinned account over soft threshold to keep its cache warm"
                            );
                        }
                        return Some(idx);
                    }
                    // Pin is HARD-blocked → fall through to the fallback path (lock
                    // drops at the end of this scope, before the affinity lock below).
                }
            }
        }

        // (B) FALLBACK — least-utilized surviving account, throttled, then re-pinned.
        let idx = {
            let mut accounts = self.accounts.write().expect("accounts lock poisoned");

            let mut best: Option<usize> = None;
            // (util as ordered bits, last_selected_seq) — utilizations are finite and
            // non-negative here (NaN/inf headers filtered at parse), so the raw bit
            // pattern orders correctly for ascending "least utilized first".
            let mut best_key: Option<(u64, u64)> = None;
            for (i, account) in accounts.iter().enumerate() {
                if tried.contains(&i) || !hard_ok(account) {
                    continue;
                }
                let util = account.quota.max_utilization(now, is_fable);
                let key = (util.to_bits(), account.last_selected_seq);
                if best_key.is_none_or(|b| key < b) {
                    best = Some(i);
                    best_key = Some(key);
                }
            }

            let idx = best?;

            // A servable candidate exists — spend the anti-storm window now (after
            // selection, so a fleet with nothing to serve never burns the valve).
            let next_at = self.next_revalidation_at_ms.load(Ordering::Relaxed);
            if now_ms < next_at {
                return None;
            }
            self.next_revalidation_at_ms
                .store(now_ms + REVALIDATION_MIN_SPACING_MS, Ordering::Relaxed);

            let tick = self.select_seq.fetch_add(1, Ordering::Relaxed);
            let util = accounts
                .get(idx)
                .map(|a| a.quota.max_utilization(now, is_fable))
                .unwrap_or_default();
            if let Some(account) = accounts.get_mut(idx) {
                account.last_selected_seq = tick;
                tracing::info!(
                    account = %account.name,
                    utilization = util,
                    is_fable,
                    "revalidation-serve (fallback): whole fleet over soft threshold — serving least-utilized allowed account"
                );
            }
            idx
        };

        // Re-pin the session to the served account (accounts lock already dropped —
        // never nest the two). Mirrors `select()`'s re-pin; no size-cap eviction here
        // because a revalidation serve only ever re-keys an EXISTING session.
        if let Some(key) = affinity {
            let mut pins = self.affinity.lock().expect("affinity lock poisoned");
            pins.insert(key, (idx, now_ms));
        }
        Some(idx)
    }

    /// The HARD gates alone — the blocks that mean *this account cannot serve at
    /// all*, with every SOFT gate deliberately absent:
    ///   - `disabled` / [`AccountStatus::Error`] → hard fail;
    ///   - a live rate-limit hold (`rate_limited_until_ms` still in the future);
    ///   - `quota.status == Some("rejected")` — Anthropic's own verdict;
    ///   - for a Fable request, an exhausted model-scoped weekly (`7d_oi`) bucket.
    ///
    /// Deliberately NOT here: the soft utilization threshold ([`crate::quota::Quota::is_near`])
    /// and pacing. Both are rotation HINTS — reasons to prefer another account for
    /// one request — never proof this one is gone. That distinction is the whole
    /// ballgame for the per-account prompt cache: [`Self::select`] uses this
    /// predicate as the SOLE authority on whether a session's pin may be re-keyed,
    /// because a re-key re-creates the conversation prefix on a cold account.
    ///
    /// Known staleness caveat: `quota.status` is written ONLY from live response
    /// headers ([`crate::quota`]), never by the background probe, so for a benched
    /// account it is stale-or-`None` and `None` passes here. That is acceptable
    /// precisely because a genuine rejection answers the next attempt with a 429,
    /// which arms a real hold — upstream stays the oracle instead of our own
    /// possibly-stale arithmetic.
    ///
    /// Pure and lock-free; the caller holds whichever accounts lock it needs.
    pub(super) fn hard_ok(
        account: &AccountRuntime,
        global_threshold: f64,
        now: OffsetDateTime,
        now_ms: i64,
        is_fable: bool,
    ) -> bool {
        if account.disabled || account.status == AccountStatus::Error {
            return false;
        }
        if let Some(until) = account.rate_limited_until_ms {
            if now_ms < until {
                return false;
            }
        }
        if account.quota.status.as_deref() == Some("rejected") {
            return false;
        }
        let threshold = account.switch_threshold.unwrap_or(global_threshold);
        if is_fable && account.quota.model_weekly_exhausted(threshold, now) {
            return false;
        }
        true
    }

    pub(super) fn eligible(
        account: &AccountRuntime,
        global_threshold: f64,
        pacing: &PacingConfig,
        respect_pacing: bool,
        now: OffsetDateTime,
        now_ms: i64,
        is_fable: bool,
    ) -> bool {
        if account.disabled || account.status == AccountStatus::Error {
            return false;
        }
        // A rate-limit hold blocks the account only while it is still in the
        // future — a past hold is treated as expired (live), no mutation needed.
        if let Some(until) = account.rate_limited_until_ms {
            if now_ms < until {
                return false;
            }
        }
        let threshold = account.switch_threshold.unwrap_or(global_threshold);
        if account.quota.is_near(threshold, now) {
            return false;
        }
        // Per-model routing: only a Fable request gates on the model-scoped weekly
        // (`7d_oi`) bucket — every non-Fable model still serves from this account.
        if is_fable && account.quota.model_weekly_exhausted(threshold, now) {
            return false;
        }
        // SOFT pacing gate, evaluated LAST so it only ever narrows an already-healthy
        // account. When `respect_pacing` is false (the fallback pass) or pacing is
        // unconfigured, this is inert — so a default-OFF build is byte-identical here
        // and the fallback pass can always still find a servable account.
        if respect_pacing && pacing.is_active() {
            if let Some(cap) = pacing.effective_max_in_flight() {
                if account.in_flight >= cap {
                    return false;
                }
            }
            if let Some(gap) = pacing.min_spacing_ms {
                if now_ms.saturating_sub(account.last_served_ms) < gap as i64 {
                    return false;
                }
            }
        }
        true
    }

    /// Why this account is out of rotation and when it clears — the display and
    /// hint-side companion to [`Self::eligible`], mirroring its HARD gates exactly
    /// (disabled/error → hold → 5-hour → weekly → Fable weekly) while deliberately
    /// omitting soft pacing, which only ever narrows an already-healthy account and
    /// never holds one out. Pure and lock-free so both [`Manager::snapshot`] and
    /// [`Manager::retry_after_hint`] can call it as the single source of truth.
    ///
    /// The terminal gates (`Disabled`, `Login`) never self-free, so their instant
    /// is `None`. Otherwise every ACTIVE gate contributes the instant it clears:
    /// a future hold its deadline, and a window at/over `threshold` its
    /// [`crate::quota::QuotaWindow::live_reset`]. An account frees only once ALL of
    /// its gates clear, so the binding gate is the LATEST-clearing one and
    /// `free_at` is the MAX of the clear-instants. An active gate with no known
    /// reset (a window over threshold whose reset is unknown) is the
    /// longest-possible constraint, so it sorts as "latest": it becomes the reason
    /// and `free_at` is `None` (we cannot promise a time). No active gate → `Ok`.
    pub(super) fn account_gate(
        account: &AccountRuntime,
        threshold: f64,
        now: OffsetDateTime,
        now_ms: i64,
        is_fable: bool,
    ) -> (GateReason, Option<OffsetDateTime>) {
        // Terminal states that never self-free — reported with no clear-instant.
        if account.disabled {
            return (GateReason::Disabled, None);
        }
        if account.status == AccountStatus::Error {
            return (GateReason::Login, None);
        }

        // Every ACTIVE hard gate paired with the instant it clears (`None` = active
        // but no known reset). Collected in eligibility order; soft pacing is
        // intentionally excluded (see the doc comment).
        let mut gates: Vec<(GateReason, Option<OffsetDateTime>)> = Vec::new();
        if let Some(until) = account.rate_limited_until_ms {
            if now_ms < until {
                gates.push((GateReason::Hold, ms_to_odt(until)));
            }
        }
        if let Some(window) = account.quota.five_hour {
            if window.effective(now) >= threshold {
                gates.push((GateReason::FiveHour, window.live_reset(now)));
            }
        }
        if let Some(window) = account.quota.seven_day {
            if window.effective(now) >= threshold {
                gates.push((GateReason::SevenDay, window.live_reset(now)));
            }
        }
        // The model-scoped weekly gates Fable requests ONLY — mirror `eligible`.
        if is_fable {
            if let Some(window) = account.quota.seven_day_oi {
                if window.effective(now) >= threshold {
                    gates.push((GateReason::FableWeekly, window.live_reset(now)));
                }
            }
        }
        // Standard (API-key) token/request limits — mirror `Quota::is_near`
        // (quota.rs) so account_gate agrees with eligible(): a standard-limited
        // account is gated, not "soon-servable". free_at = standard_reset (the
        // upstream refresh instant); None when unknown ("cannot promise a time").
        // OAuth accounts have all standard fields None → this never fires →
        // account_gate byte-identical for them. Once standard_reset has passed the
        // upstream limit has refreshed, so a spent count must NOT keep gating.
        let standard_expired = account.quota.standard_reset.is_some_and(|r| now >= r);
        if !standard_expired {
            let near = |limit: Option<i64>, remaining: Option<i64>| {
                matches!(
                    (limit, remaining),
                    (Some(l), Some(r)) if l > 0 && 1.0 - (r as f64 / l as f64) >= threshold
                )
            };
            if near(account.quota.tokens_limit, account.quota.tokens_remaining)
                || near(
                    account.quota.requests_limit,
                    account.quota.requests_remaining,
                )
            {
                gates.push((GateReason::Standard, account.quota.standard_reset));
            }
        }

        // The latest-clearing gate binds. `gate_clear_key` sorts an unknown reset
        // (`None`) after every known instant, so if any active gate has no reset it
        // wins here and carries its `None` out as `free_at` — exactly the
        // "cannot promise a time" case. `max_by_key` breaks ties toward the
        // last-collected gate, a deterministic order.
        gates
            .into_iter()
            .max_by_key(|&(_, at)| gate_clear_key(at))
            .unwrap_or((GateReason::Ok, None))
    }

    /// The best pacing-respecting eligible account not in `tried`, by ascending
    /// `(priority, last_selected_seq, soonest weekly reset)` — the pre-pacing LRU
    /// order, now additionally skipping any account the soft pacing gate holds out.
    /// Read-only (no stamp); the caller stamps the winner. Emits one INFO line per
    /// account skipped *specifically because of pacing* (healthy but capped/spaced)
    /// so the knobs are tunable live.
    fn pick_eligible(
        &self,
        accounts: &[AccountRuntime],
        tried: &HashSet<usize>,
        now: OffsetDateTime,
        now_ms: i64,
        is_fable: bool,
        respect_pacing: bool,
    ) -> Option<usize> {
        let mut best: Option<usize> = None;
        let mut best_key: Option<(i64, u64, i128)> = None;
        for (idx, account) in accounts.iter().enumerate() {
            if tried.contains(&idx) {
                continue;
            }
            if !Self::eligible(
                account,
                self.global_threshold,
                &self.pacing,
                respect_pacing,
                now,
                now_ms,
                is_fable,
            ) {
                // Distinguish a pacing skip (healthy but capped/spaced) from a real
                // ineligibility (disabled/error/quota) so the log names only the former.
                if respect_pacing
                    && self.pacing.is_active()
                    && Self::eligible(
                        account,
                        self.global_threshold,
                        &self.pacing,
                        false,
                        now,
                        now_ms,
                        is_fable,
                    )
                {
                    tracing::info!(
                        account = %account.name,
                        in_flight = account.in_flight,
                        "pacing: skip in selection"
                    );
                }
                continue;
            }
            // Unknown weekly reset sorts FIRST (probe it) — treat as the minimum.
            let reset = account
                .quota
                .governing_weekly_reset(now)
                .map_or(i128::MIN, |r| r.unix_timestamp() as i128);
            let key = (account.priority, account.last_selected_seq, reset);
            if best_key.is_none_or(|b| key < b) {
                best = Some(idx);
                best_key = Some(key);
            }
        }
        best
    }

    /// The least-loaded servable account not in `tried`, IGNORING pacing (the soft
    /// fallback pass). Sort key ascending: `(in_flight, priority, last_selected_seq,
    /// weekly reset)` — least concurrent load first, then the normal LRU order. All
    /// non-pacing eligibility (disabled/error/rate-limit/quota) still applies, so a
    /// genuinely exhausted fleet still yields `None` (a real 429), while a merely
    /// all-paced fleet always yields the coolest account. Read-only.
    fn pick_least_loaded(
        &self,
        accounts: &[AccountRuntime],
        tried: &HashSet<usize>,
        now: OffsetDateTime,
        now_ms: i64,
        is_fable: bool,
    ) -> Option<usize> {
        let mut best: Option<usize> = None;
        let mut best_key: Option<(u32, i64, u64, i128)> = None;
        for (idx, account) in accounts.iter().enumerate() {
            if tried.contains(&idx) {
                continue;
            }
            if !Self::eligible(
                account,
                self.global_threshold,
                &self.pacing,
                false,
                now,
                now_ms,
                is_fable,
            ) {
                continue;
            }
            let reset = account
                .quota
                .governing_weekly_reset(now)
                .map_or(i128::MIN, |r| r.unix_timestamp() as i128);
            let key = (
                account.in_flight,
                account.priority,
                account.last_selected_seq,
                reset,
            );
            if best_key.is_none_or(|b| key < b) {
                best = Some(idx);
                best_key = Some(key);
            }
        }
        best
    }
}

/// Sort key for a gate's clear-instant that ranks an unknown reset (`None`) after
/// every known instant: an active gate with no known reset is the
/// longest-possible constraint, so it must sort as the LATEST-clearing gate.
/// Among known instants the natural chronological order applies. Used by
/// [`Manager::account_gate`] to pick the binding (latest-clearing) gate.
fn gate_clear_key(at: Option<OffsetDateTime>) -> (u8, OffsetDateTime) {
    match at {
        Some(t) => (0, t),
        None => (1, OffsetDateTime::UNIX_EPOCH),
    }
}
