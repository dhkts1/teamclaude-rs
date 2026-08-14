//! `Manager` account-selection and eligibility methods, split verbatim from `mod.rs`.

use super::*;

/// Bound on [`Manager::conn_affinity`] — see its doc-comment for why the map is
/// memory-only and never persisted.
const CONN_AFFINITY_CAP: usize = 256;
/// Idle TTL for a [`Manager::conn_affinity`] entry: an entry older than this is
/// treated as absent (the connection is presumed gone). Short on purpose — this
/// map exists only to keep NOISE traffic (`/api/event_logging`, `/mcp-registry`)
/// on one account for the lifetime of one connection, not to survive a
/// reconnect the way [`Manager::affinity`] does.
const CONN_AFFINITY_TTL_MS: i64 = 5 * 60 * 1000;

/// The three-way split of an UNPINNED request (control account, part 2 — see
/// the module doc and `docs/plans/control-routing-bridge-coder.md`). Classified
/// from `path` alone (the caller's already query-stripped request path), never
/// from a session/affinity key — see [`classify_request`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RequestClass {
    /// `/v1/messages`, `/v1/messages/count_tokens` — pool only. **Never** the
    /// control account, even when `controlAccount` names an ENABLED (pooled)
    /// account — see [`Manager::select`]'s pool-pick exclusion.
    Inference,
    /// `/api/event_logging*`, `/mcp-registry*` — high-volume noise. Follows the
    /// requesting connection ([`Manager::conn_affinity`]) rather than the
    /// control account, so it never burns the one account being kept clean.
    Noise,
    /// Everything else — the identity/control plane. Prefers the control
    /// account (bypassing its `disabled` gate — see
    /// [`Manager::control_eligible`]) when it is genuinely usable; degrades to
    /// normal rotation otherwise. The classification **fails safe**: an unknown
    /// path defaults here rather than to the pool, the opposite of
    /// `CLIENT_CREDENTIAL_PREFIXES`'s fail-unsafe growth.
    ControlPreferred,
}

/// Classify an unpinned request's path into the three-way split. Exact match
/// for the two inference paths (mirrors `stable_session_key`'s
/// `/v1/messages/count_tokens must NOT prefix-match /v1/messages` guard);
/// prefix match for the two noise paths, matching the bridge's own examples
/// (`/api/event_logging/v2/batch`, and any `/mcp-registry...` route).
pub(super) fn classify_request(path: &str) -> RequestClass {
    if path == "/v1/messages" || path == "/v1/messages/count_tokens" {
        RequestClass::Inference
    } else if path.starts_with("/api/event_logging") || path.starts_with("/mcp-registry") {
        RequestClass::Noise
    } else {
        RequestClass::ControlPreferred
    }
}

