//! The lease: a budget with a deadline, and the guard that outranks it.
//!
//! A lease is `{window, unit, fraction, ttl, max_inflight}`, which window, how
//! much of it, for how long, how many requests at once. The **lender's** ledger
//! is authoritative and persisted; the borrower's copy is a cached hint and
//! every surface that renders it says so.
//!
//! # Who debits
//!
//! The lender, exactly once, on observing the response, from the utilization
//! rise it measures on its own account. The borrower never debits. A retried or
//! diamond-delivered relay debits once, keyed on the request id.
//!
//! [`MIN_DEBIT`] is charged when the observed rise is zero, because the
//! granularity of the upstream utilization header is undocumented: "no rise"
//! and "a rise below the reporting step" are the same bytes, and a lease that
//! charges nothing for them never ends. **This is the one constant here that is
//! a guess rather than a measurement**, and the check that settles it is
//! reading consecutive header values off live responses and looking at the step
//! size. Do that before trusting the number.
//!
//! # A lease can never spend the owner's guard
//!
//! [`may_relay`] answers in a fixed order, expired, spent, then owner-guard,
//! and **the third fires on its own even with budget left on the lease**. It
//! inherits the failure mode the main config already records for the same
//! headers: they lag, upstream is the oracle, and it answers 200s for accounts
//! it is about to bench. That is exactly why a guard band exists rather than
//! the raw threshold.
//!
//! # `max_inflight` is not a nicety
//!
//! The debit reads lagging headers, so a lease granted at a small fraction with
//! unbounded concurrency can be overdrawn inside the lag window before the
//! first debit lands. The cap bounds the overdraft to a known number of
//! requests rather than to however many the borrower can open.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use axum::http::HeaderMap;
use axum::response::Response;
use futures::future::BoxFuture;
use tcr_peer_wire::{
    Control, Lease, LeaseGrant, LeaseRefusal, LeaseRequest, LeaseUnit, LendScope, PeerId,
    StreamHeader, StreamKind, Window,
};

use crate::fallback::{Ask, FallbackProvider};
use crate::peer::config::{LendGrant, PeerRow, PeerStore};
use crate::peer::serve;

/// Why one RELAYED REQUEST was refused, as opposed to why a LEASE was.
///
/// [`LeaseRefusal`] is the wire vocabulary and every variant of it describes the
/// lease. `max_inflight` is not about the lease, the lease is live, funded and
/// inside the owner's guard, it is about this request arriving while n others
/// are still running, and the honest answer names the cap rather than claiming
/// the lease is spent.
///
/// Typed here rather than as a sixth [`LeaseRefusal`] variant because that enum
/// is in `tcr_peer_wire` and has no such variant. **The wire needs the
/// variant**: until it has one, a lender can enforce the cap locally but cannot
/// tell the borrower which of the two happened, and a borrower that reads
/// `lease-spent` for a full pipe will stop asking when it should retry in a
/// moment. Reported, not worked around.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayRefusal {
    /// The lease itself said no. Pass straight to the borrower.
    Lease(LeaseRefusal),
    /// The lease is fine; this request is the (n+1)th at once.
    TooManyInflight {
        /// [`Lease::max_inflight`], so the refusal says what the cap is.
        max: u8,
    },
    /// **This `(lease_id, request_id)` pair has already been relayed here.**
    ///
    /// The review's H2: [`Ledger::debit`] was idempotent per pair and the SERVE
    /// was not, so one replayed `request_id` was served on the lender's account
    /// as many times as the borrower liked while [`Lease::spent`] moved exactly
    /// once, and `lease.spent >= budget` could then never become true. The
    /// lease was unbounded in the one dimension it exists to bound.
    ///
    /// Refused rather than served-and-charged-again: the pair is the accounting
    /// key, so a second serve under one key has no honest charge, and a borrower
    /// that really wants the request served again has a fresh id to ask with.
    ///
    /// **The wire word is [`LeaseRefusal::Unsupported`] and it is the wrong
    /// word**, reported rather than worked around: `LeaseRefusal` lives in
    /// `tcr_peer_wire`, and nothing here adds a variant to it. The variant
    /// that belongs there is
    /// `LeaseRefusal::RequestReplayed`, terminal like `Unsupported` (do not
    /// retry THIS id) rather than "retry in a moment" like
    /// [`LeaseRefusal::InFlightFull`].
    Replayed,
    /// The lease is live and funded, and it was granted to a DIFFERENT peer.
    ///
    /// The review's M1: every ledger entry point keyed on the lease id alone,
    /// so a lease was a bearer token, any pinned peer holding `inspect` could
    /// spend any lease id it learned, on the amount another peer negotiated.
    /// The authorization was the secrecy of a random number, and
    /// [`Ledger::grant`] already holds the authenticated peer one line before
    /// it mints the id.
    ///
    /// [`LeaseRefusal::LeaseExpired`] on the wire, deliberately: it is the
    /// answer this ledger already gives for a lease id it does not hold (see
    /// [`Ledger::may_relay`]), and a peer spending somebody else's id must not
    /// be told the difference between "that is not yours" and "there is no such
    /// lease", the first sentence is an oracle for lease ids.
    NotTheGrantee,
}

impl RelayRefusal {
    /// The wire word for this refusal.
    ///
    /// [`LeaseRefusal::InFlightFull`] exists for the second arm, added to
    /// `crates/tcr-peer-wire` for exactly this. Before it, a lender could
    /// enforce the cap and had no way to
    /// say so, and a borrower reading `lease-spent` for a full pipe stops asking
    /// when it should retry in a moment.
    pub fn to_wire(self) -> LeaseRefusal {
        match self {
            Self::Lease(refusal) => refusal,
            Self::TooManyInflight { .. } => LeaseRefusal::InFlightFull,
            // See [`Self::Replayed`]: the honest variant does not exist on the
            // wire yet and `Unsupported` is the closest terminal answer.
            Self::Replayed => LeaseRefusal::Unsupported,
            // See [`Self::NotTheGrantee`]: the SAME answer as a lease id this
            // ledger never minted, on purpose.
            Self::NotTheGrantee => LeaseRefusal::LeaseExpired,
        }
    }
}

/// The spendable budget of a lease, as a fraction, or `None` for a unit this
/// build refuses.
///
/// [`LeaseUnit::Tokens`] is defined on the wire and refused here, deliberately:
/// it is real for an api-key account and no account in this fleet lends one yet,
/// so the refusal is typed instead of arriving as a surprise comparison between
/// a token count and a utilization fraction.
fn fraction_budget(unit: LeaseUnit) -> Option<f64> {
    match unit {
        LeaseUnit::Fraction(fraction) => Some(fraction),
        LeaseUnit::Tokens(_) | LeaseUnit::Unknown => None,
    }
}

/// Is `now` inside `schedule`, and if not, the refusal to
/// answer with.
///
/// `None` on `schedule`, no schedule at all, what every grant written before
/// schedules existed means, is always permitted, same as `Schedule::always()` would
/// be. A parameter rather than a read off [`LendGrant`], mirroring
/// [`clamp_to_grant`]'s own `end: Option<u64>`: this stays pure and testable
/// at any instant without a peers file, and the one caller
/// ([`Ledger::grant`]) is the one that reads the grant's own field.
///
/// Called from [`Ledger::grant`], which reads
/// [`crate::peer::config::LendGrant::schedule`] off the grant it picked and
/// asks this before it measures the fleet: a Mac outside its lending hours
/// has nothing to measure for.
pub fn schedule_refusal(
    schedule: Option<&crate::peer::schedule::Schedule>,
    now: time::OffsetDateTime,
) -> Option<LeaseRefusal> {
    match schedule {
        Some(schedule) if !schedule.contains(now) => Some(LeaseRefusal::OutsideSchedule),
        _ => None,
    }
}

/// When the window `schedule` is open inside, at `now`, stops being open,
/// as unix milliseconds; `None` for a schedule with no intraday edge at all
/// ([`crate::peer::schedule::Schedule::always`]).
///
/// **Assumes `now` is already inside `schedule`**
/// ([`schedule_refusal`] answered `None` for it): the one caller
/// ([`Ledger::grant`]) asks this right after that check, on the same
/// reading. A `between` with no midnight crossing closes today at `end`; a
/// crossing window closes today at `end` for the tail half (past midnight,
/// before `end`) and tomorrow at `end` for the head half (past `start`,
/// before midnight). `between: None` with a `days` filter closes at the next
/// local midnight, the one intraday edge a day-only schedule has.
///
/// Built here rather than as a method on [`crate::peer::schedule::Schedule`]:
/// `Hhmm`'s and `Schedule`'s own fields are public, this is the only caller,
/// and the one thing this fix touches outside `lease.rs`'s own module is a
/// wire refusal that already existed, so the close-time arithmetic stays
/// beside the clamp ([`clamp_to_grant`]'s `schedule_close_ms`) it feeds
/// rather than widening `schedule.rs`, which this unit does not own.
fn schedule_closes_at_ms(
    schedule: &crate::peer::schedule::Schedule,
    now: time::OffsetDateTime,
) -> Option<i64> {
    let offset = time::UtcOffset::local_offset_at(now).unwrap_or(time::UtcOffset::UTC);
    let local = now.to_offset(offset);
    let minute = u16::from(local.hour()) * 60 + u16::from(local.minute());
    let (close_time, next_day) = match schedule.between {
        None => {
            // No time-of-day restriction, only a `days` filter: the one edge
            // that filter has is the day changing at local midnight.
            schedule.days.as_ref()?;
            (time::Time::MIDNIGHT, true)
        }
        Some((start, end)) => {
            let (start_m, end_m) = (start.minute_of_day(), end.minute_of_day());
            let close_time = time::Time::from_hms(end.hour(), end.minute(), 0).ok()?;
            match start_m.cmp(&end_m) {
                std::cmp::Ordering::Less => (close_time, false),
                std::cmp::Ordering::Greater => {
                    if minute < end_m {
                        (close_time, false)
                    } else {
                        (close_time, true)
                    }
                }
                // Never open (see `Schedule::contains_at_local`), so `now`
                // cannot honestly be inside it under this function's own
                // precondition; answered rather than reached.
                std::cmp::Ordering::Equal => return None,
            }
        }
    };
    let date = if next_day {
        local.date().next_day()?
    } else {
        local.date()
    };
    let close = date.with_time(close_time).assume_offset(local.offset());
    i64::try_from(close.unix_timestamp_nanos() / 1_000_000).ok()
}

/// What [`Ledger::grant`] answers: the frame the borrower gets, and the
/// operator's own grant that funded it.
///
/// # Why the funding grant leaves the function at all
///
/// A review found the caller re-deriving it. `listener.rs` read the
/// lease's MODE back out of the peers file with
/// `row.lend.iter().find(|g| g.window == minted.window)`, a window-only match,
/// while the lease had been funded by
/// [`crate::peer::config::PeerRow::grant_for`], which also skips a grant that
/// has ENDED and one whose scope this Mac's picker cannot hold. A row carrying
/// an ended `hand` grant and a live `serve` grant on the same window therefore
/// minted from the serve grant and handed the owner's bearer over under the
/// ended hand one.
///
/// So the funding grant is returned rather than looked up twice: one value,
/// read where it was decided, and no second filter that can disagree with the
/// first.
///
/// [`Self::funding`] is the grant this answer was CUT FROM, whether or not a
/// lease came out of it. A refusal reached before a grant was even picked
/// carries `None`.
#[derive(Debug, Clone)]
pub struct Granted {
    /// The answer that goes on the wire, unchanged: this type adds nothing to
    /// the frame and the frame carries no policy of the lender's.
    pub answer: LeaseGrant,
    /// The `LendGrant` the lease was funded by, as
    /// [`crate::peer::config::PeerRow::grant_for`] picked it. Never sent
    /// anywhere: it names the lender's own scope.
    pub funding: Option<LendGrant>,
    /// What this lease draws from, the same value [`Ledger::grant`] recorded
    /// beside the lease, and the argument
    /// [`crate::manager::Manager::handoff_bearer`] resolves an account out of.
    ///
    /// # Why it is carried out rather than read back
    ///
    /// The handoff arm used to re-read it with
    /// `serving.ledger.lock().map(|held| held.scope_of(id)).unwrap_or_default()`,
    /// so a POISONED ledger lock quietly became [`LendScope::All`] and the
    /// owner's bearer was then resolved against every account on the Mac
    /// instead of the ones the operator lent. A second lock is what made the
    /// silent fallback expressible at all, so it is gone: the scope leaves the
    /// function that decided it, in the same value as the answer and the
    /// funding grant.
    pub scope: LendScope,
}

impl Granted {
    /// A refusal decided before any grant was picked, so there is nothing that
    /// funded it.
    fn unfunded(refusal: LeaseRefusal) -> Self {
        Self {
            answer: LeaseGrant {
                lease: None,
                refusal: Some(refusal),
            },
            funding: None,
            scope: LendScope::All,
        }
    }

    /// The mode the lease was funded under, which is the one question the
    /// handoff arm asks. [`crate::peer::config::LendMode::Serve`] when no grant
    /// funded it, because the refusal path hands nothing over either way.
    pub fn mode(&self) -> crate::peer::config::LendMode {
        self.funding
            .as_ref()
            .map_or_else(Default::default, |grant| grant.mode)
    }
}

/// The whole of the lender's grant arithmetic, as a pure function: what the
/// borrower asked for, clamped by what this peer was granted, clamped again by
/// what the owner's guard band actually leaves.
///
/// Pure and separate from [`Ledger::grant`] so it can be tested without a peers
/// file and without a random source, and so a reader can see that the clamps are
/// the only decision being made. The order is the point, the narrowest of the
/// three always wins, and `lendable` is last because it is the only one measured
/// rather than configured.
///
/// `inspect` is the LENDER's side of the two opt-ins. The borrower's
/// `allow.disclose` is checked on the borrower, before it ever asks, which is
/// why it is not a parameter here: a lender that enforced both would be
/// pretending it can see the borrower's config.
///
/// `max_inflight` is passed through as the operator configured it, `0` included:
/// a `0` grant mints a lease that [`Ledger::enter_relay`] refuses every time
/// with the cap named, which is what an operator who wrote `0` asked for. It is
/// not silently raised to `1`.
/// `end` is the absolute end of the LENDING, in unix seconds, and it
/// is the fourth clamp: a grant whose end has already passed mints nothing at
/// all, and one still ahead is copied onto the lease so the borrower can render
/// "ends in 1 h" without a second round trip.
///
/// It is a PARAMETER rather than read off `granted`
/// ([`crate::peer::config::LendGrant::until`], which is where it is now stored)
/// so this function stays pure and testable at any instant: the clamp being
/// decided is "is this end ahead of `now_ms`", and a function that dug the end
/// out of the grant itself could not be asked about an end the grant does not
/// carry. [`Ledger::grant`] is the one caller that passes `grant.until`.
///
/// `schedule_close_ms` is the FIFTH clamp, on [`Lease::expires_at_ms`] rather
/// than on whether a lease mints at all: `--between`/`--days` is consulted
/// once, here, by [`Ledger::grant`], and a lease minted a minute before the
/// window closes used to keep its full TTL, so it went on serving hours past
/// the hours it was lent for because nothing after mint ever asked the
/// schedule again. `expires_at_ms` is already documented as the renewal TTL a
/// borrower re-asks at, never lowered below what the schedule allows, so
/// clamping it here means the re-ask lands back in [`Ledger::grant`], which
/// refuses `OutsideSchedule` honestly once the window is shut, the same
/// answer a borrower asking for the first time after close already gets. A
/// `None` here is "this grant has no schedule, or the mint is already
/// refused for a different reason", never "the window is always open"; the
/// caller passes `None` in both cases and this function does not tell them
/// apart, because neither one clamps.
#[allow(clippy::too_many_arguments)]
pub fn clamp_to_grant(
    ask: &LeaseRequest,
    granted: Option<LendGrant>,
    inspect: bool,
    lendable: f64,
    now_ms: i64,
    lease_id: u128,
    end: Option<u64>,
    schedule_close_ms: Option<i64>,
) -> LeaseGrant {
    let refuse = |refusal: LeaseRefusal| LeaseGrant {
        lease: None,
        refusal: Some(refusal),
    };
    if !inspect {
        return refuse(LeaseRefusal::InspectNotGranted);
    }
    if ask.window == Window::Unknown {
        return refuse(LeaseRefusal::Unsupported);
    }
    let Some(wanted) = fraction_budget(ask.unit) else {
        return refuse(LeaseRefusal::Unsupported);
    };
    // No grant for this window is not "zero of it": this peer was never told it
    // could borrow this window at all.
    let Some(grant) = granted.filter(|grant| grant.window == ask.window) else {
        return refuse(LeaseRefusal::InspectNotGranted);
    };
    let amount = wanted.min(grant.fraction).min(lendable);
    // A grant below MIN_DEBIT is not a small lease, it is a lease the first
    // request overdraws. `OwnerGuard` rather than `LeaseSpent`, because nothing
    // has been spent: the owner's own headroom is what is missing.
    if amount < MIN_DEBIT || amount.is_nan() {
        return refuse(LeaseRefusal::OwnerGuard);
    }
    // The end, checked here rather than left for `may_relay`: a lease
    // minted past its own end is a lease whose first relay is refused, and
    // refusing to mint it is the same answer told one round trip earlier. The
    // comparison is in SECONDS because that is the unit `Lease::until` carries
    // and converting the deadline (rather than the clock) would round the
    // operator's 18:00 to something else.
    if end.is_some_and(|end| end <= seconds_of(now_ms)) {
        return refuse(LeaseRefusal::LeaseExpired);
    }
    let ttl_s = ask.ttl_s.min(grant.ttl_s);
    let expires_at_ms = now_ms + i64::from(ttl_s) * 1_000;
    // THE FIFTH CLAMP. See this function's own doc: a schedule is consulted
    // at mint only, so the TTL is never allowed to outlive the window it was
    // minted inside.
    let expires_at_ms = match schedule_close_ms {
        Some(close_ms) => expires_at_ms.min(close_ms),
        None => expires_at_ms,
    };
    LeaseGrant {
        lease: Some(Lease {
            lease_id,
            window: ask.window,
            unit: LeaseUnit::Fraction(amount),
            granted_at_ms: now_ms,
            expires_at_ms,
            spent: 0.0,
            max_inflight: ask.max_inflight.min(grant.max_inflight),
            until: end,
        }),
        refusal: None,
    }
}

/// Unix milliseconds as unix SECONDS, floored, for a comparison against
/// [`Lease::until`].
///
/// Floored and not rounded, and negative-safe (`div_euclid`), because the one
/// question asked of it is "has this instant passed?", rounding up would
/// answer yes for the last half-second of a lease the operator still owns.
fn seconds_of(now_ms: i64) -> u64 {
    u64::try_from(now_ms.div_euclid(1_000)).unwrap_or(0)
}

/// Turn `--for <d>` / `--until <clock>` into [`Lease::until`]:
/// an absolute unix second, or `None` for "no end".
///
/// The clock is INJECTED (`now`) rather than read here, which is what makes the
/// gate a measurement: `--for 2h` is asserted to write exactly two hours past a
/// chosen instant, which a function reading the wall clock could only be
/// asserted about approximately.
///
/// Three spellings, and nothing else:
///
/// - `none` or an empty string: no end. Named rather than implied, so an
///   operator can clear an end with the same flag they set it with.
/// - a duration: `30s`, `90m`, `2h`, `3d`, which is `--for`;
/// - a wall clock: `18:00` or `18:00:30`, which is `--until`, resolved in the
///   OPERATOR's local offset (`UtcOffset::local_offset_at`, the same
///   fallback-to-UTC pattern [`crate::peer::schedule::Schedule::contains`]
///   uses, since the lookup refuses in a multithreaded process). A clock that
///   has already passed today is REFUSED, never rolled to tomorrow: rolling
///   silently is what let a lease edited through the panel at 18:00 for
///   "tomorrow at 18:00" round-trip as an end later THIS minute instead,
///   because the panel's own refusal (`LeaseDraft.refusal`) and this
///   function disagreed about which clock a bare `18:00` resolves against.
///   The refusal names the time it resolved to, so `--for 24h` or an
///   absolute end is the way forward, not a second bare clock.
///
/// A bare number is REFUSED rather than read as seconds or as an hour: `2` is
/// two hours to one reader and two seconds to another, and a lease is not
/// something to guess a unit on.
pub fn parse_lend_end(spec: &str, now: time::OffsetDateTime) -> Result<Option<u64>> {
    let offset = time::UtcOffset::local_offset_at(now).unwrap_or(time::UtcOffset::UTC);
    parse_lend_end_at_local(spec, now.to_offset(offset))
}

