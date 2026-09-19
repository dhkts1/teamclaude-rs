//! `Manager` peer-lending method, on the manager's side of the boundary.
//!
//! # Why this file exists at all
//!
//! The guard-band arithmetic a lease must respect is
//! `effective_threshold(threshold, reserve, allow_reserve)`
//! (`src/manager/select.rs:248`), and it is **`pub(super)`**, so
//! `src/peer/lease.rs` cannot call it, and cannot name its result either. The
//! two wrong answers are widening that function's visibility and copying its
//! arithmetic; both put the fleet's guard band in two places, and the house
//! pattern ten lines below it says why that is the failure to avoid: it calls
//! itself "the ONE resolver" so two callers "can never independently drift".
//!
//! So the peer code asks the manager, and this sibling module is where the
//! manager answers. `pub(super)` reaches a sibling, which is the whole trick.
//!
//! **`src/manager/select.rs` changes by ZERO lines**, and that is the tripwire:
//! if it changes by one, stop and re-read this comment. Explicitly permitted
//! and not a trip: the single `mod peer_lend;` line in `src/manager/mod.rs`,
//! because a sibling module that is not declared does not exist.
//!
//! # The reserve is being GENERALISED, not transcribed
//!
//! `effective_threshold`'s own doc says it is inert in the current
//! (control-disabled) configuration and exists for the control account
//! specifically. A second consumer widens what the reserve MEANS: the
//! arithmetic is reused, the meaning is not. Saying that out loud is the
//! decision; claiming pure reuse would be the drift.

use super::*;

impl crate::config::EgressPin {
    /// Does this exit lock forbid handing the account's bearer to a borrower?
    ///
    /// # One sentence, three readers, and the one that disagreed
    ///
    /// A hand-mode grant sends the owner's bearer to another Mac and the
    /// request leaves from THERE. `strict` is the operator saying this
    /// account's requests leave through one exit or not at all, so a handoff
    /// breaks it whatever that exit is: `strict` with a LOCAL egress says "out
    /// of this Mac and nowhere else", which a borrower on another Mac breaks
    /// as surely as a pin naming a third one.
    ///
    /// Three places asked this question and one answered differently. The lend
    /// verb refused only `strict && !egress.is_local()`, so
    /// `--exits-from local --must` took a hand grant at the CLI that the
    /// serving path then refused to fund on every request: a grant that looks
    /// granted, buys nothing, and reports nothing. The other two read `strict`
    /// alone. This is the one answer now, and the lend verb was the one that
    /// moved.
    pub fn forbids_handoff(&self) -> bool {
        self.strict
    }

    /// May the runtime account at `index` have its bearer handed over, given
    /// the exit locks read off the config?
    ///
    /// # An index that does not resolve is a REFUSAL
    ///
    /// The two vectors are appended to together and never reordered, so the
    /// index is normally sound. `Manager::add_account` pushes the runtime row
    /// first and the config row second, taking the two locks one at a time on
    /// purpose, and in that window the runtime vector is one longer than the
    /// pins. `pins.get(index)` is then `None`, and reading that as "no pin,
    /// so not strict" hands over the bearer of an account whose exit lock this
    /// code has not seen yet.
    ///
    /// A missing pin is not "no pin", it is "not known", and the two are the
    /// same shape and opposite decisions. The window is short and the account
    /// being added is rarely the one in scope; it is also a credential leaving
    /// this Mac, and that is not a class of bug to leave open because it is
    /// narrow.
    pub fn may_be_handed(pins: &[Self], index: usize) -> bool {
        pins.get(index).is_some_and(|pin| !pin.forbids_handoff())
    }
}

impl crate::config::Account {
    /// Whether this account's exit lock forbids handing its bearer over.
    ///
    /// The whole rule is in [`crate::config::EgressPin::forbids_handoff`]; this
    /// is the form the two callers that hold a config account want, so neither
    /// of them re-derives the pin.
    pub fn cannot_be_handed(&self) -> bool {
        self.egress_pin().forbids_handoff()
    }
}

impl Manager {
    /// The bearer a `hand`-mode grant hands over, when it stops working, and
    /// the owner's own utilization on `window` at that instant.
    ///
    /// A grant may carry the owner's SHORT-LIVED access token to
    /// the borrower so the request leaves the borrower's own machine. This is
    /// the one place that token is read for that purpose, and it is on the
    /// manager for the reason the whole of this module is: the scope is
    /// resolved by `scope_covers`, the same answer the picker restriction and
    /// the `lentTo` line use, so a hand-mode grant can never reach an account a
    /// serve-mode grant of the same scope would not have picked.
    ///
    /// # It is the account [`Self::handable_fraction`] measured, and that is
    /// not a coincidence
    ///
    /// Both answers come from one call to [`Self::lend_candidates`] and are
    /// ranked by one function, [`LendCandidate::handable_room`]. A review found
    /// the two disagreeing: this ranked by the least room across three windows
    /// while `lendable_fraction`, which sized the lease, folded `max` over one
    /// and excluded neither a held account nor a pinned one, so the token a
    /// borrower was handed was routinely not the account the fraction had been
    /// measured on, and a scope whose usable accounts were all held or pinned
    /// funded a lease with no bearer at all. One candidate set and one ranking
    /// is the fix, and `window` is now a parameter because the ranking needs
    /// it: the doc here used to name one that did not exist.
    ///
    /// # Every refusal is a `None` rather than a worse token
    ///
    /// No account the scope covers, no account that is usable (disabled,
    /// errored, or held out by a 429 right now), an account whose bearer an
    /// exit lock forbids handing over, an account with no expiry recorded, and
    /// an account whose `window` this Mac has never measured. The last two are
    /// deliberate. A bearer handed over with no deadline is one the borrower
    /// would use until upstream 401s, and the frame's whole contract is that
    /// the borrower knows when to stop; a bearer handed over with no measured
    /// utilization is one whose spend the borrower cannot report, because the
    /// hint it sends back is a RISE and a rise needs the baseline this figure
    /// is ([`crate::peer::lease::usage_hint`]).
    ///
    /// **The refresh token is never read here and never leaves this Mac.** That
    /// is the difference between lending an account for a while and giving it
    /// away, and it is why revocation is "stop renewing".
    pub fn handoff_bearer(
        &self,
        scope: &tcr_peer_wire::LendScope,
        window: tcr_peer_wire::Window,
    ) -> Option<HandedCredential> {
        let now = OffsetDateTime::now_utc();
        Self::best_handable(self.lend_candidates(scope, window, now))
            .and_then(LendCandidate::into_credential)
    }

