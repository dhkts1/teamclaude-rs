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

/// Why the group-respecting first pass of [`Manager::select_with_group`] came up
/// empty, so the soft fallback had to drop the group and serve from the whole
/// pool.
///
/// Exists because the log line at that fallback used to hardcode "under pacing"
/// for every group miss. Pacing ships OFF ([`crate::config::default_pacing`]), so
/// on a default install that sentence named the one constraint that was NOT in
/// play and hid the one that was. Measured on the live fleet 2026-08-23: a group
/// whose only member was the control account fell back on 3 of 3 probes (a
/// two-account group landed in-group 3 of 3), and every one of the nine log lines
/// produced while diagnosing it blamed pacing, which was unconfigured.
///
/// [`Self::OnlyControl`] is the arm that matters: it is not a transient miss. No
/// retry, no quota recovering and no restart fixes it, because the exclusion is
/// unconditional for [`RequestClass::Inference`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GroupMiss {
    /// No configured account carries this label at all — a typo, or a label
    /// removed from every account while a client still asks for it.
    Unknown,
    /// Every member is the control account, which an inference request never
    /// selects. This group can NEVER serve inference.
    OnlyControl,
    /// Members exist and are selectable in principle, but every one is held out
    /// right now by a hard gate: disabled, errored, rate-limited, over its
    /// switch threshold, model-blocked, or reserved away from this caller.
    AllGated,
    /// At least one member would serve but for the SOFT pacing gate — the only
    /// arm the old message was ever right about.
    Paced,
}

impl GroupMiss {
    /// One greppable token for the `reason=` log field, so an operator can count
    /// group misses by cause without parsing prose.
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "no-such-group",
            Self::OnlyControl => "control-account-only",
            Self::AllGated => "all-members-unavailable",
            Self::Paced => "all-members-paced",
        }
    }

    /// The sentence a human reads in the log. Says what to DO about it in the
    /// one case that has a remedy the operator must apply by hand.
    pub(super) fn explain(self) -> &'static str {
        match self {
            Self::Unknown => "no configured account carries the requested group",
            Self::OnlyControl => {
                "the requested group's only member is the control account, which inference never selects — no request will ever route to this group until another account joins it, the control account is cleared, or the group opts in with `tcr group allow-control <g>`"
            }
            Self::AllGated => "every account in the requested group is currently unavailable",
            Self::Paced => "every account in the requested group is paced out",
        }
    }
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

/// Bound on [`Manager::divert_ledger`] — same cap `select()`'s `AFFINITY_CAP`
/// applies to `self.affinity` (`:664-675`). [`DivertEpisode`] carries no
/// separate last-touch stamp (§4.1's struct is quoted verbatim from the
/// design doc), so `until_ms` is the eviction sort key here — the closest
/// available recency proxy, not a perfect one. A session that never diverts
/// again after arming a short hold ages out of this bound the same way a
/// long-idle `affinity` entry does.
const DIVERT_LEDGER_CAP: usize = 1024;

/// The three-way verdict [`divert_verdict`] hands back for one divert
/// decision. `Fresh` and `Sticky` are consulted every divert regardless of
/// budget (§4.3 — "stickiness does not consult the budget at all"); `Block`
/// only becomes reachable once a caller passes a nonzero `budget` (unit E's
/// job, §4.4) — `select()`'s own call site always passes `0` in this phase,
/// so `Block` cannot be produced from here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DivertVerdict {
    /// No episode for this session matches `(pin, until_ms)` — either there
    /// is none yet, or the live `(pin, until_ms)` differs from the stored
    /// one (a new hold on the same pin, or a different pin entirely). This
    /// divert is free to land on any eligible account; whichever one it
    /// picks becomes this episode's sticky destination.
    Fresh,
    /// The episode matches and already has a known-warm destination — reuse
    /// it. Offered unconditionally (budget plays no part in this arm): a
    /// repeat divert to an already-touched account costs nothing new.
    Sticky(usize),
    /// The episode matches, `budget != 0`, and its distinct-destination
    /// count is already at or over that budget. Reserved for unit E's
    /// blocking half (§4.4) — never produced while every call site passes
    /// `budget = 0`.
    Block,
}