/// [`parse_lend_end`]'s decision, given `local_now` already resolved to the
/// operator's own offset.
///
/// Split out for the reason [`crate::peer::schedule::Schedule::contains_at_local`]
/// is: this is what the unit tests below exercise directly, so they assert
/// this function's arithmetic and never the host machine's timezone, and
/// `local_offset_at` is not called a second time somewhere that would refuse
/// it in a multithreaded process.
fn parse_lend_end_at_local(spec: &str, local_now: time::OffsetDateTime) -> Result<Option<u64>> {
    let spec = spec.trim();
    if spec.is_empty() || spec.eq_ignore_ascii_case("none") {
        return Ok(None);
    }

    if let Some((hours, minutes, seconds)) = parse_clock(spec) {
        let wanted = time::Time::from_hms(hours, minutes, seconds).with_context(|| {
            format!("peer lend: {spec} is not a time of day (00:00:00 to 23:59:59)")
        })?;
        let at = local_now.replace_time(wanted);
        if at <= local_now {
            anyhow::bail!(
                "peer lend: {spec} has already passed today (it resolved to {}), so that end \
                 is in the past; give a clock time still ahead today, or an absolute end \
                 instead of a bare `--until`",
                at.format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_else(|_| spec.to_string())
            );
        }
        return Ok(Some(u64::try_from(at.unix_timestamp()).unwrap_or(0)));
    }

    let split_at = spec
        .char_indices()
        .next_back()
        .map_or(0, |(byte_index, _)| byte_index);
    let (digits, unit) = spec.split_at(split_at);
    let multiplier = match unit {
        "s" => 1_i64,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        _ => anyhow::bail!(
            "peer lend: {spec:?} is neither a duration (`30s`, `90m`, `2h`, `3d`) nor a time \
             of day (`18:00`); a bare number is refused rather than guessed at"
        ),
    };
    let count: i64 = digits
        .parse()
        .with_context(|| format!("peer lend: {digits:?} in {spec:?} is not a whole number"))?;
    if count <= 0 {
        anyhow::bail!(
            "peer lend: {spec:?} is not a duration into the future; `--for none` is how a \
             lease is lent with no end"
        );
    }
    let offset = count.checked_mul(multiplier).with_context(|| {
        format!("peer lend: {spec:?} is too large a duration to compute an end from")
    })?;
    let end = local_now
        .unix_timestamp()
        .checked_add(offset)
        .with_context(|| {
            format!("peer lend: {spec:?} is too large a duration to compute an end from")
        })?;
    Ok(Some(u64::try_from(end).unwrap_or(0)))
}

/// `HH:MM` or `HH:MM:SS` as its three numbers, or `None` for anything else.
///
/// Split out so [`parse_lend_end_at_local`] reads as the three spellings it
/// accepts rather than as a parser: the interesting decision in that function
/// is refusing a clock already behind local now, and it is invisible inside a
/// `split(':')` chain.
fn parse_clock(spec: &str) -> Option<(u8, u8, u8)> {
    let mut parts = spec.split(':');
    let hours: u8 = parts.next()?.parse().ok()?;
    let minutes: u8 = parts.next()?.parse().ok()?;
    let seconds: u8 = match parts.next() {
        Some(seconds) => seconds.parse().ok()?,
        None => 0,
    };
    if parts.next().is_some() {
        return None;
    }
    Some((hours, minutes, seconds))
}

/// What is charged when the observed utilization rise is 0.000. See the module
/// docs: a guess, flagged as one, with the measurement that would replace it.
pub const MIN_DEBIT: f64 = 0.002;

/// How many `(lease_id, request_id)` pairs one ledger remembers.
///
/// The same figure [`crate::peer::listener::REQUEST_DEDUP_CAPACITY`] uses, and
/// named through it rather than re-typed: the two caches answer the same
/// question at two layers (the stream gate per connection, this one per
/// process) and two spellings of one bound is how they come to disagree about
/// what a burst is.
pub const SERVED_CAPACITY: usize = crate::peer::listener::REQUEST_DEDUP_CAPACITY;

/// How long a served pair is remembered. Same figure, same reason.
pub const SERVED_TTL_MS: i64 = crate::peer::listener::REQUEST_DEDUP_TTL_MS;

/// How many request ids are remembered **per lease**.
///
/// [`SERVED_CAPACITY`] is the whole process's bound and it used to be the only
/// one: one FIFO for every lease, so a borrower sending 4 096 requests of its
/// own evicted every other borrower's pairs, and the replay defence for those
/// leases went quiet. Worse, it could evict its OWN older pair and then replay
/// it for free.
///
/// So the cache is per lease and the eviction is per lease: a busy borrower
/// can only ever push its own oldest id out, which is the one direction that
/// costs nobody else anything. The process-wide bound is
/// [`SERVED_CAPACITY`] still, applied across leases by
/// [`Ledger::admit_pair`], so the memory is bounded by the same figure it
/// always was.
pub const SERVED_PER_LEASE_CAPACITY: usize = 256;

/// How many LIVE leases one pinned peer may hold here at once.
///
/// The review's MEDIUM: nothing capped how many leases one peer could mint, and
/// every mint rewrites the whole state file under the same [`FileLock`] that
/// `tcr peer accept` and `tcr peer block` take with a hard five-second
/// give-up, so an authenticated peer asking in a loop was both an unbounded
/// ledger and an operator whose Accept could not get the lock.
///
/// Eight, and per GRANTEE rather than per Mac, because the honest question is
/// "how many leases does one borrower legitimately need at once" and the answer
/// is a small number: a lease is asked for per window, held for its ttl and
/// renewed rather than re-minted ([`PeerLeaseProvider::lease_for`] re-asks only
/// when its cached one has expired or the window changed). Two windows and a
/// couple of retries in flight is inside it; a loop is not.
///
/// A cap and never a queue: the (n+1)th is REFUSED, the same decision
/// [`Lease::max_inflight`] makes one layer down and for the same reason, a
/// borrower told no can serve the request locally now, and a borrower made to
/// wait learns nothing and still holds the client.
///
/// # The wire has its own word for this refusal
///
/// It is answered as [`LeaseRefusal::TooManyLeases`], not
/// [`LeaseRefusal::OwnerGuard`]: the lease is refused by how many this peer
/// already holds, not by anything about the owner's headroom or the ask, and
/// a borrower that cannot tell the two apart either hammers a dead lease or
/// backs off from a live one for the wrong reason.
pub const MAX_LEASES_PER_PEER: usize = 8;

/// The borrower's side of `hand` mode: the owner's short-lived
/// bearers, in memory, keyed by lease.
///
/// # Memory only, and that is the whole type
///
/// There is no `Serialize`, no `Deserialize`, no path, and nothing in `Config`
/// or in the peers file reaches it. A handed bearer is the one credential that
/// crosses a host boundary in this design, so the machine that receives it must
/// not be able to write it down: a restart loses every handed token and the
/// owner pushes fresh ones, which costs one round trip and is the correct price.
///
/// `rg -n 'HandedTokens' src/config.rs src/manager/mod.rs` is the containment
/// gate, and it needs a positive control beside it: an empty grep with no
/// control is a claim about the probe, not about the code.
///
/// # What the owner can and cannot take back
///
/// Revocation is "stop renewing". [`Self::forget`] drops the
/// local copy when the lease ends or the owner revokes, but a token already
/// handed over cannot be recalled: that is a property of bearer tokens, not of
/// this type, and it is why the bearer handed over is the SHORT-LIVED access
/// token and never the refresh token.
#[derive(Debug, Default)]
pub struct HandedTokens {
    by_lease: HashMap<u128, (String, i64)>,
}

impl HandedTokens {
    /// An empty store, for a test about this type and nothing else.
    ///
    /// **A store built here is not the store the request path reads, and a
    /// test that mixes the two reads it as a broken static.**
    /// [`PeerLeaseProvider::try_serve`]'s hand arm can only ask
    /// [`handed_tokens`], the one store this process has, and no seam hands it
    /// another. So a test that puts a bearer into its OWN `HandedTokens` and
    /// then drives the provider sees `len 1` in front of it and an empty store
    /// inside the library, microseconds apart, for the same lease id: two
    /// stores by construction, which looks exactly like one store compiled
    /// twice and is not.
    ///
    /// A test that drives the PATH therefore puts into [`handed_tokens`] and
    /// calls [`Self::forget`] on the way out, so the next test in the same
    /// binary does not inherit the bearer; `tests/peer_hand.rs` and
    /// `tests/peer_lease.rs` both do that. A test about the type itself builds
    /// one here.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the bearer the owner just pushed for `lease_id`, replacing any
    /// earlier one. A renewal is this call: the owner pushes before the old
    /// bearer expires, so the borrower never has a gap to handle.
    pub fn put(&mut self, lease_id: u128, token: String, expires_at_ms: i64) {
        self.by_lease.insert(lease_id, (token, expires_at_ms));
    }

    /// The live bearer for `lease_id`, or `None` when there is none or the one
    /// held has expired.
    ///
    /// Expiry is checked on READ rather than swept on a timer: a sweep is a
    /// second clock to get wrong, and a bearer whose deadline passed while
    /// nothing asked for it did no harm sitting in a map.
    pub fn bearer(&self, lease_id: u128, now_ms: i64) -> Option<&str> {
        self.by_lease
            .get(&lease_id)
            .filter(|(_, expires_at_ms)| *expires_at_ms > now_ms)
            .map(|(token, _)| token.as_str())
    }

    /// Drop the bearer for one lease: the lease ended, the owner revoked, or
    /// the borrower gave it back.
    pub fn forget(&mut self, lease_id: u128) -> bool {
        self.by_lease.remove(&lease_id).is_some()
    }

    /// How many leases this store holds a bearer for, expired or not. For a
    /// test and for a log line that must never print a token.
    pub fn len(&self) -> usize {
        self.by_lease.len()
    }

    /// Whether the store holds nothing at all.
    pub fn is_empty(&self) -> bool {
        self.by_lease.is_empty()
    }
}

/// The one store this process reads on the request path.
///
/// A `OnceLock` behind a `Mutex`, for the reason the fallback provider is one:
/// the alternative is threading a handle from boot through the proxy's request
/// loop, and the handle would then be a field on something `Config` can reach,
/// which is exactly the containment this type exists to keep.
pub fn handed_tokens() -> &'static Mutex<HandedTokens> {
    static HANDED: std::sync::OnceLock<Mutex<HandedTokens>> = std::sync::OnceLock::new();
    HANDED.get_or_init(|| Mutex::new(HandedTokens::new()))
}

/// The control session a hand-mode borrow can report its spend back on, per
/// lease.
///
/// # Why this exists at all
///
/// A hand-mode lease was never debited by anybody. `usage_hint` built the
/// frame and had no production caller: the owner hands a bearer over, the
/// borrower serves from its own Mac, and nothing told the owner what the lease
/// had cost, so `Ledger::debit` ran only for `serve` mode and a hand-mode
/// lease's `spent` stayed at 0.0 for its whole life. The ceiling
/// `Ledger::may_relay` enforces was therefore never reached, and the owner's
/// own `tcr peer ls` said a lease that had spent the account's window was
/// untouched.
///
/// The session that carries the hint is the one the lease was granted on: the
/// owner already keeps it open to push and renew the bearer
/// ([`read_handed_bearers`]), so the hint goes back the way the bearer came,
/// on a stream both sides have authenticated, rather than opening a second one
/// per request.
///
/// [`HandedMeter::last`] is what makes a rise measurable on this side at all.
/// The borrower never sees the owner's window except through the rate-limit
/// headers on its own answers, so the pair `usage_hint` needs is the PREVIOUS
/// answer's figure and this one's. The first answer for a lease reports
/// nothing: one reading is not a rise.
#[derive(Debug)]
struct HandedMeter {
    /// Where a hint frame is queued for the reader task that owns the stream.
    sink: tokio::sync::mpsc::UnboundedSender<Control>,
    /// The window utilization the previous answer on this lease reported.
    last: Option<f64>,
}

/// Every live hand-mode lease's meter, keyed by lease id.
///
/// Process-local and unexported, for [`handed_tokens`]' reason: a hand-mode
/// lease's session is a live socket, not configuration, and a handle threaded
/// from boot through the request loop would put it on something `Config` can
/// reach.
fn handed_meters() -> &'static Mutex<HashMap<u128, HandedMeter>> {
    static METERS: std::sync::OnceLock<Mutex<HashMap<u128, HandedMeter>>> =
        std::sync::OnceLock::new();
    METERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register the channel a lease's hints go out on. Replaces any earlier one:
/// a lease is granted on one session, and a second grant of the same id would
/// be a lender contradicting itself.
fn register_handed_meter(lease_id: u128, sink: tokio::sync::mpsc::UnboundedSender<Control>) {
    if let Ok(mut meters) = handed_meters().lock() {
        // THE BASELINE SURVIVES the channel being replaced. A lease whose
        // session is re-established, or which is re-asked for inside its own
        // life, is the same lease against the same window, and a meter reset to
        // `None` here makes the next answer free: the first reading is not a
        // rise, so it charges nothing at all. Only the sink is new.
        let last = meters.get(&lease_id).and_then(|meter| meter.last);
        meters.insert(lease_id, HandedMeter { sink, last });
    }
}

/// Take the owner's own utilization, off a [`Control::Handoff`], as the figure
/// this lease's next answer is measured against.
///
/// **Monotone: `max`, never a replacement.** With `max_inflight` above one, two
/// answers can land out of order, and a baseline moved BACKWARDS by the older
/// of them turns the next answer's rise into a second charge for quota already
/// billed. A window that genuinely reset reads as a fall, which is the owner's
/// windfall exactly as [`Ledger::debit`] says, and never a credit here.
fn note_handed_baseline(lease_id: u128, utilization: f64) {
    if !utilization.is_finite() {
        return;
    }
    if let Ok(mut meters) = handed_meters().lock() {
        if let Some(meter) = meters.get_mut(&lease_id) {
            meter.last = Some(meter.last.map_or(utilization, |last| last.max(utilization)));
        }
    }
}

/// Drop a lease's meter when its session ends, so this map holds only live
/// leases. Same rule as [`HandedTokens::forget`] and for the same reason.
fn forget_handed_meter(lease_id: u128) {
    if let Ok(mut meters) = handed_meters().lock() {
        meters.remove(&lease_id);
    }
}

/// Report what one hand-mode answer cost, on the lease's own window.
///
/// `utilization` is what the answer's own rate-limit headers said; `None` (an
/// answer that reported nothing about the window) leaves the meter untouched,
/// because "the window did not move" and "this answer said nothing about it"
/// are different facts and charging the second as the first is how a lease
/// stops tracking anything.
///
/// Never blocks and never fails the request: a hint that cannot be sent is a
/// debit the owner does not learn about, which is worth a line and is not worth
/// failing a served request over.
fn report_handed_spend(lease_id: u128, utilization: Option<f64>) {
    let Some(utilization) = utilization else {
        return;
    };
    let Ok(mut meters) = handed_meters().lock() else {
        return;
    };
    let Some(meter) = meters.get_mut(&lease_id) else {
        return;
    };
    let before = meter.last;
    // `max`, not `replace`, for [`note_handed_baseline`]'s reason: with
    // `max_inflight` above one the answers arrive in whatever order the network
    // hands them over, and a baseline pushed backwards by the older one bills
    // the same quota twice on the next answer.
    meter.last = Some(before.map_or(utilization, |last| last.max(utilization)));
    let Some(hint) = usage_hint(lease_id, before, Some(utilization)) else {
        return;
    };
    if meter.sink.send(hint).is_err() {
        tracing::debug!(
            "peer hand: the lease's control session has ended, so this spend is not \
             reported to its owner"
        );
    }
}

/// The utilization one window's rate-limit header reported on a response, or
/// `None` when this answer said nothing about that window.
///
/// The header names are `src/quota.rs`'s own; read here rather than through
/// `Quota::update_from_headers` because that merges into an account's quota,
/// and a borrower has no account of the owner's to merge into. It is one
/// figure, for one window, off one answer.
fn reported_utilization(head: &[(String, String)], window: Window) -> Option<f64> {
    let name = match window {
        Window::FiveHour => "anthropic-ratelimit-unified-5h-utilization",
        Window::SevenDay => "anthropic-ratelimit-unified-7d-utilization",
        Window::SevenDayOi => "anthropic-ratelimit-unified-7d_oi-utilization",
        Window::Unknown => return None,
    };
    head.iter()
        .find(|(header, _)| header.eq_ignore_ascii_case(name))
        .and_then(|(_, value)| value.parse::<f64>().ok())
        .filter(|utilization| utilization.is_finite())
}

/// Whether the owner should push a [`Control::Handoff`] for this lease right
/// now, and what it carries.
///
/// The three refusals are the whole of "only, and stops at":
///
/// - a `Serve` grant is never handed anything, whatever else is true;
/// - a lease past its `until` is over, so the owner stops renewing (that IS the
///   revoke, and there is no recall frame);
/// - a bearer the owner cannot produce for the grant's scope is not a reason to
///   send an empty frame.
///
/// The caller is responsible for the fourth condition, which this function
/// cannot see: the frame goes out only on an `IK`-authenticated CONTROL session
/// to the grantee of `lease`. That is a property of the socket, not of the
/// arguments, and `Ledger::grantee_of` is what the sender checks it against.
pub fn handoff_for(
    mode: crate::peer::config::LendMode,
    lease: &Lease,
    bearer: Option<crate::manager::HandedCredential>,
    now_ms: i64,
) -> Option<Control> {
    use crate::peer::config::LendMode;
    if mode != LendMode::Hand {
        return None;
    }
    if lease_has_ended(lease, now_ms) {
        return None;
    }
    let crate::manager::HandedCredential {
        access_token,
        expires_at_ms,
        utilization,
    } = bearer?;
    // A bearer that has already expired is not a handoff, it is a frame that
    // costs the borrower a request to discover is useless. The lease's own
    // expiry is checked above and they are different clocks: a live lease can
    // sit beside a credential whose refresh has not happened yet, and this
    // function used to hand that one over.
    if expires_at_ms <= now_ms {
        return None;
    }
    Some(Control::Handoff {
        lease_id: lease.lease_id,
        access_token: tcr_peer_wire::HandoffToken::new(access_token),
        expires_at_ms,
        // THE BASELINE, and the reason this frame carries a figure at all. The
        // borrower can only see the owner's window through the rate-limit
        // headers on its own answers, so a rise needs a reading from BEFORE the
        // first of them; without one the first answer on every lease was free,
        // and a borrower that re-asked per request was never charged for
        // anything. See [`HandedMeter`] and [`usage_hint`].
        utilization: Some(utilization),
    })
}

/// Has this lease stopped, by its own expiry or by the lending's `until`?
///
/// `until` is in unix SECONDS (the peers file and the wire both say so) and
/// everything else here is milliseconds, so the comparison converts once, in
/// one place, rather than at each of the callers that would otherwise each pick
/// a unit.
pub fn lease_has_ended(lease: &Lease, now_ms: i64) -> bool {
    if lease.expires_at_ms <= now_ms {
        return true;
    }
    lease.until.is_some_and(|until_s| {
        // `until` is unix SECONDS and unsigned; a value too large for an
        // `i64` of milliseconds is a date no clock will reach, which is
        // "not ended" and never a panic.
        i64::try_from(until_s.saturating_mul(1_000)).is_ok_and(|end_ms| end_ms <= now_ms)
    })
}