    /// What a `hand`-mode lease on `scope` and `window` may be sized at: the
    /// room on the one account whose bearer could actually go out, and `0.0`
    /// when there is none.
    ///
    /// The figure and the token are the same account's, because this and
    /// [`Self::handoff_bearer`] fold the same list with the same key. That is
    /// what makes "a lease is funded only if a bearer exists" true by
    /// construction rather than by a second check somebody has to remember.
    ///
    /// The room is the LEAST this account has on any window it has measured,
    /// not the room on `window` alone. A handed bearer leaves this Mac and
    /// serves whatever the borrower sends for as long as it lives, so it
    /// touches every window; an account with room on the 5-hour window and none
    /// on the 7-day one is one 429 away for a borrower who cannot see either
    /// figure.
    pub fn handable_fraction(
        &self,
        scope: &tcr_peer_wire::LendScope,
        window: tcr_peer_wire::Window,
        now: OffsetDateTime,
    ) -> f64 {
        Self::best_handable(self.lend_candidates(scope, window, now))
            .map_or(0.0, |candidate| candidate.handable_room())
    }

    /// The one ranking a hand-mode lease is measured and picked by.
    ///
    /// `reduce` and a strict `>` rather than `max_by`, so a tie keeps the
    /// EARLIER account: with nothing measured every candidate has the same
    /// room, and the answer is then the vector order this module has always
    /// had rather than its reverse.
    fn best_handable(candidates: Vec<LendCandidate>) -> Option<LendCandidate> {
        candidates
            .into_iter()
            .filter(LendCandidate::may_be_handed)
            .reduce(|best, candidate| {
                if candidate.handable_room() > best.handable_room() {
                    candidate
                } else {
                    best
                }
            })
    }

    /// Every account this Mac could lend on for `scope`, with the facts both
    /// the sizing and the bearer question need, read once.
    ///
    /// # One set, two questions
    ///
    /// The eligibility a lease of ANY mode needs is here and nowhere else: not
    /// disabled, not [`AccountStatus::Error`], and covered by the scope. What
    /// separates the two modes is then a property of the candidate, never a
    /// second filter written out twice: [`LendCandidate::bearer`] is present
    /// only for an account whose token may leave this Mac right now.
    ///
    /// A [`AccountStatus::Throttled`] account is still a candidate, because a
    /// hold is a timer and a lease outlives it, so counting it keeps the
    /// advertised serve figure from flapping; it simply carries no bearer,
    /// because a token the borrower would meet a 429 on for a hold it cannot
    /// see is not a token to hand over.
    ///
    /// The exit locks are collected under the config lock and out of it again
    /// before the accounts lock is taken, so this holds one lock at a time and
    /// can be no half of a deadlock, whatever order any other reader takes them
    /// in. It is the order this module already had.
    fn lend_candidates(
        &self,
        scope: &tcr_peer_wire::LendScope,
        window: tcr_peer_wire::Window,
        now: OffsetDateTime,
    ) -> Vec<LendCandidate> {
        // Hot-reloaded for the same reason the whole of this module reloads: a
        // lease scoped to a group follows the group.
        self.reload_groups_if_changed();
        // By index, which is the pairing `Manager::account_egress` and
        // `Manager::access_token` already rest on: the config vector and the
        // runtime vector are appended to together and never reordered.
        let pins: Vec<crate::config::EgressPin> = self
            .config
            .lock()
            .expect("config lock poisoned")
            .accounts
            .iter()
            .map(crate::config::Account::egress_pin)
            .collect();
        let accounts = self.accounts.read().expect("accounts lock poisoned");
        accounts
            .iter()
            .enumerate()
            .filter(|(_, account)| !account.disabled && account.status != AccountStatus::Error)
            .filter(|(_, account)| {
                // An account whose own name is not a label (an email, a uuid)
                // can never be NAMED in a scope, because a scope is written to
                // a file and printed by a CLI in a public repository. It is
                // still lendable under `All` and under a group it belongs to:
                // what it cannot be is picked out by name. `scope_covers` is the
                // one answer to this question, shared with the picker
                // restriction and with the `lentTo` line, so a scope cannot
                // mean one set of accounts here and another when the request
                // arrives.
                let label = tcr_peer_wire::sanitize_label(&account.name).unwrap_or_default();
                crate::peer::lease::scope_covers(scope, &label, &account.groups)
            })
            .map(|(index, account)| {
                let threshold = account.switch_threshold.unwrap_or(self.global_threshold);
                let guard =
                    super::select::effective_threshold(threshold, self.control_reserve, true);
                let utilization = window_utilization(&account.quota, window, now);
                let room = utilization.map_or(0.0, |utilization| (guard - utilization).max(0.0));
                // An account with no measured window at all is neither the
                // best nor the worst candidate: it is unknown, and `0.0` says
                // so in the one direction that cannot hand a borrower a token
                // this Mac has evidence against.
                let room_anywhere = [
                    tcr_peer_wire::Window::FiveHour,
                    tcr_peer_wire::Window::SevenDay,
                    tcr_peer_wire::Window::SevenDayOi,
                ]
                .into_iter()
                .filter_map(|window| window_utilization(&account.quota, window, now))
                .map(|utilization| (guard - utilization).max(0.0))
                .fold(f64::INFINITY, f64::min);
                let room_anywhere = if room_anywhere.is_finite() {
                    room_anywhere
                } else {
                    0.0
                };
                // **A HELD ACCOUNT IS NOT A LENDABLE BEARER**, and neither is a
                // strictly pinned one. An exit lock is the operator saying this
                // account's requests leave from ONE address because the address
                // is load-bearing, an allow-listed office IP or a session an
                // origin ties to one address. A hand-mode grant sends from the
                // BORROWER's machine, so handing over a pinned account's bearer
                // breaks the pin on every request, silently, on a Mac this one
                // cannot see. Every strict pin, not only `via <other Mac>`:
                // `strict` with a local egress is the same sentence, and a
                // handoff breaks that one too. A non-strict pin is a preference
                // its own doc says may fall back, so it is lent like any other
                // account.
                let handable = account.status != AccountStatus::Throttled
                    && crate::config::EgressPin::may_be_handed(&pins, index);
                let bearer = handable
                    .then(|| {
                        account
                            .expires_at_ms
                            .map(|at| (account.access_token.clone(), at))
                    })
                    .flatten()
                    // No measured utilization is no baseline, and no baseline
                    // is a borrow the owner could never charge. See this
                    // method's own doc.
                    .zip(utilization);
                LendCandidate {
                    room,
                    room_anywhere,
                    bearer,
                }
            })
            .collect()
    }

