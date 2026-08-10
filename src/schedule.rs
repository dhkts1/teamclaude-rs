//! Per-account randomized schedules for the background probe and keep-warm loops.
//!
//! # What this replaces, and why
//!
//! Both background loops used to be a single `tokio::time::interval` driving a
//! whole-fleet sweep: one tick, then EVERY account probed back-to-back
//! [`crate::probe::PROBE_SPACING`] apart. Three properties of that shape were
//! bad and none of them were accidental:
//!
//! * **One window.** All N accounts were touched inside the same `N * 350ms`
//!   burst, so upstream saw the whole fleet arrive at once — repeatedly, forever.
//! * **An exact period.** No variation at all, which makes the fleet trivially
//!   fingerprintable and lines it up with every other periodic thing on the box.
//! * **Phase re-anchored on every restart.** `interval`'s first tick fires
//!   immediately, so the fleet's phase was "whenever the process last booted"
//!   and two restarts close together produced two identical schedules.
//!
//! So the scheduler here gives **each account its own independent, randomly
//! drawn schedule**: a random initial offset, then after each probe a fresh
//! random sleep before that account's next one. Nothing is shared, nothing is
//! derived from an index (`i / N` offsets are deterministic, which is a wobble,
//! not randomness), and a restart re-draws every offset.
//!
//! # The distribution, exactly
//!
//! Two draws, both uniform over integer seconds, both from
//! [`Rng`] (see below):
//!
//! | draw | range | used for |
//! |---|---|---|
//! | initial offset | `[0, cadence]` inclusive | the first fire of a newly-eligible account |
//! | interval | `[cadence - 30%, cadence + 30%]` inclusive, floored at 1s | every subsequent fire |
//!
//! At the default cadence of [`crate::probe::DEFAULT_PROBE_SECONDS`] (300s) that
//! is a first fire uniform in `0..=300s` and thereafter a sleep uniform in
//! `210..=390s` — 181 distinct values. The spread is [`JITTER_PERCENT`]. Neither
//! draw can reach 0 seconds ([`jittered_interval_secs`] floors at 1) and neither
//! can run away: the interval is bounded above by `cadence * 1.3` by
//! construction, so a bad draw cannot silently disable probing.
//!
//! # Randomness without a `rand` dependency
//!
//! This crate deliberately carries no direct `rand` dependency — the existing
//! jitter in `src/proxy.rs` derives its randomness from the loop clock
//! (`now.nanosecond()`) for exactly that reason. Re-reading the clock per
//! account does NOT work here, though: the initial offsets for a whole fleet are
//! drawn inside one tight loop, and successive `nanosecond()` reads in a tight
//! loop are strongly correlated — which is precisely the synchronization this
//! module exists to remove. So the clock is read **once** to seed a SplitMix64
//! generator ([`Rng`]) and every later draw comes from advancing that state.
//! SplitMix64 is a handful of wrapping multiplies with no dependency, and its
//! output stream is decorrelated by construction.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::manager::Manager;

/// Half-width of the interval draw, in percent of the configured cadence. A draw
/// is uniform over `[cadence - JITTER_PERCENT%, cadence + JITTER_PERCENT%]`.
pub const JITTER_PERCENT: u64 = 30;

/// Longest the scheduler loop sleeps in one hop, regardless of how far away the
/// next due account is. Bounds how long a newly-eligible account (an errored row
/// whose backoff elapsed, an account re-enabled from the TUI) waits to be picked
/// up, and bounds the shutdown latency of the loop to something well under
/// [`crate::server::DEFAULT_SHUTDOWN_GRACE`].
pub const MAX_SLEEP: Duration = Duration::from_secs(5);

/// Floor on the scheduler's sleep, so a clock jump or a pile of already-due
/// accounts can never turn the loop into a busy spin.
pub const MIN_SLEEP: Duration = Duration::from_millis(50);

/// SplitMix64 — a seedable, dependency-free generator whose successive outputs
/// are decorrelated even for adjacent seeds.
///
/// Not cryptographic and does not need to be: the only requirement is that two
/// accounts drawing in the same microsecond get unrelated numbers.
#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
}

impl Rng {
    /// Seed explicitly. Used by the tests so a schedule is reproducible.
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// Seed from the wall clock: milliseconds for the coarse component and the
    /// sub-second nanoseconds for the fine one, mixed so two processes booting in
    /// the same millisecond still diverge. This is the ONLY clock read — every
    /// later draw advances the state instead.
    pub fn from_clock() -> Self {
        let now = time::OffsetDateTime::now_utc();
        let seed = (crate::now_ms() as u64) ^ mix64(now.nanosecond() as u64);
        Self::new(mix64(seed))
    }

    /// Advance the state and return the next value.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        mix64(self.state)
    }
}

