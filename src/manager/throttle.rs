//! `Manager` throttle methods, split verbatim from `mod.rs`.

use std::num::NonZeroU32;
use std::time::Duration;

use governor::clock::MonotonicClock;
use governor::state::keyed::DefaultKeyedStateStore;
use governor::state::{InMemoryState, NotKeyed};
use governor::{Quota, RateLimiter as GovRateLimiter};

use super::*;

/// One GCRA bucket shared by the whole fleet — today's default.
type FleetLimiter = GovRateLimiter<NotKeyed, InMemoryState, MonotonicClock>;
/// One GCRA bucket per account, keyed by the account's index in
/// [`Manager::accounts`] (the same identity `enter_in_flight`/`ensure_fresh`/
/// `record_probe` already use for per-account state — see `src/manager/mod.rs`).
type KeyedLimiter = GovRateLimiter<usize, DefaultKeyedStateStore<usize>, MonotonicClock>;

/// Backing engine for the global outbound throttle: `governor`'s GCRA rate
/// limiter, explicitly on [`MonotonicClock`] — never the crate's default
/// `quanta`-backed clock (open governor issue #299, "not monotonic").
///
/// Both variants are lock-free: `InMemoryState` and the `DashMap`-backed keyed
/// store (`DefaultKeyedStateStore`) update via atomic CAS inside `check`/
/// `check_key`, which are synchronous, `&self`, non-async methods — they
/// return before any `.await` is reached. That's what keeps
/// [`Manager::throttle_send`]'s "holds no resource across the sleep"
/// guarantee true after this port: there is no guard of any kind to hold.
pub(super) enum ThrottleLimiter {
    /// `throttle.is_active()` is false: no config, fully inert.
    Inert,
    /// Fleet-wide bucket (default: `throttle.perAccount` absent/`false`).
    Fleet(FleetLimiter),
    /// Per-account bucket (opt-in: `throttle.perAccount: true`). Same
    /// `minSpacingMs`/`burst` quota as the fleet-wide case, but one instance
    /// per account instead of one shared by the whole fleet.
    Keyed(KeyedLimiter),
}

impl ThrottleLimiter {
    /// Builds the engine from a snapshotted [`ThrottleConfig`]. Mirrors
    /// [`ThrottleConfig::is_active`]/[`ThrottleConfig::effective_per_account`]
    /// exactly, so an absent/inert config produces [`Self::Inert`] — the
    /// no-throttle build stays byte-identical.
    pub(super) fn from_config(cfg: &ThrottleConfig) -> Self {
        let Some(spacing_ms) = cfg.effective_min_spacing() else {
            return Self::Inert;
        };
        // `effective_min_spacing` already treats `Some(0)` as `None`, so
        // `spacing_ms` here is always > 0 — `Quota::with_period` only returns
        // `None` for a zero period.
        let quota = Quota::with_period(Duration::from_millis(spacing_ms))
            .expect("spacing_ms > 0, guaranteed by ThrottleConfig::effective_min_spacing")
            .allow_burst(
                NonZeroU32::new(cfg.effective_burst())
                    .expect("ThrottleConfig::effective_burst clamps to >= 1"),
            );
        if cfg.effective_per_account() {
            Self::Keyed(GovRateLimiter::new(
                quota,
                DefaultKeyedStateStore::default(),
                MonotonicClock,
            ))
        } else {
            Self::Fleet(GovRateLimiter::new(
                quota,
                InMemoryState::default(),
                MonotonicClock,
            ))
        }
    }
}