    /// How much of one window this node may lend right now, as a fraction,
    /// after the owner's own guard band, never the raw threshold.
    ///
    /// Zero is a complete answer and the common one: it means "nothing to lend
    /// on this window", which a lender returns as a refusal rather than a small
    /// grant.
    ///
    /// The lease layer calls this and nothing else on the manager. It never
    /// reaches into the picker: `select_with_group` returns an index into a
    /// local vector where accounts are appended and never removed, and every
    /// gate in `eligible` is a pure function of a LOCAL account's runtime. A
    /// remote candidate has no index and no runtime, so there is no honest way
    /// to put one in there, which is why the fallback seam lives at the
    /// picker's `None` arm (`crate::fallback`) instead.
    ///
    /// # The answer is the BEST SINGLE ACCOUNT's headroom, not the fleet's sum
    ///
    /// A lease is spent one request at a time and the lender's own picker
    /// chooses which account serves each one, so the figure a borrower can
    /// actually rely on is what one account can absorb. Summing the fleet would
    /// advertise a fraction no single request can reach and would then be
    /// refused by [`crate::peer::lease::Ledger::may_relay`] on the very first
    /// relay, a number that is arithmetically true and operationally a lie.
    ///
    /// # The reserve is applied to EVERY account, which is the generalisation
    ///
    /// `pool_pick_respects_control_reserve` (`src/manager/select.rs:2303`)
    /// applies `effective_threshold` to the control account alone, because there
    /// the reserve means "leave the control account headroom against our own
    /// pool traffic". Here it means "leave every account headroom against
    /// somebody else's traffic", so it is applied to all of them: the arithmetic
    /// is reused, the meaning is wider, and that is the decision rather than a
    /// transcription. Saying it out loud is the whole reason the module doc
    /// above exists.
    ///
    /// # An unmeasured window lends nothing
    ///
    /// An absent [`crate::quota::QuotaWindow`] means "never read", not "empty".
    /// It contributes `0.0`, so a fleet that has not been probed lends nothing
    /// at all. Guessing in the other direction gives away the owner's quota on
    /// no evidence, and the owner is not the one who benefits from the guess.
    ///
    /// A disabled account, and one whose credential is dead
    /// ([`AccountStatus::Error`]), contribute nothing either: neither can serve
    /// the borrowed request when it arrives. A [`AccountStatus::Throttled`]
    /// account DOES contribute, a 429 hold is a timer, the lease outlives it,
    /// and excluding it would make the advertised figure flap with every hold.
    ///
    /// # `scope` is the reason two Macs get two numbers
    ///
    /// A lease draws from `All` of this node's accounts, from one `Group`, or
    /// from a named set ([`tcr_peer_wire::LendScope`]). The fraction is of the
    /// SCOPE's headroom, so this reduces over the scope's accounts only, which
    /// is what makes "attic-nuc gets 20 % of the `work` group" a different
    /// figure from "studio-mac gets 20 % of account A", on the same fleet, at
    /// the same instant. A single fleet-wide answer could not express either.
    ///
    /// Membership is answered by [`crate::peer::lease::scope_covers`], the same
    /// function the `lentTo` line and the picker restriction read, because a
    /// scope that meant one set of accounts when the headroom was computed and
    /// another when the request was served would advertise one account's room
    /// and spend another's.
    ///
    /// A scope that covers NO account answers `0.0`, like an unmeasured window
    /// and for the same reason: a fold over nothing is nothing to lend, and the
    /// operator who scoped a lease to a group they later emptied gets a refusal
    /// rather than the fleet.
    pub fn lendable_fraction(
        &self,
        scope: &tcr_peer_wire::LendScope,
        window: tcr_peer_wire::Window,
        now: OffsetDateTime,
    ) -> f64 {
        // The SAME candidate set a hand grant's bearer comes out of, so the
        // two answers about one scope can never be measured over two different
        // sets of accounts. What differs is only the fold: this is the best
        // single account's room on the window asked about, held and pinned
        // accounts included, because a serve-mode relay leaves from THIS Mac
        // and meets neither restriction.
        self.lend_candidates(scope, window, now)
            .into_iter()
            .map(|candidate| candidate.room)
            .fold(0.0f64, f64::max)
    }