/// SplitMix64's finalizer: an avalanche mix that turns a counter into a
/// well-distributed 64-bit value.
fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// One account's next sleep: uniform over
/// `[cadence - JITTER_PERCENT%, cadence + JITTER_PERCENT%]` seconds inclusive,
/// never 0.
///
/// The spread is computed with integer arithmetic, so it floors — at a cadence of
/// 1s the spread is 0 and the draw is a constant 1s, which is the only sane
/// answer for a cadence that small and is still never 0.
pub fn jittered_interval_secs(cadence_secs: u64, draw: u64) -> u64 {
    let cadence = cadence_secs.max(1);
    let spread = cadence.saturating_mul(JITTER_PERCENT) / 100;
    let low = cadence.saturating_sub(spread);
    let span = spread.saturating_mul(2).saturating_add(1);
    (low + draw % span).max(1)
}

/// A newly-eligible account's phase: uniform over `[0, cadence]` seconds
/// inclusive. Drawing over the FULL cadence (rather than a narrow window) is what
/// makes a restart re-scatter the fleet instead of re-aligning it.
pub fn initial_offset_secs(cadence_secs: u64, draw: u64) -> u64 {
    draw % (cadence_secs.max(1) + 1)
}

/// Every eligible account's next-due instant, in epoch milliseconds.
///
/// Keyed by account index. An index that leaves the eligible set (disabled,
/// errored, throttled) is forgotten and re-draws a fresh offset if it comes back
/// — deliberately, since an account returning to eligibility should not inherit
/// a phase from before it left.
#[derive(Debug, Default)]
pub struct FleetSchedule {
    due_ms: HashMap<usize, i64>,
}

impl FleetSchedule {
    /// Give every newly-eligible account a random first-due instant and forget
    /// every account that is no longer eligible. Accounts already scheduled keep
    /// their instant — this is called every loop iteration and must not re-draw.
    pub fn sync(&mut self, now_ms: i64, eligible: &[usize], cadence_secs: u64, rng: &mut Rng) {
        self.due_ms.retain(|idx, _| eligible.contains(idx));
        for &idx in eligible {
            self.due_ms.entry(idx).or_insert_with(|| {
                now_ms + 1000 * initial_offset_secs(cadence_secs, rng.next_u64()) as i64
            });
        }
    }

    /// The accounts due at `now_ms`, earliest first.
    pub fn due(&self, now_ms: i64) -> Vec<usize> {
        let mut due: Vec<(i64, usize)> = self
            .due_ms
            .iter()
            .filter(|(_, &at)| at <= now_ms)
            .map(|(&idx, &at)| (at, idx))
            .collect();
        due.sort_unstable();
        due.into_iter().map(|(_, idx)| idx).collect()
    }

    /// Draw account `idx`'s next instant. Called right after it fired, so the
    /// gap between two fires of the SAME account is a fresh random draw every
    /// time rather than a fixed period.
    pub fn reschedule(&mut self, idx: usize, now_ms: i64, cadence_secs: u64, rng: &mut Rng) {
        let next = now_ms + 1000 * jittered_interval_secs(cadence_secs, rng.next_u64()) as i64;
        self.due_ms.insert(idx, next);
    }

    /// Re-draw EVERY scheduled account, used after something outside the
    /// scheduler already actioned the whole fleet (the keep-warm wake path).
    pub fn reschedule_all(&mut self, now_ms: i64, cadence_secs: u64, rng: &mut Rng) {
        let idxs: Vec<usize> = self.due_ms.keys().copied().collect();
        for idx in idxs {
            self.reschedule(idx, now_ms, cadence_secs, rng);
        }
    }

    /// The earliest scheduled instant, or `None` when nothing is scheduled.
    pub fn next_due_ms(&self) -> Option<i64> {
        self.due_ms.values().copied().min()
    }

    /// How many accounts are scheduled.
    pub fn len(&self) -> usize {
        self.due_ms.len()
    }

    /// Whether nothing is scheduled.
    pub fn is_empty(&self) -> bool {
        self.due_ms.is_empty()
    }
}

/// Which background job a [`run`] loop drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Job {
    /// Zero-spend quota probe. Eligibility is `Manager::probeable_indices`.
    Probe,
    /// Keep-warm (spends real quota, off by default). Eligibility is
    /// `Manager::warm_targets`.
    Warm,
}

/// How long to sleep before looking again, given the earliest due instant.
/// Clamped into `[MIN_SLEEP, MAX_SLEEP]`; `None` (nothing scheduled) waits the
/// full cap and re-checks eligibility then.
pub fn sleep_for(now_ms: i64, next_due_ms: Option<i64>) -> Duration {
    let wait = match next_due_ms {
        Some(due) => Duration::from_millis((due - now_ms).max(0) as u64),
        None => MAX_SLEEP,
    };
    wait.clamp(MIN_SLEEP, MAX_SLEEP)
}