/// The chain-head predicate (§4.4): given this session's current divert
/// episode (or `None`), the pin and hold-deadline of the divert being
/// decided right now, and a distinct-destination `budget` (`0` = unlimited),
/// decide whether to reuse a known destination, allow a fresh pick, or (once
/// a nonzero budget is wired in) block. Pure function over its four
/// arguments — no lock, no I/O — so unit F's replay harness can run this
/// EXACT code over reconstructed log sequences (§6) rather than a
/// reimplementation.
///
/// Episode identity is `(pin, until_ms)`, not the session key alone: a
/// mismatch on either field means the stored episode is stale (a new hold,
/// or the pin itself changed) and is treated as no episode at all — the
/// caller is expected to overwrite the ledger entry from scratch on the next
/// `record_divert` (§4.1, "reset is structural, not timed").
///
/// `Block` fires only when `budget != 0` (the kill-switch: `0` never
/// blocks) and the episode's `destinations.count_ones() >= budget`. This
/// phase's own `select()` call site always passes `0`, so `Block` is
/// unreachable from production code today; unit E wires a real budget in at
/// its own call site(s) and is responsible for deciding, at THAT call site,
/// whether a `Sticky` destination should still be attempted before honouring
/// `Block` — this function does not know whether the caller has already
/// tried and rejected the sticky account.
pub fn divert_verdict(
    ep: Option<DivertEpisode>,
    pin: usize,
    until_ms: i64,
    budget: u32,
) -> DivertVerdict {
    let Some(episode) = ep.filter(|e| e.pin == pin && e.until_ms == until_ms) else {
        return DivertVerdict::Fresh;
    };
    if budget != 0 && episode.destinations.count_ones() >= budget {
        DivertVerdict::Block
    } else {
        DivertVerdict::Sticky(episode.sticky)
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
    /// Within a priority tier we pick from the soonest **reset-urgency bucket**
    /// first ([`Self::reset_urgency_tier`] — the governing weekly reset floored
    /// into `resetUrgencyTierHours`-wide buckets, default 24h), because unused
    /// weekly quota is worth nothing once its window resets. Within a bucket we
    /// pick the **least-recently-selected** account (lowest `last_selected_seq`;
    /// a never-selected account sorts first) so consecutive requests fan out
    /// instead of hammering one account. Ordering by quota headroom was rejected
    /// deliberately: a single request barely moves a weekly bar, so "most
    /// headroom first" would deterministically pin one account until its bar
    /// caught up — the exact overload this fixes. Ranking on the RAW reset
    /// instant would pin it the same way, which is why the urgency term buckets
    /// instead of comparing directly. The winner is stamped with the next
    /// monotonic tick *before returning*, so even a burst of concurrent selects
    /// rotates within its bucket (each sees the previous stamp). The soonest
    /// weekly reset is the final cold-start tiebreak (all-unseen startup).
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
    ///
    /// Thin wrapper over [`Self::select_with_group`] with no group preference —
    /// kept as the stable signature the test suite (and every caller that has no
    /// `--group` to express) already calls by the hundred, so a per-request
    /// preference this narrow does not force every one of them to grow a new
    /// trailing `None`.
    pub fn select(
        &self,
        tried: &HashSet<usize>,
        now: OffsetDateTime,
        model: Option<&str>,
        affinity: Option<u64>,
        path: &str,
        conn_key: Option<u64>,
    ) -> Option<usize> {
        self.select_with_group(tried, now, model, affinity, path, conn_key, None)
    }

    /// [`Self::select`], with an optional PREFER-semantics group (`tcr run
    /// --group <name>`, Phase 1 — see `docs/plans/account-groups-bridge-phase1.md`).
    /// `group` narrows only the pacing-respecting first pass of the normal pick
    /// (mirroring how that pass already narrows on `respect_pacing`); the soft
    /// fallback pass always passes `None`, so a group with no current capacity
    /// degrades to the whole pool rather than a 429. Every OTHER eligibility check
    /// in this function — honouring an existing pin, a connection's noise
    /// affinity, a sticky divert destination — passes `None` unconditionally: an
    /// already-warm session is never re-keyed by a per-request preference.
    #[allow(clippy::too_many_arguments)]
    pub fn select_with_group(
        &self,
        tried: &HashSet<usize>,
        now: OffsetDateTime,
        model: Option<&str>,
        affinity: Option<u64>,
        path: &str,
        conn_key: Option<u64>,
        group: Option<&str>,
    ) -> Option<usize> {
        // Live-reload group membership/settings before anything below reads them —
        // see `Self::reload_groups_if_changed`'s doc. Cheap no-op when the config
        // file's mtime has not moved since the last check.
        self.reload_groups_if_changed();

        // Hard account lock: pin ALL traffic to the configured account, bypassing
        // rotation/affinity/migration. `tried` still ends the rotation loop — once the
        // locked account has failed this request, return None (no failover to the pool).
        if let Some(li) = self.locked_idx {
            return if tried.contains(&li) { None } else { Some(li) };
        }

        // A single point-in-time snapshot for the whole call — see
        // `Self::reserved_groups`'s doc for why this clones rather than holding
        // the lock across the accounts/affinity locks taken below.
        let reserved_groups = self.reserved_groups();
        // Same point-in-time-snapshot reasoning as `reserved_groups` above — see
        // `Self::control_allowed_groups`'s doc.
        let control_allowed_groups = self.control_allowed_groups();
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
        // The pin's `rate_limited_until_ms` at the moment `keep_pin` was set —
        // the divert episode's identity half (§4.1), captured once here under
        // whichever accounts guard was already held at that site rather than
        // taking a second lock later. `0` when the pin has no live hold (a
        // paced/model-class/pin-tried divert): a stable sentinel, not a real
        // deadline, so those diverts still key one shared episode per pin.
        let mut divert_until_ms: i64 = 0;

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
                        .is_some_and(|a| Self::account_hard_ok(a, now_ms, group, &reserved_groups))
                    {
                        keep_pin = Some(idx);
                        divert_reason = Some("pin-tried");
                        divert_until_ms = accounts
                            .get(idx)
                            .and_then(|a| a.rate_limited_until_ms)
                            .unwrap_or(0);
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
                                None,  // no group PREFERENCE — honouring an existing pin
                                group, // but reservation is not a preference — real ask
                                &reserved_groups,
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
                            let account_alive = accounts.get(idx).is_some_and(|a| {
                                Self::account_hard_ok(a, now_ms, group, &reserved_groups)
                            });
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
                                divert_until_ms = accounts
                                    .get(idx)
                                    .and_then(|a| a.rate_limited_until_ms)
                                    .unwrap_or(0);
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
                                        None,  // no group PREFERENCE — honouring an existing pin
                                        group, // but reservation is not a preference — real ask
                                        &reserved_groups,
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
                                            None, // no group PREFERENCE — honouring an existing connection
                                            group, // but reservation is not a preference — real ask
                                            &reserved_groups,
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

        // Sticky-destination overlay (§4.3): when this request diverted
        // (`keep_pin` is `Some` — the exact mirror of the control-routing
        // overlay above, which is gated on `keep_pin.is_none()` and is
        // therefore already skipped for every divert), prefer this episode's
        // already-warmed destination over spending a fresh one. Shape copied
        // from the NOISE overlay (`:506-548` pre-this-change): read the
        // remembered index, check `!tried.contains` and `eligible`; otherwise
        // fall through to the normal pick below, which records the winner as
        // this episode's destination on the way out (§4.2).
        //
        // The ledger is read here in its OWN critical section — no accounts
        // or affinity lock is held at this point (both dropped above) — and
        // `budget = 0` is hardcoded: stickiness never consults the budget
        // (see this file's `divert_verdict` doc-comment); the real
        // `divert_budget()` read is unit E's, at its own (blocking) call
        // site.
        let sticky_pick: Option<usize> = if let Some(pin_idx) = keep_pin {
            let episode = affinity.and_then(|key| {
                self.divert_ledger
                    .lock()
                    .expect("divert ledger lock poisoned")
                    .get(&key)
                    .copied()
            });
            match divert_verdict(episode, pin_idx, divert_until_ms, 0) {
                DivertVerdict::Sticky(sticky) if !tried.contains(&sticky) => {
                    let accounts = self.accounts.read().expect("accounts lock poisoned");
                    let usable = accounts.get(sticky).is_some_and(|a| {
                        Self::eligible(
                            a,
                            self.global_threshold,
                            &self.pacing,
                            true,
                            now,
                            now_ms,
                            is_fable,
                            None,  // no group PREFERENCE — honouring a sticky destination
                            group, // but reservation is not a preference — real ask
                            &reserved_groups,
                        )
                    });
                    usable.then_some(sticky)
                }
                _ => None,
            }
        } else {
            None
        };
        let via_sticky = sticky_pick.is_some();

        // Normal LRU/priority pick (identical to the pre-affinity path when no
        // control account is set). The accounts lock is scoped so it is released
        // before we touch the affinity lock again for the re-pin below.
        let best = if let Some(sticky) = sticky_pick {
            let mut accounts = self.accounts.write().expect("accounts lock poisoned");
            let tick = self.select_seq.fetch_add(1, Ordering::Relaxed);
            if let Some(account) = accounts.get_mut(sticky) {
                account.last_selected_seq = tick;
            }
            Some(sticky)
        } else {
            let mut accounts = self.accounts.write().expect("accounts lock poisoned");

            // Inference must NEVER select the control account, even one that is
            // ENABLED (pooled) — the default-disabled control account is already
            // excluded by `eligible`'s own disabled check, so this only starts
            // doing real work the day `controlAccount` stops being disabled.
            // Modeled as an extra `tried` member rather than a new parameter
            // threaded through `pick_eligible`/`pick_least_loaded`, so both
            // passes (and the pacing/least-loaded fallback) honour it for free.
            //
            // Hoisted into its own binding (it used to be inlined in the `match`
            // below) because the group-miss classifier on the fallback path needs
            // the SAME answer: an account held out only by this exclusion is the
            // one case where a `--group` ask can never be satisfied by any retry,
            // and naming it requires knowing which index was excluded and why.
            //
            // Opt-in carve-out: an explicit `--group g` ask is allowed to select
            // the control account when the control account itself carries `g`
            // AND `g` has opted in via `groupSettings.g.allowControlAccount`
            // (`tcr group allow-control g`). Both conditions are checked here
            // rather than deferred to `eligible`, so this stays the single place
            // that decides whether the control account is even a CANDIDATE for
            // inference — `pool_pick_respects_control_reserve`'s reserve-floor
            // guard already applies unconditionally once the account becomes
            // selectable, so this carve-out stays safe without further changes.
            let control_excluded: Option<usize> = if request_class == RequestClass::Inference {
                self.control().filter(|&idx| {
                    let opted_in = group.is_some_and(|g| {
                        control_allowed_groups.contains(g)
                            && accounts
                                .get(idx)
                                .is_some_and(|a| a.groups.iter().any(|gr| gr == g))
                    });
                    !opted_in
                })
            } else {
                None
            };
            let pool_tried: std::borrow::Cow<'_, HashSet<usize>> = match control_excluded {
                Some(control_idx) if !tried.contains(&control_idx) => {
                    let mut t = tried.clone();
                    t.insert(control_idx);
                    std::borrow::Cow::Owned(t)
                }
                _ => std::borrow::Cow::Borrowed(tried),
            };
            let pool_tried: &HashSet<usize> = &pool_tried;

            // First pass: honour pacing (skip accounts at the concurrency cap or
            // inside the min-spacing window) AND, when `--group` was requested,
            // prefer that group. With pacing OFF and no group this is byte-identical
            // to the pre-pacing pick.
            let mut best =
                self.pick_eligible(&accounts, pool_tried, now, now_ms, is_fable, true, group);

            // Soft fallback (CRITICAL — pacing must never DROP a servable request,
            // and a group with no current capacity must never either): if the first
            // pass found nothing, retry ignoring BOTH pacing and the group, serving
            // the least-loaded (lowest in_flight, then the normal LRU key) account in
            // the WHOLE pool. With pacing OFF and no group, the first pass and this
            // pass use identical eligibility, so a None first pass ⟹ None here too —
            // default-OFF stays byte-identical (no spurious fallback, no log).
            if best.is_none() {
                if let Some(idx) =
                    self.pick_least_loaded(&accounts, pool_tried, now, now_ms, is_fable, None)
                {
                    if let Some(account) = accounts.get(idx) {
                        // This fallback fires whenever the first (group- and
                        // pacing-respecting) pass came up empty — which can be pacing
                        // alone, the group alone, or both together. Name whichever
                        // constraints were actually IN PLAY on the first pass rather
                        // than always blaming pacing: an operator debugging why
                        // `--group` did not land where expected must not read a log
                        // line about pacing when pacing was never the reason.
                        match group {
                            Some(g) => {
                                let miss = Self::classify_group_miss(
                                    &accounts,
                                    tried,
                                    control_excluded,
                                    self.global_threshold,
                                    &self.pacing,
                                    now,
                                    now_ms,
                                    is_fable,
                                    g,
                                    &reserved_groups,
                                );
                                tracing::info!(
                                    account = %account.name,
                                    in_flight = account.in_flight,
                                    group = g,
                                    reason = miss.as_str(),
                                    "group: falling back to the whole pool (group ignored), serving least-loaded — {}",
                                    miss.explain(),
                                );
                            }
                            None => tracing::info!(
                                account = %account.name,
                                in_flight = account.in_flight,
                                "pacing: all accounts paced, serving least-loaded"
                            ),
                        }
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

        // Record this divert's destination in the episode ledger (§4.2 rule
        // 3: written only after the affinity guard below has dropped, in its
        // own critical section — never nested under it). Every divert
        // records, not just sticky ones, so a fresh divert's destination
        // becomes the NEXT divert's sticky offer.
        if let (Some(key), Some(pin_idx), Some(dest)) = (affinity, keep_pin, best) {
            self.record_divert(key, pin_idx, divert_until_ms, dest, now_ms);
        }

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
                // A SIBLING line, not a replacement (§4.8): the line above is a
                // parsed interface (`divert-history.py:33-36`) and stays exactly
                // as it was. This one exists only to prove fan-out was avoided —
                // the same regex cannot match it (different second word).
                if via_sticky {
                    tracing::info!(
                        "affinity: divert-sticky session {} pin {} -> reusing {} (reason={}, warm=true)",
                        short_session_id(key),
                        pin_name,
                        serve_name,
                        reason
                    );
                }
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
        // Live-reload group membership/settings — same call `select_with_group`
        // makes, so a session revalidated through THIS path also sees a just-
        // reserved group and re-keys (bridge's pin-reclaim-via-reload case).
        self.reload_groups_if_changed();

        // Anti-storm spacing on the FALLBACK path only: the upstream 429→hold and the
        // global egress throttle are the real backstops; this just stops a
        // synchronized burst from slamming one over-threshold account when the fleet
        // saturates. The pin-honor path is never throttled here.
        const REVALIDATION_MIN_SPACING_MS: i64 = 2000;

        let reserved_groups = self.reserved_groups();
        let now_ms = odt_to_ms(now);
        let is_fable = model.is_some_and(crate::model::is_fable_model);

        // HARD-gate predicate shared by both paths: the blocks that even a
        // revalidation serve must honor (soft utilization/pacing deliberately absent).
        //
        // `group: None` — this path (unlike `select_with_group`) carries no
        // `--group` context at all, so it cannot tell a grouped session's real
        // ask from an ungrouped one. Passing `None` is the conservative choice
        // for a HARD reservation gate: it can only ever narrow who this path
        // revalidates onto a reserved account, never widen it, so a reserved
        // account is never leaked to traffic this path cannot identify.
        let hard_ok = |account: &AccountRuntime| -> bool {
            Self::hard_ok(
                account,
                self.global_threshold,
                now,
                now_ms,
                is_fable,
                None,
                &reserved_groups,
            )
        };

        // Mirrors `select()`'s `keep_pin`: set to the OLD pin index when the pin
        // cannot serve THIS request but the ACCOUNT is alive, so the fallback below
        // serves elsewhere while re-writing the OLD index. Without it a Fable request
        // that reaches this path re-keys the session and the next Opus turn pays a
        // cold prefix — the same defect this file fixed in `select()`, one path over.
        let mut keep_pin: Option<usize> = None;
        // The pin's `rate_limited_until_ms` at the moment `keep_pin` was set — the
        // other half of the divert episode's identity (`select()`'s
        // `divert_until_ms`, mirrored here). Only meaningful when `keep_pin` is
        // `Some`; `divert_verdict` is never consulted otherwise.
        let mut pin_until_ms: i64 = 0;

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
                    .is_some_and(|a| Self::account_hard_ok(a, now_ms, None, &reserved_groups))
                {
                    keep_pin = Some(idx);
                    pin_until_ms = accounts
                        .get(idx)
                        .and_then(|a| a.rate_limited_until_ms)
                        .unwrap_or(0);
                }
            }
        }

        // Sticky-destination reuse for the FALLBACK path (design doc §4.4,
        // do-not #7: "not a second copy of the rule" — same ledger, same
        // `divert_verdict` predicate `select()`'s sticky overlay uses, one call
        // site here). Read the episode BEFORE the fallback's accounts lock is
        // taken below — the accounts lock PIN-HONOR held has already dropped
        // (its scope ended above), so this ledger read nests under neither
        // (§4.2 rule 1).
        //
        // `budget = 0`: stickiness never consults the budget (unit E wires the
        // real `divert_budget()` in for the blocking half at this same site).
        // Sticky-before-Block decision (documented in this unit's FINAL-REPORT):
        // this call site always offers Sticky first — `divert_verdict` itself
        // only returns `Block` once a caller passes a nonzero budget, which
        // this phase never does, so the "try Sticky before honouring Block"
        // question is moot AT budget=0, but the shape here is the one E's
        // budget wiring extends without a second rewrite: E swaps the `0`
        // for `self.divert_budget()` and adds a branch on `DivertVerdict::Block`
        // immediately after this match — sticky is already tried first because
        // it's a separate, unconditional arm of the same predicate.
        let sticky_pick: Option<usize> = keep_pin.and_then(|pin_idx| {
            let episode = affinity.and_then(|key| {
                self.divert_ledger
                    .lock()
                    .expect("divert ledger lock poisoned")
                    .get(&key)
                    .copied()
            });
            match divert_verdict(episode, pin_idx, pin_until_ms, 0) {
                DivertVerdict::Sticky(sticky) if !tried.contains(&sticky) => Some(sticky),
                _ => None,
            }
        });

        // (B) FALLBACK — least-utilized surviving account, throttled, then re-pinned.
        let via_sticky;
        let idx = {
            let mut accounts = self.accounts.write().expect("accounts lock poisoned");

            let sticky_usable = sticky_pick.filter(|&s| accounts.get(s).is_some_and(&hard_ok));
            via_sticky = sticky_usable.is_some();

            let idx = if let Some(sticky) = sticky_usable {
                sticky
            } else {
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
                best?
            };

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
                if via_sticky {
                    tracing::info!(
                        account = %account.name,
                        utilization = util,
                        is_fable,
                        "revalidation-serve (fallback, sticky): reusing this episode's already-warmed destination"
                    );
                } else {
                    tracing::info!(
                        account = %account.name,
                        utilization = util,
                        is_fable,
                        "revalidation-serve (fallback): whole fleet over soft threshold — serving least-utilized allowed account"
                    );
                }
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
            // Record this fallback serve in the episode ledger (§4.2 rule 3: written
            // only after the affinity guard above has dropped, its own critical
            // section, never nested under it or the accounts lock below). Only when
            // `keep_pin` was set — i.e. the pin stayed and this is genuinely a
            // divert, not a hard re-key onto a fresh home with no live hold to key
            // an episode on. Mirrors `select()`'s own record-divert guard.
            if let Some(pin_idx) = keep_pin {
                self.record_divert(key, pin_idx, pin_until_ms, idx, now_ms);
            }
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
    ///
    /// `group` is the SESSION's real requested group (`tcr run --group`, or
    /// `None`) — never the PREFER-narrowing override a pin-honoring call site
    /// passes to [`Self::eligible`]. A reservation is not a preference: this is
    /// the predicate that decides whether a warm pin survives, so it must see
    /// what the session actually asked for, not the bypass value the four
    /// pin/connection/sticky call sites in [`Manager::select_with_group`] pass
    /// to `eligible` to avoid re-litigating group PREFERENCE against an
    /// established pin. See [`Self::reserved_blocks`].
    pub(super) fn account_hard_ok(
        account: &AccountRuntime,
        now_ms: i64,
        group: Option<&str>,
        reserved: &HashSet<String>,
    ) -> bool {
        if Self::account_terminal_gate(account).is_some() {
            return false;
        }
        if Self::hold_outlives_cache(account, now_ms) {
            return false;
        }
        if Self::reserved_blocks(account, group, reserved) {
            return false;
        }
        true
    }

    /// Whether a RESERVATION (not a preference — see the bridge's "Semantics"
    /// section) holds `account` out for a request that asked for `group`.
    ///
    /// `A eligible iff g ∈ groups(A)` when the session asked for a specific `g`
    /// — reservation never adds a SECOND requirement on top of that match, so
    /// `Some(_)` always returns `false` here (the ordinary group-preference
    /// check already governs, wherever the caller applies it). Only an
    /// unrequested ask (`None`) — which includes the soft-fallback pass, which
    /// deliberately treats itself as unrequested for this purpose exactly as it
    /// does for group preference — is ever blocked, and then only if `account`
    /// carries ANY reserved group: an account in reserved `codereview` and
    /// plain `dev` stays reachable by `--group dev`, and is blocked for traffic
    /// that asked for nothing.
    pub(super) fn reserved_blocks(
        account: &AccountRuntime,
        group: Option<&str>,
        reserved: &HashSet<String>,
    ) -> bool {
        if group.is_some() {
            return false;
        }
        account.groups.iter().any(|g| reserved.contains(g))
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
        group: Option<&str>,
        reserved: &HashSet<String>,
    ) -> bool {
        Self::account_hard_ok(account, now_ms, group, reserved)
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

    /// Why the group-respecting pass found nothing — see [`GroupMiss`].
    ///
    /// Called ONLY from the soft fallback, which runs after that pass already
    /// returned `None`, so this scan costs nothing on a request that routes
    /// normally. Pure: takes the accounts slice the caller already holds a guard
    /// on, and reads no lock of its own.
    ///
    /// `control_excluded` is the index [`Self::select_with_group`] forced into
    /// its pool-`tried` set for THIS request — `Some` only for an inference
    /// request with a control account configured. Passed in rather than
    /// re-derived from `self.control()` so the answer here can never disagree
    /// with the exclusion that actually ran: the classifier's whole job is to
    /// name that exclusion when it is the reason.
    ///
    /// `tried` is the caller's ORIGINAL set, deliberately not the
    /// control-injected `pool_tried` — a member that is only in the set because
    /// of the injection must be attributed to [`GroupMiss::OnlyControl`], not
    /// silently counted as "already tried".
    #[allow(clippy::too_many_arguments)]
    pub(super) fn classify_group_miss(
        accounts: &[AccountRuntime],
        tried: &HashSet<usize>,
        control_excluded: Option<usize>,
        global_threshold: f64,
        pacing: &PacingConfig,
        now: OffsetDateTime,
        now_ms: i64,
        is_fable: bool,
        group: &str,
        reserved: &HashSet<String>,
    ) -> GroupMiss {
        let members: Vec<usize> = accounts
            .iter()
            .enumerate()
            .filter(|(_, account)| account.groups.iter().any(|label| label == group))
            .map(|(idx, _)| idx)
            .collect();
        if members.is_empty() {
            return GroupMiss::Unknown;
        }
        if members.iter().all(|idx| Some(*idx) == control_excluded) {
            return GroupMiss::OnlyControl;
        }
        // Would any member serve with the soft pacing gate lifted? If yes, pacing
        // really was the reason and the old message was right by accident. If no,
        // the members are hard-gated, and saying "paced" would be the same lie in
        // the other direction — which is exactly the defect this classifier exists
        // to end, so it must not be reintroduced here.
        let servable_unpaced = members.iter().any(|idx| {
            Some(*idx) != control_excluded
                && !tried.contains(idx)
                && accounts.get(*idx).is_some_and(|account| {
                    Self::eligible(
                        account,
                        global_threshold,
                        pacing,
                        false,
                        now,
                        now_ms,
                        is_fable,
                        Some(group),
                        Some(group),
                        reserved,
                    )
                })
        });
        if servable_unpaced {
            GroupMiss::Paced
        } else {
            GroupMiss::AllGated
        }
    }

    /// `reserved_group` is the SESSION's real requested group, used ONLY for
    /// the reservation gate below — see [`Self::reserved_blocks`] and
    /// [`Self::account_hard_ok`]'s doc-comment for why this is a separate
    /// value from `group` (the PREFER-narrowing argument): the four
    /// pin/connection/sticky call sites in [`Manager::select_with_group`] pass
    /// `group: None` to bypass PREFER re-narrowing against an established pin,
    /// but must still pass the session's ACTUAL ask here, because a
    /// reservation is not a preference. Every other caller passes the same
    /// value for both.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn eligible(
        account: &AccountRuntime,
        global_threshold: f64,
        pacing: &PacingConfig,
        respect_pacing: bool,
        now: OffsetDateTime,
        now_ms: i64,
        is_fable: bool,
        group: Option<&str>,
        reserved_group: Option<&str>,
        reserved: &HashSet<String>,
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
        // Reservation (not a preference — see this fn's `reserved_group` doc
        // above and [`Self::reserved_blocks`]): a hard block, independent of
        // the PREFER narrowing below, so it applies even at the four call
        // sites that pass `group: None` to skip that narrowing.
        if Self::reserved_blocks(account, reserved_group, reserved) {
            return false;
        }
        // `--group` PREFER routing (Phase 1: no `--only`, so this never hard-blocks
        // an account on its own — it only narrows the CALLER's chosen pass, since
        // the fallback pass always calls this with `group: None`). A HARD-looking
        // early return by design, positioned after the model-block gate and before
        // the SOFT pacing gate below, matching this account's true selectivity: it
        // filters exactly like a hard gate WITHIN one pass. `account_gate` /
        // `account_hard_ok` are untouched — Phase 1 never reports a group as the
        // reason an account is hard-blocked.
        if let Some(g) = group {
            if !account.groups.iter().any(|acct_group| acct_group == g) {
                return false;
            }
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
    /// `group` is the SESSION's real requested group — same argument
    /// [`Self::account_hard_ok`] takes, for the same reason: this display-side
    /// mirror must agree with the routing predicate on which accounts a
    /// RESERVED gate actually blocks. Every current caller ([`Manager::snapshot`],
    /// [`Manager::retry_after_hint`]) reports the fleet as unrequested traffic
    /// sees it, so both pass `None`.
    pub(super) fn account_gate(
        account: &AccountRuntime,
        threshold: f64,
        now: OffsetDateTime,
        now_ms: i64,
        is_fable: bool,
        group: Option<&str>,
        reserved: &HashSet<String>,
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
        // Reservation never self-frees on its own (only an operator's `tcr
        // group unreserve` clears it) — `None` clear-instant, mirroring the
        // terminal gates above; kept as an ordinary `gates` entry rather than
        // an early return because it is a fact about THIS request's group ask,
        // not about the credential — see `Self::reserved_blocks`.
        if Self::reserved_blocks(account, group, reserved) {
            gates.push((GateReason::Reserved, None));
        }
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
    /// `(priority, reset-urgency bucket, last_selected_seq, soonest weekly reset)`
    /// — the pre-pacing LRU order, now bucketed by reset urgency
    /// ([`Self::reset_urgency_tier`]) and additionally skipping any account the
    /// soft pacing gate holds out.
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

    /// The reset-urgency bucket `account` falls into — its governing weekly
    /// reset, floored into [`Manager::reset_urgency_tier_ms`]-wide buckets
    /// measured forward from `now`. **Ascending: a lower bucket is spent
    /// first**, because unused weekly quota is worth nothing once its window
    /// resets.
    ///
    /// Why a bucket and not the reset instant itself: a reset is a
    /// millisecond-precision instant that essentially never ties, so ranking on
    /// it directly would retire the `last_selected_seq` tiebreak entirely and
    /// park every unpinned request on the single soonest-resetting account until
    /// it gated. That is the same deterministic pin [`Manager::select`]'s
    /// doc-comment rejects quota-headroom ordering for, and this pool pick
    /// carries no `in_flight` term to dilute it (only
    /// [`Manager::pick_least_loaded`] does), so a burst would land undiluted.
    /// Bucketing re-creates the tie: at the default 24h, the live fleet's four
    /// soonest-resetting accounts share one bucket and still fan out inside it.
    ///
    /// Two arms deliberately return a value that is NOT a bucket index:
    ///  - **no known live reset → [`i64::MIN`]**, sorting the account ahead of
    ///    every bucket. This preserves the pre-tier "unknown quota sorts first,
    ///    probe it" contract byte-for-byte — the same reason the raw-`reset`
    ///    term below maps `None` to [`i128::MIN`]. It covers both a genuinely
    ///    unmeasured window and one whose reset has already elapsed;
    ///    [`crate::quota::QuotaWindow::live_reset`] collapses those two into
    ///    `None` and this does not try to tell them apart.
    ///  - **feature disabled (`reset_urgency_tier_ms == 0`) → a constant `0`**,
    ///    which ties for every account and hands the decision straight back to
    ///    the LRU tick. That restores the pre-tier ordering exactly, including
    ///    the unknown-reset-first behaviour, which survives on the raw-`reset`
    ///    term the key still carries.
    ///
    /// Self-rotating by construction: spending an account does not move its
    /// reset, but crossing that reset starts a fresh window ~7 days out, which
    /// drops the account to the back of the bucket order. No account starves —
    /// each one's window comes due in turn.
    fn reset_urgency_tier(&self, account: &AccountRuntime, now: OffsetDateTime) -> i64 {
        if self.reset_urgency_tier_ms == 0 {
            return 0;
        }
        let Some(reset) = account.quota.governing_weekly_reset(now) else {
            return i64::MIN;
        };
        // `live_reset` only ever hands back a FUTURE instant, so this difference
        // is positive in practice; `max(0)` keeps a clock that stepped backwards
        // between the two reads from producing a negative bucket that would
        // outrank a genuinely urgent account. `try_from` saturates rather than
        // wrapping for the same reason.
        let remaining_ms = i64::try_from((reset - now).whole_milliseconds())
            .unwrap_or(i64::MAX)
            .max(0);
        remaining_ms / self.reset_urgency_tier_ms
    }

    #[allow(clippy::too_many_arguments)]
    fn pick_eligible(
        &self,
        accounts: &[AccountRuntime],
        tried: &HashSet<usize>,
        now: OffsetDateTime,
        now_ms: i64,
        is_fable: bool,
        respect_pacing: bool,
        group: Option<&str>,
    ) -> Option<usize> {
        let reserved_groups = self.reserved_groups();
        let mut best: Option<usize> = None;
        let mut best_key: Option<(i64, i64, u64, i128)> = None;
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
                group,
                group, // this pass's own real ask — see `eligible`'s `reserved_group` doc
                &reserved_groups,
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
                        group,
                        group,
                        &reserved_groups,
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
            let key = (
                account.priority,
                self.reset_urgency_tier(account, now),
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

    /// The least-loaded servable account not in `tried`, IGNORING pacing (the soft
    /// fallback pass). Sort key ascending: `(in_flight, priority, reset-urgency
    /// bucket, last_selected_seq, weekly reset)` — least concurrent load first,
    /// then the normal bucketed LRU order. `in_flight` stays ahead of the urgency
    /// bucket deliberately: this pass exists to find the COOLEST account when the
    /// fleet is all-paced, and letting reset urgency outrank live load would send
    /// the overflow straight back onto the busy account the fallback is trying to
    /// relieve. All
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
        group: Option<&str>,
    ) -> Option<usize> {
        let reserved_groups = self.reserved_groups();
        let mut best: Option<usize> = None;
        let mut best_key: Option<(u32, i64, i64, u64, i128)> = None;
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
                group,
                group,
                &reserved_groups,
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
                self.reset_urgency_tier(account, now),
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

    /// Write half of [`Manager::divert_ledger`] (§4.1/§4.2): record `dest` as
    /// a destination this session was diverted to, in the episode identified
    /// by `(pin, until_ms)`. Called only AFTER the affinity guard for this
    /// request has already dropped — never nested under it or the accounts
    /// lock (§4.2 rule 3; this method takes only the ledger's own lock).
    ///
    /// Reset is structural, not timed (§4.1): if the stored entry's
    /// `(pin, until_ms)` no longer matches, it is a stale episode and is
    /// overwritten from scratch rather than merged into. On a genuinely new
    /// episode (or after that reset) the first `dest` recorded becomes
    /// [`DivertEpisode::sticky`]; every later `dest` in the same episode only
    /// grows the `destinations` mask — `sticky` stays the FIRST destination,
    /// by design (§4.1: "so the sticky overlay has a single preferred index
    /// without scanning the mask").
    ///
    /// Bounded by [`DIVERT_LEDGER_CAP`] + LRU-by-`until_ms` eviction, mirroring
    /// `select()`'s `AFFINITY_CAP` pattern (`:664-675`) on this separate,
    /// never-persisted map.
    pub(super) fn record_divert(
        &self,
        session_key: u64,
        pin: usize,
        until_ms: i64,
        dest: usize,
        _now_ms: i64,
    ) {
        // The mask is a u64; `DivertEpisode::destinations`' own doc-comment
        // caps this at 64 accounts, "nowhere near" the fleet size. Fail safe
        // rather than panic on an out-of-range shift: the episode simply
        // isn't recorded for this one destination, so sticky never offers it
        // (it would have failed the ordinary eligibility gate anyway once the
        // fleet ever did reach 64 accounts).
        let Some(bit) = 1u64.checked_shl(dest as u32) else {
            return;
        };
        let mut ledger = self
            .divert_ledger
            .lock()
            .expect("divert ledger lock poisoned");
        let entry = ledger
            .entry(session_key)
            .and_modify(|e| {
                if e.pin != pin || e.until_ms != until_ms {
                    *e = DivertEpisode {
                        pin,
                        until_ms,
                        destinations: 0,
                        sticky: dest,
                    };
                }
            })
            .or_insert(DivertEpisode {
                pin,
                until_ms,
                destinations: 0,
                sticky: dest,
            });
        if entry.destinations == 0 {
            entry.sticky = dest;
        }
        entry.destinations |= bit;

        if ledger.len() > DIVERT_LEDGER_CAP {
            if let Some((&oldest, _)) = ledger.iter().min_by_key(|(_, e)| e.until_ms) {
                if oldest != session_key {
                    ledger.remove(&oldest);
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

#[cfg(test)]
mod divert_verdict_tests {
    //! Pure-function tests for [`divert_verdict`] (the divert-budget design notes
    //! §4.4) — all three arms, driven by explicit `budget` values, exactly as
    //! the brief requires. These are a SELF-authored oracle (this module wrote
    //! both the fix and the test), so they cap at σ2: sensitivity to the
    //! change is not independence. Unit F's replay over the retained logs
    //! (§6) is the independent oracle that can promote this past σ2.
    use super::*;

    fn ep(pin: usize, until_ms: i64, destinations: u64, sticky: usize) -> DivertEpisode {
        DivertEpisode {
            pin,
            until_ms,
            destinations,
            sticky,
        }
    }

    #[test]
    fn no_episode_is_fresh() {
        assert_eq!(divert_verdict(None, 0, 1_000, 1), DivertVerdict::Fresh);
    }

    #[test]
    fn mismatched_pin_is_fresh_even_with_a_recorded_episode() {
        let episode = ep(0, 1_000, 0b10, 1);
        // Same deadline, different pin: a different account is now held, so
        // this is not a continuation of the same episode.
        assert_eq!(
            divert_verdict(Some(episode), 2, 1_000, 1),
            DivertVerdict::Fresh
        );
    }

    #[test]
    fn mismatched_deadline_is_fresh_even_with_the_same_pin() {
        let episode = ep(0, 1_000, 0b10, 1);
        // Same pin, different deadline: a NEW hold on the same account — the
        // whole point of keying the episode on `(pin, until_ms)` rather than
        // the pin alone (§4.1).
        assert_eq!(
            divert_verdict(Some(episode), 0, 2_000, 1),
            DivertVerdict::Fresh
        );
    }

    #[test]
    fn matching_episode_under_unlimited_budget_offers_sticky() {
        let episode = ep(0, 1_000, 0b10, 1);
        // budget = 0 is the kill switch (§4.6): never Block, regardless of
        // how many distinct destinations are already recorded.
        assert_eq!(
            divert_verdict(Some(episode), 0, 1_000, 0),
            DivertVerdict::Sticky(1)
        );
    }

    #[test]
    fn matching_episode_under_a_nonzero_budget_offers_sticky() {
        let episode = ep(0, 1_000, 0b10, 1); // one destination recorded
        assert_eq!(
            divert_verdict(Some(episode), 0, 1_000, 2),
            DivertVerdict::Sticky(1)
        );
    }

    #[test]
    fn matching_episode_at_the_budget_cap_blocks() {
        let episode = ep(0, 1_000, 0b10, 1); // count_ones() == 1
        assert_eq!(
            divert_verdict(Some(episode), 0, 1_000, 1),
            DivertVerdict::Block
        );
    }

    #[test]
    fn matching_episode_over_the_budget_cap_blocks() {
        // Two distinct destinations already recorded (bits 1 and 3), budget 1.
        let episode = ep(0, 1_000, 0b1010, 1);
        assert_eq!(
            divert_verdict(Some(episode), 0, 1_000, 1),
            DivertVerdict::Block
        );
    }
}

/// Unit D's gate (the divert-budget design notes §4.4, do-not #7): does
/// `select_revalidation`'s FALLBACK arm (`:934-` in this file) consult the SAME
/// episode ledger and `divert_verdict` predicate B wired into `select()`, or
/// does it still spend a fresh least-utilised account every time?
///
/// These are full `Manager` integration tests, so the helpers below are
/// deliberately minimal, standalone re-implementations of `mod.rs`'s
/// `mod tests` fixtures (`account`/`config_with`/`build_manager`) rather than
/// imports — this unit's brief is `src/manager/select.rs` ONLY, and those
/// helpers are private to `crate::manager::tests`, a sibling module this file
/// cannot reach into. `LiveUsageProber`/`LiveWarmer`/`NoRefresh` are the real,
/// non-test, already-`pub` production types: `select_revalidation` never
/// calls `probe()`/`warm()`, so wiring the live (never-invoked) impls in is
/// safe and avoids yet another local test double.
///
/// Self-authored oracle — σ2. Sensitivity ("fails without the fix, passes
/// with it") is demonstrated in this unit's FINAL-REPORT via a break/restore
/// cycle, not by this file, per `CLAUDE.md` § "Verifying a change".
#[cfg(test)]
mod revalidation_sticky_tests {
    use super::*;
    use crate::config::{Account, ProxyConfig};
    use crate::oauth::NoRefresh;
    use crate::probe::LiveUsageProber;
    use crate::warmer::LiveWarmer;
    use std::collections::HashSet;

    fn account(name: &str) -> Account {
        Account {
            name: name.to_string(),
            account_type: "oauth".to_string(),
            account_uuid: None,
            org_uuid: None,
            org_name: None,
            access_token: format!("at-{name}"),
            refresh_token: Some(format!("rt-{name}")),
            expires_at: Some(crate::now_ms() + 3_600_000),
            priority: Some(0),
            switch_threshold: None,
            disabled: None,
            groups: None,
            extra: serde_json::Map::new(),
        }
    }

    fn config_with(accounts: Vec<Account>) -> Config {
        Config {
            quarantined_accounts: Vec::new(),
            migrated_legacy_throttle: false,
            proxy: ProxyConfig::default(),
            upstream: "https://api.anthropic.com".to_string(),
            switch_threshold: 0.90,
            pacing: PacingConfig::default(),
            account_throttle: ThrottleConfig::default(),
            fleet_throttle: ThrottleConfig::default(),
            lock_account: None,
            control_account: None,
            control_reserve: 0.05,
            reset_urgency_tier_hours: 24,
            http1_only: false,
            accounts,
            group_settings: HashMap::new(),
            pricing: Default::default(),
            usage_retention_days: 90,
            extra: serde_json::Map::new(),
        }
    }

    fn build_manager(config: Config) -> Arc<Manager> {
        Manager::new(
            config,
            Arc::new(NoRefresh),
            Arc::new(LiveUsageProber::new()),
            Arc::new(LiveWarmer::new()),
            None,
        )
    }

    /// The headline claim: after `select()` diverts and records this
    /// episode's sticky destination in `Manager::divert_ledger`, a request
    /// that reaches `select_revalidation`'s FALLBACK arm for the SAME
    /// episode must reuse that same destination — not the fleet's
    /// least-utilised account, which (three accounts, mirroring B's own
    /// anti-churn-defeating fixture) would otherwise be the OTHER alternate.
    #[test]
    fn revalidation_fallback_reuses_the_episode_sticky_destination() {
        let manager = build_manager(config_with(vec![
            account("home"),
            account("alt1"),
            account("alt2"),
        ]));
        let now = OffsetDateTime::now_utc();
        let key = 636_363u64;

        let home = manager
            .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
            .expect("an account is eligible");
        // A long hold: outlives the soft-wait ceiling in the real ladder
        // (§1.2), which is exactly the case this design's §1.2 "revalidation
        // leak" is about — but `select_revalidation` itself takes no ceiling,
        // so any hold that keeps `home` HARD-ok-but-rate-limited exercises
        // the same PIN-HONOR-falls-through-to-FALLBACK path.
        manager.mark_rate_limited(home, 120);

        // `select()` diverts and records the episode's sticky destination —
        // this is the ledger entry `select_revalidation` must now consult.
        let sticky_dest = manager
            .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
            .expect("an un-held alternate serves this divert");
        assert_ne!(sticky_dest, home);

        // `select_revalidation`'s FALLBACK must reuse `sticky_dest`, not the
        // OTHER un-held alternate — the assertion three accounts (not two)
        // makes real, per B's own fixture comment.
        let served = manager
            .select_revalidation(&HashSet::new(), now, None, Some(key))
            .expect("an un-held alternate serves this revalidation request");
        assert_eq!(
            served, sticky_dest,
            "select_revalidation's FALLBACK must consult the same episode \
             ledger `select()` populated, reusing its sticky destination \
             instead of spending a fresh least-utilised account"
        );

        // The episode ledger must record the FALLBACK's own serve too (§4.2:
        // "every divert records, not just sticky ones"), same destination,
        // still one distinct destination.
        let episode = manager
            .divert_ledger
            .lock()
            .expect("divert ledger lock poisoned")
            .get(&key)
            .copied()
            .expect("episode recorded");
        assert_eq!(episode.pin, home);
        assert_eq!(episode.sticky, sticky_dest);
        assert_eq!(
            episode.destinations.count_ones(),
            1,
            "select() and select_revalidation reusing the SAME destination \
             is one distinct destination, not two"
        );
    }
}

/// Replay of a divert trace observed in a live deployment: four consecutive
/// diverts for one session pinned to a rate-limited account, landing on three
/// distinct destinations. Session key and account names are fabricated here —
/// the shape is what carries over, and this repository is public. Drives the
/// REAL `select()` across repeated calls rather than `divert_verdict` in
/// isolation (that predicate is already covered by `divert_tests` above).
///
/// The bounce this replay exists to catch was observable because the build
/// carrying `select()`'s sticky overlay (`:688-716`) had not yet booted when
/// those diverts fired; no `divert-sticky` line had been emitted at all at
/// the time this test was written.
///
/// Self-authored oracle — σ2, same cap as `revalidation_sticky_tests` above
/// and for the same reason (the same change added both the overlay's test
/// coverage and this replay). Neutralizing the overlay turns this test red
/// with the destination-bounce it asserts against, which shows sensitivity to
/// the change but not independence from it. A genuinely independent oracle
/// would be unit F's replay harness reading retained log lines directly
/// (§6), which this test does not have access to.
#[cfg(test)]
mod sticky_divert_replay_tests {
    use super::*;
    use crate::config::{Account, ProxyConfig};
    use crate::oauth::NoRefresh;
    use crate::probe::LiveUsageProber;
    use crate::warmer::LiveWarmer;
    use std::collections::HashSet;

    fn account(name: &str) -> Account {
        Account {
            name: name.to_string(),
            account_type: "oauth".to_string(),
            account_uuid: None,
            org_uuid: None,
            org_name: None,
            access_token: format!("at-{name}"),
            refresh_token: Some(format!("rt-{name}")),
            expires_at: Some(crate::now_ms() + 3_600_000),
            priority: Some(0),
            switch_threshold: None,
            disabled: None,
            groups: None,
            extra: serde_json::Map::new(),
        }
    }

    fn config_with(accounts: Vec<Account>) -> Config {
        Config {
            quarantined_accounts: Vec::new(),
            migrated_legacy_throttle: false,
            proxy: ProxyConfig::default(),
            upstream: "https://api.anthropic.com".to_string(),
            switch_threshold: 0.90,
            pacing: PacingConfig::default(),
            account_throttle: ThrottleConfig::default(),
            fleet_throttle: ThrottleConfig::default(),
            lock_account: None,
            control_account: None,
            control_reserve: 0.05,
            reset_urgency_tier_hours: 24,
            http1_only: false,
            accounts,
            group_settings: HashMap::new(),
            pricing: Default::default(),
            usage_retention_days: 90,
            extra: serde_json::Map::new(),
        }
    }

    fn build_manager(config: Config) -> Arc<Manager> {
        Manager::new(
            config,
            Arc::new(NoRefresh),
            Arc::new(LiveUsageProber::new()),
            Arc::new(LiveWarmer::new()),
            None,
        )
    }

    /// Four `select()` calls in a row for one pinned, rate-limited session —
    /// the exact shape of the retained trace (four diverts, `pin-tried`
    /// every time because the pin is already in the request's own `tried`
    /// set after the first attempt upstream, matching the real log's
    /// `reason=pin-tried` on all four lines). Every call after the first
    /// must return the SAME alternate: that reuse is the entire point of
    /// PR #110, and its absence (a fresh least-utilised pick every time) is
    /// the bounce the production trace shows.
    #[test]
    fn four_diverts_in_one_episode_reuse_the_same_alternate() {
        let manager = build_manager(config_with(vec![
            account("alice"),
            account("bob"),
            account("carol"),
        ]));
        let now = OffsetDateTime::now_utc();
        let key = 42_u64;

        // Initial pin, mirroring the trace: the session pins to `alice`,
        // which then gets rate-limited.
        let pin = manager
            .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
            .expect("an account is eligible for the initial pin");
        manager.mark_rate_limited(pin, 120);

        // Every one of the four replayed diverts hits the SAME upstream
        // gate the log's `reason=pin-tried` names: the pin already failed
        // this request (it is in `tried`), so `select()` diverts and keeps
        // the pin rather than re-keying the session.
        let mut tried = HashSet::new();
        tried.insert(pin);

        let first = manager
            .select(&tried, now, None, Some(key), "/v1/messages", None)
            .expect("an alternate is eligible for the first divert");
        assert_ne!(
            first, pin,
            "the first divert must land on an alternate, not the held pin"
        );

        for n in 2..=4 {
            let dest = manager
                .select(&tried, now, None, Some(key), "/v1/messages", None)
                .unwrap_or_else(|| panic!("an alternate is eligible for divert #{n}"));
            assert_eq!(
                dest, first,
                "divert #{n} must reuse the episode's sticky destination {first}, \
                 not bounce to a different alternate — this is the exact shape \
                 of the production trace this test replays"
            );
        }

        // The episode ledger backs the reuse: one recorded episode, keyed
        // on this session, with exactly one distinct destination touched
        // across all four diverts.
        let episode = manager
            .divert_ledger
            .lock()
            .expect("divert ledger lock poisoned")
            .get(&key)
            .copied()
            .expect("episode recorded");
        assert_eq!(episode.pin, pin);
        assert_eq!(episode.sticky, first);
        assert_eq!(
            episode.destinations.count_ones(),
            1,
            "four diverts reusing the same destination is one distinct \
             destination, not four"
        );

        // Episode reset is structural, not timed (§4.1): a NEW rate-limit
        // deadline on the same pin changes the episode's identity
        // `(pin, until_ms)`, so the next divert is `Fresh` again and is free
        // to land on any eligible alternate — including a different one.
        // With `first` now the most-recently-selected account (its
        // `last_selected_seq` is ahead of the untouched third account), the
        // LRU-ordered normal pick this fixture exercises actually lands on
        // the OTHER alternate, making the "allowed to change" claim
        // concrete rather than vacuous.
        manager.mark_rate_limited(pin, 200);
        let after_reset = manager
            .select(&tried, now, None, Some(key), "/v1/messages", None)
            .expect("an alternate is eligible after the episode resets");
        assert_ne!(
            after_reset, first,
            "a new rate-limit deadline is a new episode (§4.1) — the sticky \
             destination must be free to change, and with an untouched \
             third account in the fleet the LRU pick actually does change it"
        );
    }
}

/// Reserved-group semantics test #5 (`docs/plans/reserved-groups-bridge.md`):
/// `eligible` and `account_gate` must AGREE on the RESERVED gate over many
/// account/reserved-set/ask combinations — a property test over the two
/// predicates, not two example tests that can drift apart the way
/// `gate_and_hard_ok_agree_on_every_variant` (`manager/mod.rs`) already
/// guards the OTHER eight `GateReason` variants. Pure-predicate, so it needs
/// no `Manager`/config plumbing — just [`AccountRuntime`] and the same
/// `reserved_blocks` these two predicates both call.
#[cfg(test)]
mod reserved_gate_agreement_tests {
    use super::*;
    use crate::config::{Account, PacingConfig};

    fn account_runtime(groups: &[&str]) -> AccountRuntime {
        AccountRuntime::from_config(
            &Account {
                name: "probe".to_string(),
                account_type: "oauth".to_string(),
                account_uuid: None,
                org_uuid: None,
                org_name: None,
                access_token: "at-probe".to_string(),
                refresh_token: None,
                expires_at: Some(crate::now_ms() + 3_600_000),
                priority: Some(0),
                switch_threshold: None,
                disabled: None,
                groups: Some(groups.iter().map(|g| g.to_string()).collect()),
                extra: serde_json::Map::new(),
            },
            false,
        )
    }

    /// Unrequested traffic (`group: None`, both `eligible`'s narrowing AND
    /// reserved-check args): `eligible` and `account_gate` must agree with
    /// `reserved_blocks` for every combination of an account's groups against
    /// every reserved set — the account is otherwise perfectly healthy, so
    /// RESERVED is the only thing that can hold it out.
    #[test]
    fn agree_on_unrequested_traffic_across_group_and_reserved_combinations() {
        let now = OffsetDateTime::now_utc();
        let now_ms = odt_to_ms(now);
        let pacing = PacingConfig::default();

        let group_combos: [&[&str]; 4] = [&[], &["codereview"], &["dev"], &["codereview", "dev"]];
        let reserved_combos: [&[&str]; 4] = [
            &[],
            &["codereview"],
            &["dev"],
            &["codereview", "dev", "burst"],
        ];

        for groups in group_combos {
            let account = account_runtime(groups);
            for reserved_list in reserved_combos {
                let reserved: HashSet<String> =
                    reserved_list.iter().map(|s| s.to_string()).collect();
                let blocked = Manager::reserved_blocks(&account, None, &reserved);

                let elig = Manager::eligible(
                    &account, 0.90, &pacing, false, now, now_ms, false, None, None, &reserved,
                );
                assert_eq!(
                    elig, !blocked,
                    "groups={groups:?} reserved={reserved_list:?}: eligible \
                     disagrees with reserved_blocks on an otherwise-healthy account"
                );

                let (gate, _) =
                    Manager::account_gate(&account, 0.90, now, now_ms, false, None, &reserved);
                assert_eq!(
                    gate == GateReason::Reserved,
                    blocked,
                    "groups={groups:?} reserved={reserved_list:?}: account_gate \
                     disagrees with reserved_blocks"
                );

                let hard_ok = Manager::account_hard_ok(&account, now_ms, None, &reserved);
                assert_eq!(
                    hard_ok, !blocked,
                    "groups={groups:?} reserved={reserved_list:?}: account_hard_ok \
                     disagrees with reserved_blocks — a reserved pin would survive \
                     the hard-gate re-test"
                );
            }
        }
    }

    /// An explicit ask for one of the account's own groups always clears the
    /// reserved gate in `eligible`, regardless of what else is reserved — the
    /// spec's "session asked for group g -> eligible iff g in groups(A))"
    /// branch, which never adds a second requirement on top of the match.
    #[test]
    fn an_explicit_matching_ask_is_never_reserved_blocked() {
        let now = OffsetDateTime::now_utc();
        let now_ms = odt_to_ms(now);
        let pacing = PacingConfig::default();
        let account = account_runtime(&["codereview", "dev"]);
        let reserved: HashSet<String> = ["codereview".to_string(), "dev".to_string()].into();

        for ask in ["codereview", "dev"] {
            assert!(
                Manager::eligible(
                    &account,
                    0.90,
                    &pacing,
                    false,
                    now,
                    now_ms,
                    false,
                    Some(ask),
                    Some(ask),
                    &reserved,
                ),
                "ask={ask}: an explicit ask for the account's own (reserved) group \
                 must clear the reserved gate"
            );
            let (gate, _) =
                Manager::account_gate(&account, 0.90, now, now_ms, false, Some(ask), &reserved);
            assert_ne!(gate, GateReason::Reserved, "ask={ask}: account_gate agrees");
        }
    }
}

/// Group-miss classification (`Manager::classify_group_miss`): the log line at
/// the soft fallback must name the constraint that was actually in play.
///
/// The arm that earns the module is [`GroupMiss::OnlyControl`]. Before this
/// existed, a group whose only member was the control account fell back on every
/// single request and the log said "under pacing" — on a fleet where pacing was
/// unconfigured. These tests pin each cause to its own answer so the message can
/// never again describe a constraint that was switched off.
#[cfg(test)]
mod group_miss_tests {
    use super::*;
    use crate::config::{Account, PacingConfig};

    fn account(name: &str, groups: &[&str], disabled: bool) -> AccountRuntime {
        AccountRuntime::from_config(
            &Account {
                name: name.to_string(),
                account_type: "oauth".to_string(),
                account_uuid: None,
                org_uuid: None,
                org_name: None,
                access_token: format!("at-{name}"),
                refresh_token: None,
                expires_at: Some(crate::now_ms() + 3_600_000),
                priority: Some(0),
                switch_threshold: None,
                disabled: disabled.then_some(true),
                groups: Some(groups.iter().map(|g| g.to_string()).collect()),
                extra: serde_json::Map::new(),
            },
            false,
        )
    }

    fn classify(
        accounts: &[AccountRuntime],
        control_excluded: Option<usize>,
        pacing: &PacingConfig,
        group: &str,
    ) -> GroupMiss {
        let now = OffsetDateTime::now_utc();
        Manager::classify_group_miss(
            accounts,
            &HashSet::new(),
            control_excluded,
            0.95,
            pacing,
            now,
            odt_to_ms(now),
            false,
            group,
            &HashSet::new(),
        )
    }

    /// The live defect, in one assertion: `research` holds exactly one account
    /// and that account is the control one, which inference never picks. The
    /// group is not busy and not paced — it is permanently unroutable, and the
    /// reason must say so.
    #[test]
    fn a_group_holding_only_the_control_account_is_named_control_account_only() {
        let accounts = [
            account("gil@example.com", &["research"], false),
            account("worker@example.com", &[], false),
        ];
        let miss = classify(&accounts, Some(0), &PacingConfig::default(), "research");
        assert_eq!(miss, GroupMiss::OnlyControl, "{miss:?}");
        assert_eq!(miss.as_str(), "control-account-only");
        assert!(
            miss.explain().contains("control account"),
            "the sentence an operator reads must name the control account: {}",
            miss.explain()
        );
    }

    /// Same fleet, same group — but the request is NOT inference, so nothing
    /// excluded the control account and the group is perfectly routable. Guards
    /// against reporting `OnlyControl` from group shape alone.
    #[test]
    fn the_same_group_is_not_control_blocked_when_nothing_was_excluded() {
        let accounts = [
            account("gil@example.com", &["research"], false),
            account("worker@example.com", &[], false),
        ];
        assert_ne!(
            classify(&accounts, None, &PacingConfig::default(), "research"),
            GroupMiss::OnlyControl
        );
    }

    /// A label no account carries is its own cause — a typo or a label removed
    /// from every account, not a busy group.
    #[test]
    fn a_label_no_account_carries_is_unknown() {
        let accounts = [account("worker@example.com", &["dev"], false)];
        assert_eq!(
            classify(&accounts, None, &PacingConfig::default(), "resarch"),
            GroupMiss::Unknown
        );
    }

    /// A second, non-control member exists, so the group is not control-blocked
    /// — but it is disabled, which is a hard gate and not pacing.
    #[test]
    fn a_hard_gated_member_is_unavailable_not_paced() {
        let accounts = [
            account("gil@example.com", &["research"], false),
            account("worker@example.com", &["research"], true),
        ];
        assert_eq!(
            classify(&accounts, Some(0), &PacingConfig::default(), "research"),
            GroupMiss::AllGated
        );
    }

    /// The one case the old hardcoded message was right about: pacing is
    /// configured, the member is healthy, and it is inside its min-spacing
    /// window. Pacing keeps its name — the fix is not to stop saying "paced",
    /// it is to stop saying it when pacing is off.
    #[test]
    fn a_healthy_member_inside_the_spacing_window_is_paced() {
        let pacing = PacingConfig {
            max_in_flight_per_account: None,
            min_spacing_ms: Some(60_000),
        };
        let mut accounts = [account("worker@example.com", &["research"], false)];
        accounts[0].last_served_ms = crate::now_ms();
        assert_eq!(
            classify(&accounts, None, &pacing, "research"),
            GroupMiss::Paced
        );
    }
}

/// Tests for the reset-urgency bucket ([`Manager::reset_urgency_tier`]) — the
/// second ranking key inside a priority tier.
///
/// The headline claim every test here circles: **an account whose weekly
/// headroom expires sooner is spent ahead of one that was selected less
/// recently**, because unused weekly quota is worth nothing after its reset.
/// That is a deliberate inversion of the pre-tier LRU order, so the discriminating
/// test below is built to FAIL on the pre-tier code — it stamps the urgent
/// account as the most-recently-selected one, which is exactly the account plain
/// LRU sorts last.
#[cfg(test)]
mod reset_urgency_tests {
    use super::*;
    use crate::config::{Account, ProxyConfig};
    use crate::oauth::NoRefresh;
    use crate::probe::LiveUsageProber;
    use crate::quota::QuotaWindow;
    use crate::warmer::LiveWarmer;
    use std::collections::HashSet;

    fn account(name: &str) -> Account {
        Account {
            name: name.to_string(),
            account_type: "oauth".to_string(),
            account_uuid: None,
            org_uuid: None,
            org_name: None,
            access_token: format!("at-{name}"),
            refresh_token: Some(format!("rt-{name}")),
            expires_at: Some(crate::now_ms() + 3_600_000),
            priority: Some(0),
            switch_threshold: None,
            disabled: None,
            groups: None,
            extra: serde_json::Map::new(),
        }
    }

    fn config_with(accounts: Vec<Account>, tier_hours: u32) -> Config {
        Config {
            quarantined_accounts: Vec::new(),
            migrated_legacy_throttle: false,
            proxy: ProxyConfig::default(),
            upstream: "https://api.anthropic.com".to_string(),
            switch_threshold: 0.90,
            pacing: PacingConfig::default(),
            account_throttle: ThrottleConfig::default(),
            fleet_throttle: ThrottleConfig::default(),
            lock_account: None,
            control_account: None,
            control_reserve: 0.05,
            reset_urgency_tier_hours: tier_hours,
            http1_only: false,
            accounts,
            group_settings: HashMap::new(),
            pricing: Default::default(),
            usage_retention_days: 90,
            extra: serde_json::Map::new(),
        }
    }

    fn build_manager(config: Config) -> Arc<Manager> {
        Manager::new(
            config,
            Arc::new(NoRefresh),
            Arc::new(LiveUsageProber::new()),
            Arc::new(LiveWarmer::new()),
            None,
        )
    }

    /// Give account `idx` a weekly window resetting `hours` from `now` at a
    /// utilization well under the fixture's 0.90 threshold, so the account is
    /// ranked but never gated — this suite is about ORDER, and an account held
    /// out by quota would prove nothing about it.
    fn set_weekly(manager: &Manager, idx: usize, hours: i64, now: OffsetDateTime) {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[idx].quota.seven_day = Some(QuotaWindow {
            utilization: 0.10,
            reset: Some(now + time::Duration::hours(hours)),
        });
    }

    fn set_seq(manager: &Manager, idx: usize, seq: u64) {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[idx].last_selected_seq = seq;
    }

    fn pick(manager: &Manager, now: OffsetDateTime) -> usize {
        manager
            .select(&HashSet::new(), now, None, None, "/v1/messages", None)
            .expect("an account is eligible")
    }

    /// **The discriminating test.** Two accounts in one priority tier: `urgent`
    /// resets in 2h (bucket 0 at the default 24h width), `roomy` in 100h
    /// (bucket 4). `urgent` is stamped as the MOST recently selected account and
    /// `roomy` as never-selected, so pre-tier LRU would pick `roomy` — this test
    /// fails on the old ordering, which is the point of it.
    ///
    /// Mirrors the live fleet that motivated the change (measured 2026-08-26):
    /// 13 accounts all at priority 0, resets spread 2.5h–152.5h, and the
    /// soonest-resetting account sitting on 44% of a weekly bucket that was
    /// worth nothing a few hours later.
    #[test]
    fn a_sooner_resetting_account_outranks_a_less_recently_selected_one() {
        let manager = build_manager(config_with(vec![account("urgent"), account("roomy")], 24));
        let now = OffsetDateTime::now_utc();

        set_weekly(&manager, 0, 2, now);
        set_weekly(&manager, 1, 100, now);
        // Bias plain LRU as hard as possible TOWARD `roomy`: `urgent` looks
        // most-recently-selected, `roomy` never-selected.
        set_seq(&manager, 0, 999);
        set_seq(&manager, 1, 0);

        assert_eq!(
            pick(&manager, now),
            0,
            "the account whose weekly window resets in 2h must be spent before \
             the one with 100h left, even though it is the most recently \
             selected of the two — its unused headroom is the only headroom \
             about to expire"
        );
    }

    /// The other half of the design: bucketing must NOT collapse into a
    /// deterministic pin. Two accounts resetting 2h apart share bucket 0 at the
    /// default 24h width, so LRU still decides between them and consecutive
    /// picks alternate — the fan-out that ranking on the raw reset instant would
    /// have destroyed.
    #[test]
    fn accounts_in_one_bucket_still_fan_out_by_lru() {
        let manager = build_manager(config_with(vec![account("first"), account("second")], 24));
        let now = OffsetDateTime::now_utc();

        set_weekly(&manager, 0, 2, now);
        set_weekly(&manager, 1, 4, now);

        let picks: Vec<usize> = (0..4).map(|_| pick(&manager, now)).collect();
        assert!(
            picks.contains(&0) && picks.contains(&1),
            "two accounts sharing one urgency bucket must both be served — \
             got {picks:?}, which is the deterministic pin the bucket exists \
             to prevent"
        );
    }

    /// `resetUrgencyTierHours: 0` is the config-only rollback: the urgency term
    /// ties for every account and the pre-tier LRU order returns. Same fixture
    /// as the discriminating test, opposite expected winner.
    #[test]
    fn tier_hours_zero_restores_the_pre_tier_lru_order() {
        let manager = build_manager(config_with(vec![account("urgent"), account("roomy")], 0));
        let now = OffsetDateTime::now_utc();

        set_weekly(&manager, 0, 2, now);
        set_weekly(&manager, 1, 100, now);
        set_seq(&manager, 0, 999);
        set_seq(&manager, 1, 0);

        assert_eq!(
            pick(&manager, now),
            1,
            "with the tier term disabled the never-selected account wins on \
             plain LRU, exactly as it did before this feature existed"
        );
    }

    /// An account with no known live weekly reset keeps sorting FIRST — the
    /// pre-tier "unknown quota → probe it" contract, which the bucket must
    /// carry through rather than quietly re-rank. `roomy` here is both
    /// never-selected AND resetting soon; the unmeasured account still beats it.
    #[test]
    fn an_unknown_weekly_reset_still_sorts_ahead_of_every_bucket() {
        let manager = build_manager(config_with(
            vec![account("unmeasured"), account("soon")],
            24,
        ));
        let now = OffsetDateTime::now_utc();

        // `unmeasured` deliberately gets NO weekly window at all.
        set_weekly(&manager, 1, 1, now);
        set_seq(&manager, 0, 999);
        set_seq(&manager, 1, 0);

        assert_eq!(
            pick(&manager, now),
            0,
            "an account whose weekly window was never measured must be picked \
             (and so probed) ahead of every bucketed account"
        );
    }

    /// Unit-level check of the bucket arithmetic itself, independent of
    /// selection: the floor division, the disabled arm, and the unknown-reset
    /// sentinel. Widths are checked at the boundary (24h lands in bucket 1, not
    /// 0) because an off-by-one here silently re-ranks the whole fleet.
    #[test]
    fn bucket_arithmetic_floors_and_handles_both_sentinels() {
        let manager = build_manager(config_with(vec![account("a")], 24));
        let now = OffsetDateTime::now_utc();

        let tier = |hours: i64| {
            set_weekly(&manager, 0, hours, now);
            let accounts = manager.accounts.read().expect("accounts lock poisoned");
            manager.reset_urgency_tier(&accounts[0], now)
        };

        assert_eq!(tier(2), 0, "2h into a 24h width is bucket 0");
        assert_eq!(tier(23), 0, "23h is still bucket 0");
        assert_eq!(tier(25), 1, "25h crosses into bucket 1");
        assert_eq!(tier(100), 4, "100h floors to bucket 4");
        assert_eq!(tier(152), 6, "the live fleet's furthest account, bucket 6");

        // Unknown reset → the minimum, ahead of every bucket.
        {
            let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
            accounts[0].quota.seven_day = None;
        }
        {
            let accounts = manager.accounts.read().expect("accounts lock poisoned");
            assert_eq!(
                manager.reset_urgency_tier(&accounts[0], now),
                i64::MIN,
                "an unmeasured weekly window sorts ahead of every bucket"
            );
        }

        // Disabled → a constant tie for everyone, including the unknown arm.
        let off = build_manager(config_with(vec![account("a")], 0));
        {
            let accounts = off.accounts.read().expect("accounts lock poisoned");
            assert_eq!(
                off.reset_urgency_tier(&accounts[0], now),
                0,
                "a disabled tier term must tie, not rank"
            );
        }
    }
}