/// Should the owner push a FRESH bearer before the one it last handed over
/// stops working?
///
/// `handed_expires_at_ms` is what the last [`Control::Handoff`] carried, and
/// `None` means none was ever sent. The lead time is [`HANDOFF_RENEW_LEAD_MS`].
///
/// Read rather than scheduled, for the reason the expiry check in
/// [`HandedTokens::bearer`] is read rather than swept: a laptop sleeps, and a
/// timer that fired while the lid was shut is a renewal nobody received.
pub fn handoff_renewal_due(handed_expires_at_ms: Option<i64>, now_ms: i64) -> bool {
    match handed_expires_at_ms {
        None => true,
        Some(expires_at_ms) => expires_at_ms - now_ms <= HANDOFF_RENEW_LEAD_MS,
    }
}

/// What one session last handed over for one lease.
///
/// # Why the token's fingerprint is here and not just its expiry
///
/// [`handoff_renewal_due`] is true for the whole lead time, and the control
/// loop asks it once a second. With only the expiry recorded, an owner whose
/// credential had not been refreshed yet re-sent the SAME bearer on every poll
/// for the last five minutes of its life: about three hundred identical frames
/// per lease, each one a Noise message the borrower decodes and stores over an
/// identical copy. The renewal is due for as long as the token has not
/// changed, so "due" cannot be the whole test; what makes a handoff worth
/// sending is that the bearer is DIFFERENT from the one the borrower holds,
/// and that is what the fingerprint answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandedBearer {
    /// When the bearer that went out stops working, off the `Handoff` frame.
    pub expires_at_ms: i64,
    /// [`bearer_fingerprint`] of the token that went out.
    pub fingerprint: u64,
}

/// A fingerprint of a bearer token, for telling one from another without
/// keeping a second copy of the credential.
///
/// In-memory only, for the life of one session, which is why the standard
/// library's hasher is enough: nothing compares a fingerprint across processes
/// or writes one anywhere, and the only question ever asked of it is "is this
/// the same token I already sent on this session". A credential never reaches
/// a log, a file or the wire through this: a `u64` is not the token, and it is
/// not treated as a secret either, because it is never sent.
/// Should the owner send `next` on a session that last sent `last`?
///
/// Two questions, and both have to be yes: the renewal is DUE
/// ([`handoff_renewal_due`]), and the bearer is actually different from the one
/// that went out. The second is what keeps one push per token instead of one
/// per poll: the lead time is five minutes and the control loop polls every
/// second, so "due" alone re-sends an unchanged token about three hundred
/// times.
///
/// A pure function rather than a condition inline in the loop, because the
/// claim worth measuring is about a SEQUENCE of polls, and a test can run five
/// simulated minutes of them through this in a millisecond.
pub fn handoff_push_is_due(last: Option<HandedBearer>, next: HandedBearer, now_ms: i64) -> bool {
    if !handoff_renewal_due(last.map(|sent| sent.expires_at_ms), now_ms) {
        return false;
    }
    !last.is_some_and(|sent| sent.fingerprint == next.fingerprint)
}

pub fn bearer_fingerprint(access_token: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    access_token.hash(&mut hasher);
    hasher.finish()
}

/// The borrower's quota hint for one hand-mode response, or `None` when there
/// is nothing honest to report.
///
/// # Read from the response, not from a counter
///
/// `before` and `after` are the window's utilization as the rate-limit headers
/// reported it either side of one request, which is the same evidence the owner
/// debits from in `serve` mode ([`crate::peer::serve::utilization_rise`]) read
/// on the machine that actually got the response. A response with no rate-limit
/// header reports `None` rather than zero: "the window did not move" and "this
/// response said nothing about the window" are different facts, and charging
/// the second as the first is how a lease stops tracking anything.
///
/// A window that RESET under the request reads as a fall. That is `None` too:
/// a reset is the owner's windfall, exactly as [`Ledger::debit`] says, and a
/// borrower has no business reporting a negative.
pub fn usage_hint(lease_id: u128, before: Option<f64>, after: Option<f64>) -> Option<Control> {
    let (before, after) = (before?, after?);
    if !before.is_finite() || !after.is_finite() {
        return None;
    }
    let rise = after - before;
    if rise <= 0.0 {
        return None;
    }
    Some(Control::UsageHint {
        lease_id,
        // THE FLOOR. The utilization header moves in steps, so a request whose
        // cost is smaller than one step reports a rise of zero on the answer
        // that paid for it and the whole of it later, on some other lease's
        // answer or on none at all. The serve path charges [`MIN_DEBIT`] for
        // exactly this case (`Ledger::debit`), and a hand lease that did not
        // was a lease a small-request borrower could hold open indefinitely.
        //
        // It floors a rise that is already POSITIVE and never invents one: an
        // answer that moved nothing is still free, which is what keeps "it can
        // only raise `spent`" honest rather than turning every request into a
        // charge.
        spent: rise.max(MIN_DEBIT),
    })
}

/// The baseline a [`Control::Handoff`] brings, or the line an operator has to
/// read instead.
///
/// # A handoff with no baseline is refused, and the lender is NAMED
///
/// The figure is what a borrowed answer's spend is a rise above
/// ([`usage_hint`]), so a bearer that arrives without one is a bearer whose
/// every answer this Mac would take for free: the owner's ledger would show a
/// lease that was never spent and its ceiling would never bind. Only a build
/// OLDER than this one leaves it out, which is why the refusal names the Mac to
/// update rather than reporting a protocol error nobody can act on. The lease
/// itself stands and the borrow falls back to the lender's own serve path.
///
/// A function returning the line, rather than a `warn!` inline, so a test can
/// assert that the peer is named: "refused" and "refused in a way the operator
/// can act on" are different claims and the second one needs a value.
pub fn handed_baseline(lender: &PeerId, utilization: Option<f64>) -> Result<f64, String> {
    match utilization {
        Some(utilization) if utilization.is_finite() => Ok(utilization),
        _ => Err(format!(
            "peer lease: {} handed this Mac a bearer without the utilization its spend would \
             be measured against, which is what a build older than this one sends, so the \
             bearer is refused and this borrow stays on that Mac's own path until it is \
             updated",
            lender.display()
        )),
    }
}

/// How long before a handed bearer expires the owner pushes the next one.
///
/// Five minutes, which is the same order as the lease ttl the defaults use
/// (300 s) and comfortably longer than a LAN round trip plus a token refresh.
/// It is a choice, not a measurement: the figure that would settle it is the
/// observed spread of `expires_at` on a refreshed credential, which nobody
/// has measured.
pub const HANDOFF_RENEW_LEAD_MS: i64 = 5 * 60 * 1_000;

/// Serve one borrowed request LOCALLY, on the bearer the owner handed over.
///
/// This is the half of the rule that makes `hand` mode different from
/// `serve` mode at all: the bytes never touch the owner's Mac, so the request
/// leaves from this node's own client and the owner never sees the plaintext.
/// In `serve` mode the same lease would have gone through
/// [`serve::open_serve`] and out of the owner's machine instead.
///
/// `client` is supplied by the caller rather than built here, because the
/// caller is the only code that knows which client this node's egress policy
/// says to use, and because a test points one at a fake origin with
/// `reqwest::ClientBuilder::resolve` (the escape `src/peer/egress.rs` already
/// documents: it overrides DNS only, so TLS still validates the real hostname).
///
/// `Ok(None)` for "no live handed bearer for this lease", which is not an
/// error: the owner may not have pushed one yet, or the lease may have ended.
/// The fallback ladder's next rung answers the client, and an unauthenticated
/// call to upstream never happens.
pub async fn serve_on_handed_bearer(
    upstream_base: &str,
    client: &reqwest::Client,
    ask: &Ask<'_>,
    lease_id: u128,
    window: Window,
    now_ms: i64,
) -> Result<Option<Response>> {
    // The client's own headers, narrowed to the names a lender forwards, for
    // the reason `try_serve` hands them to `open_serve`: this request goes to
    // the API, and the API answers 400 to one with no `anthropic-version`. Hand
    // mode sends from THIS Mac, so the narrowing that `serve_request_from` does
    // on the wire is done here instead; `build_handed_bearer_headers` then puts
    // the owner's bearer on and strips what a pooled request never carries.
    let mut forwarded = HeaderMap::new();
    for (name, value) in ask.headers.iter() {
        if crate::peer::serve::lender_forwards_header(name.as_str()) {
            forwarded.append(name.clone(), value.clone());
        }
    }
    let Some(headers) = crate::proxy::build_handed_bearer_headers(&forwarded, lease_id, now_ms)
    else {
        return Ok(None);
    };

    // `set_path` on a parsed base, never `format!`, for the reason
    // `peer::serve::serve_on_own_account` gives at length: a path that does not
    // begin with `/` turns the authority into userinfo and sends this request
    // to a host the client named.
    let mut url =
        reqwest::Url::parse(upstream_base).context("peer hand: the upstream base is not a URL")?;
    url.set_path(ask.path);
    url.set_query(ask.query);

    let mut send = client.request(
        reqwest::Method::from_bytes(ask.method.as_bytes())
            .context("peer hand: the client's method is not a method")?,
        url,
    );
    for (name, value) in headers.iter() {
        send = send.header(name.clone(), value.clone());
    }
    if !ask.body.is_empty() {
        send = send.body(ask.body.clone());
    }

    let response = send
        .send()
        .await
        .context("peer hand: the request on the handed bearer did not reach upstream")?;
    let status = response.status().as_u16();
    let head: Vec<(String, String)> = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_string(), value.to_string()))
        })
        .collect();
    // WHAT THIS ANSWER COST THE OWNER, REPORTED BACK. A hand-mode lease was
    // debited by nobody: the request leaves from this Mac, so the owner never
    // sees it, and the only evidence of the spend is the rate-limit header on
    // this answer. `report_handed_spend` turns two successive answers into the
    // rise and sends it up the lease's own control session. See
    // [`HandedMeter`].
    report_handed_spend(lease_id, reported_utilization(&head, window));

    let body = response
        .bytes()
        .await
        .context("peer hand: the upstream response body did not read")?;

    // Built here rather than through `peer::serve`'s private `response_from`:
    // that function belongs to the relay path, this node is not relaying, and
    // widening its visibility would make one helper answer for two paths that
    // have different reasons to drop a header.
    let mut builder = Response::builder().status(
        axum::http::StatusCode::from_u16(status)
            .with_context(|| format!("peer hand: {status} is not a status code"))?,
    );
    for (name, value) in &head {
        // Hop-by-hop response headers describe the connection this node just
        // made, not the one it is answering, exactly as the proxy drops them on
        // the way back from upstream.
        if crate::proxy::is_response_skip(name) {
            continue;
        }
        let Ok(name) = axum::http::HeaderName::try_from(name.as_str()) else {
            continue;
        };
        let Ok(value) = axum::http::HeaderValue::from_str(value) else {
            continue;
        };
        builder = builder.header(name, value);
    }
    builder
        .body(axum::body::Body::from(body))
        .context("peer hand: could not build the response for this node's own client")
        .map(Some)
}

/// One lease scope's canonical key in [`Ledger::headroom`].
///
/// [`LendScope`]'s own `Display` is the CLI's `--scope` spelling
/// (`all`, `group:<name>`, `account:<a>,<b>`), which is already canonical,
/// one string per distinct scope, and the string an operator typed. A separate
/// hash derive on the wire type would be a second answer to "are these the same
/// scope".
fn scope_key(scope: &LendScope) -> String {
    scope.to_string()
}

/// Could a lease on `held` be spending accounts a lease on `wanted` would also
/// draw from?
///
/// # It answers with what the LEDGER knows, and errs towards subtracting
///
/// This ledger holds scopes as the operator wrote them and no membership at
/// all: groups hot-reload, which is the whole reason [`LendScope::Group`] is a
/// name and not a frozen list, so "does `group:work` contain account `a`" is a
/// question only the manager can answer and only at the instant it is asked.
///
/// So three answers are derivable here and the fourth is a choice.
/// [`LendScope::All`] overlaps everything. Two [`LendScope::Accounts`] sets
/// overlap when they name an account in common. Identical scopes overlap.
/// Anything involving a GROUP against a different scope is not derivable, and
/// it answers "overlapping": the cost of subtracting room that is really
/// somebody else's is a lease smaller than it could have been, and the cost of
/// not subtracting is the owner promising the same headroom twice, which is the
/// failure this function exists for.
fn scopes_overlap(wanted: &LendScope, held: &LendScope) -> bool {
    match (wanted, held) {
        (LendScope::All, _) | (_, LendScope::All) => true,
        (LendScope::Accounts(wanted), LendScope::Accounts(held)) => {
            wanted.iter().any(|label| held.contains(label))
        }
        // A group against anything but an identical group: not derivable here,
        // so it counts. See this function's own doc.
        _ => true,
    }
}

/// One relayed request this ledger remembers, and everything it knows about it.
///
/// The `charged` flag is why this is a struct and not the `(request_id, at_ms)`
/// tuple it replaced: the serve half and the charge half used to keep separate
/// maps of separate pairs, and a pair could be forgotten by one while the other
/// still held it. See [`Ledger::pairs`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RelayedPair {
    request_id: u128,
    /// The millisecond this pair was admitted for relay, on the clock
    /// [`Ledger::enter_relay`]'s caller passed. Every entry here is stamped by
    /// that one caller, so [`Ledger::admit_pair`]'s expiry and eviction compare
    /// readings of the same clock.
    at_ms: i64,
    /// Whether [`Ledger::debit`] has already charged this pair.
    charged: bool,
}

/// The lender's ledger: every lease it has granted, with absolute deadlines.
///
/// Persisted and restored at boot, TTL-bounded, and it logs how many rows it
/// restored and how many it dropped, the same shape session affinity already
/// uses, because "restored=N expired=M" is the line that tells an operator
/// whether a restart cost them anything.
#[derive(Debug, Clone, Default)]
pub struct Ledger {
    leases: Vec<Lease>,
    /// Where [`Self::persist`] writes the rows [`Self::restored_from`] reads
    /// back, or `None` for a ledger that is process state only (every test that
    /// builds one with [`Self::new`], and any build with no peers file).
    ///
    /// The review's M2: this type's own doc promised "Persisted and restored at
    /// boot … restored=N expired=M" and nothing serialized it, while
    /// [`crate::peer::state::PeerState::leases`] declared the slot and was
    /// written by nobody. A restart voided every lease. The promise is kept
    /// rather than deleted, because a restart mid-lease otherwise strands a
    /// borrower whose ceiling the lender has forgotten, and an unbounded
    /// re-grant after a restart is the same hole H2 opened, one reboot wide.
    state_path: Option<PathBuf>,
    /// Who each lease was granted TO, on the lender and only on the lender.
    ///
    /// The review's M1. See [`RelayRefusal::NotTheGrantee`]. A side map rather
    /// than a field on [`Lease`] for the same reason [`Self::scopes`] is one:
    /// `Lease` is the wire message, and a grantee on it would tell the borrower
    /// its own id back while adding a field the wire message does not need.
    grantees: HashMap<u128, PeerId>,
    /// The owner's own remaining headroom per window, as last measured by
    /// [`crate::manager::Manager::lendable_fraction`].
    ///
    /// **This field is why [`Ledger::may_relay`] can answer
    /// [`LeaseRefusal::OwnerGuard`] at all.** The skeleton's signature takes a
    /// lease id and a clock and nothing else, so the guard has to already be in
    /// the ledger when the question is asked; the lender writes it through
    /// [`Ledger::note_owner_headroom`] whenever it re-reads its own quota. An
    /// unmeasured window is **absent, not zero-headroom and not
    /// infinite-headroom**, and [`Ledger::may_relay`] refuses on absence: giving
    /// away quota on a window nobody has measured is the one direction where
    /// guessing costs the owner rather than the borrower.
    ///
    /// # Keyed by SCOPE and window, not by window alone
    ///
    /// The fraction is of the SCOPE's
    /// headroom, and this map was keyed by window, so a lease scoped to one
    /// group was clamped by (and its owner guard decided against) the whole
    /// fleet's headroom. A group with no room left then went on lending the
    /// pool's.
    ///
    /// The key is [`scope_key`]'s canonical spelling of the scope rather than
    /// the [`LendScope`] itself, because that type is in `tcr_peer_wire` and
    /// does not derive `Hash`, and nothing here adds the derive there. Both
    /// writers ([`Self::grant`] and
    /// [`crate::peer::serve::handle_serve_on`]) note the scope's own figure
    /// through [`crate::peer::serve::WindowUtilization::lendable`] immediately
    /// before they ask, so the measurement a relay is decided against is the
    /// scope's and is seconds old.
    headroom: HashMap<(String, Window), f64>,
    /// Relayed requests currently in flight, per lease, against
    /// [`Lease::max_inflight`]. See [`Ledger::enter_relay`].
    ///
    /// A key at zero is REMOVED rather than left behind (the review's M4): a
    /// map a peer can grow one entry per lease-ask is a memory bug with a
    /// security label, which is the bar `RefusalLog` and `RequestDedup` next
    /// door already hold themselves to.
    inflight: HashMap<u128, u8>,
    /// Every `(lease_id, request_id)` pair this ledger remembers, newest last,
    /// each carrying whether the charge for it has landed yet.
    ///
    /// Three defects in one field. The review's **H2**: the memory was the
    /// charge's alone, consulted only by [`Self::debit`], so a replayed
    /// `request_id` was served again for free; [`Self::enter_relay`] now
    /// refuses the pair, so the ceiling binds. The review's **M4**: it was an
    /// unbounded `HashSet` that grew one entry per relayed request for the life
    /// of the process, on input a peer chooses. Bounded by [`SERVED_CAPACITY`]
    /// and [`SERVED_TTL_MS`], exactly the way
    /// [`crate::peer::listener::RequestDedup`] already is, the `VecDeque` is
    /// what makes "drop the oldest" possible at all. **Keyed by lease**, the
    /// second half of the M4 fix: one FIFO for the whole process meant a
    /// borrower sending its own [`SERVED_CAPACITY`] requests evicted every
    /// other borrower's pairs, and its own older ones, which it could then
    /// replay for free. See [`SERVED_PER_LEASE_CAPACITY`].
    ///
    /// **And it is ONE map rather than two.** "Served here" and "charged here"
    /// lived in separate maps, filled by different callers on different clocks:
    /// every admitted relay entered the first, only the ones that reached the
    /// charge entered the second, and the two FIFOs therefore evicted different
    /// pairs at different moments. A pair that had left the serve map while
    /// still sitting in the charge map was admitted again (new to one) and
    /// charged nothing (old to the other), which is a relay bought on the
    /// lender's own account for free. One map with a state per pair makes
    /// forgetting a pair forget both facts about it at once, so the two answers
    /// cannot disagree.
    pairs: HashMap<u128, VecDeque<RelayedPair>>,
    /// What each lease draws from, on the LENDER and only on the lender.
    ///
    /// A side map rather than a field on [`Lease`], because `Lease` is the wire
    /// message, and a scope never crosses it: the
    /// borrower never learns which of the lender's accounts paid. The gate that
    /// holds that is `every_wire_type_serializes_only_allowlisted_keys` in
    /// `tests/peer_wire.rs`, not this comment.
    ///
    /// A lease with no entry here is [`LendScope::All`], which is what every
    /// lease minted before scopes existed meant, including the ones a restore at
    /// boot reads back off a ledger file written by an older build.
    scopes: HashMap<u128, LendScope>,
    /// A `spent` figure that has moved since the last write, waiting for
    /// [`Self::flush`].
    ///
    /// **Why the write is not where the charge is.** [`Self::persist`] takes
    /// [`crate::peer::config::FileLock`], loads the whole state file and saves
    /// it, and [`Self::debit`] is called from
    /// [`crate::peer::serve::handle_serve_on`] with this ledger's own mutex
    /// held, on every single relayed request. So the relay path paid a locked
    /// file round trip per request, and a lockfile held by a concurrent
    /// `tcr peer accept` made every relay on this Mac wait out
    /// [`crate::peer::config::LOCK_STALE_MS`] with the ledger mutex in hand:
    /// one slow operator verb stalled the whole lender.
    ///
    /// A grant still writes immediately ([`Self::record_scoped`]): it is rare,
    /// it is the row a restart cannot reconstruct, and nothing is in flight
    /// against it yet. A charge only marks this flag, and the lender's
    /// debounced flusher ([`crate::peer::listener::LEDGER_FLUSH_INTERVAL`])
    /// does the write off the hot path. The cost of a crash inside one
    /// debounce window is bounded by that interval and is `spent` moving
    /// backwards by at most one window's charges, which `may_relay` still
    /// bounds by the lease's own budget and TTL.
    dirty: bool,
    /// The path each lease's borrows are served over, when the serving side
    /// knew one.
    ///
    /// A side map for [`Self::grantees`]' reason and one more: a path is not a
    /// property of the lease at all, it is a property of the STREAM that spends
    /// it, and a lease whose borrower moves house keeps its id and changes its
    /// path. The newest note wins, which is what makes the figure it feeds
    /// ([`crate::peer::tunnel::PathMeter`]) a measurement of this hour rather
    /// than of the hour the lease was minted in.
    ///
    /// A lease with no entry is a borrow whose path this build could not
    /// attribute, and its tokens are charged to no path at all, never to a
    /// guessed one. Nothing notes it from the serving stream yet;
    /// [`Self::note_lease_path`] is this side of it.
    lease_paths: HashMap<u128, crate::peer::config::Locator>,
}