/// Drive one background job on per-account random schedules until cancelled.
///
/// The loop itself is a plain "sleep until the earliest due account" scheduler:
/// it never ticks on a period, so there is no fleet-wide instant for accounts to
/// pile onto. When two accounts do happen to come due together they are still
/// spaced [`crate::probe::PROBE_SPACING`] apart, which is the burst bound the
/// usage endpoint cares about — the aggregate rate here is strictly LOWER than
/// the old sweep's, since the old one packed every account into one 350ms-spaced
/// run per cadence and this one spreads the same N calls across the whole
/// cadence.
///
/// This never returns; the caller wraps it in a `select!` against shutdown.
pub async fn run(manager: Arc<Manager>, job: Job, cadence_secs: u64) {
    let mut schedule = FleetSchedule::default();
    let mut rng = Rng::from_clock();

    loop {
        let now = crate::now_ms();
        let eligible = match job {
            Job::Probe => manager.probeable_indices(),
            Job::Warm => manager.warm_targets(),
        };
        schedule.sync(now, &eligible, cadence_secs, &mut rng);

        let due = schedule.due(now);
        let last = due.len().saturating_sub(1);
        for (i, idx) in due.into_iter().enumerate() {
            match job {
                Job::Probe => manager.probe_account(idx).await,
                Job::Warm => manager.warm_one(idx).await,
            }
            schedule.reschedule(idx, crate::now_ms(), cadence_secs, &mut rng);
            if i < last {
                tokio::time::sleep(crate::probe::PROBE_SPACING).await;
            }
        }

        let wait = sleep_for(crate::now_ms(), schedule.next_due_ms());
        match job {
            Job::Probe => tokio::time::sleep(wait).await,
            // Keep-warm additionally wakes on the edge-triggered signal both
            // quota-latch sites fire, which is what keeps `warm_targets`' boot
            // gate from being a kill switch on a proxy restarted more often than
            // `warmupSeconds`. A wake means the fleet's eligibility just changed
            // wholesale, so it runs the ONE-SHOT sweep and re-draws every phase.
            Job::Warm => {
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = manager.warm_wake().notified() => {
                        manager.warm_all().await;
                        schedule.reschedule_all(crate::now_ms(), cadence_secs, &mut rng);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every interval draw lands inside the documented +/-30% band AND the draws
    /// actually vary.
    ///
    /// The `distinct` assertion is the one that discriminates: a fixed-interval
    /// implementation (`jittered_interval_secs` returning `cadence`) satisfies
    /// the band trivially and produces exactly ONE distinct value, so it fails
    /// here. Watched fail against that mutation.
    #[test]
    fn interval_draws_stay_in_band_and_vary() {
        let mut rng = Rng::new(0xDEAD_BEEF);
        let cadence = 300;
        let mut seen = std::collections::HashSet::new();
        for _ in 0..500 {
            let secs = jittered_interval_secs(cadence, rng.next_u64());
            assert!(
                (210..=390).contains(&secs),
                "draw {secs} outside the documented [210, 390] band for a 300s cadence"
            );
            seen.insert(secs);
        }
        assert!(
            seen.len() > 100,
            "only {} distinct intervals in 500 draws — this is a fixed period, not a random schedule",
            seen.len()
        );
    }

    /// A draw can never be zero or absurd, at any cadence including the
    /// degenerate small ones.
    #[test]
    fn interval_draws_are_never_zero_or_unbounded() {
        let mut rng = Rng::new(7);
        for cadence in [1_u64, 2, 5, 75, 300, 3600] {
            for _ in 0..200 {
                let secs = jittered_interval_secs(cadence, rng.next_u64());
                assert!(secs >= 1, "cadence {cadence} produced a {secs}s sleep");
                assert!(
                    secs <= cadence.max(1) * (100 + JITTER_PERCENT) / 100 + 1,
                    "cadence {cadence} produced a runaway {secs}s sleep"
                );
            }
        }
    }

    /// The initial offset never exceeds one cadence, so no account waits longer
    /// than its own period for its first fire.
    #[test]
    fn initial_offsets_stay_within_one_cadence() {
        let mut rng = Rng::new(99);
        for _ in 0..500 {
            assert!(initial_offset_secs(300, rng.next_u64()) <= 300);
        }
    }

    /// Accounts scheduled at the SAME instant do not fire at the same instant.
    ///
    /// This is the fleet-sweep property under test: the old scheduler gave every
    /// account the identical due instant (one shared tick), so it produces one
    /// distinct due time here and fails both assertions. The minute assertion is
    /// the owner's requirement stated directly — with 12 accounts and a 300s
    /// cadence the draws must not collapse into a single minute.
    #[test]
    fn accounts_seeded_together_get_independent_phases() {
        let mut rng = Rng::new(0x5EED);
        let mut schedule = FleetSchedule::default();
        let now = 1_700_000_000_000;
        let eligible: Vec<usize> = (0..12).collect();
        schedule.sync(now, &eligible, 300, &mut rng);

        assert_eq!(schedule.len(), 12);
        let dues: Vec<i64> = eligible
            .iter()
            .map(|idx| schedule.due_ms[idx])
            .collect::<Vec<_>>();
        let distinct: std::collections::HashSet<i64> = dues.iter().copied().collect();
        assert!(
            distinct.len() >= 10,
            "12 accounts produced only {} distinct due instants — they are still sweeping together",
            distinct.len()
        );

        let minutes: std::collections::HashSet<i64> =
            dues.iter().map(|due| (due - now) / 60_000).collect();
        assert!(
            minutes.len() >= 4,
            "12 accounts landed in only {} distinct minutes — 'not on the same minute' is violated",
            minutes.len()
        );
    }

    /// One account's SUCCESSIVE gaps differ from each other. A fixed-interval
    /// scheduler yields one distinct gap and fails.
    #[test]
    fn one_account_gets_a_fresh_draw_every_cycle() {
        let mut rng = Rng::new(0xC0FFEE);
        let mut schedule = FleetSchedule::default();
        let mut now = 1_700_000_000_000;
        schedule.sync(now, &[0], 300, &mut rng);

        let mut gaps = std::collections::HashSet::new();
        for _ in 0..40 {
            schedule.reschedule(0, now, 300, &mut rng);
            let next = schedule.due_ms[&0];
            gaps.insert(next - now);
            now = next;
        }
        assert!(
            gaps.len() > 15,
            "only {} distinct gaps across 40 cycles — the account is on a fixed period",
            gaps.len()
        );
    }

    /// Two processes booting at different instants do not produce the same
    /// schedule — the restart-realignment the old `interval` had.
    #[test]
    fn two_boots_do_not_produce_the_same_schedule() {
        let phases = |seed: u64| {
            let mut rng = Rng::new(seed);
            let mut schedule = FleetSchedule::default();
            schedule.sync(0, &(0..8).collect::<Vec<_>>(), 300, &mut rng);
            (0..8).map(|i| schedule.due_ms[&i]).collect::<Vec<i64>>()
        };
        assert_ne!(phases(1), phases(2));
        // …and a clock-seeded generator is not a constant either.
        assert_ne!(Rng::from_clock().next_u64(), Rng::new(0).next_u64());
    }

    /// An account already scheduled keeps its instant across a `sync`, so the
    /// per-iteration re-sync cannot starve it by pushing it forever forward.
    #[test]
    fn sync_does_not_redraw_an_already_scheduled_account() {
        let mut rng = Rng::new(11);
        let mut schedule = FleetSchedule::default();
        schedule.sync(0, &[0, 1], 300, &mut rng);
        let before = schedule.due_ms[&0];
        for _ in 0..50 {
            schedule.sync(0, &[0, 1], 300, &mut rng);
        }
        assert_eq!(schedule.due_ms[&0], before);
    }

    /// An account that leaves the eligible set is forgotten, and coming back
    /// draws a fresh phase rather than firing instantly on a stale instant.
    #[test]
    fn ineligible_accounts_are_forgotten() {
        let mut rng = Rng::new(13);
        let mut schedule = FleetSchedule::default();
        schedule.sync(0, &[0, 1], 300, &mut rng);
        schedule.sync(0, &[0], 300, &mut rng);
        assert_eq!(schedule.len(), 1);
        assert!(!schedule.is_empty());
        schedule.sync(1_000_000, &[0, 1], 300, &mut rng);
        assert!(schedule.due_ms[&1] >= 1_000_000);
    }

    /// `due` returns only accounts at or past their instant, earliest first.
    #[test]
    fn due_is_ordered_and_excludes_the_future() {
        let mut schedule = FleetSchedule::default();
        schedule.due_ms.insert(0, 300);
        schedule.due_ms.insert(1, 100);
        schedule.due_ms.insert(2, 900);
        assert_eq!(schedule.due(500), vec![1, 0]);
        assert_eq!(schedule.next_due_ms(), Some(100));
    }

    /// The loop's sleep is clamped, so neither a far-future schedule nor a pile
    /// of overdue accounts changes its shape.
    #[test]
    fn sleep_is_clamped_both_ways() {
        assert_eq!(sleep_for(0, Some(-5_000)), MIN_SLEEP);
        assert_eq!(sleep_for(0, Some(1_000_000)), MAX_SLEEP);
        assert_eq!(sleep_for(0, None), MAX_SLEEP);
        assert_eq!(sleep_for(0, Some(1_000)), Duration::from_secs(1));
    }
}