    /// Every account's utilization on one window, in account order.
    ///
    /// The raw figures, with no guard band applied: this is not "what may be
    /// lent" ([`Self::lendable_fraction`] answers that) but "what has been
    /// spent", read twice around one relayed request so
    /// [`crate::peer::serve::utilization_rise`] can charge the lease what the
    /// request actually cost.
    ///
    /// Positional, and that is load-bearing. `select_with_group` returns an
    /// index into a vector accounts are appended to and never removed from, so
    /// index `i` is the same account across the two reads; a single scalar
    /// (a maximum, a sum) cannot say which account moved, and one account's
    /// probe landing between the reads would then be charged to the borrower.
    ///
    /// Disabled and errored accounts are NOT filtered out here, unlike in
    /// `lendable_fraction`: filtering would shift every later index between two
    /// reads if an account's status changed in between, which is precisely the
    /// mis-pairing this returns a positional vector to avoid.
    pub fn window_utilizations(
        &self,
        window: tcr_peer_wire::Window,
        now: OffsetDateTime,
    ) -> Vec<Option<f64>> {
        let accounts = self.accounts.read().expect("accounts lock poisoned");
        accounts
            .iter()
            .map(|account| window_utilization(&account.quota, window, now))
            .collect()
    }
}

/// One account this Mac could lend on, read once and answered from twice.
///
/// It exists so that "how much may be lent" and "whose token goes out" cannot
/// be measured over two different sets of accounts, which is what they were:
/// see [`Manager::handoff_bearer`]'s own doc for the review finding.
struct LendCandidate {
    /// Room on the window asked about, after the guard band, `0.0` for a window
    /// this Mac has never measured.
    room: f64,
    /// The LEAST room this account has on any window it has measured, which is
    /// what a bearer that leaves this Mac actually meets. `0.0` when nothing at
    /// all is measured.
    room_anywhere: f64,
    /// The credential and the owner's own utilization on the asked window, for
    /// an account whose bearer may be handed over right now and `None` for
    /// every account that fails one of the refusals in
    /// [`Manager::handoff_bearer`]'s doc.
    bearer: Option<((String, i64), f64)>,
}

impl LendCandidate {
    /// The one figure a hand-mode lease is both sized by and picked by.
    fn handable_room(&self) -> f64 {
        self.room.min(self.room_anywhere)
    }

    /// Whether this account's bearer could go out at all.
    fn may_be_handed(&self) -> bool {
        self.bearer.is_some()
    }

    /// The frame's half of this candidate, or `None` for one that carries no
    /// bearer.
    fn into_credential(self) -> Option<HandedCredential> {
        self.bearer.map(
            |((access_token, expires_at_ms), utilization)| HandedCredential {
                access_token,
                expires_at_ms,
                utilization,
            },
        )
    }
}

/// Everything one `Control::Handoff` frame carries, chosen together.
///
/// The three travel as one value because they are one account's facts at one
/// instant: a bearer paired with another account's expiry is a borrow that dies
/// early, and a bearer paired with another account's utilization is a lease
/// charged for somebody else's window.
#[derive(Debug, Clone, PartialEq)]
pub struct HandedCredential {
    /// The owner's short-lived access token. Never the refresh token.
    pub access_token: String,
    /// When that token stops working, absolute, so the borrower can stop using
    /// it without waiting for a 401.
    pub expires_at_ms: i64,
    /// The owner's own utilization on the lease's window at this instant, the
    /// baseline the borrower measures its rises against. Raw, with no guard
    /// band applied: it is compared against what the borrower reads off its own
    /// answers' rate-limit headers, which is the same raw figure.
    pub utilization: f64,
}

/// The lender's own fleet is what a relayed request is charged against.
///
/// The whole of the manager the lease layer reaches, alongside
/// [`Manager::lendable_fraction`]: the `now` a relay is measured at is the
/// instant of the read, so the trait takes no clock and this reads its own.
/// Both reads of one relay therefore see the wall clock they happened at, which
/// is what makes a window that reset between them read as a reset rather than
/// as a rise.
impl crate::peer::serve::WindowUtilization for Manager {
    fn read(&self, window: tcr_peer_wire::Window) -> Vec<Option<f64>> {
        self.window_utilizations(window, OffsetDateTime::now_utc())
    }

    /// The headroom, over the SCOPE's accounts only.
    ///
    /// A measurement found the mismatch: [`Manager::lendable_fraction`] takes a
    /// scope and the ledger's headroom was keyed by window alone, with
    /// `Ledger::note_owner_headroom` called for `LendScope::All` and nothing
    /// else, so a group-scoped lease was clamped by, and its owner guard
    /// decided against, the whole fleet's room. This is the wiring: the same
    /// function, asked per scope, by the two readers that decide a scoped lease
    /// (`Ledger::grant` when it mints, `handle_serve::handle_serve_on` when it
    /// relays).
    ///
    /// # `None` when the SCOPE has no measurement, which is not the same as
    /// zero
    ///
    /// [`Self::lendable_fraction`] answers `0.0` for two different facts: a
    /// window measured and full, and a window never measured at all (its own
    /// `map` returns `0.0` for a `None` quota). `Ledger::headroom`'s doc is
    /// explicit that those must not be conflated, "an unmeasured window is
    /// **absent, not zero-headroom and not infinite-headroom**", so the
    /// ambiguity is resolved HERE, where the quota is in hand, rather than
    /// passed to a ledger that cannot tell them apart.
    ///
    /// So: `None` when not one account inside the scope has a measured window,
    /// which leaves the ledger's `LendScope::All` note standing as the ceiling
    /// (the pre-decision-12 behaviour, and what the boot ticker wrote);
    /// `Some(fraction)` the moment there is something real to divide, which is
    /// after the lender's own first request on that window.
    ///
    /// The clock is read here, like [`Self::read`] above and for the same
    /// reason: the `now` a relay is measured at is the instant of the read, so
    /// the trait takes no clock.
    fn lendable(
        &self,
        scope: &tcr_peer_wire::LendScope,
        window: tcr_peer_wire::Window,
    ) -> Option<f64> {
        let now = OffsetDateTime::now_utc();
        // The same hot-reload and the same eligibility filter
        // `lendable_fraction` applies, because the question is about the same
        // set of accounts: a scope that covers only a disabled account has
        // nothing measured to lend.
        self.reload_groups_if_changed();
        let anything_measured = {
            let accounts = self.accounts.read().expect("accounts lock poisoned");
            accounts
                .iter()
                .filter(|account| !account.disabled && account.status != AccountStatus::Error)
                .filter(|account| {
                    let label = tcr_peer_wire::sanitize_label(&account.name).unwrap_or_default();
                    crate::peer::lease::scope_covers(scope, &label, &account.groups)
                })
                .any(|account| window_utilization(&account.quota, window, now).is_some())
        };
        if !anything_measured {
            return None;
        }
        Some(self.lendable_fraction(scope, window, now))
    }