impl Ledger {
    /// An empty ledger with no file behind it: process state only.
    ///
    /// [`Self::restored_from`] is what a lender boots with. This stays for the
    /// tests and for a build with no peers file, and it is now honest about
    /// what it is rather than promising a restore that never happened.
    pub fn new() -> Self {
        Self::default()
    }

    /// The ledger as of the last write, read back off `state_path`, with the
    /// `restored=N expired=M` line this type's doc has always promised.
    ///
    /// A state file that cannot be read is an EMPTY ledger plus a warning, and
    /// never a refusal to boot: the recovery is already designed and already
    /// gentle ([`Self::may_relay`] answers `LeaseExpired` for an id it does not
    /// hold, `open_serve` turns that into `Ok(None)`, and the borrower drops its
    /// cached hint and re-asks). A lender that refused to serve local traffic
    /// over a stale runtime-state file would be the worse trade.
    ///
    /// Expired rows are counted and DROPPED here rather than recorded and left
    /// to [`Self::may_relay`], which is the opposite of what [`Self::record`]
    /// does on purpose: `record` is the wire path, where a silent drop would put
    /// the count and the ledger out of step, and this is the boot path, where
    /// the count IS the log line and carrying dead rows forward would grow the
    /// file by every lease the machine ever granted.
    pub fn restored_from(state_path: &std::path::Path) -> Self {
        let mut ledger = Self {
            state_path: Some(state_path.to_path_buf()),
            ..Self::default()
        };
        let now_ms = crate::now_ms();
        let rows = match crate::peer::state::load(state_path, now_ms) {
            Ok(state) => state.leases,
            Err(err) => {
                tracing::warn!(
                    path = %state_path.display(),
                    error = %err,
                    "peer lease: the ledger's state file could not be read, so this lender \
                     starts with no leases (every borrower re-asks)"
                );
                return ledger;
            }
        };
        let mut expired = 0_usize;
        let mut ungranted = 0_usize;
        for row in rows {
            if row.lease.expires_at_ms <= now_ms {
                expired += 1;
                continue;
            }
            // THE OTHER HALF OF `Self::persist`'s rule. No build writes an
            // all-zero grantee any more, but an older one did and the file is
            // hand-editable JSON, so the reader refuses it rather than trusting
            // that nothing ever wrote it: a lease nobody can spend
            // (`enter_relay` answers `NotTheGrantee` to every peer, including
            // one whose id really is all-zero, because the all-zero id is not a
            // static key any handshake can produce) would still count in `live`
            // and `committed_fraction` and hold headroom for its whole TTL.
            if row.peer == PeerId([0_u8; 32]) {
                ungranted += 1;
                continue;
            }
            ledger.grantees.insert(row.lease.lease_id, row.peer);
            ledger.scopes.insert(row.lease.lease_id, row.scope);
            ledger.leases.push(row.lease);
        }
        tracing::info!(
            path = %state_path.display(),
            restored = ledger.leases.len(),
            expired,
            ungranted,
            "peer lease: ledger restored"
        );
        ledger
    }

    /// Write every live lease back to the state file, or do nothing for a
    /// ledger with no file behind it.
    ///
    /// Called from the two places that CHANGE what a restart would have to know:
    /// [`Self::record_scoped`] (a new lease exists) and [`Self::debit`] (a live
    /// lease's `spent` moved, which is the ceiling H2 exists to keep binding).
    /// Not from `enter_relay`/`leave_relay`: in-flight is concurrency state, and
    /// a restart releases every slot by construction.
    ///
    /// A failed write is LOGGED and never propagated. The caller is answering a
    /// borrower on a live stream, and a lender that refused a request it can
    /// serve because a cache file is read-only would be trading a real service
    /// for a durability promise about a restart that may never happen. The
    /// warning is how an operator learns their leases will not survive one.
    /// Write the charges [`Self::debit`] has recorded since the last write, and
    /// report whether there was anything to write.
    ///
    /// The lender's debounced flusher
    /// ([`crate::peer::listener::flush_ledger_periodically`]) is the caller,
    /// plus shutdown. Idempotent and cheap when nothing moved: a lender that
    /// served no relayed request takes no lock and touches no file.
    pub fn flush(&mut self) -> bool {
        if !self.dirty {
            return false;
        }
        self.persist();
        self.dirty = false;
        true
    }