impl Manager {
    /// Global outbound-initiation throttle. Inert (returns immediately) unless
    /// `throttle` is configured. When active, applies a GCRA token bucket at the
    /// single send site so a cold fan-out cannot burst the shared upstream
    /// limiter — fleet-wide by default (one bucket for the whole account set),
    /// or, opt-in (`throttle.perAccount: true`), one bucket per `idx`. Holds NO
    /// resource across the sleep — pure initiation delay, cannot deadlock, never
    /// turns a request into a failure. `idx` is ignored on the fleet-wide and
    /// inert paths; it is the account about to send, in the same numbering
    /// [`Manager::enter_in_flight`]/[`Manager::ensure_fresh`] use.
    pub async fn throttle_send(&self, idx: usize) {
        match &self.throttle_limiter {
            ThrottleLimiter::Inert => {}
            // `until_ready`/`until_key_ready`: check (sync, lock-free, returns
            // before any await), and only on a negative outcome does it await a
            // delay — never with the check's result still borrowed.
            ThrottleLimiter::Fleet(limiter) => {
                limiter.until_ready().await;
            }
            ThrottleLimiter::Keyed(limiter) => {
                limiter.until_key_ready(&idx).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;
    use std::time::Duration;

    use governor::clock::{Clock, FakeRelativeClock};
    use governor::{Quota, RateLimiter};

    /// Ports `throttle_slot_burst1_is_strict_spacing` onto governor's own GCRA
    /// engine and clock. The arithmetic is identical (`gcra.rs:91`:
    /// `tau = t * (max_burst - 1)`, `gcra.rs:113-128` `test_and_update`) — traced
    /// by hand against this exact quota below, not just re-stated from a brief.
    ///
    /// Where it CANNOT be a literal port: `throttle_slot` was a reservation
    /// scheduler (every call advances the TAT and hands back a — possibly
    /// future — `allow_at`; it never refuses). `governor::check()` is
    /// test-and-reject: at a FIXED `now` a second call is simply `Err`, and a
    /// denial leaves the stored TAT untouched (no advance). So the old test's
    /// "thread 3 calls through the TAT at one fixed `now`" shape has no
    /// governor equivalent — there is no reservation to thread. What ported
    /// instead is the actual production shape: call, and if denied, wait
    /// exactly the returned `earliest_possible()` and retry (this is exactly
    /// what `until_ready()` in `throttle_send` does). That retry-after-wait
    /// loop reproduces the SAME 1000 → 1100 → 1200 spacing the old literals
    /// asserted, because it's driving the clock forward instead of assuming a
    /// reservation.
    #[test]
    fn governor_gcra_burst1_reproduces_strict_spacing_via_retry_after_wait() {
        let clock = FakeRelativeClock::default();
        clock.advance(Duration::from_millis(1000)); // mirrors the old test's fixed `now = 1000`
        let quota = Quota::with_period(Duration::from_millis(100))
            .expect("100ms is a nonzero period")
            .allow_burst(NonZeroU32::new(1).expect("1 is nonzero"));
        let lim = RateLimiter::direct_with_clock(quota, clock.clone());

        // 1st send: instant (allow_at == now == 1000), same as the old test.
        assert!(lim.check().is_ok(), "first send admits instantly");

        // 2nd send at the SAME instant is DENIED (no reservation), but the
        // denial exposes exactly the old test's `allow1 + spacing == 1100`.
        let denied = lim
            .check()
            .expect_err("second send at the same instant is denied");
        assert_eq!(
            denied.earliest_possible(),
            clock.now() + Duration::from_millis(100),
            "denied earliest_possible must be exactly now + spacing_ms"
        );

        // Advance the clock to that earliest_possible and retry: NOW it admits
        // — this is the real mechanism `until_ready()` uses in production.
        clock.advance(Duration::from_millis(100));
        assert!(
            lim.check().is_ok(),
            "retrying at earliest_possible must admit"
        );

        // A 4th send at that same (now-advanced) instant is denied again, at
        // exactly the next spacing boundary: 1100 + 100 == 1200.
        let denied2 = lim
            .check()
            .expect_err("4th send at the same instant is denied");
        assert_eq!(
            denied2.earliest_possible(),
            clock.now() + Duration::from_millis(100),
            "second denial's earliest_possible must be exactly the next spacing boundary"
        );
    }

    /// Ports `throttle_slot_burst3_admits_then_paces`: B=3 (tau=200ms) lets 3
    /// sends fire instantly at a fixed `now`, and the 4th is denied. Unlike the
    /// burst=1 case, the 4th call's `earliest_possible()` IS reproducible at
    /// the fixed `now` (no clock advance needed) — traced by hand:
    /// call1 tat 1000→1100, call2 1100→1200, call3 1200→1300, call4 at t0=1000
    /// sees `earliest_time = 1300 - 200 = 1100` — exactly the old
    /// `throttle_slot_burst3_admits_then_paces` literal `(tat4, allow4) ==
    /// (1400, 1100)`'s `allow4`.
    ///
    /// What is NOT reproducible: the old test's per-call `(tat, allow_at)`
    /// pairs for the ADMITTED calls — `(1100, 800)`, `(1200, 900)`,
    /// `(1300, 1000)`. `check()`'s positive outcome for `NoOpMiddleware` is
    /// `()`; governor's public API exposes the internal TAT only through a
    /// `NotUntil` on the DENIED path. Those three admitted-call values cannot
    /// be observed through governor's API at all — this is a finding, not an
    /// adjusted expectation.
    #[test]
    fn governor_gcra_burst3_admits_then_denies_at_the_documented_boundary() {
        let clock = FakeRelativeClock::default();
        clock.advance(Duration::from_millis(1000)); // mirrors the old test's fixed `now = 1000`
        let quota = Quota::with_period(Duration::from_millis(100))
            .expect("100ms is a nonzero period")
            .allow_burst(NonZeroU32::new(3).expect("3 is nonzero"));
        let lim = RateLimiter::direct_with_clock(quota, clock.clone());

        assert!(lim.check().is_ok(), "1st of 3 burst sends admits instantly");
        assert!(lim.check().is_ok(), "2nd of 3 burst sends admits instantly");
        assert!(lim.check().is_ok(), "3rd of 3 burst sends admits instantly");

        // 4th send, same fixed `now`: denied — earliest_possible is exactly
        // the old literal `1100` (now + spacing, NOT observable admitted TATs).
        let denied = lim.check().expect_err("4th send exceeds the burst");
        assert_eq!(
            denied.earliest_possible(),
            clock.now() + Duration::from_millis(100),
            "4th-call earliest_possible must be exactly now + spacing_ms (== 1100 in the \
             old test's literal timeline)"
        );
    }
}