    /// The same question for a `hand` grant, answered over the accounts whose
    /// bearer can actually leave this Mac.
    ///
    /// `Some` always, and `Some(0.0)` is the load-bearing answer: it says this
    /// fleet CAN tell hand mode from serve mode and has no bearer to hand over,
    /// which is what makes `Ledger::grant` refuse rather than mint a funded
    /// lease nothing can spend. The trait's default `None` is the other fact,
    /// "this reader cannot answer", and the two must not be conflated.
    fn lendable_by_hand(
        &self,
        scope: &tcr_peer_wire::LendScope,
        window: tcr_peer_wire::Window,
    ) -> Option<f64> {
        Some(self.handable_fraction(scope, window, OffsetDateTime::now_utc()))
    }

    /// The picker restriction, answered against THIS fleet.
    ///
    /// Three facts decide it, and each one is read here rather than assumed:
    ///
    /// 1. `All` needs no restriction.
    /// 2. A `Group` is enforceable through
    ///    [`crate::proxy::GROUP_HEADER_NAME`] **for exactly the groups the
    ///    picker holds strictly, which is the whole of the picker's own rule
    ///    and not half of it**: `select_with_group`'s `strict_group`
    ///    (`src/manager/select.rs:1148`) is
    ///    `reserved_groups.contains(g) || !spill_groups.contains(g)`, a
    ///    RESERVED group ignores `spillToPool` and stays strict. This used to
    ///    read the spill half alone and so refused a reserved-and-spill group
    ///    the picker would have held perfectly, which is a lease refused for a
    ///    reason that was not true. A group that is spill and NOT reserved is
    ///    still refused, and that half is unchanged: its request really does
    ///    degrade to the whole pool, which for a lease means serving somebody
    ///    else's account.
    /// 3. A group with no member is refused too: a request restricted to an
    ///    empty group is one the picker answers `None` to, and refusing it here
    ///    says so in the lease vocabulary instead of as a 429 the borrower
    ///    cannot read.
    /// 4. `Accounts` is enforceable through
    ///    [`crate::proxy::ACCOUNTS_HEADER_NAME`], which the picker reads as a
    ///    strict account set, for exactly the labels this fleet can serve on:
    ///    a label naming no account here is dropped, and a set that names none
    ///    of them is refused for the same reason an empty group is. The label
    ///    is [`tcr_peer_wire::sanitize_label`]'s, which is what the CLI's
    ///    `--scope account:<label>` accepts and what `tcr peer ls --json`
    ///    prints, so a scope names accounts in one vocabulary end to end.
    fn scope_restriction(
        &self,
        scope: &tcr_peer_wire::LendScope,
    ) -> crate::peer::serve::ScopeRestriction {
        use crate::peer::serve::ScopeRestriction;

        match scope {
            tcr_peer_wire::LendScope::All => ScopeRestriction::Unrestricted,
            tcr_peer_wire::LendScope::Accounts(labels) => {
                // The labels this fleet can actually serve on, in the fleet's
                // own order and de-duplicated by the account list itself: the
                // header names accounts, so a label no account here carries
                // would be a name the picker answers nothing to. Dropping it
                // and keeping the rest is the same rule the group arm applies
                // to an empty group, one level down.
                //
                // `scope_covers` is not called here because it answers a
                // question about ONE account and this is the set the header
                // carries; both read the SAME sanitized label, which is what
                // keeps the two in step.
                let named: Vec<String> = {
                    let accounts = self.accounts.read().expect("accounts lock poisoned");
                    accounts
                        .iter()
                        .filter_map(|account| tcr_peer_wire::sanitize_label(&account.name).ok())
                        .filter(|label| labels.iter().any(|named| named == label))
                        .collect()
                };
                if named.is_empty() {
                    return ScopeRestriction::Unenforceable;
                }
                ScopeRestriction::Accounts(named)
            }
            tcr_peer_wire::LendScope::Group(name) => {
                // Both reads are of hot-reloaded state, so this follows a
                // `tcr group spill` or a membership change without a restart,
                // the same rule `lendable_fraction` above already follows.
                self.reload_groups_if_changed();
                if self.spill_groups().contains(name) && !self.reserved_groups().contains(name) {
                    return ScopeRestriction::Unenforceable;
                }
                let members = {
                    let accounts = self.accounts.read().expect("accounts lock poisoned");
                    accounts
                        .iter()
                        .filter(|account| account.groups.iter().any(|group| group == name))
                        .count()
                };
                if members == 0 {
                    return ScopeRestriction::Unenforceable;
                }
                ScopeRestriction::Group(name.clone())
            }
        }
    }
}

/// One account's utilization on one wire window, or `None` when this build has
/// no measurement of it.
///
/// The three arms are the wire's name for the three separately named
/// `Option<QuotaWindow>` fields in `src/quota.rs`, there is no enum on that
/// side, which is why the mapping is written once, here, and not at each caller.
/// [`QuotaWindow::effective`] is what reads it, so a window past its reset is a
/// fresh window rather than the stale last value.
///
/// [`tcr_peer_wire::Window::Unknown`] is `None`: a window this build cannot name
/// is one it cannot measure, and lending against it would be lending against
/// nothing.
fn window_utilization(
    quota: &Quota,
    window: tcr_peer_wire::Window,
    now: OffsetDateTime,
) -> Option<f64> {
    let measured = match window {
        tcr_peer_wire::Window::FiveHour => quota.five_hour,
        tcr_peer_wire::Window::SevenDay => quota.seven_day,
        tcr_peer_wire::Window::SevenDayOi => quota.seven_day_oi,
        tcr_peer_wire::Window::Unknown => None,
    };
    measured.map(|window| window.effective(now))
}