/// `threshold − reserve` (floored at 0.0) when `allow_reserve` is true — the
/// effective switch threshold a GENERAL (non-control-preferred) pick applies
/// to the control account specifically, so ordinary pool traffic leaves it
/// some headroom instead of racing it to the same edge as every other
/// account. A control-preferred pick, or `allow_reserve = false`, uses the
/// full `threshold` unchanged. Pure; see [`Manager::control_reserve`]'s
/// doc-comment for why this is inert in the current (control-disabled)
/// configuration.
pub(super) fn effective_threshold(threshold: f64, reserve: f64, allow_reserve: bool) -> f64 {
    if allow_reserve {
        (threshold - reserve).max(0.0)
    } else {
        threshold
    }
}

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
    /// ONLY when the pinned ACCOUNT fails a HARD gate** ([`Self::account_hard_ok`] is
    /// the sole authority). Anthropic's prompt cache is per-account, so a re-key
    /// re-creates the entire conversation prefix on a cold account. Only disabled /
    /// [`AccountStatus::Error`] / `rejected` / a hold that OUTLIVES the prompt cache
    /// ([`CACHE_WARM_HOLD_SECS`]) re-key durably. This is self-healing: a durable
    /// failure arms a long hold or sets `rejected` on the very next attempt, and
    /// those fail `account_hard_ok`.
    ///
    /// Everything else is a fact about ONE REQUEST, not about the account. Such a
    /// fact never re-keys, and — the second half of the same argument — mostly does
    /// not even DIVERT, because a divert costs that request the same cold prefix a
    /// re-key would. The five split by how much they actually know:
    ///  - **over the utilization threshold** → **SERVE the pin anyway.** That
    ///    threshold is our own arithmetic over headers that go stale by minutes, and
    ///    Anthropic keeps answering 200s for accounts it benches. Upstream is the
    ///    oracle: serve, and let a real 429 arm a real (HARD) hold.
    ///  - **paced out** (at the in-flight cap / inside min-spacing, see
    ///    [`Self::paced_out`]) → divert this ONE request, keep the pin. Our own
    ///    concurrency is measured exactly and never stale, and spreading a session's
    ///    burst is precisely what the cap exists to do.
    ///  - **model-class blocked** (a Fable request whose pin has an exhausted Fable
    ///    weekly, see [`Self::model_blocked`]) → divert this ONE request, keep the
    ///    pin. The account cannot answer THIS request and answers every other model
    ///    class perfectly, so treating it as death let a one-line title call drag a
    ///    200k-token Opus conversation onto a cold account.
    ///  - **already in `tried`** → divert this ONE request, keep the pin. It has
    ///    failed THIS request upstream, so re-serving it would spin — but one blip is
    ///    not proof the account is gone.
    ///  - **held by a rate-limit timer that clears while the cache is still warm**
    ///    (see [`Self::hold_clears_while_warm`]) → divert this ONE request, keep the
    ///    pin. A hold is a timer, not a death, and the ones we arm are mostly short:
    ///    a no-guidance park is 15s + jitter, and a `retry-after` park is clamped to
    ///    300s. Re-keying on one would discard a prefix that is still warm when the
    ///    account frees. Past [`CACHE_WARM_HOLD_SECS`] the trade inverts and the hold
    ///    goes back to being an ACCOUNT-level hard gate.
    ///
    /// Every divert routes to the fall-through pick while the pin stays put (see
    /// `keep_pin`); the utilization case returns from the fast-path.
    ///
    /// The one remaining voluntary mover — the load-balancing migration that
    /// re-pins a stacked session onto a less-loaded account — is therefore
    /// OPT-IN and ships OFF (top-level `loadBalanceMigration`, see
    /// [`Self::load_balance_migration_enabled`]). Disabled, the scan is skipped
    /// entirely and the pin is honoured; set to `true`, the balancing behaviour
    /// below is unchanged.
    ///
    /// **Control-account routing (part 2), applied ONLY when this call reaches
    /// this point with no existing pin honoured above** (`keep_pin.is_none()` —
    /// invariant: a pin, even a per-request-diverted one, is never touched by
    /// this overlay): `path` is classified by [`classify_request`] into the
    /// three-way split — `Inference` is pool-only and excluded from the control
    /// account even in the normal pick below; `Noise` prefers whichever account
    /// `conn_key` ([`crate::proxy::SessionKey`]'s per-connection value, distinct
    /// from `affinity`) already served on this connection
    /// ([`Self::conn_affinity`]); everything else prefers the control account
    /// via [`Self::control_eligible`] (which deliberately bypasses `disabled`).
    /// Inert whenever [`Self::control`] is `None`.
    pub fn select(
        &self,
        tried: &HashSet<usize>,
        now: OffsetDateTime,
        model: Option<&str>,
        affinity: Option<u64>,
        path: &str,
        conn_key: Option<u64>,
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
        // Compute the control-account three-way split ONCE (part 2). Consulted
        // twice below: the routing overlay (control-preferred / noise) after the
        // affinity fast-path, and the pool-pick exclusion (inference must never
        // land on the control account) inside the normal pick.
        let request_class = classify_request(path);

        // Set (to the OLD pin index) for the four per-REQUEST failures that DIVERT —
        // the pin is paced out, it cannot serve this request's model class, it is
        // already in `tried`, or it is parked on a hold that clears while its prompt
        // cache is still warm — while still clearing every ACCOUNT-level HARD gate
        // (`Self::account_hard_ok`). A per-request fact may divert a request; it may
        // never RE-KEY a session: Anthropic's prompt cache is per-account, so a re-pin
        // re-creates the whole conversation prefix on the next request. The
        // fall-through re-pin at the bottom honours this and keeps the old index.
        // The fourth — over the utilization threshold — does not divert at all; the
        // fast-path below serves the pin and returns.
        let mut keep_pin: Option<usize> = None;
        // WHY the pin was kept while another account serves, carried down to the
        // re-pin at the bottom because only there are BOTH the kept pin and the
        // account actually serving known. `None` means no divert happened. Purely
        // for the log line — nothing branches on it.
        let mut divert_reason: Option<&'static str> = None;

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
                    // 5xx) leaves every ACCOUNT-level gate clear. So divert this
                    // request and keep the pin. Self-healing: a DURABLE failure arms a
                    // hold or sets `rejected`, both of which fail `account_hard_ok`, so
                    // the re-key still happens — one request later, on evidence.
                    //
                    // `account_hard_ok`, not `hard_ok`: the model-class gate is about
                    // THIS request, so a Fable-exhausted pin that a Fable request
                    // already tried still keeps its pin for the session's Opus turns.
                    let accounts = self.accounts.read().expect("accounts lock poisoned");
                    if accounts
                        .get(idx)
                        .is_some_and(|a| Self::account_hard_ok(a, now_ms))
                    {
                        keep_pin = Some(idx);
                        divert_reason = Some("pin-tried");
                    }
                } else {
                    let count_x = counts.get(&idx).copied().unwrap_or(0);
                    // Load-balancing migration ships OFF (see
                    // [`Self::load_balance_migration_enabled`]): a session that HAS a
                    // pin is by definition already warm, so moving it to even out
                    // counts is a guaranteed prompt-cache loss. Read the flag HERE,
                    // while NO other lock is held — the affinity lock dropped above
                    // and the accounts lock is taken below, so the config lock is
                    // never nested under either. Short-circuited on `count_x`, so the
                    // common lone-session path does not touch the config lock at all.
                    let migration_ok = count_x >= 2 && self.load_balance_migration_enabled();
                    // (2) UNDER THE ACCOUNTS LOCK ONLY: confirm the pin `X` is still
                    // usable, then — and only when migration is ENABLED and >=2
                    // sessions stack on `X` — look for the least-loaded ELIGIBLE
                    // account `Y` that strictly improves balance
                    // (`count(Y)+1 < count(X)`). Stamp whichever we settle on.
                    // `None` means `X` is ineligible → fall through to the normal
                    // pick/re-pin path (which already handles a dead pin).
                    let decision: Option<(usize, Option<(String, String)>)> = {
                        let mut accounts = self.accounts.write().expect("accounts lock poisoned");
                        // The full (soft-inclusive) gate. Failing it does NOT bench the
                        // pin — it only means we drop into the hard-gate re-test below,
                        // which decides serve-anyway vs durable re-key.
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
                            // Re-test the SAME account against the ACCOUNT-level HARD
                            // gates ALONE to separate a per-REQUEST yield from a real
                            // block. Passing there means the pin cleared
                            // disabled/error/hold/rejected and only tripped a gate that
                            // describes this one request — the utilization threshold
                            // (`quota.is_near`), the concurrency-cap / min-spacing
                            // pacing gate, or the model-class gate. None of those is
                            // proof this account is gone. Failing it IS a genuine HARD
                            // block → fall through to the normal pick, which durably
                            // re-keys (the session's account really is gone).
                            //
                            // The four then part ways, because they are not the same
                            // kind of claim (see `Self::paced_out`, `Self::model_blocked`,
                            // `Self::hold_clears_while_warm`):
                            //
                            //  - PACED OUT → yield THIS request and keep the pin, as
                            //    before. Our own concurrency is a fact we measure
                            //    exactly, and spreading a burst is what the cap is for.
                            //
                            //  - HELD, BUT THE HOLD CLEARS WHILE THE CACHE IS WARM →
                            //    same shape. A hold is a TIMER, not a death, and the
                            //    timers we arm are mostly short (a no-guidance park is
                            //    15s + jitter). Diverting the one request costs one cold
                            //    prefix; re-keying throws away a cache that would still
                            //    have been warm when the account came back, and the
                            //    session never returns to it. A hold that OUTLIVES the
                            //    cache is a different claim and fails `account_alive`
                            //    above — see `CACHE_WARM_HOLD_SECS`.
                            //
                            //  - MODEL-CLASS BLOCKED (a Fable request, Fable weekly
                            //    exhausted) → same shape: this account cannot answer
                            //    THIS request, and answers every other model class
                            //    fine. Divert the one request, keep the pin — otherwise
                            //    a one-line title call re-keys a 200k-token Opus
                            //    conversation onto a cold account.
                            //
                            //  - OVER THE UTILIZATION THRESHOLD → SERVE THE PIN. That
                            //    threshold is our own arithmetic over headers stale by
                            //    minutes, and Anthropic keeps answering 200s for
                            //    accounts it benches. Keeping the pin while still
                            //    diverting the request — the earlier half-fix — bought
                            //    nothing: the pin survived and the request paid the
                            //    cold prefix anyway (measured live: a 44.4%
                            //    account-switch rate on SUCCESSFUL serves, with zero
                            //    pacing events and zero hard failures). This is the
                            //    behaviour `select_revalidation`'s PIN-HONOR path
                            //    already had, hoisted to where it is reachable; both
                            //    log the identical line so they grep together.
                            let account_alive = accounts
                                .get(idx)
                                .is_some_and(|a| Self::account_hard_ok(a, now_ms));
                            let model_blocked = accounts.get(idx).is_some_and(|a| {
                                Self::model_blocked(a, self.global_threshold, now, is_fable)
                            });
                            let paced_out = accounts
                                .get(idx)
                                .is_some_and(|a| Self::paced_out(a, &self.pacing, now_ms));
                            // Reached only when `account_alive` holds, so any live hold
                            // left here is by construction a short one — named anyway so
                            // the branch does not depend on the ordering above.
                            let held_briefly = accounts
                                .get(idx)
                                .is_some_and(|a| Self::hold_clears_while_warm(a, now_ms));
                            if !account_alive {
                                // Genuinely gone: fall through and durably re-key.
                                None
                            } else if held_briefly || model_blocked || paced_out {
                                // Yield this ONE request to the fall-through pick; the
                                // re-pin at the bottom re-inserts the OLD index and now
                                // logs WHICH of the three per-request gates diverted it.
                                keep_pin = Some(idx);
                                divert_reason = Some(if held_briefly {
                                    "short-hold"
                                } else if model_blocked {
                                    "model-class"
                                } else {
                                    "paced"
                                });
                                None
                            } else {
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
                                // `target == idx`, so the commit section below is a
                                // plain pin refresh: OLD index, fresh `now_ms`.
                                Some((idx, None))
                            }
                        } else {
                            // Default: honour the pin. With migration disabled (the
                            // default), or when `count_x < 2` (a LONE session),
                            // `target` stays `idx` and nothing below runs, so this
                            // path is byte-identical to the pre-migration behaviour —
                            // a warm session's cache is never moved.
                            let mut target = idx;
                            let mut migrate_names: Option<(String, String)> = None;
                            if migration_ok {
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
                        // The map changed, so the on-disk copy is now behind. A
                        // relaxed store under the lock: no allocation, no second
                        // lock, nothing that can block the request path.
                        self.mark_affinity_dirty();
                        return Some(committed);
                    }
                }
            }
        }

        // Control-account routing overlay (part 2 — three-way split of an
        // UNPINNED request, see `classify_request`). Gated on `keep_pin.is_none()`
        // so it NEVER touches a session that already has a pin — even one only
        // diverted for this one request (invariant 1: a pin is re-keyed ONLY by
        // an ACCOUNT-level hard gate, never by this preference).
        if keep_pin.is_none() {
            match request_class {
                RequestClass::Inference => {
                    // Pool-only — no preference here; the exclusion lives in the
                    // normal pick below so it also covers the case where control
                    // points at an ENABLED (pooled) account.
                }
                RequestClass::Noise => {
                    // Follow the connection: reuse whichever account this
                    // connection already served, while it is still eligible
                    // (the ordinary gate — no disabled-bypass here, unlike
                    // control preference). `tried` still excludes an account
                    // that already failed THIS request.
                    if let Some(k) = conn_key {
                        if let Some(idx) = self.conn_affinity_get(k, now_ms) {
                            if !tried.contains(&idx) {
                                let usable = {
                                    let accounts =
                                        self.accounts.read().expect("accounts lock poisoned");
                                    accounts.get(idx).is_some_and(|a| {
                                        Self::eligible(
                                            a,
                                            self.global_threshold,
                                            &self.pacing,
                                            true,
                                            now,
                                            now_ms,
                                            is_fable,
                                        )
                                    })
                                };
                                if usable {
                                    let mut accounts =
                                        self.accounts.write().expect("accounts lock poisoned");
                                    let tick = self.select_seq.fetch_add(1, Ordering::Relaxed);
                                    if let Some(account) = accounts.get_mut(idx) {
                                        account.last_selected_seq = tick;
                                        tracing::info!(
                                            account = %account.name,
                                            conn = k,
                                            "control: noise request follows its connection's account"
                                        );
                                    }
                                    drop(accounts);
                                    self.conn_affinity_record(k, idx, now_ms);
                                    return Some(idx);
                                }
                            }
                        }
                    }
                    // No usable connection-pinned account — fall through to the
                    // normal pick below, which re-records `conn_affinity` once
                    // it settles on a winner.
                }
                RequestClass::ControlPreferred => {
                    if let Some(control_idx) = self.control() {
                        if !tried.contains(&control_idx) {
                            let usable = {
                                let accounts =
                                    self.accounts.read().expect("accounts lock poisoned");
                                accounts
                                    .get(control_idx)
                                    .is_some_and(|a| Self::control_eligible(a, now_ms))
                            };
                            if usable {
                                let mut accounts =
                                    self.accounts.write().expect("accounts lock poisoned");
                                let tick = self.select_seq.fetch_add(1, Ordering::Relaxed);
                                if let Some(account) = accounts.get_mut(control_idx) {
                                    account.last_selected_seq = tick;
                                    tracing::info!(
                                        account = %account.name,
                                        "control: routing unpinned identity-plane request to the control account"
                                    );
                                }
                                return Some(control_idx);
                            }
                        }
                    }
                    // Control unset, held, errored, rejected, or already tried —
                    // degrade to normal rotation (no second designated identity).
                }
            }
        }

        // Normal LRU/priority pick (identical to the pre-affinity path when no
        // control account is set). The accounts lock is scoped so it is released
        // before we touch the affinity lock again for the re-pin below.
        let best = {
            let mut accounts = self.accounts.write().expect("accounts lock poisoned");

            // Inference must NEVER select the control account, even one that is
            // ENABLED (pooled) — the default-disabled control account is already
            // excluded by `eligible`'s own disabled check, so this only starts
            // doing real work the day `controlAccount` stops being disabled.
            // Modeled as an extra `tried` member rather than a new parameter
            // threaded through `pick_eligible`/`pick_least_loaded`, so both
            // passes (and the pacing/least-loaded fallback) honour it for free.
            let pool_tried: std::borrow::Cow<'_, HashSet<usize>> =
                if request_class == RequestClass::Inference {
                    match self.control() {
                        Some(control_idx) if !tried.contains(&control_idx) => {
                            let mut t = tried.clone();
                            t.insert(control_idx);
                            std::borrow::Cow::Owned(t)
                        }
                        _ => std::borrow::Cow::Borrowed(tried),
                    }
                } else {
                    std::borrow::Cow::Borrowed(tried)
                };
            let pool_tried: &HashSet<usize> = &pool_tried;

            // First pass: honour pacing (skip accounts at the concurrency cap or
            // inside the min-spacing window). With pacing OFF this is byte-identical
            // to the pre-pacing pick.
            let mut best = self.pick_eligible(&accounts, pool_tried, now, now_ms, is_fable, true);

            // Soft fallback (CRITICAL — pacing must never DROP a servable request):
            // if pacing gated EVERY account out but at least one is servable ignoring
            // pacing, serve the least-loaded (lowest in_flight, then the normal LRU
            // key). With pacing OFF the first pass and this pass use identical
            // eligibility, so a None first pass ⟹ None here too — default-OFF stays
            // byte-identical (no spurious fallback, no log).
            if best.is_none() {
                if let Some(idx) =
                    self.pick_least_loaded(&accounts, pool_tried, now, now_ms, is_fable)
                {
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
            // A per-REQUEST-gated pin (paced, model-class blocked, or transiently in
            // `tried`) diverted THIS request only: re-insert the OLD index, not the
            // account we are about to serve. The insert still has to happen — this
            // `now_ms` is one of only two writers of the last-touch stamp that the
            // AFFINITY_CAP eviction below sorts on, so skipping it would make a
            // heavily-diverted session the eviction victim.
            let pin_idx = keep_pin.unwrap_or(idx);
            let moved_off = {
                let mut pins = self.affinity.lock().expect("affinity lock poisoned");
                let previous = pins.get(&key).map(|&(i, _)| i);
                pins.insert(key, (pin_idx, now_ms));
                self.mark_affinity_dirty();
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
            // Reaching this line now means the pin failed an ACCOUNT-level HARD gate,
            // so the line doubles as the audit trail for the invariant.
            // The accounts lock is taken ONLY after the affinity lock has dropped
            // above; the two are never held simultaneously.
            if let Some(previous) = moved_off {
                let accounts = self.accounts.read().expect("accounts lock poisoned");
                let old_name = accounts.get(previous).map_or("?", |a| a.name.as_str());
                let new_name = accounts.get(pin_idx).map_or("?", |a| a.name.as_str());
                tracing::info!(
                    "affinity: re-pin session {} off {} -> {} (pin failed an ACCOUNT HARD gate)",
                    short_session_id(key),
                    old_name,
                    new_name
                );
            }
            // The SOFT counterpart, and until now it emitted nothing at all: the pin
            // survived, a DIFFERENT account served, and no line said so — which is why
            // off-pin serves were unattributable in the live logs. Normally exclusive
            // with `moved_off` (a divert re-inserts the OLD index, so `previous ==
            // pin_idx` and `moved_off` is `None`); a concurrent select that re-pinned
            // this session between the two reads can make both fire, which is two
            // honest lines about one request, not a defect. The accounts lock is taken
            // only after the affinity lock dropped above — never nested.
            if let Some(reason) = divert_reason {
                let accounts = self.accounts.read().expect("accounts lock poisoned");
                let pin_name = accounts.get(pin_idx).map_or("?", |a| a.name.as_str());
                let serve_name = accounts.get(idx).map_or("?", |a| a.name.as_str());
                tracing::info!(
                    "affinity: divert session {} pin {} -> serving {} (reason={}, pin kept)",
                    short_session_id(key),
                    pin_name,
                    serve_name,
                    reason
                );
            }
        }

        // Re-record the connection's account for NOISE traffic that fell
        // through to the normal pick above (no usable conn-pinned account, or
        // affinity itself is off). A direct hit already returned early via
        // `conn_affinity_record` in the overlay; this is the "otherwise normal
        // rotation, and re-record" half of §2. Never touches `self.affinity`.
        if request_class == RequestClass::Noise {
            if let (Some(k), Some(idx)) = (conn_key, best) {
                self.conn_affinity_record(k, idx, now_ms);
            }
        }
        best
    }

    /// LAST-RESORT serve when normal [`Self::select`] found nothing (the whole
    /// fleet reads over the SOFT switch threshold). Serves an account that Anthropic
    /// still allows, ignoring the soft utilization/pacing gates but honoring the
    /// HARD blocks:
    ///   - `disabled` / [`AccountStatus::Error`] → skip;
    ///   - ANY live rate-limit hold (`rate_limited_until_ms` in the future) → skip.
    ///     A SERVE decision honours the whole hold; only the PIN decision softens the
    ///     short ones (see [`Self::hard_ok`] and [`CACHE_WARM_HOLD_SECS`]);
    ///   - `quota.status == Some("rejected")` → skip (genuinely blocked; would 429);
    ///   - a Fable request whose model-scoped weekly (`7d_oi`) is a hard reject → skip
    ///     (that same account still serves every non-Fable model).
    ///
    /// TWO paths, mirroring [`Self::select`]'s never-nested lock discipline (read the
    /// pin under the affinity lock, DROP it before taking the accounts lock):
    ///  - **(A) PIN-HONOR** (cache-warm): when `affinity` is `Some(key)` and the
    ///    session's pinned account is not in `tried` and passes the HARD gates, serve
    ///    THAT pin — even over the soft threshold — to keep the operator's prompt
    ///    cache warm. NO revalidation throttle here (the global egress throttle
    ///    already paces); the pin is left as-is. [`Self::select`] now runs this same
    ///    serve-the-pin rule in its own affinity fast-path (identical log line), so a
    ///    servable pin is honoured there and this path is the belt-and-braces copy for
    ///    any caller that reaches revalidation directly.
    ///  - **(B) FALLBACK** (no pin, or the pin is rejected/held/tried/model-blocked):
    ///    the least-utilized surviving account wins (lowest
    ///    [`crate::quota::Quota::max_utilization`]; ties: LRU `last_selected_seq`).
    ///    The anti-storm throttle applies HERE ONLY — at most one NEW-account
    ///    revalidation per [`REVALIDATION_MIN_SPACING_MS`]; inside that window return
    ///    `None` (the caller emits the honest 429). The window is spent only once a
    ///    servable candidate exists. On success the session is RE-PINNED to the
    ///    chosen account, mirroring `select()`'s re-pin — EXCEPT when the pin was
    ///    passed over merely because it cannot serve THIS request: its MODEL CLASS
    ///    ([`Self::model_blocked`]) or a hold that clears while its cache is still
    ///    warm ([`Self::hold_clears_while_warm`]). Both are per-request facts, so
    ///    they re-write the OLD index instead (`keep_pin`, exactly as `select()`
    ///    does).
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

        // Mirrors `select()`'s `keep_pin`: set to the OLD pin index when the pin
        // cannot serve THIS request but the ACCOUNT is alive, so the fallback below
        // serves elsewhere while re-writing the OLD index. Without it a Fable request
        // that reaches this path re-keys the session and the next Opus turn pays a
        // cold prefix — the same defect this file fixed in `select()`, one path over.
        let mut keep_pin: Option<usize> = None;

        // (A) PIN-HONOR — read the pin under the affinity lock, DROP it, then check
        // the pin's HARD gates under the accounts lock (never nested). No throttle.
        if let Some(key) = affinity {
            let pinned = {
                let pins = self.affinity.lock().expect("affinity lock poisoned");
                pins.get(&key).map(|&(idx, _)| idx)
            };
            if let Some(idx) = pinned {
                let mut accounts = self.accounts.write().expect("accounts lock poisoned");
                // `tried` gates SERVING and nothing else: the pin already failed THIS
                // request upstream, so it cannot answer it. Whether the PIN MOVES is
                // decided below, on ACCOUNT-level evidence alone — see the comment
                // there.
                if !tried.contains(&idx) && accounts.get(idx).is_some_and(&hard_ok) {
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
                // The pin cannot serve THIS request → fall through to the fallback
                // path (lock drops at the end of this scope, before the affinity
                // lock below). Whether that fall-through also MOVES the pin depends
                // on WHY: a model-class block leaves the account alive for every
                // other class, a short hold leaves it alive for every LATER request,
                // and membership in `tried` says only that ONE request failed on it
                // — none of the three is evidence the account is gone, so in all of
                // them the pin stays. Only an ACCOUNT-level HARD block is a durable
                // re-key.
                //
                // This test used to sit INSIDE `if !tried.contains(&idx)`, so a pin
                // that merely failed this one request left `keep_pin` at `None` and
                // the fallback's `pins.insert` re-keyed the session for good — a
                // transient blip throwing away a warm prompt cache. `select()` takes
                // the opposite decision on the identical fact and documents why
                // (see its affinity fast-path); the two now agree.
                if accounts
                    .get(idx)
                    .is_some_and(|a| Self::account_hard_ok(a, now_ms))
                {
                    keep_pin = Some(idx);
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

        // Re-pin the session (accounts lock already dropped — never nest the two).
        // Mirrors `select()`'s re-pin, `keep_pin` included: a model-class divert
        // re-writes the OLD index with a fresh `now_ms`, so the session comes home on
        // its next request of another class. No size-cap eviction here because a
        // revalidation serve only ever touches an EXISTING session's pin.
        if let Some(key) = affinity {
            let pin_idx = keep_pin.unwrap_or(idx);
            let previous = {
                let mut pins = self.affinity.lock().expect("affinity lock poisoned");
                let previous = pins.get(&key).map(|&(i, _)| i);
                pins.insert(key, (pin_idx, now_ms));
                self.mark_affinity_dirty();
                previous
            };
            // Until now this arm emitted nothing, so a request served off-pin by the
            // fallback was unattributable: the logs showed a serve on an account the
            // session was not pinned to and no line explaining it. One greppable line
            // carrying the same `reason=` field as `select()`'s divert log, so all
            // four reasons grep together. It says KEPT or RE-KEYED rather than
            // assuming a divert, because this arm does both: `keep_pin` set means the
            // pin stayed and another account served; `keep_pin` unset means the
            // fallback account became the new pin. The accounts lock is taken only
            // after the affinity lock has dropped above — never simultaneously.
            let accounts = self.accounts.read().expect("accounts lock poisoned");
            let pin_name = accounts.get(pin_idx).map_or("?", |a| a.name.as_str());
            let serve_name = accounts.get(idx).map_or("?", |a| a.name.as_str());
            tracing::info!(
                "affinity: revalidation session {} pin {} -> serving {} (reason=revalidation-fallback, pin {})",
                short_session_id(key),
                pin_name,
                serve_name,
                if previous.is_some_and(|p| p != pin_idx) {
                    "re-keyed"
                } else {
                    "kept"
                }
            );
        }
        Some(idx)
    }

    /// Milliseconds still to run on a LIVE rate-limit hold, or `None` when the
    /// account is not held right now — either no hold is armed, or its deadline has
    /// already passed (a past hold reads as expired live, no mutation needed).
    ///
    /// The single place `rate_limited_until_ms` is turned into a duration, so the
    /// three hold questions — held at all? clears warm? outlives the cache? — can
    /// never drift apart. Pure and lock-free.
    pub(super) fn hold_remaining_ms(account: &AccountRuntime, now_ms: i64) -> Option<i64> {
        account
            .rate_limited_until_ms
            .map(|until| until - now_ms)
            .filter(|&remaining_ms| remaining_ms > 0)
    }

    /// Whether account `idx` has NO live rate-limit hold at `now_ms` — either none
    /// was ever armed, or the one that was has already run out.
    ///
    /// The locking wrapper around [`Self::hold_remaining_ms`] for callers OUTSIDE
    /// the manager (the proxy's rotation loop), so `rate_limited_until_ms` stays
    /// private and the "is it still held?" question keeps exactly one answer.
    ///
    /// An index that names no account answers `false` (treated as still held) —
    /// the safe direction, since the only caller uses a `true` to re-admit an
    /// account into a request's rotation.
    pub fn hold_expired(&self, idx: usize, now_ms: i64) -> bool {
        let accounts = self.accounts.read().expect("accounts lock poisoned");
        accounts
            .get(idx)
            .is_some_and(|account| Self::hold_remaining_ms(account, now_ms).is_none())
    }

    /// Whether a live hold is still running when this account's prompt cache dies
    /// ([`CACHE_WARM_HOLD_SECS`] or more remaining) — the only hold long enough to
    /// be worth re-keying a session for, and so the only one
    /// [`Self::account_hard_ok`] counts as account death.
    ///
    /// The boundary is `>=`: a hold that clears at exactly the cache TTL is treated
    /// as LONG, because the prefix is gone at the same instant the account frees.
    ///
    /// Pure and lock-free; the caller holds whichever accounts lock it needs.
    pub(super) fn hold_outlives_cache(account: &AccountRuntime, now_ms: i64) -> bool {
        Self::hold_remaining_ms(account, now_ms)
            .is_some_and(|remaining_ms| remaining_ms >= CACHE_WARM_HOLD_SECS * 1000)
    }

    /// Whether a live hold clears while this account's prompt cache is still warm
    /// (under [`CACHE_WARM_HOLD_SECS`] remaining). The exact complement of
    /// [`Self::hold_outlives_cache`] over live holds, and the SOFT case: this
    /// account cannot answer THIS request, but it is a timer and not a death, so
    /// divert the one request and leave the pin where it is.
    ///
    /// Pure and lock-free; the caller holds whichever accounts lock it needs.
    pub(super) fn hold_clears_while_warm(account: &AccountRuntime, now_ms: i64) -> bool {
        Self::hold_remaining_ms(account, now_ms)
            .is_some_and(|remaining_ms| remaining_ms < CACHE_WARM_HOLD_SECS * 1000)
    }

    /// The TERMINAL account-level gates — the blocks that are a fact about the
    /// CREDENTIAL rather than about one request or one window, and that never
    /// self-free (so they carry no clear-instant): `disabled`,
    /// [`AccountStatus::Error`], and `quota.status == Some("rejected")` —
    /// Anthropic's own verdict, which unlike a window has no reset to wait on.
    /// `None` when no terminal gate is active.
    ///
    /// **This is the one list both [`Self::account_hard_ok`] and
    /// [`Self::account_gate`] read**, and it exists because they used to keep two
    /// hand-maintained copies of it that drifted: `account_hard_ok` blocked on
    /// `rejected` while `account_gate` had no `Rejected` arm at all, so an account
    /// Anthropic had explicitly rejected was held out of rotation while rendering
    /// `OK` in the TUI and feeding [`Manager::retry_after_hint`] as though it were
    /// healthy. A terminal gate added HERE reaches both callers at once; one added
    /// to either caller alone is exactly the bug this prevents.
    ///
    /// Pure and lock-free; the caller holds whichever accounts lock it needs.
    pub(super) fn account_terminal_gate(account: &AccountRuntime) -> Option<GateReason> {
        if account.disabled {
            return Some(GateReason::Disabled);
        }
        if account.status == AccountStatus::Error {
            return Some(GateReason::Login);
        }
        if account.quota.status.as_deref() == Some("rejected") {
            return Some(GateReason::Rejected);
        }
        None
    }

    /// The ACCOUNT-level HARD gates alone — the blocks that mean *this account is
    /// gone for every model class*, with every SOFT gate deliberately absent:
    ///   - every terminal gate ([`Self::account_terminal_gate`]: `disabled`,
    ///     [`AccountStatus::Error`], `rejected`) → hard fail;
    ///   - a rate-limit hold that OUTLIVES the prompt cache
    ///     ([`Self::hold_outlives_cache`]) — a SHORTER hold is a timer, not a death,
    ///     and deliberately passes here.
    ///
    /// **This — not [`Self::hard_ok`] — is the SOLE authority on whether a session's
    /// pin may be re-keyed**, because a re-key re-creates the conversation prefix on
    /// a cold account. Everything absent from it is a fact about ONE REQUEST rather
    /// than about the account, and a per-request fact may divert a request but must
    /// never move a pin. That covers the soft utilization threshold
    /// ([`crate::quota::Quota::is_near`]), pacing, an entry in `tried`, model-scoped
    /// exhaustion ([`Self::model_blocked`]), and — the newest — a rate-limit hold
    /// that clears while the cache is still warm ([`Self::hold_clears_while_warm`]).
    ///
    /// The hold split is why this predicate is NOT the right one for a SERVE
    /// decision: a short-held account passes here (its pin survives) while it still
    /// cannot answer a request until the timer runs out. [`Self::hard_ok`] adds that
    /// back.
    ///
    /// Known staleness caveat: `quota.status` is written ONLY from live response
    /// headers ([`crate::quota`]), never by the background probe, so for a benched
    /// account it is stale-or-`None` and `None` passes here. That is acceptable
    /// precisely because a genuine rejection answers the next attempt with a 429,
    /// which arms a real hold — upstream stays the oracle instead of our own
    /// possibly-stale arithmetic.
    ///
    /// Pure and lock-free; the caller holds whichever accounts lock it needs.
    pub(super) fn account_hard_ok(account: &AccountRuntime, now_ms: i64) -> bool {
        if Self::account_terminal_gate(account).is_some() {
            return false;
        }
        if Self::hold_outlives_cache(account, now_ms) {
            return false;
        }
        true
    }

    /// Whether this account cannot serve **this request's model class** while still
    /// serving every other one: a Fable request against an exhausted model-scoped
    /// weekly (`7d_oi`) bucket.
    ///
    /// Model-scoped exhaustion is a property of the REQUEST CLASS, never of the
    /// account — an account out of its Fable weekly answers Opus perfectly — so it is
    /// deliberately NOT part of [`Self::account_hard_ok`]. At a pin it belongs with
    /// the soft-divert family: divert THAT ONE request and keep the pin, so the
    /// session's next Opus turn still lands on the account holding its warm prefix.
    /// It remains a hard skip for every SERVE decision ([`Self::hard_ok`],
    /// [`Self::eligible`]) — serving a Fable request from an exhausted bucket only
    /// buys a 429.
    ///
    /// The live shape that makes the distinction load-bearing: every Claude Code
    /// session mixes classes — Opus for the conversation plus a one-line Fable call
    /// for titles and summaries — while a real fleet reads 95-99% on the Fable weekly
    /// across nearly every account. Counted as account death, one cheap title request
    /// re-keyed the session and dragged a 200k-token conversation onto a cold
    /// account.
    ///
    /// Pure and lock-free; the caller holds whichever accounts lock it needs.
    pub(super) fn model_blocked(
        account: &AccountRuntime,
        global_threshold: f64,
        now: OffsetDateTime,
        is_fable: bool,
    ) -> bool {
        let threshold = account.switch_threshold.unwrap_or(global_threshold);
        is_fable && account.quota.model_weekly_exhausted(threshold, now)
    }

    /// Can this account SERVE this request at all, soft gates aside: the
    /// account-level blocks ([`Self::account_hard_ok`]), **any** live rate-limit
    /// hold, and this request's own model-class block ([`Self::model_blocked`]).
    ///
    /// The short-hold term is the one that is not implied by `account_hard_ok`, and
    /// it is load-bearing in exactly one direction. A hold under
    /// [`CACHE_WARM_HOLD_SECS`] deliberately passes `account_hard_ok` so the session
    /// keeps its pin — but the account is still parked, so serving it only buys
    /// another 429. Every SERVE decision therefore re-adds the full hold here; only
    /// the PIN decision gets the softened one.
    ///
    /// The right predicate for a serve decision (may this account answer *this*
    /// request?) and the wrong one for a pin decision (is this session's account
    /// gone?) — see [`Self::account_hard_ok`], which owns the latter.
    ///
    /// Pure and lock-free; the caller holds whichever accounts lock it needs.
    pub(super) fn hard_ok(
        account: &AccountRuntime,
        global_threshold: f64,
        now: OffsetDateTime,
        now_ms: i64,
        is_fable: bool,
    ) -> bool {
        Self::account_hard_ok(account, now_ms)
            && !Self::hold_clears_while_warm(account, now_ms)
            && !Self::model_blocked(account, global_threshold, now, is_fable)
    }

    /// Whether the CONTROL account may be preferred for an unpinned
    /// identity-plane request (control-account part 2, §1). Deliberately
    /// **bypasses the `disabled` gate**: the control account ships `disabled`
    /// on purpose (out of the inference rotation) while remaining the admin
    /// identity — see [`Manager::control_idx`]'s doc-comment. Still honours
    /// every gate that means genuinely broken:
    ///   - [`AccountStatus::Error`] → skip (a dead login is a dead login);
    ///   - `quota.status == Some("rejected")` → skip (Anthropic's own verdict);
    ///   - a LIVE rate-limit hold (`rate_limited_until_ms` in the future) → skip.
    ///
    /// **Deliberately NOT [`Self::eligible`] and NOT [`Self::account_hard_ok`]**
    /// — both fold `disabled` into their terminal gate
    /// ([`Self::account_terminal_gate`]), so calling either here would make the
    /// whole control-routing feature a silent no-op: it compiles, its tests
    /// pass, and it never fires because the one account it is meant to route to
    /// always reads as ineligible. This predicate is the bypass; it does NOT
    /// loosen `eligible` or `account_hard_ok` themselves, which every other
    /// caller still depends on.
    ///
    /// No utilization/pacing/model-class check: those exist to spread LOAD
    /// across a fleet, which does not apply to a single designated identity —
    /// the reserve for a POOLED control account is [`effective_threshold`]'s
    /// job, applied on the GENERAL pick's side, not here.
    ///
    /// Pure and lock-free; the caller holds whichever accounts lock it needs.
    pub(super) fn control_eligible(account: &AccountRuntime, now_ms: i64) -> bool {
        if account.status == AccountStatus::Error {
            return false;
        }
        if account.quota.status.as_deref() == Some("rejected") {
            return false;
        }
        if let Some(until) = account.rate_limited_until_ms {
            if now_ms < until {
                return false;
            }
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
        // Kept here on purpose: for an UNPINNED pick a Fable-exhausted account
        // genuinely cannot serve this request, so it must be skipped. What changed
        // is only that it is no longer grounds to MOVE AN EXISTING PIN — that
        // decision reads [`Self::account_hard_ok`], which excludes this gate.
        if Self::model_blocked(account, global_threshold, now, is_fable) {
            return false;
        }
        // SOFT pacing gate, evaluated LAST so it only ever narrows an already-healthy
        // account. When `respect_pacing` is false (the fallback pass) or pacing is
        // unconfigured, this is inert — so a default-OFF build is byte-identical here
        // and the fallback pass can always still find a servable account.
        if respect_pacing && Self::paced_out(account, pacing, now_ms) {
            return false;
        }
        true
    }

    /// Whether the SOFT pacing gate ALONE holds this account out: it is at the
    /// per-account in-flight cap, or inside the min-spacing window since its last
    /// serve. Inert (always `false`) when pacing is unconfigured, which is how it
    /// ships.
    ///
    /// Split out of [`Self::eligible`] because [`Self::select`] needs to tell the two
    /// soft gates apart. They are not the same kind of signal:
    ///   - the **utilization threshold** is OUR arithmetic over headers that go stale
    ///     by minutes, and it routinely benches an account Anthropic is still
    ///     answering 200s for — so it may not bench a warm pin;
    ///   - **pacing** is a fact we measure exactly and continuously about our OWN
    ///     concurrency, and spreading a session's burst is the entire reason the cap
    ///     exists. It stays a per-REQUEST yield: divert this one, keep the pin.
    pub(super) fn paced_out(account: &AccountRuntime, pacing: &PacingConfig, now_ms: i64) -> bool {
        if !pacing.is_active() {
            return false;
        }
        if let Some(cap) = pacing.effective_max_in_flight() {
            if account.in_flight >= cap {
                return true;
            }
        }
        if let Some(gap) = pacing.min_spacing_ms {
            if now_ms.saturating_sub(account.last_served_ms) < gap as i64 {
                return true;
            }
        }
        false
    }

    /// Why this account is out of rotation and when it clears — the display and
    /// hint-side companion to [`Self::eligible`], mirroring its HARD gates exactly
    /// (disabled/error/rejected → hold → 5-hour → weekly → Fable weekly) while deliberately
    /// omitting soft pacing, which only ever narrows an already-healthy account and
    /// never holds one out. Pure and lock-free so both [`Manager::snapshot`] and
    /// [`Manager::retry_after_hint`] can call it as the single source of truth.
    ///
    /// The terminal gates ([`Self::account_terminal_gate`]: `Disabled`, `Login`,
    /// `Rejected`) never self-free, so their instant is `None` — a rejected account
    /// therefore stops feeding [`Manager::retry_after_hint`] a clear-instant it was
    /// never going to honour. Otherwise every ACTIVE gate contributes the instant it clears:
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
        // Shared with `account_hard_ok` so the two can no longer disagree about
        // which blocks are account-level (see `account_terminal_gate`).
        if let Some(reason) = Self::account_terminal_gate(account) {
            return (reason, None);
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
    /// The GENERAL pick's extra narrowing of the control account (§3): `true`
    /// for every account that is not the control account (a no-op), and for the
    /// control account itself, `true` only while its utilization stays under
    /// `effective_threshold(threshold, control_reserve, true)` rather than the
    /// full threshold [`Self::eligible`] already checked. Applied AFTER
    /// `eligible` returns true, so it only ever narrows further — never widens.
    /// **Inert while the control account is `disabled`**: `eligible` excludes a
    /// disabled account before this is ever reached (see
    /// [`Manager::control_reserve`]'s doc-comment).
    fn pool_pick_respects_control_reserve(
        &self,
        idx: usize,
        account: &AccountRuntime,
        now: OffsetDateTime,
    ) -> bool {
        if self.control() != Some(idx) {
            return true;
        }
        let threshold = account.switch_threshold.unwrap_or(self.global_threshold);
        let reserved = effective_threshold(threshold, self.control_reserve, true);
        !account.quota.is_near(reserved, now)
    }

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
            if !self.pool_pick_respects_control_reserve(idx, account, now) {
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
            if !self.pool_pick_respects_control_reserve(idx, account, now) {
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

    /// Lookup half of [`Manager::conn_affinity`]: the account this connection
    /// was last served on, or `None` if never recorded or the entry has aged
    /// past [`CONN_AFFINITY_TTL_MS`] (treated as absent — the connection is
    /// presumed gone). Does NOT check the account's current eligibility; the
    /// caller ([`Manager::select`]'s noise-routing overlay) does that itself so
    /// it can log/fall through distinctly.
    pub(super) fn conn_affinity_get(&self, conn_key: u64, now_ms: i64) -> Option<usize> {
        let map = self
            .conn_affinity
            .lock()
            .expect("conn affinity lock poisoned");
        map.get(&conn_key).and_then(|&(idx, touch)| {
            if now_ms.saturating_sub(touch) < CONN_AFFINITY_TTL_MS {
                Some(idx)
            } else {
                None
            }
        })
    }

    /// Write half of [`Manager::conn_affinity`]: record (or refresh) which
    /// account served connection `conn_key`, then evict the single
    /// oldest-last-touch entry if the map grew past [`CONN_AFFINITY_CAP`] —
    /// mirrors `select()`'s `AFFINITY_CAP` eviction for `self.affinity`, but on
    /// this SEPARATE, never-persisted map. Never touches `self.affinity` and
    /// never marks it dirty.
    pub(super) fn conn_affinity_record(&self, conn_key: u64, idx: usize, now_ms: i64) {
        let mut map = self
            .conn_affinity
            .lock()
            .expect("conn affinity lock poisoned");
        map.insert(conn_key, (idx, now_ms));
        if map.len() > CONN_AFFINITY_CAP {
            if let Some((&oldest, _)) = map.iter().min_by_key(|(_, &(_, touch))| touch) {
                if oldest != conn_key {
                    map.remove(&oldest);
                }
            }
        }
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