    /// Whether a charge is waiting for [`Self::flush`], so a gate reads the
    /// hot path's promise off the ledger rather than off a copy.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    fn persist(&self) {
        let Some(path) = &self.state_path else {
            return;
        };
        // A LEASE WITH NO GRANTEE IS NOT WRITTEN. `enter_relay` refuses every
        // request against one (`RelayRefusal::NotTheGrantee`), so such a row
        // can never be spent, but it is not inert either: `live` and
        // `committed_fraction` count it, so restoring one holds a slice of this
        // lender's headroom against a lease nobody on earth can use, until its
        // own TTL runs out. This path used to write the all-zero id as "the
        // honest round-trip of nobody"; a row that costs headroom and serves
        // nobody is the one thing a restart is better off forgetting. Every
        // lease `grant` mints is recorded through `record_scoped` with the
        // grantee from its authenticated session, so this drops nothing an
        // operator asked for; a `record`-only caller (the older entry point)
        // is what it catches.
        let mut ungranted = 0_usize;
        let rows: Vec<crate::peer::state::LeaseRow> = self
            .leases
            .iter()
            .filter_map(|lease| {
                let Some(peer) = self.grantees.get(&lease.lease_id).copied() else {
                    ungranted += 1;
                    return None;
                };
                Some(crate::peer::state::LeaseRow {
                    lease: *lease,
                    peer,
                    scope: self.scope_of(lease.lease_id),
                })
            })
            .collect();
        if ungranted > 0 {
            tracing::warn!(
                path = %path.display(),
                ungranted,
                persisted = rows.len(),
                "peer lease: leases with no grantee are not persisted; nothing can spend one \
                 and restoring it would hold this lender's headroom for its whole TTL"
            );
        }
        if let Err(err) = crate::peer::state::save_leases(path, &rows) {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "peer lease: the ledger could not be persisted, so a restart voids these \
                 leases and every borrower re-asks"
            );
        }
    }

    /// Put one lease in the ledger: the single insertion point, used by
    /// [`Self::grant`] when it mints one and by the restore-at-boot path when it
    /// reads one back off disk.
    ///
    /// An ALREADY EXPIRED lease handed to THIS function is accepted rather than
    /// filtered, because [`Self::may_relay`] refuses one anyway and a caller
    /// that wrote it deserves to read it back. The boot path does its own
    /// counting and dropping ([`Self::restored_from`]) precisely so the
    /// "restored=N expired=M" line and the ledger cannot disagree: one place
    /// counts, one place accepts, and neither silently does the other's job.
    pub fn record(&mut self, lease: Lease) {
        // The review's M4: `leases` was pushed to and never pruned, and
        // `may_relay`/`debit` each walk it linearly, so a peer that asks for a
        // lease per second grows both the memory and the cost of every later
        // question. An expired row can answer nothing but `LeaseExpired`, which
        // is the same answer an absent one gives, so dropping it here loses no
        // fact, and it happens on the ONE insertion point, so there is no path
        // that grows the vector without paying for the prune.
        let now_ms = crate::now_ms();
        self.leases.retain(|held| held.expires_at_ms > now_ms);
        let live: HashSet<u128> = self.leases.iter().map(|held| held.lease_id).collect();
        self.grantees.retain(|id, _| live.contains(id));
        self.scopes.retain(|id, _| live.contains(id));
        self.inflight.retain(|id, _| live.contains(id));
        self.leases.push(lease);
    }

    /// [`Self::record`], plus what the lease draws from.
    ///
    /// Two entry points rather than one with an `Option`, because the restore
    /// path and the older callers genuinely mean [`LendScope::All`] and saying
    /// so by omission is how a scoped lease comes to be restored as unscoped.
    /// [`Self::scope_of`] answers `All` for anything recorded the other way.
    ///
    /// `peer` is the GRANTEE and it is not optional: the review's M1 was that a
    /// lease remembered nothing about who asked, so the id alone spent it. A
    /// lease recorded here can be spent by this peer and by nobody else
    /// ([`RelayRefusal::NotTheGrantee`]), and the pair is what
    /// [`Self::persist`] writes so a restart does not turn every lease back
    /// into a bearer token.
    pub fn record_scoped(&mut self, lease: Lease, peer: PeerId, scope: LendScope) {
        self.record(lease);
        // AFTER `record`, which prunes the maps down to the live lease set: the
        // other order dropped the row this call is about.
        self.grantees.insert(lease.lease_id, peer);
        self.scopes.insert(lease.lease_id, scope);
        // A GRANT writes now. It is rare, it is the row a restart cannot
        // reconstruct from anywhere else, and no request is in flight against
        // it yet, the opposite of `debit` on all three counts, which is why
        // that one only marks `Self::dirty`.
        self.persist();
        self.dirty = false;
    }

    /// Who one lease was granted to, or `None` for a lease this ledger does not
    /// hold. For `tcr peer ls --json` and the panel row.
    pub fn grantee_of(&self, lease_id: u128) -> Option<PeerId> {
        self.grantees.get(&lease_id).copied()
    }

    /// What one lease draws from, on the lender. [`LendScope::All`] for a lease
    /// recorded without one. See [`Self::scopes`].
    pub fn scope_of(&self, lease_id: u128) -> LendScope {
        self.scopes
            .get(&lease_id)
            .cloned()
            .unwrap_or(LendScope::All)
    }

    /// Tell the ledger what the owner's own guard band leaves on one window.
    ///
    /// `fraction` is [`crate::manager::Manager::lendable_fraction`]'s answer:
    /// headroom ABOVE the guard band, never the raw threshold, and `0.0` is the
    /// common answer. Called by the lender whenever it re-reads its own quota,
    /// so [`Self::may_relay`] reads a measurement rather than taking one.
    /// `LendScope::All`'s figure, which is what a lender's boot ticker measures
    /// for the whole fleet. [`Self::note_scope_headroom`] is the scoped form.
    pub fn note_owner_headroom(&mut self, window: Window, fraction: f64) {
        self.note_scope_headroom(&LendScope::All, window, fraction);
    }

    /// Tell the ledger what one SCOPE's accounts leave on one window.
    ///
    /// The rule is: "the fraction is of the SCOPE's headroom … computed by
    /// `lendable_fraction` over the scope's accounts only". Both readers of a
    /// scoped lease note the scope's own figure immediately before they ask,
    /// [`Self::grant`] when it mints, `handle_serve_on` when it relays, so the
    /// guard a scoped relay is decided against is the scope's and not the
    /// fleet's. See [`Self::headroom`].
    pub fn note_scope_headroom(&mut self, scope: &LendScope, window: Window, fraction: f64) {
        self.headroom
            .insert((scope_key(scope), window), fraction.max(0.0));
    }

    /// What this ledger last measured for one scope on one window.
    ///
    /// **`LendScope::All`'s figure is the fallback and it is a CEILING, not a
    /// substitute**: `min` of the two, so a scope with no note of its own can
    /// never be told it has more room than the whole fleet has. That direction
    /// is the decision, the other one (trust the scope's own note alone) would
    /// let a stale per-scope figure outlive the fleet running dry, and refusing
    /// outright on an absent scope note would refuse every lease minted by a
    /// build or a test whose reader cannot answer per scope
    /// ([`crate::peer::serve::WindowUtilization::lendable`] defaults to `None`).
    ///
    /// `None` for a window nothing has measured at all, which
    /// [`Self::may_relay`] refuses, because giving away quota on an unmeasured
    /// window is the one direction where guessing costs the owner.
    fn headroom_for(&self, scope: &LendScope, window: Window) -> Option<f64> {
        let all = self
            .headroom
            .get(&(scope_key(&LendScope::All), window))
            .copied();
        let scoped = self.headroom.get(&(scope_key(scope), window)).copied();
        match (scoped, all) {
            (Some(scoped), Some(all)) => Some(scoped.min(all)),
            (Some(scoped), None) => Some(scoped),
            (None, all) => all,
        }
    }

    /// Answer a borrower's request. **The answer is the offer**: there is no
    /// negotiation round, because a fraction changes on every request and a
    /// second round trip would be spent on a stale number.
    ///
    /// Reads the grant for that peer, then what the manager says is lendable
    /// right now ([`crate::manager::Manager::lendable_fraction`]), and never
    /// the raw threshold.
    /// # The arithmetic is not here, and that is the point
    ///
    /// Everything this function DECIDES is in [`clamp_to_grant`], which is pure
    /// and tested without a peers file or a random source. What is left is two
    /// reads (the peer's row, the owner's last measured headroom), a lease id
    /// from the node's CSPRNG ([`random_id`]) and a `record`. None of the
    /// lease's arithmetic is hiding in it.
    ///
    /// The peer's row is re-read through `PeerStore::reload_if_changed`, so a
    /// grant revoked a second ago is refused without a restart, which is the
    /// whole reason peer policy lives in its own hot-reloaded file.
    ///
    /// `fleet` is the lender's own fleet, and it is here for one question:
    /// which of this peer's grants this Mac's picker could actually hold a
    /// request inside ([`crate::peer::serve::WindowUtilization::scope_restriction`]).
    /// The SAME reader the serving leg uses, so a lease this mints is one the
    /// serve path will not turn round and refuse. See
    /// [`crate::peer::config::PeerRow::grant_for`].
    pub fn grant(
        &mut self,
        peer: &PeerId,
        ask: &LeaseRequest,
        store: &PeerStore,
        fleet: &dyn crate::peer::serve::WindowUtilization,
    ) -> Granted {
        store.reload_if_changed();
        // THE CAP, before the row is even looked at: this refusal is about how
        // many leases this peer already holds and nothing about the ask, so
        // asking it first is what keeps a peer in a mint loop from costing a
        // peers-file read and a state-file rewrite per ask. See
        // [`MAX_LEASES_PER_PEER`].
        let now_ms = crate::now_ms();
        let held = self.live_for(peer, now_ms);
        if held >= MAX_LEASES_PER_PEER {
            tracing::warn!(
                peer = %peer.display(),
                held,
                cap = MAX_LEASES_PER_PEER,
                window = ?ask.window,
                "peer lease: this peer already holds the most leases one peer may hold here, \
                 so nothing is granted until one of them expires or is spent"
            );
            return Granted::unfunded(LeaseRefusal::TooManyLeases);
        }
        let row = store.row(peer);
        let inspect = row.as_ref().is_some_and(|row| row.allow.inspect);
        // A peer may hold SEVERAL grants, so this picks the one
        // on the asked-for window that has not ended and whose scope this Mac
        // can serve inside. An ended or unenforceable grant is skipped rather
        // than picked-and-refused: an operator whose `work` lease ran to 18:00
        // and whose `all` lease has no end meant the second one to keep
        // serving, and picking the first row by window alone would refuse the
        // request with a lease sitting right beside it.
        let granted = row
            .as_ref()
            .and_then(|row| {
                row.grant_for(ask.window, seconds_of(crate::now_ms()), &|scope| {
                    fleet.scope_restriction(scope)
                        != crate::peer::serve::ScopeRestriction::Unenforceable
                })
            })
            .cloned();
        // The window, read off the same grant, and asked BEFORE
        // the fleet is measured: a Mac outside its lending hours has nothing to
        // measure for, and a measurement noted here would be spent deciding a
        // request that is already refused.
        //
        // `now_utc()` and not `now_ms`: `Schedule::contains` converts to this
        // Mac's local offset itself, which is the offset an operator means by
        // "between 22:00 and 08:00". A grant with no schedule at all answers
        // `None` from `LendGrant::schedule` and never reaches this branch.
        //
        // The instant itself is kept beside the schedule (`schedule_now`),
        // because `schedule_close_ms` below answers off the SAME reading:
        // asking the wall clock again a few lines down would let a request
        // straddling a clock tick see `contains` say yes against one second
        // and the close computed against a different one.
        let schedule_now = time::OffsetDateTime::now_utc();
        let schedule = granted.as_ref().and_then(|grant| grant.schedule());
        if let Some(schedule) = schedule.as_ref() {
            if let Some(refusal) = schedule_refusal(Some(schedule), schedule_now) {
                tracing::info!(
                    peer = %peer.display(),
                    window = ?ask.window,
                    "peer lease: this grant is outside the hours it was lent for, so nothing \
                     is granted until it opens again"
                );
                return Granted::unfunded(refusal);
            }
        }
        // THE FIFTH CLAMP's input: the window this lease is being minted
        // inside stops being open at this instant, so nothing after mint has
        // to consult the schedule again. See `clamp_to_grant`'s own doc and
        // `schedule_closes_at_ms` below, this module's own helper:
        // `Schedule`'s fields are public and this is the one caller, so the
        // close-time arithmetic lives beside the clamp it feeds rather than
        // adding a method to a module this unit does not own.
        let schedule_close_ms = schedule
            .as_ref()
            .and_then(|schedule| schedule_closes_at_ms(schedule, schedule_now));
        // The scope, read off the grant the operator wrote. It stays
        // HERE: recorded beside the lease in this ledger and never put on the
        // wire.
        let scope = granted
            .as_ref()
            .map_or(LendScope::All, |grant| grant.scope.clone());
        // What the owner's guard band leaves ON THIS SCOPE's accounts, measured
        // NOW through the same fleet reader the serving leg uses. A measurement
        // found this keyed by window alone, which clamped a group-scoped
        // lease by the whole fleet's room. The figure is NOTED as well as used,
        // so `may_relay` decides the first relay against the same measurement
        // rather than against an absence.
        // **Measured HERE, for this scope, at this instant**, and noted, so
        // `may_relay` decides the first relay against the same figure rather
        // than against an absence.
        //
        // Every scope, `LendScope::All` included. The boot ticker
        // (`server.rs`'s `note_owner_headroom`, before the accept loop and every
        // 30 s after) is not a second answer to be avoided: it is the FLOOR, the
        // thing that gives `may_relay` a figure when nobody has asked for a
        // lease yet. A reader that is holding the fleet and about to decide is
        // strictly better placed than a note up to thirty seconds old, measured
        // on the e2e, where a lender warmed one request and the boot note still
        // said `0.0`, so the borrow was refused `owner-guard` for half a minute
        // after the room existed.
        //
        // `None` is not zero: `Manager::lendable` answers it when no account in
        // the scope has a measured window at all, which leaves the ticker's
        // figure standing. See that method's doc for why the distinction has to
        // be made where the quota is.
        if let Some(measured) = fleet.lendable(&scope, ask.window) {
            self.note_scope_headroom(&scope, ask.window, measured);
        }
        // WHAT IS ALREADY PROMISED, SUBTRACTED. `headroom_for` is what this
        // Mac's own fleet has left; it says nothing about what this Mac has
        // already lent out of it. A borrower that re-asks therefore held up to
        // `MAX_LEASES_PER_PEER` leases at a time, each clamped to the FULL
        // fraction, so eight asks promised eight times the room that exists,
        // and every one of them passed `may_relay` until the quota itself ran
        // out. See [`Self::committed_fraction`].
        let lendable = (self.headroom_for(&scope, ask.window).unwrap_or(0.0)
            - self.committed_fraction(&scope, ask.window, now_ms))
        .max(0.0);
        // A HAND GRANT IS CLAMPED AGAIN, by what the accounts whose bearer can
        // leave this Mac have left. The two figures were measured over two
        // different sets of accounts: `handoff_bearer` skips a held account and
        // a strictly pinned one, `lendable_fraction` skipped neither, so a
        // scope whose usable accounts were all held or pinned funded a lease
        // with no bearer at all and every request on it fell back to the
        // lender's own path. It is a clamp rather than a replacement of the
        // note above, because the noted headroom is what `may_relay` decides
        // SERVE relays against too and lowering it there would refuse them for
        // a hand lease's reason.
        //
        // Read off the grant the ledger itself picked, never re-derived: this
        // is the same `granted` the mode, the scope and the end come off.
        let lendable = match granted.as_ref().map(|grant| grant.mode) {
            Some(crate::peer::config::LendMode::Hand) => fleet
                .lendable_by_hand(&scope, ask.window)
                .map_or(lendable, |handable| lendable.min(handable)),
            _ => lendable,
        };

        let lease_id = match random_id() {
            Ok(id) => id,
            Err(err) => {
                // A CSPRNG that will not answer is not a refusal about this
                // lease, and it must not be told as one: a guessable lease id is
                // a lease anybody on the mesh can spend, so the only safe answer
                // is no lease at all, loudly.
                tracing::error!(
                    error = %err,
                    "peer lease: no random source for a lease id, so nothing is granted"
                );
                return Granted::unfunded(LeaseRefusal::Unsupported);
            }
        };

        // The end, read off the grant the operator wrote. A grant
        // whose end has passed mints nothing (`clamp_to_grant`), and one still
        // ahead is copied onto the lease so the borrower can render "ends in
        // 1 h" without a second round trip.
        let end = granted.as_ref().and_then(|grant| grant.until);
        let grant = clamp_to_grant(
            ask,
            granted.clone(),
            inspect,
            lendable,
            crate::now_ms(),
            lease_id,
            end,
            schedule_close_ms,
        );
        if let Some(lease) = grant.lease {
            // The grantee, from the AUTHENTICATED session this answer is for,
            // the review's M1. Nothing about the ask reaches this argument.
            self.record_scoped(lease, *peer, scope.clone());
        }
        Granted {
            answer: grant,
            funding: granted,
            scope,
        }
    }

    /// Enter one relayed request against a lease, or refuse it.
    ///
    /// The whole of [`Self::may_relay`] first, then [`Lease::max_inflight`]. The
    /// (n+1)th concurrent request is **refused, never queued**: a queue in front
    /// of a lease whose debit reads lagging headers just moves the overdraft
    /// later and hides it, and a borrower that is told no can serve the request
    /// locally or answer the honest 429 now instead of after a wait.
    ///
    /// Every `Ok` is paired with exactly one [`Self::leave_relay`]. The caller
    /// holds that pairing in a guard, the way the proxy's own in-flight
    /// accounting does at `src/proxy.rs:2525`, so a rotate, a terminal return or
    /// a panic-unwind all release the slot.
    pub fn enter_relay(
        &mut self,
        lease_id: u128,
        peer: &PeerId,
        request_id: u128,
        now_ms: i64,
    ) -> Result<(), RelayRefusal> {
        // THE GRANTEE, FIRST, and before `may_relay` says anything about the
        // lease: a peer spending somebody else's id must not be able to tell
        // `LeaseSpent` from `LeaseExpired` from `OwnerGuard` about a lease that
        // is not its own. See `RelayRefusal::NotTheGrantee`.
        if self.grantees.get(&lease_id) != Some(peer) {
            return Err(RelayRefusal::NotTheGrantee);
        }
        self.may_relay(lease_id, now_ms)
            .map_err(RelayRefusal::Lease)?;
        // THE REPLAY, and this is the review's H2: the SERVE is made idempotent
        // here, where refusing costs nothing, rather than left to `debit`, where
        // idempotence meant "served again, charged once". Recorded on the way IN,
        // before the request is served, so two concurrent copies of one
        // request id cannot both pass this line.
        if !self.admit_served(lease_id, request_id, now_ms) {
            return Err(RelayRefusal::Replayed);
        }
        // `may_relay` proved the lease is here, so this lookup cannot miss.
        let max = self
            .leases
            .iter()
            .find(|lease| lease.lease_id == lease_id)
            .map_or(0, |lease| lease.max_inflight);
        // Read, never `entry(..).or_insert(0)`: a refusal must leave this map
        // EXACTLY as it found it. The line this replaced removed the key on a
        // cap refusal, which forgot the count of the `max` requests that were
        // in flight at that very moment, so the next `enter_relay` read zero,
        // admitted, and the cap stopped binding after the first request it
        // refused. And the `or_insert` itself parked a zero for a lease whose
        // `max_inflight` is zero, against the rule `Self::leave_relay`
        // documents: zero and absent are the same fact here.
        let in_flight = self.inflight(lease_id);
        if in_flight >= max {
            // The served pair stays recorded. A refused request was not served,
            // so its id could honestly be retried, but a cap refusal is the
            // one refusal a borrower is told to retry AFTER
            // (`LeaseRefusal::InFlightFull`), and it retries with a fresh id
            // because `open_serve` mints one per call. Keeping it is the safe
            // direction: forgetting it would hand a replayer a free slot per
            // refusal.
            return Err(RelayRefusal::TooManyInflight { max });
        }
        self.inflight.insert(lease_id, in_flight + 1);
        Ok(())
    }

    /// Record a `(lease_id, request_id)` pair and report whether it is NEW.
    ///
    /// `false` means this exact request has already been admitted for relay
    /// here. Bounded by [`SERVED_CAPACITY`] and [`SERVED_TTL_MS`]. See
    /// [`Self::pairs`], which carries every defect this closes.
    fn admit_served(&mut self, lease_id: u128, request_id: u128, now_ms: i64) -> bool {
        Self::admit_pair(&mut self.pairs, lease_id, request_id, now_ms)
    }

    /// One admission into one of the two per-lease caches: expire, refuse a
    /// repeat, evict this LEASE's oldest id if it is at its own bound, and
    /// record.
    ///
    /// **Every eviction is inside one lease.** The cache was a single FIFO, so
    /// a borrower with requests of its own to make pushed other borrowers'
    /// pairs out of it and turned their replay defence off; and once it had
    /// filled the queue by itself it evicted its own older ids, which it could
    /// then send again for free. A per-lease bound makes the only id a
    /// borrower can lose one of its own, at which point the pair is older than
    /// [`SERVED_PER_LEASE_CAPACITY`] of its own requests and a replay of it
    /// buys a request it has already paid for.
    ///
    /// The process-wide bound is kept as well: a ledger holding more than
    /// [`SERVED_CAPACITY`] ids across all leases drops the lease whose oldest
    /// id is oldest. The number of leases is capped
    /// ([`MAX_LEASES_PER_PEER`] per peer) and every entry expires at
    /// [`SERVED_TTL_MS`], so this is a ceiling rather than the working
    /// mechanism.
    fn admit_pair(
        cache: &mut HashMap<u128, VecDeque<RelayedPair>>,
        lease_id: u128,
        request_id: u128,
        now_ms: i64,
    ) -> bool {
        cache.retain(|_, ids| {
            ids.retain(|pair| now_ms.saturating_sub(pair.at_ms) < SERVED_TTL_MS);
            !ids.is_empty()
        });
        while cache.values().map(VecDeque::len).sum::<usize>() >= SERVED_CAPACITY {
            let Some(oldest) = cache
                .iter()
                .filter_map(|(lease, ids)| ids.front().map(|pair| (*lease, pair.at_ms)))
                .min_by_key(|(_, at)| *at)
                .map(|(lease, _)| lease)
            else {
                break;
            };
            if let Some(ids) = cache.get_mut(&oldest) {
                ids.pop_front();
                if ids.is_empty() {
                    cache.remove(&oldest);
                }
            }
        }
        let ids = cache.entry(lease_id).or_default();
        if ids.iter().any(|pair| pair.request_id == request_id) {
            return false;
        }
        while ids.len() >= SERVED_PER_LEASE_CAPACITY {
            ids.pop_front();
        }
        ids.push_back(RelayedPair {
            request_id,
            at_ms: now_ms,
            charged: false,
        });
        true
    }

    /// How many `(lease_id, request_id)` pairs are remembered right now, so a
    /// gate can read the bound off the ledger the relay decided on rather than
    /// off a copy.
    pub fn served_len(&self) -> usize {
        self.pairs.values().map(VecDeque::len).sum()
    }

    /// How many request ids are remembered for ONE lease, so a gate can read
    /// the per-lease bound off the ledger rather than off a copy of it.
    pub fn served_len_for(&self, lease_id: u128) -> usize {
        self.pairs.get(&lease_id).map_or(0, VecDeque::len)
    }

    /// Mark one remembered pair CHARGED, and report whether this call is the
    /// one that charged it.
    ///
    /// `false` means the charge for this pair has already landed, which is what
    /// makes a retried or diamond-delivered relay debit once.
    ///
    /// A pair this ledger does not remember is CHARGED and recorded as charged,
    /// which is the only way the idempotence survives an
    /// eviction that two separate maps used to break. An absent pair means one of two things and both
    /// want the same answer: the relay was admitted and its entry has since
    /// aged out (so nobody has charged it, and answering `false` would be the
    /// free relay one map exists to prevent), or nothing admitted it at all (so
    /// this is the first charge). Recording it is what makes the SECOND call
    /// for the same pair answer `false`.
    fn mark_charged(&mut self, lease_id: u128, request_id: u128) -> bool {
        if let Some(pair) = self
            .pairs
            .get_mut(&lease_id)
            .and_then(|ids| ids.iter_mut().find(|pair| pair.request_id == request_id))
        {
            if pair.charged {
                return false;
            }
            pair.charged = true;
            return true;
        }
        // The same bounded insert [`Self::admit_served`] uses, so a pair that
        // arrives here alone cannot grow this map past its own ceiling. The
        // clock is this process's wall clock, the one every production caller
        // of [`Self::enter_relay`] stamps its entries with.
        Self::admit_pair(&mut self.pairs, lease_id, request_id, crate::now_ms());
        if let Some(pair) = self
            .pairs
            .get_mut(&lease_id)
            .and_then(|ids| ids.iter_mut().find(|pair| pair.request_id == request_id))
        {
            pair.charged = true;
        }
        true
    }

    /// How many remembered pairs have been charged. See [`Self::served_len`].
    pub fn debited_len(&self) -> usize {
        self.pairs
            .values()
            .flat_map(VecDeque::iter)
            .filter(|pair| pair.charged)
            .count()
    }

    /// Release one slot taken by [`Self::enter_relay`].
    ///
    /// Saturating rather than wrapping: an unpaired release is a caller bug, and
    /// a counter that wraps to 255 would strand the lease for its whole TTL.
    pub fn leave_relay(&mut self, lease_id: u128) {
        if let Some(in_flight) = self.inflight.get_mut(&lease_id) {
            *in_flight = in_flight.saturating_sub(1);
            // The review's M4: the key was kept at zero, so this map grew one
            // entry per lease and never shrank. Zero and absent are the same
            // fact (`Self::inflight` answers `0` for a missing key), so the
            // entry is dropped rather than parked.
            if *in_flight == 0 {
                self.inflight.remove(&lease_id);
            }
        }
    }

    /// How many relayed requests are in flight against one lease, for the panel
    /// row and `tcr peer ls --json`.
    pub fn inflight(&self, lease_id: u128) -> u8 {
        self.inflight.get(&lease_id).copied().unwrap_or(0)
    }

    /// How many leases have an in-flight entry at all, so a gate can read the
    /// rule [`Self::leave_relay`] documents, zero and absent are the same
    /// fact, off the ledger rather than off a copy of it. Same shape and same
    /// reason as [`Self::served_len`].
    pub fn inflight_tracked(&self) -> usize {
        self.inflight.len()
    }

    /// May this relayed request proceed against this lease?
    ///
    /// The order is the contract: [`LeaseRefusal::LeaseExpired`], then
    /// [`LeaseRefusal::LeaseSpent`], then [`LeaseRefusal::OwnerGuard`]. The
    /// third is not a consequence of the first two and must be able to fire
    /// alone.
    /// A lease id this ledger does not hold answers
    /// [`LeaseRefusal::LeaseExpired`] rather than a variant of its own. The
    /// lender's ledger is the authority and it drops rows it no longer honours,
    /// so "I have never heard of this lease" and "that lease is over" are the
    /// same fact told to the borrower, and a separate variant would invite a
    /// borrower to retry one of them.
    ///
    /// A unit this build refuses ([`LeaseUnit::Tokens`],
    /// [`LeaseUnit::Unknown`]) answers [`LeaseRefusal::Unsupported`], checked
    /// AFTER expiry and BEFORE spend: a dead lease is dead whatever its unit,
    /// but there is no honest way to compare a spend figure against a budget
    /// this build cannot read.
    pub fn may_relay(&self, lease_id: u128, now_ms: i64) -> Result<(), LeaseRefusal> {
        let Some(lease) = self.leases.iter().find(|lease| lease.lease_id == lease_id) else {
            return Err(LeaseRefusal::LeaseExpired);
        };
        if lease.expires_at_ms <= now_ms {
            return Err(LeaseRefusal::LeaseExpired);
        }
        // The end, checked beside the renewal deadline and BEFORE the
        // spend: at `until` the lender stops renewing, so a lease whose end has
        // passed is over whatever is left on its budget.
        //
        // The answer is `LeaseExpired` and not a variant of its own. A
        // `LeaseEnded` would be the honest word and it would be a NEW variant of
        // `LeaseRefusal`, which is in `tcr_peer_wire`, and nothing here adds a
        // variant there. `LeaseExpired`'s own doc is "the lease's absolute deadline has
        // passed", which `until` is, and the borrower's action is identical:
        // stop spending this lease.
        if lease.until.is_some_and(|until| until <= seconds_of(now_ms)) {
            return Err(LeaseRefusal::LeaseExpired);
        }
        let Some(budget) = fraction_budget(lease.unit) else {
            return Err(LeaseRefusal::Unsupported);
        };
        if lease.spent >= budget {
            return Err(LeaseRefusal::LeaseSpent);
        }
        // THE THIRD CHECK, AND IT FIRES ALONE. Reached with budget left on the
        // lease and the deadline still ahead, because the owner's guard band is
        // the owner's and a lease can never spend it. An absent measurement is a
        // refusal: see `Ledger::headroom`.
        //
        // Read for THIS LEASE'S SCOPE, not for the fleet: a lease
        // scoped to a group whose accounts are spent is refused even while the
        // pool has room, which is the whole of what scoping one means.
        match self.headroom_for(&self.scope_of(lease_id), lease.window) {
            Some(headroom) if headroom > 0.0 => Ok(()),
            _ => Err(LeaseRefusal::OwnerGuard),
        }
    }

    /// Note the path this lease's borrows are being served over.
    ///
    /// Called by whatever holds the stream, which is the only layer that knows
    /// which of the borrower's endpoints answered, and called again whenever
    /// that changes. Unknown is [`Option::None`] at the CALLER: there is no
    /// clearing call, because "this borrow arrived and I could not attribute
    /// it" must not erase the attribution of the borrow before it.
    pub fn note_lease_path(&mut self, lease_id: u128, path: crate::peer::config::Locator) {
        self.lease_paths.insert(lease_id, path);
    }

    /// Tokens this lease's charge is worth, or `None` when the lease is not
    /// measured in tokens.
    ///
    /// A [`LeaseUnit::Fraction`] lease has NO token count: it is a share of the
    /// owner's window, and the number of tokens that share buys is a fact about
    /// upstream's pricing that nothing on this Mac holds. Converting one would
    /// be inventing the figure, which is the distinction
    /// [`crate::status::PathStatus::tokens_per_hour`] is built on.
    fn tokens_of(unit: LeaseUnit, charge: f64) -> Option<u64> {
        let LeaseUnit::Tokens(amount) = unit else {
            return None;
        };
        let tokens = (amount as f64) * charge.clamp(0.0, 1.0);
        if !tokens.is_finite() || tokens <= 0.0 {
            return None;
        }
        // Bounded by the clamp above and by `amount`, so the cast cannot
        // saturate into a figure nobody granted.
        Some(tokens.round() as u64)
    }

    /// Debit one relayed request, exactly once.
    ///
    /// `observed_rise` is the utilization delta the lender measured on its own
    /// account; `request_id` is what makes a retry or a diamond delivery debit
    /// once.
    /// Returns what THIS call charged, so `0.0` means "already debited, nothing
    /// added" rather than "free". The running total is [`Lease::spent`], and a
    /// caller that wants it reads the lease.
    ///
    /// A negative `observed_rise`, a window that reset under the request, which
    /// the utilization headers really do report, charges [`MIN_DEBIT`] like any
    /// other sub-step rise. It never credits the lease: a reset is the owner's
    /// windfall, not the borrower's.
    pub fn debit(&mut self, lease_id: u128, _request_id: u128, observed_rise: f64) -> f64 {
        // The charge stays idempotent per pair, which is the invariant this
        // module's own docs state ("a retried or diamond-delivered relay debits
        // once, keyed on the request id"). What CHANGED for the review's H2 is
        // that this is no longer the only place the pair is looked at:
        // `enter_relay` refuses a replayed pair before the request is served, so
        // "debited once" and "served once" are now two facts about one
        // remembered pair instead of one sentence covering for the other. Both are kept, a replay cannot reach
        // here any more, and an accounting key that only one layer enforces is
        // an accounting key that stops being enforced the day that layer moves.
        //
        // Bounded, unlike the `HashSet` this was (the review's M4): a peer grew
        // it one entry per relayed request for the life of the process.
        //
        // And it reads the SAME map `enter_relay` wrote. The charge used to
        // keep a map of its own, filled only by the
        // relays that got this far and swept on the wall clock rather than on
        // the caller's, so the two evicted different pairs at different
        // moments and a pair the serve half had forgotten was served again for
        // nothing. There is no clock here any more because there is nothing
        // here to expire.
        if !self.mark_charged(lease_id, _request_id) {
            return 0.0;
        }
        let charge = if observed_rise > MIN_DEBIT {
            observed_rise
        } else {
            MIN_DEBIT
        };
        // An INDEX rather than a `&mut` row, because `self.persist()` below
        // needs `&self` and a live mutable borrow of `self.leases` would hold
        // the whole struct.
        let Some(at) = self
            .leases
            .iter()
            .position(|lease| lease.lease_id == lease_id)
        else {
            // No row to charge. The pair `enter_relay` recorded still stands,
            // so a retry of the same request cannot find a restored row and
            // charge it twice.
            return 0.0;
        };
        self.leases[at].spent += charge;
        // The per-path half. Three things have to be true for a charge to be
        // attributable: the lease is measured in tokens, the serving side noted
        // a path, and the ledger knows whose lease it is. Every one of the three
        // is absent on some real borrow, so the arithmetic is skipped rather
        // than defaulted. A default here would put somebody else's tokens on a
        // path they never went over.
        if let (Some(tokens), Some(path), Some(peer)) = (
            Self::tokens_of(self.leases[at].unit, charge),
            self.lease_paths.get(&lease_id).copied(),
            self.grantees.get(&lease_id).copied(),
        ) {
            match crate::peer::tunnel::path_meter().lock() {
                Ok(mut meter) => meter.charge_tokens(&peer, path, tokens, crate::now_ms()),
                // Never silent, and never a refused relay: the request is
                // already served and the lease is already charged by the time
                // this runs. See the same trade in `handle_forward_on`.
                Err(_) => tracing::error!(
                    "peer lease: the per-path meter's lock is poisoned, so this lease's tokens \
                     are missing from every per-path figure until restart"
                ),
            }
        }
        // A moved `spent` is the ceiling H2 exists to keep binding, so it is the
        // one figure a restart must not forget, but it is NOT written here.
        // This runs with the ledger's mutex held, once per relayed request, and
        // `persist` is a locked file round trip. See `Self::dirty` and
        // `Self::flush`.
        self.dirty = true;
        charge
    }

    /// Apply a borrower's [`Control::UsageHint`] to one lease.
    ///
    /// # Why the owner needs it at all
    ///
    /// In `serve` mode the owner sees the response and debits from the
    /// utilization rise it measured itself ([`Self::debit`]). A `hand`-mode
    /// response never reaches the owner, so nothing it can observe moves, and a
    /// lease whose `spent` never rises is a lease that never ends. The borrower
    /// reports the rise it saw and this adds it.
    ///
    /// # It can only ever RAISE `spent`
    ///
    /// The reporter is the party that gains by under-reporting, so a negative
    /// or zero hint is dropped rather than credited: the worst a lying borrower
    /// can do here is report nothing, which is the same position the owner is
    /// in without the frame at all. `until`, the lease expiry and the owner's
    /// choice to stop renewing all bind without any hint ever arriving.
    ///
    /// **Reconciliation against the owner's real usage probe
    /// (`src/manager/probing.rs`) is OUT OF SCOPE here.** That is the
    /// check that would turn this hint into an accounting figure, and until it
    /// exists a hand-mode lease is trusted to the limit of its `until`.
    ///
    /// Returns what it charged, so `0.0` means "nothing applied": an unknown
    /// lease id, a lease this ledger does not hold, or a hint that was not a
    /// positive, finite number.
    pub fn apply_usage_hint(&mut self, lease_id: u128, hinted: f64) -> f64 {
        if !hinted.is_finite() || hinted <= 0.0 {
            return 0.0;
        }
        let Some(at) = self
            .leases
            .iter()
            .position(|lease| lease.lease_id == lease_id)
        else {
            return 0.0;
        };
        self.leases[at].spent += hinted;
        // Same reason `debit` sets this rather than writing: this runs with the
        // ledger's mutex held and `persist` is a locked file round trip.
        self.dirty = true;
        hinted
    }

    /// Which window one lease is against, or `None` for a lease this ledger
    /// does not hold. For the lender's log line, which names the window and
    /// never the path or the model.
    pub fn window_of(&self, lease_id: u128) -> Option<Window> {
        self.leases
            .iter()
            .find(|lease| lease.lease_id == lease_id)
            .map(|lease| lease.window)
    }

    /// Every live lease, for `tcr peer ls --json` and the panel's activity
    /// line.
    ///
    /// A lease is live on a wall clock and nothing else: there is no heartbeat,
    /// no failure detector and no peer-down event anywhere in this design,
    /// because a sleeping laptop delivers no death notice and inferring one is
    /// how a lease turns into a timeout with extra steps.
    pub fn live(&self, now_ms: i64) -> Vec<Lease> {
        self.leases
            .iter()
            .copied()
            .filter(|lease| lease.expires_at_ms > now_ms)
            .collect()
    }

    /// How much of one scope's window this Mac has already promised to peers
    /// and not yet seen spent.
    ///
    /// **The figure a new grant has to be cut out of what is LEFT, not out of
    /// the whole.** `Ledger::grant` read `headroom_for` alone, which is what
    /// this Mac's own fleet has left, and nothing about what it had already
    /// lent: a borrower that re-asked held up to [`MAX_LEASES_PER_PEER`]
    /// leases, each at the full fraction, and the lender promised several
    /// times the room it had.
    ///
    /// Counted as the UNSPENT part of every live lease on the same scope and
    /// window, `unit` minus `spent`, never below zero: a lease that has
    /// already been spent has had its cost taken out of the measured headroom
    /// by the quota itself, so counting it twice would make a lender stop
    /// lending after one busy borrower.
    ///
    /// A lease with a byte or token unit contributes nothing: it is not a
    /// fraction of a window and there is no honest conversion.
    ///
    /// This is the figure the comments in this file used to call
    /// `lent_fraction`, which never existed under that name.
    pub fn committed_fraction(&self, scope: &LendScope, window: Window, now_ms: i64) -> f64 {
        self.leases
            .iter()
            .filter(|lease| lease.expires_at_ms > now_ms)
            .filter(|lease| lease.window == window)
            .filter(|lease| {
                // EVERY LEASE OVER ACCOUNTS THIS SCOPE ALSO DRAWS FROM, not
                // only the ones whose scope string matches. This compared
                // `scope_key`s, so an `all` lease and a `group:work` lease over
                // the same accounts never subtracted from each other and the
                // same room was promised twice, while `headroom_for` combines
                // the two figures with `scoped.min(all)` and therefore treats
                // them as one pool. See [`scopes_overlap`].
                let held = self
                    .scopes
                    .get(&lease.lease_id)
                    .map_or(&LendScope::All, |scope| scope);
                scopes_overlap(scope, held)
            })
            .map(|lease| match lease.unit {
                LeaseUnit::Fraction(fraction) => (fraction - lease.spent).max(0.0),
                LeaseUnit::Tokens(_) | LeaseUnit::Unknown => 0.0,
            })
            .sum()
    }

    /// How many live leases `peer` holds here right now, what
    /// [`MAX_LEASES_PER_PEER`] is measured against.
    ///
    /// Counted off [`Self::live`] and the grantee map rather than off a
    /// per-peer counter, for the reason every other figure in this type is:
    /// a counter is a second answer that has to be decremented on expiry, on
    /// revoke and on restore, and the one that gets forgotten locks a borrower
    /// out of a lender that owes it nothing.
    pub fn live_for(&self, peer: &PeerId, now_ms: i64) -> usize {
        self.leases
            .iter()
            .filter(|lease| lease.expires_at_ms > now_ms)
            .filter(|lease| self.grantees.get(&lease.lease_id) == Some(peer))
            .count()
    }
}