#[cfg(test)]
mod tests {
    use time::Duration;

    use super::*;
    use crate::quota::QuotaWindow;

    /// `0.95 - 0.05`, the default `switchThreshold` minus the default
    /// `controlReserve`, which is the guard band every figure below is measured
    /// against. Read off `config::default_switch_threshold` (`src/config.rs:33`)
    /// and `config::default_control_reserve` (`:39`); `Manager::from_runtimes`
    /// builds its config from `{}`, so both defaults are what apply here.
    ///
    /// Written as the subtraction rather than `0.90` so a change to either
    /// default shows up as a failing assertion with both terms visible, instead
    /// of a magic number that silently stops describing the code.
    const GUARD: f64 = 0.95 - 0.05;

    /// `LendScope::All`, what every figure in this module meant before
    /// a scope, so each pre-existing gate below stays a test of the
    /// guard-band arithmetic rather than becoming one of the scope filter.
    const ALL: tcr_peer_wire::LendScope = tcr_peer_wire::LendScope::All;

    /// One fake account. No real email, no org uuid, no account uuid: this
    /// repository is public and a fixture is the easiest place to leak one.
    fn account(name: &str) -> AccountRuntime {
        let account: config::Account = serde_json::from_str(&format!(
            r#"{{"name":"{name}","accessToken":"not-a-real-token"}}"#
        ))
        .expect("the fixture is valid JSON for an Account");
        AccountRuntime::from_config(&account, false)
    }

    /// The same, in one group.
    fn account_in(name: &str, group: &str) -> AccountRuntime {
        let mut runtime = account(name);
        runtime.groups = vec![group.to_string()];
        runtime
    }

    /// A window measured at `utilization`, resetting well after `now`.
    fn measured(utilization: f64, now: OffsetDateTime) -> Option<QuotaWindow> {
        Some(QuotaWindow {
            utilization,
            reset: Some(now + Duration::hours(6)),
        })
    }

    #[test]
    fn lendable_is_the_guard_band_minus_utilization() {
        let now = OffsetDateTime::now_utc();
        let mut a = account("alice-fake");
        a.quota.seven_day = measured(0.40, now);
        let manager = Manager::from_runtimes(vec![a]);

        let lendable = manager.lendable_fraction(&ALL, tcr_peer_wire::Window::SevenDay, now);
        assert!(
            (lendable - (GUARD - 0.40)).abs() < 1e-9,
            "expected {} of headroom above the guard, got {lendable}",
            GUARD - 0.40
        );
    }

    /// The figure the guard-band gate in `tests/peer_lease.rs` is written
    /// against: at 0.89 against a 0.90 guard there is still something to lend,
    /// and one step over the guard there is exactly nothing.
    ///
    /// Both halves in one test on purpose, the second is the control that stops
    /// the first from passing for the wrong reason.
    #[test]
    fn lendable_is_zero_at_the_guard_and_positive_just_under_it() {
        let now = OffsetDateTime::now_utc();

        let mut under = account("alice-fake");
        under.quota.seven_day = measured(0.89, now);
        let lendable = Manager::from_runtimes(vec![under]).lendable_fraction(
            &ALL,
            tcr_peer_wire::Window::SevenDay,
            now,
        );
        assert!(
            (lendable - 0.01).abs() < 1e-9,
            "0.89 against a {GUARD} guard leaves 0.01, got {lendable}"
        );

        let mut over = account("alice-fake");
        over.quota.seven_day = measured(0.91, now);
        assert_eq!(
            Manager::from_runtimes(vec![over]).lendable_fraction(
                &ALL,
                tcr_peer_wire::Window::SevenDay,
                now
            ),
            0.0,
            "past the guard there is nothing to lend, and the clamp is at 0.0 not below it"
        );
    }

    /// An account's OWN `switchThreshold` wins over the global one, the same way
    /// every other gate in this manager reads it.
    #[test]
    fn an_accounts_own_threshold_sets_its_guard() {
        let now = OffsetDateTime::now_utc();
        let mut a = account("alice-fake");
        a.switch_threshold = Some(0.70);
        a.quota.seven_day = measured(0.40, now);

        let lendable = Manager::from_runtimes(vec![a]).lendable_fraction(
            &ALL,
            tcr_peer_wire::Window::SevenDay,
            now,
        );
        assert!(
            (lendable - ((0.70 - 0.05) - 0.40)).abs() < 1e-9,
            "expected the account's own 0.70 threshold to bind, got {lendable}"
        );
    }

    /// The answer is the BEST SINGLE account's headroom, never the fleet's sum:
    /// one request is served by one account.
    #[test]
    fn lendable_is_the_best_account_not_the_sum() {
        let now = OffsetDateTime::now_utc();
        let mut a = account("alice-fake");
        a.quota.seven_day = measured(0.80, now);
        let mut b = account("bob-fake");
        b.quota.seven_day = measured(0.20, now);

        let lendable = Manager::from_runtimes(vec![a, b]).lendable_fraction(
            &ALL,
            tcr_peer_wire::Window::SevenDay,
            now,
        );
        assert!(
            (lendable - (GUARD - 0.20)).abs() < 1e-9,
            "expected the roomiest account's {} and not the sum, got {lendable}",
            GUARD - 0.20
        );
    }

    /// An unmeasured window lends nothing. A fresh fleet that has never been
    /// probed gives away no quota at all.
    #[test]
    fn an_unmeasured_window_lends_nothing() {
        let now = OffsetDateTime::now_utc();
        let mut a = account("alice-fake");
        // Measured on `7d`, and therefore NOT on `7d_oi`, which is the
        // question being asked.
        a.quota.seven_day = measured(0.10, now);
        let manager = Manager::from_runtimes(vec![a]);

        assert_eq!(
            manager.lendable_fraction(&ALL, tcr_peer_wire::Window::SevenDayOi, now),
            0.0,
            "no measurement on this window is a refusal to lend, never room"
        );
        assert!(
            manager.lendable_fraction(&ALL, tcr_peer_wire::Window::SevenDay, now) > 0.0,
            "the positive control: the window that IS measured still lends"
        );
    }