/// 128 opaque bits, from the platform CSPRNG.
///
/// `getrandom` directly rather than `noise::random_secret`: a lease id is not
/// Noise material, and routing it through the one file that may name `snow::`
/// would make a handshake module the source of a number that has nothing to do
/// with a handshake. It is a DIRECT dependency, already
/// in `Cargo.lock` through snow, so it adds one dependency edge and no package.
///
/// An error surfaces. A lease id that is not random is a lease anybody on the
/// mesh can spend, so there is no fallback here that is not worse than refusing.
pub fn random_id() -> Result<u128> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).context("peer lease: the platform CSPRNG refused")?;
    Ok(u128::from_be_bytes(bytes))
}

/// Ask one lender for a lease, over a CONTROL stream.
///
/// The answer IS the offer: one round trip, no negotiation, because a fraction
/// changes on every request and a second trip would be spent on a stale number.
/// `None` is every "no" a borrower can do nothing about, no address that
/// answers, a refusal, and an error is a protocol failure, which is a different
/// fact.
///
/// **The lender's half of this exchange is one match arm that does not exist
/// yet.** `listener::serve_control` answers `Hello` and `Ping` and refuses
/// everything phase 4 owns, so a real `Control::LeaseRequest` reaching a merged
/// build is refused there, not by [`Ledger::grant`]. Both halves exist; the arm
/// that joins them belongs in `src/peer/listener.rs`.
pub async fn request_lease(
    store: &PeerStore,
    lender: &PeerId,
    ask: &LeaseRequest,
) -> Result<Option<Lease>> {
    store.reload_if_changed();
    let Some(row) = store.row(lender) else {
        return Ok(None);
    };
    if !row.allow.allow_disclose {
        return Ok(None);
    }
    // The CONTROL stream a lease is asked over takes the same ways home the
    // SERVE stream does, forwarded hop included: a lender this node can only
    // reach through a friend must be askable, or the borrow stops one frame
    // before the stream that would have worked.
    let Some(mut stream) = serve::dial_peer_reaching(&row, store).await else {
        return Ok(None);
    };
    let key = crate::peer::id::NodeKey::load_or_mint(&serve::node_key_dir(store))
        .context("peer lease: this node has no keypair to ask for a lease with")?;
    let mut session = crate::peer::noise::dial_handshake(
        &mut stream,
        key.secret_bytes(),
        crate::peer::noise::Handshake::Return,
        Some(&lender.0),
        None,
    )
    .await
    .context("peer lease: the handshake with the lender failed")?;

    let header = StreamHeader {
        kind: StreamKind::Control,
        target: None,
        via: Vec::new(),
        hops_remaining: 1,
        request_id: random_id()?,
    };
    serve::send_control(&mut stream, &mut session, &header).await?;
    serve::send_control(&mut stream, &mut session, &Control::LeaseRequest(*ask)).await?;
    let answer: Control = serve::recv_control(&mut stream, &mut session).await?;
    match answer {
        Control::LeaseGrant(grant) => {
            if let Some(refusal) = grant.refusal {
                tracing::info!(
                    peer = %lender.display(),
                    refusal = ?refusal,
                    "peer lease: the lender refused to grant a lease"
                );
            }
            if let Some(lease) = grant.lease {
                // The BORROWER half. The owner follows a hand-mode
                // grant with the bearer on THIS session and renews it there, so
                // the session outlives this function: it is handed to a reader
                // task rather than dropped with the frame still in flight,
                // which is what made `HandedTokens::put` a function with no
                // production caller.
                //
                // Not awaited here, and the reason is the serve-mode borrow:
                // the borrower cannot know which mode funded its lease (the
                // mode is the LENDER's own policy and never crosses the wire),
                // so a blocking read for a frame that may never come would put
                // a timeout on the front of every borrow. The reader waits
                // instead, and a hand-mode borrow that arrives before its
                // bearer falls through to the serve path, which is what a
                // borrower did on every build before this one.
                tokio::spawn(read_handed_bearers(stream, session, lease, *lender));
            }
            Ok(grant.lease)
        }
        other => {
            tracing::warn!(
                peer = %lender.display(),
                answer = ?other,
                "peer lease: the lender answered something other than a grant"
            );
            Ok(None)
        }
    }
}

/// When this lease stops being a lease, on the borrower's own clock.
///
/// The earlier of its renewal deadline and the lending's `until`. Both are
/// already the pair [`lease_has_ended`] compares; this is the same question
/// asked as an INSTANT, because a reader that has to stop at the end needs to
/// know when, not only whether.
fn lease_ends_at_ms(lease: &Lease) -> i64 {
    let until_ms = lease
        .until
        .and_then(|until_s| i64::try_from(until_s.saturating_mul(1_000)).ok());
    match until_ms {
        Some(until_ms) => lease.expires_at_ms.min(until_ms),
        None => lease.expires_at_ms,
    }
}

/// Hold the CONTROL session open and take the owner's bearer, and every
/// renewal after it, into this process's [`handed_tokens`] store.
///
/// # This is the whole of the borrower's side of `hand` mode
///
/// The owner sends [`Control::Handoff`] on the session that granted the lease
/// and pushes a fresh one before the old expires. Both halves existed and
/// nothing connected them: `request_lease` read one frame and returned, so the
/// session was dropped with the bearer still in flight and
/// [`HandedTokens::put`] had no production caller at all. From the outside that
/// is indistinguishable from a lender that never sent one.
///
/// # Stopping
///
/// Three ways, and these are all three. The stream ends, which
/// is the owner closing or going away. The lease reaches its end, renewal
/// deadline or `until`, whichever is sooner, which is the one timeout here and
/// is why it is measured once against [`lease_ends_at_ms`] rather than sliced:
/// a read cancelled part way through a frame would leave the session out of
/// step, and after this one fires nothing reads the stream again. And the owner
/// simply stops renewing, which IS the revoke: the bearer held expires on its
/// own deadline, [`HandedTokens::bearer`] stops answering it, and no recall
/// frame exists because a token already on another machine cannot be taken
/// back.
///
/// Whichever way it ends, the copy goes: [`HandedTokens::forget`] runs on the
/// way out rather than waiting for a read that may never come.
async fn read_handed_bearers(
    mut stream: serve::PeerStream,
    mut session: crate::peer::noise::PeerSession,
    lease: Lease,
    lender: PeerId,
) {
    // THE HINT CHANNEL, registered before the first read: this session is the
    // one a hand-mode borrow reports its spend on, and a borrow served between
    // the bearer arriving and this task being ready would otherwise find no
    // meter and report nothing. See [`HandedMeter`].
    let (hints, mut hints_rx) = tokio::sync::mpsc::unbounded_channel();
    register_handed_meter(lease.lease_id, hints);

    // THE FRAME READER, and it is what makes the `select!` below safe at all.
    // `serve::recv_control` is not cancel safe: a hint arriving while a Handoff
    // is half read dropped that read with bytes already taken off the socket,
    // and the next read then met the tail of one frame as a length prefix, so
    // the Noise stream desynced and this borrower forgot its bearer for the
    // rest of the lease. `noise::FrameReader` holds the part-read bytes in the
    // loop's own state instead of in the cancelled future, which is the failure
    // its own doc describes.
    let mut frames = crate::peer::noise::FrameReader::new();

    let remaining = lease_ends_at_ms(&lease).saturating_sub(crate::now_ms());
    if remaining > 0 {
        let until_the_lease_ends =
            std::time::Duration::from_millis(u64::try_from(remaining).unwrap_or(0));
        let reader = async {
            loop {
                let frame: Result<Control> = tokio::select! {
                    // BIASED, so a queued hint is never starved by a lender
                    // that talks constantly: the spend is what keeps the
                    // owner's ceiling binding, and a hint that waits for a
                    // quiet session is a debit that arrives after the lease is
                    // over.
                    biased;
                    Some(hint) = hints_rx.recv() => {
                        if let Err(err) =
                            serve::send_control(&mut stream, &mut session, &hint).await
                        {
                            tracing::debug!(
                                peer = %lender.display(),
                                error = %err,
                                "peer hand: this lease's spend could not be reported to its \
                                 owner, so the owner's own ledger is behind"
                            );
                            return;
                        }
                        continue;
                    }
                    bytes = frames.recv_encrypted(&mut stream, &mut session.transport) => {
                        bytes.and_then(|bytes| {
                            serde_json::from_slice::<Control>(&bytes)
                                .context("peer lease: a frame on this lease's session did not parse")
                        })
                    }
                };
                let frame: Control = match frame {
                    Ok(frame) => frame,
                    Err(err) => {
                        tracing::debug!(
                            peer = %lender.display(),
                            error = %err,
                            "peer lease: the lender's control session ended, so no further \
                             bearer arrives on it"
                        );
                        return;
                    }
                };
                match frame {
                    Control::Handoff {
                        lease_id,
                        access_token,
                        expires_at_ms,
                        utilization,
                    } => {
                        if lease_id != lease.lease_id {
                            // A bearer for a lease this session did not mint is
                            // refused rather than stored: the store is keyed by
                            // lease id and taking this one would let a lender
                            // overwrite the bearer of a lease it does not hold.
                            tracing::warn!(
                                peer = %lender.display(),
                                "peer lease: a bearer arrived for a lease this session did \
                                 not mint, so it is dropped"
                            );
                            continue;
                        }
                        // A handoff with no baseline is refused and the lender
                        // is named: see [`handed_baseline`], which owns the
                        // whole of that rule and the line.
                        let baseline = match handed_baseline(&lender, utilization) {
                            Ok(baseline) => baseline,
                            Err(line) => {
                                tracing::warn!("{line}");
                                continue;
                            }
                        };
                        match handed_tokens().lock() {
                            Ok(mut held) => {
                                held.put(
                                    lease_id,
                                    access_token.reveal().to_string(),
                                    expires_at_ms,
                                );
                                // The baseline the next answer is measured
                                // against, taken monotonically: a renewal
                                // carries the owner's window as it stands now,
                                // and a later frame can only move it forward.
                                note_handed_baseline(lease_id, baseline);
                                // The deadline and the lease, never the token:
                                // this line is the one an operator reads while
                                // a hand-mode borrow is running.
                                tracing::info!(
                                    peer = %lender.display(),
                                    expires_at_ms,
                                    "peer lease: the lender handed this Mac a bearer for the \
                                     lease it just granted"
                                );
                            }
                            Err(_) => {
                                tracing::error!(
                                    "peer lease: the handed-token store's lock is poisoned, so \
                                     this borrow falls back to the lender's own path"
                                );
                                return;
                            }
                        }
                    }
                    other => {
                        // Not fatal: the session is the lender's to speak on and
                        // a frame this build does not expect here costs nothing
                        // to skip. `Handoff`'s own `Debug` redacts the bearer,
                        // so this line is safe whatever arrived.
                        tracing::warn!(
                            peer = %lender.display(),
                            frame = ?other,
                            "peer lease: an unexpected frame arrived on a lease's control \
                             session; ignoring it"
                        );
                    }
                }
            }
        };
        if tokio::time::timeout(until_the_lease_ends, reader)
            .await
            .is_err()
        {
            tracing::debug!(
                peer = %lender.display(),
                "peer lease: this lease reached its end, so the owner's renewals stop being \
                 read and the bearer held for it is dropped"
            );
        }
    }
    match handed_tokens().lock() {
        Ok(mut held) => {
            held.forget(lease.lease_id);
        }
        Err(_) => tracing::error!(
            "peer lease: the handed-token store's lock is poisoned, so an ended lease's bearer \
             is left in memory until restart"
        ),
    }
    // The meter goes with the bearer: nothing can be reported on a session
    // that has ended, and a meter left behind would hold a sender nobody reads
    // for the life of the process.
    forget_handed_meter(lease.lease_id);
}

/// The peer-lease implementation of the proxy's fallback seam.
///
/// Implementation ONE of [`FallbackProvider`]; the api-key backend is slot two
/// behind the same trait.
pub struct PeerLeaseProvider {
    /// The peers file this provider reads its policy from. Held as a path and
    /// opened per ask rather than as a long-lived store, because this seam is
    /// reached only when the whole local fleet could not serve a request, a
    /// rare event by construction, and a path cannot go stale.
    peers_path: PathBuf,
    /// Leases this node has been granted, by lender. A CACHED HINT and every
    /// surface that renders it says so: the lender's ledger is authoritative,
    /// which is why a refusal frame drops the row here rather than arguing.
    leases: Mutex<HashMap<PeerId, Lease>>,
    /// Where that cache is written so something other than this process can
    /// read it: the `borrowed` section of the peers file's own
    /// `peer-state.json` ([`crate::peer::state::BorrowedRow`]).
    ///
    /// Derived from `peers_path` rather than passed in, by the same
    /// [`crate::peer::serve::peer_state_path`] every other peer surface uses,
    /// so one `--peers` argument still points the whole peer surface at one
    /// directory and a test cannot write the operator's real file.
    state_path: PathBuf,
    /// One gate per lender, held for the whole of an ASK, so that N concurrent
    /// first requests against one lender mint ONE lease between them instead of
    /// N.
    ///
    /// Measured before it existed: five concurrent borrows against a lender
    /// granting `max_inflight: 2` were all five SERVED and none refused, because
    /// each of the five missed the cache, released the cache lock, asked, and
    /// got a lease of its own, so the cap that binds concurrency within one
    /// lease never bound anything across them, and the only ceiling left was the
    /// lender's [`MAX_LEASES_PER_PEER`].
    ///
    /// A `tokio::sync::Mutex` and not the `std` one beside it, because this is
    /// the one lock in this type that is deliberately held ACROSS an await, the
    /// ask itself, a dial, a handshake and two frames. `self.leases` is still
    /// the `std` lock and is still never held across an await.
    ///
    /// Bounded by the peers file: one entry per lender ever asked, and a peers
    /// file is an operator-written list, not peer-driven input. It is not
    /// pruned, for [`Ledger::live_for`]'s reason, an entry dropped on one path
    /// and not another is how a borrower ends up with two gates for one lender
    /// and no single-flight at all.
    ask_gates: tokio::sync::Mutex<HashMap<PeerId, Arc<tokio::sync::Mutex<()>>>>,
    /// Where a HAND-mode request goes and the client it goes with: decision
    /// 15's whole difference, which is that this node dials upstream itself.
    ///
    /// A field rather than a constant because a test has to point it at a fake
    /// origin, and pointing it there is the only way to observe WHICH node
    /// spent the token. `None` means this node will not serve a hand-mode
    /// request at all and every lease falls through to the `serve` path, which
    /// is the behaviour of every build before row 15.
    hand_egress: Option<(String, reqwest::Client)>,
}

impl PeerLeaseProvider {
    /// A provider that reads its policy from `peers_path`, with the borrowed
    /// leases the last process left on disk already in its cache.
    ///
    /// The restore is here and not in a separate call because there is no
    /// instant between the two where a half-built provider would be correct:
    /// a borrower that answered one request off an empty cache would ask its
    /// lender for a second lease it already holds. Expired rows are dropped by
    /// [`crate::peer::state::load`] on the way in and counted in the
    /// `restored=N expired=M` line below, the same shape the lender's
    /// [`Ledger::restored_from`] prints.
    ///
    /// An unreadable state file is an EMPTY cache and a warning, never a
    /// refusal to build: every lease in it is re-askable, and a borrower that
    /// refused to borrow over a cache file would have traded a real service
    /// for a durability promise.
    pub fn new(peers_path: PathBuf) -> Self {
        let state_path = crate::peer::serve::peer_state_path(&peers_path);
        let mut leases: HashMap<PeerId, Lease> = HashMap::new();
        let now_ms = crate::now_ms();
        match crate::peer::state::load(&state_path, now_ms) {
            Ok(state) => {
                let rows = state.borrowed.len();
                for row in state.borrowed {
                    leases.insert(row.lender, row.lease);
                }
                tracing::info!(
                    path = %state_path.display(),
                    restored = leases.len(),
                    // A second row for one lender is the same lender's newer
                    // lease overwriting its older one, which is what the cache
                    // holds anyway. Counted so the two numbers cannot be read
                    // as agreeing when they do not.
                    rows,
                    "peer lease: borrowed leases restored"
                );
            }
            Err(err) => tracing::warn!(
                path = %state_path.display(),
                error = %err,
                "peer lease: the borrowed leases could not be read, so this borrower starts \
                 with none and asks every lender again"
            ),
        }
        Self {
            peers_path,
            leases: Mutex::new(leases),
            state_path,
            ask_gates: tokio::sync::Mutex::new(HashMap::new()),
            // Built here so a production build can serve a hand-mode lease with
            // no further wiring, and `no_proxy` for the reason
            // `peer::serve::serve_on_own_account` gives: an environment proxy
            // variable must not silently route a request that is spending
            // somebody else's account through a third host.
            hand_egress: reqwest::Client::builder()
                .no_proxy()
                .build()
                .inspect_err(|err| {
                    tracing::warn!(
                        error = %err,
                        "peer lease: no client for hand-mode requests, so every lease falls \
                         back to serving through the owner"
                    );
                })
                .ok()
                .map(|client| (crate::config::default_upstream(), client)),
        }
    }

    /// Point hand-mode requests at `base` through `client`.
    ///
    /// The seam a test needs, and the seam an egress policy would use: a client
    /// carrying a `reqwest::ClientBuilder::resolve` override reaches a fake
    /// origin on loopback without any DNS or TLS pretence, which is how the
    /// gate observes which node spent the token.
    pub fn with_hand_egress(mut self, base: String, client: reqwest::Client) -> Self {
        self.hand_egress = Some((base, client));
        self
    }

    /// Is there a live handed bearer for this lease right now?
    ///
    /// The borrower's whole test for "is this lease in hand mode": the owner
    /// backs a `hand` grant with a pushed token, and a mode with no token is a
    /// request this node could not pay for.
    fn handed_bearer_is_live(&self, lease_id: u128) -> bool {
        let now_ms = crate::now_ms();
        handed_tokens()
            .lock()
            .is_ok_and(|store| store.bearer(lease_id, now_ms).is_some())
    }

    /// Serve one ask on the handed bearer, from this node.
    async fn serve_handed(&self, ask: &Ask<'_>, lease: &Lease) -> Result<Option<Response>> {
        let Some((base, client)) = self.hand_egress.as_ref() else {
            return Ok(None);
        };
        serve_on_handed_bearer(
            base,
            client,
            ask,
            lease.lease_id,
            lease.window,
            crate::now_ms(),
        )
        .await
    }

    /// The leases this node holds right now, as the state file spells them.
    ///
    /// The one read-only view of the cache: a surface that wants the borrower's
    /// side reads this rather than the state file, so it cannot disagree with
    /// the process that is spending them.
    pub fn borrowed(&self) -> Vec<crate::peer::state::BorrowedRow> {
        let Ok(cached) = self.leases.lock() else {
            return Vec::new();
        };
        cached
            .iter()
            .map(|(lender, lease)| crate::peer::state::BorrowedRow {
                lease: *lease,
                lender: *lender,
            })
            .collect()
    }

    /// Write the cache to the `borrowed` section of the state file.
    ///
    /// Called when the SET of live borrowed leases changes, one granted, one
    /// dropped after a refusal, and never per relayed request: the borrower's
    /// `spent` is the lender's own figure, kept in the lender's ledger, so
    /// there is nothing on this side that moves per request to write down.
    ///
    /// A failed write is LOGGED and never propagated, for
    /// [`Ledger::persist`]'s reason: the caller is on a live client's answer
    /// path, and a borrower that refused a request it can serve because a
    /// cache file is read-only would be the worse trade. The warning is how an
    /// operator learns their `tcr peer ls` will read `until: null`.
    fn persist_borrowed(&self) {
        let rows = self.borrowed();
        if let Err(err) = crate::peer::state::save_borrowed(&self.state_path, &rows) {
            tracing::warn!(
                path = %self.state_path.display(),
                error = %err,
                "peer lease: the borrowed leases could not be written, so no other process \
                 can see what this Mac is borrowing"
            );
        }
    }

    /// The cached lease for this lender while it is live and for this window,
    /// or `None`.
    ///
    /// Its own function because [`Self::lease_for`] reads the cache TWICE, once
    /// on the fast path and once behind the ask gate, and two hand-written
    /// copies of "live, and for this window" are two chances to disagree about
    /// what a usable cached lease is.
    fn cached_lease(&self, lender: &PeerId, window: Window) -> Option<Lease> {
        let now = crate::now_ms();
        let cached = self.leases.lock().ok()?;
        let lease = cached.get(lender)?;
        if lease.expires_at_ms > now && lease.window == window {
            Some(*lease)
        } else {
            None
        }
    }

    /// The lease to spend on this lender: the cached one while it is live,
    /// otherwise one freshly asked for.
    ///
    /// The ask is SINGLE-FLIGHTED per lender (see [`Self::ask_gates`]): the
    /// first caller to miss the cache holds that lender's gate for the whole
    /// round trip and the others wait on it, then read the lease it cached. Any
    /// other arrangement lets a borrower mint one lease per concurrent request
    /// and spend `max_inflight` once per lease, which is the same as having no
    /// cap.
    async fn lease_for(&self, store: &PeerStore, lender: &PeerId, window: Window) -> Option<Lease> {
        if let Some(lease) = self.cached_lease(lender, window) {
            return Some(lease);
        }
        let gate = {
            let mut gates = self.ask_gates.lock().await;
            Arc::clone(
                gates
                    .entry(*lender)
                    .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
            )
        };
        // Held until this function returns. The map's own lock is already gone:
        // holding it here would serialise asks against DIFFERENT lenders too,
        // which is a fleet-wide queue behind one sleeping laptop.
        let _asking = gate.lock().await;
        // Behind the gate, because the caller that just released it is the one
        // that filled the cache, this read is where the other four of five
        // concurrent asks get their lease.
        if let Some(lease) = self.cached_lease(lender, window) {
            return Some(lease);
        }
        let ask = LeaseRequest {
            window,
            unit: LeaseUnit::Fraction(DEFAULT_ASK_FRACTION),
            ttl_s: DEFAULT_ASK_TTL_S,
            max_inflight: DEFAULT_ASK_MAX_INFLIGHT,
        };
        let granted = match request_lease(store, lender, &ask).await {
            Ok(lease) => lease?,
            Err(err) => {
                tracing::warn!(
                    peer = %lender.display(),
                    error = %err,
                    "peer lease: asking for a lease failed"
                );
                return None;
            }
        };
        if let Ok(mut cached) = self.leases.lock() {
            cached.insert(*lender, granted);
        }
        // AFTER the guard is dropped: `persist_borrowed` takes the same lock to
        // read the rows out, and taking it twice on one thread is a deadlock.
        self.persist_borrowed();
        Some(granted)
    }
}

/// What a borrower asks for when nothing has told it to ask for less: half a
/// window, ten minutes, four at once. Every one of the three is clamped by the
/// lender (see [`clamp_to_grant`]), so these are an opening position and not a
/// promise, which is why they are written as an ask rather than as a policy.
const DEFAULT_ASK_FRACTION: f64 = 0.5;
const DEFAULT_ASK_TTL_S: u32 = 600;
const DEFAULT_ASK_MAX_INFLIGHT: u8 = 4;

/// The lenders this node may ask, in the order to ask them.
///
/// # The filter is what it always was, the ORDER is new
///
/// Both halves used to sit inside [`FallbackProvider::try_serve`]'s loop as a
/// `continue`, and the loop walked the peers file in file order. A lender this
/// Mac can open a socket to and one it can only reach by spending a third
/// Mac's bytes were therefore tried in whatever order an operator happened to
/// pair them in.
///
/// Sorting is the whole change and the key is one bit, has this row an address
/// of its own. A row with none is still asked, last: that is the case
/// [`serve::has_a_way_back`] exists for, a lender reachable only because a
/// mutual friend will carry, and also a lender that parked
/// a carrier at that friend because nothing can dial it at all
/// ([`crate::peer::tunnel::ReverseDesk`]). Asking it first would put a
/// forwarder's bytes and a forwarder's latency in front of a socket this node
/// could have opened itself.
///
/// The sort is stable, so within each group the peers file's own order stands
/// and an operator who ordered their file meant it.
pub fn lenders_in_ask_order(store: &PeerStore) -> Vec<PeerRow> {
    let mut rows: Vec<PeerRow> = store
        .peers()
        .into_iter()
        .filter(|row| row.allow.allow_disclose)
        // `has_a_way_back` and not `has_endpoint`: a lender whose row carries
        // no address at all is still reachable when this node holds
        // `allow.carry` on a Mac that can forward to it, and a filter that
        // read only the row would skip exactly the lender a forwarder exists
        // for.
        .filter(|row| serve::has_a_way_back(row, store))
        .collect();
    rows.sort_by_key(|row| u8::from(!row.has_endpoint()));
    rows
}

/// Did this failure happen BEFORE anything left this Mac?
///
/// The one question that separates "ask the next lender" from "tell the client
/// the outcome is unknown", asked on the hand-mode path where the request is
/// sent by this Mac's own `reqwest` client rather than over a peer stream.
///
/// The error arrives as an [`anyhow::Error`] with context wrapped round it, so
/// the chain is walked for the `reqwest::Error` underneath rather than the
/// string being matched: a connect failure and a resolver failure are the two
/// shapes where no byte was sent, and they are exactly the two `src/proxy.rs`
/// separates out on the direct path with the same two predicates. Anything
/// else, a reply that never arrived, a body that died mid-stream, may have run
/// upstream and is never offered to another lender.
pub fn nothing_left_this_mac(err: &anyhow::Error) -> bool {
    err.chain()
        .filter_map(|cause| cause.downcast_ref::<reqwest::Error>())
        .any(|cause| cause.is_connect() || crate::proxy::is_offline_error(cause))
}

/// The answer a borrower gives its client when a borrowed request was
/// delivered and its outcome is unknowable.
///
/// **502 and `x-should-retry: false`**, the shape `src/proxy.rs` already
/// answers with when an attempt got past connect before failing: the request
/// may have run upstream, so a client that retries could pay for it twice. A
/// 429 would be worse than useless here, Claude Code retries one by design.
///
/// The one thing this must never be is `None`: the ladder's next rung is a
/// retryable answer, and every rung below this point ends in the client
/// sending the same body again.
fn delivered_unknown_response() -> Response {
    let payload = serde_json::json!({
        "type": "error",
        "error": {
            "type": "proxy_error",
            "message": "This request was sent to a peer's account and no answer came back \
                        before the deadline. It may have been served there, so it was NOT \
                        sent again: retrying may pay for it twice.",
        },
    });
    let mut response = Response::new(axum::body::Body::from(payload.to_string()));
    *response.status_mut() = axum::http::StatusCode::BAD_GATEWAY;
    let headers = response.headers_mut();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    headers.insert(
        "x-should-retry",
        axum::http::HeaderValue::from_static("false"),
    );
    response
}

impl FallbackProvider for PeerLeaseProvider {
    fn name(&self) -> &'static str {
        "peer-lease"
    }

    /// The path refusal is FIRST and it is unconditional, before any lease is
    /// looked at, so a credential path cannot reach a lender even by way of a
    /// lease that would otherwise have served it. Everything after it answers
    /// `None`, which costs the caller only the next rung of the ladder.
    fn try_serve<'a>(&'a self, ask: &'a Ask<'a>) -> BoxFuture<'a, Option<Response>> {
        Box::pin(async move {
            if !serve::serve_is_allowed_for_path(ask.path) {
                tracing::debug!(
                    path = ask.path,
                    "not relaying a client-credential path, falling back to the local path"
                );
                return None;
            }

            let store = match PeerStore::open(&self.peers_path) {
                Ok(store) => store,
                Err(err) => {
                    // Surfaced, never swallowed: a peers file that cannot be
                    // read is not the same fact as a node with no peers, and
                    // the ladder's next rung answers the client either way.
                    tracing::warn!(
                        error = %err,
                        path = %self.peers_path.display(),
                        "peer lease: the peers file is unreadable, so nothing is borrowed"
                    );
                    return None;
                }
            };

            // A request with no model-scoped window can only be served by an
            // untiered lease, and `7d_oi` is the only model-scoped window
            // upstream reports at all, so an untiered window is what is asked
            // for, and a model-scoped ask would be inventing a bucket upstream
            // does not report for anything but Fable.
            let window = Window::SevenDay;

            // Filtered and ordered in one place ([`lenders_in_ask_order`]): a
            // lender this Mac can dial is asked before one that needs a third
            // Mac to carry, and a row with no way back at all is not asked.
            for row in lenders_in_ask_order(&store) {
                let Some(lease) = self.lease_for(&store, &row.node, window).await else {
                    continue;
                };
                // The `hand`/`serve` fork, and the ONLY place a borrowed request
                // chooses between the two modes. A handed bearer for this
                // lease means the owner granted it in `hand` mode and pushed a
                // token, so the request is served from here and never crosses
                // to the owner; with no handed bearer the `serve` path below is
                // unchanged, byte for byte, which is what every grant written
                // before row 15 already meant.
                //
                // The borrower reads the MODE off the presence of a token
                // rather than off a field it was told: a mode the owner
                // asserted but never backed with a bearer would otherwise
                // become a request this node cannot pay for.
                if self.handed_bearer_is_live(lease.lease_id) {
                    match self.serve_handed(ask, &lease).await {
                        Ok(Some(response)) => return Some(response),
                        Ok(None) => {}
                        // A CONNECT failure is the one error here that is not
                        // a delivery. `reqwest` reports "the socket never
                        // opened" and "the reply never arrived" as the same
                        // type, and this arm read both as delivered, so a
                        // lender whose handed bearer pointed at an upstream
                        // this Mac could not even reach ended the ladder with
                        // a no-retry 502 over a request that had not left the
                        // box. The direct path separates the two with the same
                        // predicate (`src/proxy.rs`, `is_connect` plus the
                        // resolver check), and this is that predicate.
                        Err(err) if nothing_left_this_mac(&err) => {
                            tracing::warn!(
                                peer = %row.node.display(),
                                error = %err,
                                "peer lease: the hand-mode request never reached an \
                                 upstream, so nothing was served and the next lender may \
                                 be asked"
                            );
                        }
                        Err(err) => {
                            // NOT `continue`. A hand-mode request leaves from
                            // THIS Mac, on the owner's bearer, so an error
                            // here is a request that may already be running
                            // upstream. Offering the same body to the next
                            // lender is how one POST is executed twice, which
                            // is the defect `Borrowed::DeliveredUnknown`
                            // exists for on the serve path.
                            tracing::warn!(
                                peer = %row.node.display(),
                                error = %err,
                                "peer lease: the hand-mode request failed after it left \
                                 this Mac; it is NOT offered to another lender"
                            );
                            return Some(delivered_unknown_response());
                        }
                    }
                    continue;
                }
                // The ASK's headers, which are the client's own with every
                // credential already removed by `Ask::scrubbed`. This used to be an
                // empty map, on the reading that an `Ask` could hold nothing
                // safe: the lender then sent a bearer and nothing else to the
                // API, which answers 400 to a request with no
                // `anthropic-version`, and the borrower served that 400 to its
                // client. `serve_request_from` narrows this to
                // `LENDER_FORWARDED_HEADERS` before anything crosses the host
                // boundary, and scrubs again on the way.
                match serve::open_serve(&row.node, &lease, ask, &ask.headers, &store).await {
                    Ok(serve::Borrowed::Served(response)) => return Some(response),
                    // **THE LADDER STOPS HERE.** The body crossed to that
                    // lender and no answer came back, so this Mac cannot know
                    // whether the request ran. Asking the next lender would
                    // execute and bill it a second time, and answering `None`
                    // would hand the client a 429 it is built to retry, which
                    // is the same double-send one hop further out. So the
                    // client is told the outcome is unknown, once.
                    Ok(serve::Borrowed::DeliveredUnknown) => {
                        return Some(delivered_unknown_response())
                    }
                    Ok(serve::Borrowed::NotThisLender) => {
                        // A refusal, or nothing reachable. The lease may be the
                        // stale half of it, so the hint is dropped and the next
                        // ask re-asks rather than spending a lease the lender
                        // has forgotten.
                        let dropped = match self.leases.lock() {
                            Ok(mut cached) => cached.remove(&row.node).is_some(),
                            Err(_) => false,
                        };
                        // Only when a row actually went: a refusal against a
                        // lender this node holds no lease for must not rewrite
                        // the state file on every relayed request.
                        if dropped {
                            self.persist_borrowed();
                        }
                    }
                    Err(err) => tracing::warn!(
                        peer = %row.node.display(),
                        error = %err,
                        "peer lease: the SERVE stream failed"
                    ),
                }
            }
            None
        })
    }
}