    /// A window past its reset is a FRESH window, so it lends the whole guard
    /// band rather than its stale last value.
    #[test]
    fn a_window_past_its_reset_lends_the_whole_guard_band() {
        let now = OffsetDateTime::now_utc();
        let mut a = account("alice-fake");
        a.quota.seven_day = Some(QuotaWindow {
            utilization: 0.99,
            reset: Some(now - Duration::minutes(1)),
        });

        let lendable = Manager::from_runtimes(vec![a]).lendable_fraction(
            &ALL,
            tcr_peer_wire::Window::SevenDay,
            now,
        );
        assert!(
            (lendable - GUARD).abs() < 1e-9,
            "a reset window reads 0.0, so the whole {GUARD} is lendable, got {lendable}"
        );
    }

    /// A disabled account, and one with a dead credential, lend nothing:
    /// neither can serve the borrowed request when it arrives.
    #[test]
    fn a_disabled_or_errored_account_lends_nothing() {
        let now = OffsetDateTime::now_utc();

        let mut disabled = account("alice-fake");
        disabled.disabled = true;
        disabled.quota.seven_day = measured(0.10, now);
        assert_eq!(
            Manager::from_runtimes(vec![disabled]).lendable_fraction(
                &ALL,
                tcr_peer_wire::Window::SevenDay,
                now
            ),
            0.0
        );

        let mut errored = account("alice-fake");
        errored.status = AccountStatus::Error;
        errored.quota.seven_day = measured(0.10, now);
        assert_eq!(
            Manager::from_runtimes(vec![errored]).lendable_fraction(
                &ALL,
                tcr_peer_wire::Window::SevenDay,
                now
            ),
            0.0
        );

        // A THROTTLED account still lends: a 429 hold is a timer the lease
        // outlives, and excluding it would make the advertised figure flap with
        // every hold.
        let mut throttled = account("alice-fake");
        throttled.status = AccountStatus::Throttled;
        throttled.quota.seven_day = measured(0.10, now);
        assert!(
            Manager::from_runtimes(vec![throttled]).lendable_fraction(
                &ALL,
                tcr_peer_wire::Window::SevenDay,
                now
            ) > 0.0
        );
    }

    /// An empty fleet lends nothing, and does not panic reaching for a maximum
    /// over no accounts.
    #[test]
    fn an_empty_fleet_lends_nothing() {
        let now = OffsetDateTime::now_utc();
        assert_eq!(
            Manager::from_runtimes(Vec::new()).lendable_fraction(
                &ALL,
                tcr_peer_wire::Window::SevenDay,
                now
            ),
            0.0
        );
    }

    /// **Two Macs with different scopes on the same fleet get different
    /// headroom numbers**, the gate on that rule, and the thing a
    /// fleet-wide answer cannot express.
    ///
    /// One fleet, two accounts: `alice-fake` in group `work` at 0.80, and
    /// `bob-fake` in no group at 0.20. A lease scoped to `work` sees only the
    /// first account's room; a lease scoped to `account:bob-fake` sees only the
    /// second's; `all` sees the roomier of the two. Three numbers off one fleet
    /// at one instant.
    ///
    /// Watched red by making the scope filter unconditional (`true` instead of
    /// `scope_covers(..)`): all three then answer `GUARD - 0.20` and the first
    /// assertion fails.
    #[test]
    fn two_scopes_on_one_fleet_give_two_headroom_numbers() {
        let now = OffsetDateTime::now_utc();
        let mut in_work = account_in("alice-fake", "work");
        in_work.quota.seven_day = measured(0.80, now);
        let mut outside = account("bob-fake");
        outside.quota.seven_day = measured(0.20, now);
        let manager = Manager::from_runtimes(vec![in_work, outside]);

        let group = tcr_peer_wire::LendScope::Group("work".to_string());
        let named = tcr_peer_wire::LendScope::Accounts(vec!["bob-fake".to_string()]);

        let in_group = manager.lendable_fraction(&group, tcr_peer_wire::Window::SevenDay, now);
        let by_name = manager.lendable_fraction(&named, tcr_peer_wire::Window::SevenDay, now);
        let everything = manager.lendable_fraction(&ALL, tcr_peer_wire::Window::SevenDay, now);

        assert!(
            (in_group - (GUARD - 0.80)).abs() < 1e-9,
            "the `work` group holds one account at 0.80, so it lends {}, got {in_group}",
            GUARD - 0.80
        );
        assert!(
            (by_name - (GUARD - 0.20)).abs() < 1e-9,
            "account:bob-fake is the one at 0.20, so it lends {}, got {by_name}",
            GUARD - 0.20
        );
        assert!(
            (everything - (GUARD - 0.20)).abs() < 1e-9,
            "`all` is still the roomiest single account, got {everything}"
        );
        assert!(
            in_group < by_name,
            "the whole point: two scopes, one fleet, one instant, two numbers \
             ({in_group} vs {by_name})"
        );
    }

    /// A scope that names an account this fleet does not have lends nothing,
    /// never the fleet. An operator who typoed a label, or scoped a lease to a
    /// group they later emptied, gets a refusal rather than every account.
    #[test]
    fn a_scope_that_covers_no_account_lends_nothing() {
        let now = OffsetDateTime::now_utc();
        let mut a = account("alice-fake");
        a.quota.seven_day = measured(0.10, now);
        let manager = Manager::from_runtimes(vec![a]);

        for scope in [
            tcr_peer_wire::LendScope::Group("nobody".to_string()),
            tcr_peer_wire::LendScope::Accounts(vec!["not-an-account".to_string()]),
        ] {
            assert_eq!(
                manager.lendable_fraction(&scope, tcr_peer_wire::Window::SevenDay, now),
                0.0,
                "{scope} covers no account here, so it lends nothing"
            );
        }
        assert!(
            manager.lendable_fraction(&ALL, tcr_peer_wire::Window::SevenDay, now) > 0.0,
            "the positive control: the fleet really does have room to lend under `all`"
        );
    }