/// One line of an account card's "Lent to …", which is the read-only
/// view of a lease **from the account's side**.
///
/// Computed from the lender's OWN grants and nothing else, no wire, no peer
/// asked, no ledger consulted. That is the whole design of the line: an
/// operator looking at an account wants to know what they have promised, which
/// is a fact about their own file, and a figure that needed a reachable peer
/// would render as blank on a sleeping laptop.
///
/// [`Self::peer`] is what a person reads and [`Self::peer_id`] is what a
/// program joins on. The line carried only the label, which is the operator's
/// own display string: two Macs may be labelled the same, a label is renamed
/// whenever its owner feels like it, and every other cross-surface join in this
/// tree moved to the wire id ([`crate::status::PeerStatusRow::id`]), so a panel
/// merging this line with a peer row had nothing to merge on.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LentTo {
    /// The trusted Mac's label, already sanitized in the peers file. For the
    /// screen; [`Self::peer_id`] is what a row is joined on.
    pub peer: String,
    /// The same Mac's pinned key in its WIRE form, the spelling
    /// `tcr peer ls --json` writes as a row's `node` and the only one
    /// [`tcr_peer_wire::PeerId::parse`] reads back.
    pub peer_id: String,
    /// The lease id, as hex, the handle `tcr peer lend --revoke` takes, so the
    /// panel's click target on this line can revoke or re-lend exactly the
    /// lease the operator is looking at. A string and not a number: see
    /// [`crate::peer::config::LendGrant::id`].
    pub id: String,
    /// What the lease draws from, in the CLI's own `--scope` spelling.
    pub scope: tcr_peer_wire::LendScope,
    pub window: Window,
    /// The grant's ceiling, which is what the card's "20 %" is.
    pub fraction: f64,
    /// The end, when the operator set one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<u64>,
    /// Whether that end has passed. Always present, because an absent key
    /// would read as "still running", the one direction that matters here.
    pub ended: bool,
}

/// Which accounts each of this node's grants lends from, keyed by account
/// label: the `lentTo` block of `tcr peer ls --json`.
///
/// `accounts` is `(name, groups)` per local account, the name `tcr status`
/// prints for it, and the groups it is in. Passed in rather than read off a
/// `Manager` for the reason every other pure function in this file is: the
/// decision being made is scope membership, and it is testable without a
/// fleet.
///
/// **The map is keyed by that name, and membership is decided on the SANITIZED
/// label.** The two are the same string for every account whose name is
/// already a label, and they part only for one whose is not, an email, a
/// uuid. That account is still in the map (its card shows the line under an
/// `all` or a `group:` lease, which was fixed) and still cannot
/// be picked out by `account:<label>`, because
/// [`tcr_peer_wire::sanitize_label`] refuses the name and
/// [`tcr_peer_wire::LendScope::parse`] refuses to build a scope naming it. One
/// vocabulary for the key (the panel joins its account card on it), one for
/// the match ([`crate::manager::Manager::lendable_fraction`] uses exactly the
/// same sanitized value, so the headroom and the line agree about which
/// accounts a scope draws from).
///
/// An account no grant reaches is **absent from the map, not present with an
/// empty list**: the panel hides the line when it is empty, and
/// "no key" is one fact for it to read rather than two.
///
/// # An ENDED lease is still a row
///
/// The rule is: "a lease with an end that passed is kept in the list, greyed,
/// so the operator sees what was lent and can re-lend with one click". So a
/// grant past its `until` appears here with [`LentTo::ended`] set rather than
/// being filtered out, the panel greys it, and an operator who cannot see what
/// they lent cannot re-lend it.
pub fn lent_to(
    store: &PeerStore,
    accounts: &[(String, Vec<String>)],
) -> std::collections::BTreeMap<String, Vec<LentTo>> {
    store.reload_if_changed();
    let mut out: std::collections::BTreeMap<String, Vec<LentTo>> =
        std::collections::BTreeMap::new();
    for row in store.peers() {
        for grant in &row.lend {
            for (name, groups) in accounts {
                // The sanitized label decides membership, the name is the key.
                // See this function's doc. `unwrap_or_default` gives the
                // empty string for a name that is not a label, which
                // `sanitize_label` refuses to produce and so no scope can
                // name: exactly what `Manager::lendable_fraction` does with
                // the same account.
                let label = tcr_peer_wire::sanitize_label(name).unwrap_or_default();
                if !scope_covers(&grant.scope, &label, groups) {
                    continue;
                }
                out.entry(name.clone()).or_default().push(LentTo {
                    peer: row.label.clone(),
                    peer_id: row.node.to_wire(),
                    id: crate::peer::config::lease_id_string(grant.id),
                    scope: grant.scope.clone(),
                    window: grant.window,
                    fraction: grant.fraction,
                    until: grant.until,
                    // `PeerStore::peers` derived this against the clock; it is
                    // never read off the file. See `LendGrant::ended`.
                    ended: grant.ended,
                });
            }
        }
    }
    out
}

/// Whether one lease scope draws from one account.
///
/// The ONE answer to "is this account inside that scope", read by the headroom
/// arithmetic ([`crate::manager::Manager::lendable_fraction`]), by the picker
/// restriction and by [`lent_to`]. Three readers and one function on purpose:
/// a scope that means one set of accounts when the headroom is computed and
/// another when the request is served is a lease that advertises one account's
/// room and spends another's.
///
/// A group is matched by NAME against the account's own group list, so the
/// answer follows `tcr group` hot-reloads rather than a membership frozen at
/// lend time.
pub fn scope_covers(scope: &tcr_peer_wire::LendScope, label: &str, groups: &[String]) -> bool {
    match scope {
        tcr_peer_wire::LendScope::All => true,
        tcr_peer_wire::LendScope::Group(name) => groups.iter().any(|group| group == name),
        tcr_peer_wire::LendScope::Accounts(labels) => labels.iter().any(|named| named == label),
    }
}

/// Every lender row in a peers file, for the provider and for the surfaces that
/// count them.
pub fn lender_rows(store: &PeerStore) -> Vec<PeerRow> {
    store
        .peers()
        .into_iter()
        .filter(|row| row.allow.allow_disclose && row.has_endpoint())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lease on the seven-day window, live well past `now`.
    fn live_lease(lease_id: u128, now_ms: i64) -> Lease {
        Lease {
            lease_id,
            window: Window::SevenDay,
            unit: LeaseUnit::Fraction(0.20),
            granted_at_ms: now_ms,
            expires_at_ms: now_ms + 300_000,
            spent: 0.0,
            max_inflight: 2,
            until: None,
        }
    }

    /// **A rise smaller than the utilization header's own step still costs
    /// `MIN_DEBIT`.**
    ///
    /// The header moves in steps, so a request cheaper than one step reports a
    /// rise of zero on the answer that paid for it and the whole of it later,
    /// on some other answer or on none at all. The serve path charges
    /// `MIN_DEBIT` for exactly this (`Ledger::debit`) and the hand path charged
    /// nothing, which is a lease a small-request borrower could hold open for
    /// as long as it liked.
    ///
    /// Watch it fail by returning `spent: rise` from `usage_hint`: the tiny
    /// rise is reported as itself, well under the floor.
    #[test]
    fn a_positive_rise_under_the_floor_is_charged_the_floor() {
        let tiny = MIN_DEBIT / 10.0;
        let Some(Control::UsageHint { spent, .. }) = usage_hint(7, Some(0.40), Some(0.40 + tiny))
        else {
            panic!("a risen window reports a hint");
        };
        assert!(
            (spent - MIN_DEBIT).abs() < f64::EPSILON,
            "a rise of {tiny} is charged the floor, and this charged {spent}"
        );

        let Some(Control::UsageHint { spent, .. }) = usage_hint(7, Some(0.40), Some(0.47)) else {
            panic!("a risen window reports a hint");
        };
        assert!(
            (spent - 0.07).abs() < 1e-9,
            "a rise above the floor is still itself, and this charged {spent}"
        );

        assert!(
            usage_hint(7, Some(0.40), Some(0.40)).is_none(),
            "an answer that moved nothing is still free: the floor lifts a positive rise and \
             never invents one"
        );
        assert!(
            usage_hint(7, Some(0.40), Some(0.10)).is_none(),
            "and a window that reset under the request is the owner's windfall, never a credit"
        );
    }

    /// **The meter's baseline only ever moves forward, and it survives the
    /// session being re-registered.**
    ///
    /// Two failures in one: with `max_inflight` above one the answers arrive in
    /// whatever order the network hands them over, and a baseline pushed
    /// BACKWARDS by the older of them bills the same quota twice on the next
    /// answer; and a lease re-asked for inside its own life used to reset the
    /// meter to `None`, which made the next answer free because one reading is
    /// not a rise.
    ///
    /// Watch it fail by putting `meter.last.replace(utilization)` back in
    /// `report_handed_spend`: the out-of-order pair leaves the baseline at 0.40
    /// and the third answer is charged 0.10 instead of 0.03. And by seeding
    /// `HandedMeter { last: None }` in `register_handed_meter`: the re-ask
    /// leaves nothing to measure against and the answer after it is free.
    #[test]
    fn the_baseline_moves_forward_only_and_survives_a_re_ask() {
        let lease_id = 0xBA5E_u128;
        let (sink, mut hints) = tokio::sync::mpsc::unbounded_channel();
        register_handed_meter(lease_id, sink);
        note_handed_baseline(lease_id, 0.30);

        // Out of order: the later answer lands first.
        report_handed_spend(lease_id, Some(0.50));
        report_handed_spend(lease_id, Some(0.40));
        report_handed_spend(lease_id, Some(0.53));

        let charged: Vec<f64> = std::iter::from_fn(|| hints.try_recv().ok())
            .map(|hint| match hint {
                Control::UsageHint { spent, .. } => spent,
                other => panic!("a meter reports usage and nothing else, got {other:?}"),
            })
            .collect();
        assert_eq!(
            charged.len(),
            2,
            "the answer that read BELOW the baseline reports nothing, so two of the three \
             charge: {charged:?}"
        );
        assert!(
            (charged[0] - 0.20).abs() < 1e-9,
            "the first is measured against the handoff's own baseline: {charged:?}"
        );
        assert!(
            (charged[1] - 0.03).abs() < 1e-9,
            "and the third against 0.50, which the out-of-order 0.40 must not have pulled \
             back down: {charged:?}"
        );

        // The re-ask: a second session for the same lease replaces the sink and
        // keeps the figure.
        let (sink, mut hints) = tokio::sync::mpsc::unbounded_channel();
        register_handed_meter(lease_id, sink);
        report_handed_spend(lease_id, Some(0.56));
        let Some(Control::UsageHint { spent, .. }) = hints.try_recv().ok() else {
            panic!("the first answer after a re-ask is charged, not free");
        };
        assert!(
            (spent - 0.03).abs() < 1e-9,
            "and it is charged against the baseline the meter already held: {spent}"
        );
        forget_handed_meter(lease_id);
    }

    /// **A usage hint landing mid-frame does not cost the borrower its
    /// bearer.**
    ///
    /// The reader holds a `biased` `select!` between the hint channel and the
    /// stream, so a hint queued while a `Handoff` is half read DROPS the read.
    /// `serve::recv_control` is not cancel safe: the bytes it had already taken
    /// off the socket went with the dropped future, the next read met the tail
    /// of one frame as a length prefix, the Noise stream desynced, and this
    /// borrower forgot its bearer for the rest of the lease.
    /// `noise::FrameReader` keeps the part-read bytes in the loop's own state,
    /// which is the failure its own doc describes.
    ///
    /// Driven against the real function over a duplex with a real Noise
    /// session, and the frame is split by hand with the hint pushed between the
    /// halves, because the claim is about a cancellation that only happens when
    /// those two land in that order.
    ///
    /// Watch it fail by putting `frame = serve::recv_control(&mut stream, &mut
    /// session) => frame` back in `read_handed_bearers`'s `select!`: the second
    /// bearer never arrives and this times out on the first one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_hint_landing_mid_frame_does_not_lose_the_bearer() {
        use tokio::io::AsyncWriteExt as _;

        let lease_id = 0xF4A3_u128;
        let now_ms = crate::now_ms();
        let (borrower_end, mut lender_end) = tokio::io::duplex(64 * 1024);
        let (borrower_secret, borrower_public) =
            crate::peer::noise::generate_static().expect("a borrower keypair");
        let (lender_secret, lender_public) =
            crate::peer::noise::generate_static().expect("a lender keypair");

        let mut borrower_end = borrower_end;
        let dialling = tokio::spawn(async move {
            let session = crate::peer::noise::dial_handshake(
                &mut borrower_end,
                &borrower_secret,
                crate::peer::noise::Handshake::Return,
                Some(&lender_public),
                None,
            )
            .await
            .expect("the borrower dials");
            (borrower_end, session)
        });
        let mut lender =
            crate::peer::noise::accept_handshake(
                &mut lender_end,
                &lender_secret,
                crate::peer::noise::Handshake::Return,
                &[],
                |offered| {
                    let key: [u8; 32] = offered.try_into().map_err(|_| {
                        crate::peer::noise::PinRefusal::Malformed { len: offered.len() }
                    })?;
                    Ok(PeerId(key))
                },
            )
            .await
            .expect("the lender accepts");
        let (borrower_end, borrower_session) = dialling.await.expect("the dial finishes");

        let lender_id = PeerId(borrower_public);
        tokio::spawn(read_handed_bearers(
            Box::new(borrower_end),
            borrower_session,
            live_lease(lease_id, now_ms),
            lender_id,
        ));

        let handoff = |token: &str| Control::Handoff {
            lease_id,
            access_token: tcr_peer_wire::HandoffToken::new(token.to_string()),
            expires_at_ms: now_ms + 300_000,
            utilization: Some(0.30),
        };
        let held = |token: &str| {
            handed_tokens()
                .lock()
                .expect("the handed-token store")
                .bearer(lease_id, crate::now_ms())
                == Some(token)
        };
        let wait_for = |token: String| async move {
            for _ in 0..200 {
                if held(&token) {
                    return true;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            false
        };

        let first = serde_json::to_vec(&handoff("fake-first-bearer")).expect("it serializes");
        crate::peer::noise::send_encrypted(&mut lender_end, &mut lender.transport, &first)
            .await
            .expect("the first handoff goes out whole");
        assert!(
            wait_for("fake-first-bearer".to_string()).await,
            "the whole frame arrives, which is the control: without it the split frame below \
             would prove nothing"
        );

        // The second frame, encrypted into a buffer so it can be handed over in
        // two pieces with a cancellation between them.
        let second = serde_json::to_vec(&handoff("fake-second-bearer")).expect("it serializes");
        let mut framed: Vec<u8> = Vec::new();
        crate::peer::noise::send_encrypted(&mut framed, &mut lender.transport, &second)
            .await
            .expect("the second handoff encrypts");
        let split = 3;
        lender_end
            .write_all(&framed[..split])
            .await
            .expect("the first piece goes out");
        lender_end.flush().await.expect("and is flushed");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // THE CANCELLATION: a hint on the biased arm while the read is parked
        // part way through the frame above.
        report_handed_spend(lease_id, Some(0.45));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        lender_end
            .write_all(&framed[split..])
            .await
            .expect("the rest goes out");
        lender_end.flush().await.expect("and is flushed");

        assert!(
            wait_for("fake-second-bearer".to_string()).await,
            "the renewal that straddled the cancelled read must still reach the store; the \
             bearer held is whatever the desynced stream left behind"
        );
    }

    /// **A handoff with no baseline is refused, and the line names the Mac to
    /// update.**
    ///
    /// The field is what a build older than this one leaves out. Storing the
    /// bearer anyway would make every answer on that lease free, so it is
    /// refused; the borrow falls back to the lender's own serve path, which is
    /// a working borrow and not an outage, and the only thing an operator can
    /// act on is which Mac to update.
    ///
    /// Watch it fail by returning `Ok(0.0)` for `None` in `handed_baseline`.
    #[test]
    fn a_handoff_without_a_baseline_is_refused_by_name() {
        let lender = PeerId([9_u8; 32]);
        let refused = handed_baseline(&lender, None).expect_err("no baseline is a refusal");
        assert!(
            refused.contains(&lender.display()),
            "the refusal names the Mac whose build is behind, and it reads: {refused}"
        );
        assert!(
            handed_baseline(&lender, Some(f64::NAN)).is_err(),
            "a figure that is not a number is no baseline either"
        );
        assert_eq!(
            handed_baseline(&lender, Some(0.30)).ok(),
            Some(0.30),
            "and a figure that is one is taken as it stands"
        );
    }
}

#[cfg(test)]
mod parse_lend_end_tests {
    use super::*;
    use time::macros::datetime;

    /// A fixed LOCAL reading, `+02:00`, chosen off the machine this suite
    /// happens to run on: 2023-11-14 22:13:20 local. Every clock-time
    /// assertion below goes through [`parse_lend_end_at_local`] rather than
    /// [`parse_lend_end`] for the reason
    /// [`crate::peer::schedule::Schedule::contains_at_local`]'s own tests do
    /// the same split: `UtcOffset::local_offset_at` is real and
    /// host-dependent, and a test asserting against it would pass or fail
    /// depending on which machine ran the suite rather than on the code.
    fn local_now() -> time::OffsetDateTime {
        datetime!(2023-11-14 22:13:20 +02:00)
    }

    #[test]
    fn a_clock_still_ahead_today_resolves_to_today() {
        let local = local_now();
        let end = parse_lend_end_at_local("23:00", local)
            .expect("23:00 parses")
            .expect("a clock time is an end");
        let at = time::OffsetDateTime::from_unix_timestamp(i64::try_from(end).expect("in range"))
            .expect("a valid instant")
            .to_offset(local.offset());
        assert_eq!((at.hour(), at.minute()), (23, 0));
        assert_eq!(at.date(), local.date(), "later today stays today");
    }

    #[test]
    fn a_clock_already_passed_today_is_refused_not_rolled_to_tomorrow() {
        let local = local_now(); // 22:13:20
        let err =
            parse_lend_end_at_local("09:00", local).expect_err("a past clock must be refused");
        let message = err.to_string();
        assert!(
            message.contains("09:00"),
            "names the spelling refused: {message}"
        );
        assert!(
            message.contains("already passed") && message.contains("past"),
            "says the end is in the past rather than silently rolling: {message}"
        );
    }

    #[test]
    fn a_duration_and_none_are_resolved_against_the_same_instant_regardless_of_offset() {
        let local = local_now();
        assert_eq!(
            parse_lend_end_at_local("2h", local).expect("2h parses"),
            Some(u64::try_from(local.unix_timestamp() + 7_200).expect("positive"))
        );
        assert_eq!(
            parse_lend_end_at_local("none", local).expect("none parses"),
            None
        );
        assert_eq!(
            parse_lend_end_at_local("", local).expect("empty parses"),
            None
        );
    }

    #[test]
    fn a_bare_number_is_still_refused() {
        for refused in ["2", "2w", "0h", "-1h", "25:00", "18:60", "18:00:00:00"] {
            assert!(
                parse_lend_end_at_local(refused, local_now()).is_err(),
                "{refused:?} is not an end and must be refused rather than guessed at"
            );
        }
    }

    /// A smoke test on the public wrapper: it must actually convert `now`
    /// into SOME offset and delegate rather than panic, on the one input
    /// (`none`) whose answer cannot depend on which offset that was.
    #[test]
    fn the_public_wrapper_resolves_an_offset_and_delegates() {
        let now = time::OffsetDateTime::from_unix_timestamp(1_700_000_000)
            .expect("a literal unix timestamp is a valid instant");
        assert_eq!(parse_lend_end("none", now).expect("none parses"), None);
    }

    /// `split_at(spec.len() - 1)` splits by byte, not by character: a
    /// trailing multibyte character (`--for 2é`) lands mid-character and
    /// panics rather than being refused. `é` is not a real duration unit
    /// either way, so this must be a clean refusal, never a panic.
    #[test]
    fn a_multibyte_trailing_unit_is_refused_not_a_panic() {
        let err = parse_lend_end_at_local("2é", local_now())
            .expect_err("a multibyte unit is not a duration and must be refused");
        assert!(
            err.to_string().contains("2é"),
            "names the spelling refused: {err}"
        );
    }

    /// `count * multiplier` overflowing used to go through `unwrap_or(0)`
    /// and hand back `until = 0`: a lease dead on arrival, with no sign
    /// anything went wrong. An overflow must be a refusal an operator can
    /// read instead.
    #[test]
    fn an_overflowing_duration_is_refused_not_a_dead_on_arrival_zero() {
        let err = parse_lend_end_at_local(&format!("{}d", i64::MAX), local_now())
            .expect_err("a duration whose arithmetic overflows must be refused");
        assert!(
            err.to_string().contains("too large"),
            "names why the duration was refused: {err}"
        );
    }
}