    /// **A scope is enforceable through the group header, or it is refused,
    /// never served on whatever the picker liked.**
    ///
    /// The picker restriction, answered against a real fleet. The
    /// five arms are the five facts:
    ///
    /// - `all` needs no restriction;
    /// - a group with a member is the group header, which
    ///   `select_with_group`'s `strict_group` holds strictly;
    /// - a group with `spillToPool` on is NOT enforceable, because that same
    ///   line drops the group and the request falls into the whole pool;
    /// - a group with no member is not enforceable either, the picker would
    ///   answer nothing, and saying so in the lease vocabulary is the honest
    ///   answer;
    /// - a group that is BOTH reserved and spill is enforceable, because the
    ///   picker's own rule is `reserved || !spill` and a reserved group
    ///   ignores `spillToPool` (`src/manager/select.rs:1139`). This arm
    ///   asserted the opposite once: reading the spill half alone
    ///   refused a lease the picker would have held perfectly;
    /// - a named account set is enforceable for the labels this fleet carries,
    ///   on `proxy::ACCOUNTS_HEADER_NAME`, and unenforceable when it names
    ///   none of them. An earlier version of this test asserted that every
    ///   account set was unenforceable, because no account-set header existed;
    ///   one exists now, so that assertion described a build that is gone.
    ///
    /// The spill arm is the one worth the config: it is the difference between
    /// "the lender's picker keeps this inside `work`" and "the lender's picker
    /// serves it on any account it likes", and nothing in the scope itself says
    /// which.
    ///
    /// Watched red by returning `ScopeRestriction::Group(name.clone())` for
    /// every group: the spill and the empty arms then both come back as a
    /// group header. And by dropping the `!self.reserved_groups().contains`
    /// term: the reserved-and-spill arm then comes back unenforceable.
    #[test]
    fn a_scope_is_a_group_header_only_when_the_picker_would_hold_it() {
        use crate::peer::serve::{ScopeRestriction, WindowUtilization as _};

        // `work` has a member and no spill; `loose` has a member and spills;
        // `empty` has neither. No real email, no org uuid: this repo is public.
        let config: config::Config = serde_json::from_str(
            r#"{
                "groupSettings": {
                    "loose": { "spillToPool": true },
                    "both": { "spillToPool": true, "reserved": true }
                },
                "accounts": [
                    {
                        "name": "alice-fake",
                        "accessToken": "not-a-real-token",
                        "groups": ["work"]
                    },
                    {
                        "name": "bob-fake",
                        "accessToken": "not-a-real-token",
                        "groups": ["loose", "both"]
                    }
                ]
            }"#,
        )
        .expect("the fixture is a valid config");
        let manager = Manager::with_live_refresher(config, None);

        assert_eq!(
            manager.scope_restriction(&tcr_peer_wire::LendScope::All),
            ScopeRestriction::Unrestricted
        );
        assert_eq!(
            manager.scope_restriction(&tcr_peer_wire::LendScope::Group("work".to_string())),
            ScopeRestriction::Group("work".to_string()),
            "a group with a member and no spill is held strictly by the picker"
        );
        assert_eq!(
            manager.scope_restriction(&tcr_peer_wire::LendScope::Group("loose".to_string())),
            ScopeRestriction::Unenforceable,
            "`spillToPool` makes the group header degrade to the whole pool, which for a \
             lease means serving an account outside the scope"
        );
        assert_eq!(
            manager.scope_restriction(&tcr_peer_wire::LendScope::Group("empty".to_string())),
            ScopeRestriction::Unenforceable,
            "a group with no member is a restriction the picker cannot satisfy"
        );
        assert_eq!(
            manager.scope_restriction(&tcr_peer_wire::LendScope::Group("both".to_string())),
            ScopeRestriction::Group("both".to_string()),
            "a RESERVED group ignores `spillToPool` and is held strictly, so the lease is \
             enforceable, the picker's rule is `reserved || !spill`, and reading only the \
             spill half refused a lease the picker would have honoured"
        );
        assert_eq!(
            manager.scope_restriction(&tcr_peer_wire::LendScope::Accounts(vec![
                "alice-fake".to_string()
            ])),
            ScopeRestriction::Accounts(vec!["alice-fake".to_string()]),
            "a label this fleet carries travels on the account-set header"
        );
        assert_eq!(
            manager.scope_restriction(&tcr_peer_wire::LendScope::Accounts(vec![
                "nobody-fake".to_string()
            ])),
            ScopeRestriction::Unenforceable,
            "a set naming no account here is a restriction the picker cannot satisfy, the \
             same answer an empty group gets"
        );
        assert_eq!(
            manager.scope_restriction(&tcr_peer_wire::LendScope::Accounts(vec![
                "bob-fake".to_string(),
                "nobody-fake".to_string()
            ])),
            ScopeRestriction::Accounts(vec!["bob-fake".to_string()]),
            "a set naming one account here and one it does not travels as the one it does: \
             the header can only ever NARROW the pick, so a name nothing matches would \
             widen nothing either"
        );
    }

    /// A window this build cannot name is one it cannot measure.
    #[test]
    fn an_unknown_window_lends_nothing() {
        let now = OffsetDateTime::now_utc();
        let mut a = account("alice-fake");
        a.quota.five_hour = measured(0.10, now);
        a.quota.seven_day = measured(0.10, now);
        a.quota.seven_day_oi = measured(0.10, now);
        assert_eq!(
            Manager::from_runtimes(vec![a]).lendable_fraction(
                &ALL,
                tcr_peer_wire::Window::Unknown,
                now
            ),
            0.0,
            "every window is measured here, so this can only be the Unknown arm refusing"
        );
    }
}
