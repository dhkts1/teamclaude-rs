//! Per-account quota, learned passively from upstream response headers.
//!
//! This is where **bug #2 is designed out**. In the JS proxy a quota bar keeps
//! the last-learned utilization until the next request re-learns it, so a window
//! that has already passed its reset can read as still-full. Here a window stores
//! its `reset` timestamp and [`QuotaWindow::effective`] computes utilization
//! **live** against `now`: once `now > reset`, the window reads as a fresh `0.0`.
//! Nothing is mutated on a tick — the freshness is a pure function of the clock,
//! so the TUI (and selection) can never show or act on a stale full bar.

use time::{Duration, OffsetDateTime};

/// Nominal length of the shared 5-hour session window.
const FIVE_HOUR: Duration = Duration::hours(5);
/// Nominal length of the weekly windows (`7d` and the model-scoped `7d_oi`).
const SEVEN_DAY: Duration = Duration::days(7);

/// Decide a window's `reset` when learning a fresh utilization, so a window is
/// never pinned out of rotation forever and never wrongly reads as instantly
/// fresh (bugs #4/#5 and #8 from the JS proxy):
/// - an explicit reported reset (past OR future) is authoritative;
/// - otherwise a prior reset is inherited only while still in the future;
/// - otherwise a bounded reset is synthesized `window` ahead of `now`, so a
///   utilization reported without any reset gates for at most one window length
///   and then reads fresh — never permanently, never immediately.
fn resolve_reset(
    reported: Option<OffsetDateTime>,
    prior: Option<OffsetDateTime>,
    now: OffsetDateTime,
    window: Duration,
) -> Option<OffsetDateTime> {
    if let Some(reset) = reported {
        return Some(reset);
    }
    if let Some(prior) = prior {
        if prior > now {
            return Some(prior);
        }
    }
    Some(now + window)
}

/// One rate-limit window: a utilization fraction plus when it resets.
#[derive(Debug, Clone, Copy)]
pub struct QuotaWindow {
    /// Utilization as reported (0.0–1.0; can exceed 1.0 in overage).
    pub utilization: f64,
    /// When this window resets. `None` means "unknown reset" (rare; keep util).
    pub reset: Option<OffsetDateTime>,
}

impl QuotaWindow {
    /// Utilization as it should be read at `now`. A window past its reset is a
    /// fresh window — `0.0` — never the stale last value. This is bug #2's fix.
    pub fn effective(&self, now: OffsetDateTime) -> f64 {
        match self.reset {
            Some(reset) if now >= reset => 0.0,
            _ => self.utilization,
        }
    }

    /// The reset instant only while it is still in the future at `now`.
    pub fn live_reset(&self, now: OffsetDateTime) -> Option<OffsetDateTime> {
        match self.reset {
            Some(reset) if now < reset => Some(reset),
            _ => None,
        }
    }
}

/// All quota dimensions tracked for one account.
#[derive(Debug, Clone, Default)]
pub struct Quota {
    /// Shared 5-hour session bucket (`unified-5h`).
    pub five_hour: Option<QuotaWindow>,
    /// Shared weekly bucket (`unified-7d`).
    pub seven_day: Option<QuotaWindow>,
    /// Model-scoped weekly bucket (`unified-7d_oi`, the Fable weekly on current
    /// plans). Gates Fable routing: a Fable-targeted request skips any account
    /// whose bucket is at/over threshold (see `model_weekly_exhausted` and the
    /// `is_fable` branch of `Manager::eligible`). Non-Fable routing ignores it.
    pub seven_day_oi: Option<QuotaWindow>,
    /// Last reported unified status: `allowed` | `allowed_warning` | `rejected`.
    pub status: Option<String>,
    // Standard API-key rate limits (present on non-OAuth accounts).
    pub tokens_limit: Option<i64>,
    pub tokens_remaining: Option<i64>,
    pub requests_limit: Option<i64>,
    pub requests_remaining: Option<i64>,
    /// Reset for the standard token/request limits.
    pub standard_reset: Option<OffsetDateTime>,
}

/// A generic view over "case-insensitive header name → value" so the same
/// parsing works for `reqwest`'s [`reqwest::header::HeaderMap`] and for a plain
/// map in tests.
pub trait HeaderView {
    fn get_str(&self, name: &str) -> Option<&str>;
}

impl HeaderView for reqwest::header::HeaderMap {
    fn get_str(&self, name: &str) -> Option<&str> {
        self.get(name).and_then(|v| v.to_str().ok())
    }
}

impl Quota {
    /// Merge in whatever rate-limit headers are present on a response, leaving
    /// windows that were not reported this time untouched.
    ///
    /// Returns whether this response actually reported the **5-hour** window. That
    /// is the one dimension a caller needs to distinguish from silence: a response
    /// carrying the header is first-hand evidence about the account's session
    /// window, while a response without it (an error page, a non-Anthropic
    /// upstream) says nothing at all — and the two must never be confused for the
    /// keep-warm gate. See [`crate::manager::AccountRuntime::quota_known`].
    #[must_use]
    pub fn update_from_headers(&mut self, headers: &impl HeaderView) -> bool {
        let now = OffsetDateTime::now_utc();
        let mut read_five_hour = false;
        if let Some(util) = get_parsed::<f64>(headers, "anthropic-ratelimit-unified-5h-utilization")
            .filter(|v| v.is_finite())
        {
            let reported = get_reset(headers, "anthropic-ratelimit-unified-5h-reset");
            let reset = resolve_reset(
                reported,
                self.five_hour.and_then(|w| w.reset),
                now,
                FIVE_HOUR,
            );
            self.five_hour = Some(QuotaWindow {
                utilization: util,
                reset,
            });
            read_five_hour = true;
        }
        if let Some(util) = get_parsed::<f64>(headers, "anthropic-ratelimit-unified-7d-utilization")
            .filter(|v| v.is_finite())
        {
            let reported = get_reset(headers, "anthropic-ratelimit-unified-7d-reset");
            let reset = resolve_reset(
                reported,
                self.seven_day.and_then(|w| w.reset),
                now,
                SEVEN_DAY,
            );
            self.seven_day = Some(QuotaWindow {
                utilization: util,
                reset,
            });
        }
        if let Some(util) =
            get_parsed::<f64>(headers, "anthropic-ratelimit-unified-7d_oi-utilization")
                .filter(|v| v.is_finite())
        {
            let reported = get_reset(headers, "anthropic-ratelimit-unified-7d_oi-reset");
            let reset = resolve_reset(
                reported,
                self.seven_day_oi.and_then(|w| w.reset),
                now,
                SEVEN_DAY,
            );
            self.seven_day_oi = Some(QuotaWindow {
                utilization: util,
                reset,
            });
        }

        if let Some(status) = headers.get_str("anthropic-ratelimit-unified-status") {
            self.status = Some(status.to_string());
        }

        if let Some(v) = get_parsed::<i64>(headers, "anthropic-ratelimit-tokens-limit") {
            self.tokens_limit = Some(v);
        }
        if let Some(v) = get_parsed::<i64>(headers, "anthropic-ratelimit-tokens-remaining") {
            self.tokens_remaining = Some(v);
        }
        if let Some(v) = get_parsed::<i64>(headers, "anthropic-ratelimit-requests-limit") {
            self.requests_limit = Some(v);
        }
        if let Some(v) = get_parsed::<i64>(headers, "anthropic-ratelimit-requests-remaining") {
            self.requests_remaining = Some(v);
        }
        if let Some(reset) = get_reset(headers, "anthropic-ratelimit-tokens-reset")
            .or_else(|| get_reset(headers, "anthropic-ratelimit-requests-reset"))
        {
            self.standard_reset = Some(reset);
        }
        read_five_hour
    }

    /// Is this account at/over `threshold` on any gating dimension, evaluated
    /// live at `now`? Only the shared 5-hour and weekly buckets gate rotation
    /// (plus standard API-key limits); the model-scoped `7d_oi` bucket does not.
    pub fn is_near(&self, threshold: f64, now: OffsetDateTime) -> bool {
        if let Some(w) = self.five_hour {
            if w.effective(now) >= threshold {
                return true;
            }
        }
        if let Some(w) = self.seven_day {
            if w.effective(now) >= threshold {
                return true;
            }
        }
        // Standard (API-key) token/request limits have no live-past-reset path of
        // their own, so once `standard_reset` has passed the upstream limit has
        // refreshed and a spent count must NOT keep gating — otherwise a spent
        // standard window pins the account out of rotation forever (there is no
        // OAuth probe for API-key accounts to re-read it). Only gate while the
        // window is still unexpired.
        let standard_expired = self.standard_reset.is_some_and(|r| now >= r);
        if !standard_expired {
            if let (Some(limit), Some(remaining)) = (self.tokens_limit, self.tokens_remaining) {
                if limit > 0 && 1.0 - (remaining as f64 / limit as f64) >= threshold {
                    return true;
                }
            }
            if let (Some(limit), Some(remaining)) = (self.requests_limit, self.requests_remaining) {
                if limit > 0 && 1.0 - (remaining as f64 / limit as f64) >= threshold {
                    return true;
                }
            }
        }
        false
    }

    /// Is the model-scoped weekly bucket (`7d_oi`, the Fable weekly) at/over
    /// `threshold`, evaluated live at `now`? Unlike [`Self::is_near`] — which
    /// deliberately ignores `7d_oi` so this bucket never gates non-Fable traffic —
    /// this is consulted ONLY for Fable requests by the manager. Reuses
    /// [`QuotaWindow::effective`], so a past-reset bucket reads a fresh `0.0`.
    pub fn model_weekly_exhausted(&self, threshold: f64, now: OffsetDateTime) -> bool {
        self.seven_day_oi
            .is_some_and(|w| w.effective(now) >= threshold)
    }

    /// Highest utilization across the dimensions that govern this request at `now`
    /// (shared 5-hour + weekly; the Fable weekly `7d_oi` too when `is_fable`), read
    /// live via [`QuotaWindow::effective`] so a past-reset window contributes a
    /// fresh `0.0`. A missing window contributes `0.0`. Used by
    /// [`crate::manager::Manager::select_revalidation`] to pick the least-utilized
    /// revalidation target — the account most likely to have stale headroom.
    /// Deliberately excludes the standard (API-key) token/request limits: OAuth
    /// accounts carry them `None`, and they are not trivially a single fraction.
    pub fn max_utilization(&self, now: OffsetDateTime, is_fable: bool) -> f64 {
        let mut max = 0.0f64;
        if let Some(w) = self.five_hour {
            max = max.max(w.effective(now));
        }
        if let Some(w) = self.seven_day {
            max = max.max(w.effective(now));
        }
        if is_fable {
            if let Some(w) = self.seven_day_oi {
                max = max.max(w.effective(now));
            }
        }
        max
    }

    /// Reset of the weekly bucket that governs rotation ordering, but only while
    /// it is still in the future at `now`. Used to spend the soonest-resetting
    /// quota first; `None` sorts an account first (unknown quota → probe it).
    pub fn governing_weekly_reset(&self, now: OffsetDateTime) -> Option<OffsetDateTime> {
        self.seven_day.and_then(|w| w.live_reset(now))
    }

    /// Fold a background probe's usage into the tracked windows. Only fields the
    /// probe actually reported overwrite existing values (a `None` utilization or
    /// reset leaves the prior value intact — matches the JS `applyUsageData`).
    pub fn apply_usage(&mut self, usage: &crate::probe::Usage) {
        apply_bucket(&mut self.five_hour, usage.five_hour, FIVE_HOUR);
        apply_bucket(&mut self.seven_day, usage.seven_day, SEVEN_DAY);
        apply_bucket(&mut self.seven_day_oi, usage.seven_day_oi, SEVEN_DAY);
    }
}

/// Merge a probed [`crate::probe::UsageBucket`] into an existing window in place.
/// A probed utilization that carries no reset gets a bounded synthesized reset
/// (`window` ahead of now) via [`resolve_reset`], so it can never pin the account
/// out of rotation forever (bug #4/#5).
///
/// **Exception — a ZERO utilization never synthesizes a reset.** The synthesis
/// above exists to bound a *high* utilization reported without a reset; a bucket
/// at `0.0` gates nothing, so inventing a reset for it protects nothing and
/// actively asserts a window that no real usage ever started. That fabrication
/// was not cosmetic: the zero-spend probe runs every `quotaProbeSeconds`, so it
/// stamped every idle account with a future 5h reset, and
/// [`crate::manager::Manager::warm_targets`] reads exactly that signal
/// (`five_hour.live_reset(now).is_some()`) as "already warm — skip". Measured on
/// the live fleet 2026-07-31: 7 of 13 accounts carried a future 5h reset at
/// `0.00` utilization, and keep-warm had **0 eligible targets** as a result.
/// Leaving the reset `None` here is the truthful state — we have no evidence of
/// a live window — and it is invisible to routing, because
/// [`QuotaWindow::effective`] returns `0.0` for a zero bucket under both the
/// `Some(reset)` and `None` arms.
fn apply_bucket(
    window: &mut Option<QuotaWindow>,
    bucket: Option<crate::probe::UsageBucket>,
    win: Duration,
) {
    let Some(bucket) = bucket else {
        return;
    };
    let now = OffsetDateTime::now_utc();
    let prior = *window;
    let utilization = bucket
        .utilization
        .or_else(|| prior.map(|w| w.utilization))
        .unwrap_or(0.0);
    let reported = bucket
        .reset_at_ms
        .and_then(|ms| OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000).ok());
    let reset = if reported.is_none() && utilization == 0.0 {
        // Nothing to gate — do not invent a window. See the exception above.
        None
    } else {
        resolve_reset(reported, prior.and_then(|w| w.reset), now, win)
    };
    *window = Some(QuotaWindow { utilization, reset });
}

/// Parse a trimmed header value into any `FromStr` target. The `f64` NaN/inf
/// guard is NOT applied here (the generic can't know `T` is a float) — the f64
/// call sites append `.filter(|v| v.is_finite())` themselves.
fn get_parsed<T: std::str::FromStr>(headers: &impl HeaderView, name: &str) -> Option<T> {
    headers.get_str(name).and_then(|s| s.trim().parse().ok())
}

/// Parse a reset header. Unified resets are epoch **seconds**; standard resets
/// may be an RFC3339 timestamp — try both.
fn get_reset(headers: &impl HeaderView, name: &str) -> Option<OffsetDateTime> {
    let raw = headers.get_str(name)?.trim();
    if let Ok(secs) = raw.parse::<i64>() {
        return OffsetDateTime::from_unix_timestamp(secs).ok();
    }
    OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use time::Duration;

    /// A test header bag with case-insensitive lookup.
    struct TestHeaders(HashMap<String, String>);

    impl TestHeaders {
        fn new(pairs: &[(&str, &str)]) -> Self {
            Self(
                pairs
                    .iter()
                    .map(|(k, v)| (k.to_lowercase(), v.to_string()))
                    .collect(),
            )
        }
    }

    impl HeaderView for TestHeaders {
        fn get_str(&self, name: &str) -> Option<&str> {
            self.0.get(&name.to_lowercase()).map(String::as_str)
        }
    }

    #[test]
    fn parses_unified_utilization_and_reset() {
        let reset_secs = 1_893_456_000i64;
        let headers = TestHeaders::new(&[
            ("anthropic-ratelimit-unified-5h-utilization", "0.42"),
            (
                "anthropic-ratelimit-unified-5h-reset",
                &reset_secs.to_string(),
            ),
            ("anthropic-ratelimit-unified-7d-utilization", "0.10"),
            ("anthropic-ratelimit-unified-status", "allowed_warning"),
        ]);
        let mut quota = Quota::default();
        assert!(
            quota.update_from_headers(&headers),
            "a response carrying the unified 5h header reports the window"
        );

        let five = quota.five_hour.unwrap();
        assert_eq!(five.utilization, 0.42);
        assert_eq!(
            five.reset,
            Some(OffsetDateTime::from_unix_timestamp(reset_secs).unwrap())
        );
        assert_eq!(quota.seven_day.unwrap().utilization, 0.10);
        assert_eq!(quota.status.as_deref(), Some("allowed_warning"));
    }

    /// Finding #11: a non-finite utilization header (`nan`/`inf`) must be treated
    /// as absent, not parsed into a NaN that reads as under-quota (NaN >= threshold
    /// is always false, so a spent account would stay in rotation). A normal numeric
    /// header still parses.
    #[test]
    fn rejects_non_finite_utilization_headers() {
        for bad in ["nan", "inf", "-inf"] {
            let headers = TestHeaders::new(&[("anthropic-ratelimit-unified-5h-utilization", bad)]);
            assert_eq!(
                get_parsed::<f64>(&headers, "anthropic-ratelimit-unified-5h-utilization")
                    .filter(|v| v.is_finite()),
                None,
                "{bad:?} must parse to None"
            );
            let mut quota = Quota::default();
            assert!(
                !quota.update_from_headers(&headers),
                "{bad:?} is not a readable window, so it must not report one either"
            );
            assert!(
                quota.five_hour.is_none(),
                "{bad:?} utilization must not create a window"
            );
        }

        let headers = TestHeaders::new(&[("anthropic-ratelimit-unified-5h-utilization", "0.87")]);
        assert_eq!(
            get_parsed::<f64>(&headers, "anthropic-ratelimit-unified-5h-utilization")
                .filter(|v| v.is_finite()),
            Some(0.87)
        );
    }

    /// Bug #2, proven: a window whose reset is in the PAST reads as a fresh 0.0,
    /// never the stale last-learned utilization — computed live against `now`.
    #[test]
    fn expired_quota_window_reads_as_fresh_zero() {
        let now = OffsetDateTime::now_utc();
        let window = QuotaWindow {
            utilization: 0.95,
            reset: Some(now - Duration::hours(1)), // already reset an hour ago
        };

        // Live read past the reset is fresh, not the cached 0.95.
        assert_eq!(window.effective(now), 0.0);

        // And an account carrying only that expired window is NOT near quota,
        // so it stays eligible for selection instead of being wrongly skipped.
        let quota = Quota {
            five_hour: Some(window),
            ..Quota::default()
        };
        assert!(!quota.is_near(0.90, now));

        // Sanity: before the reset the same window still reads full.
        let before_reset = now - Duration::hours(2);
        assert_eq!(window.effective(before_reset), 0.95);
        assert!(quota.is_near(0.90, before_reset));
    }

    #[test]
    fn is_near_true_when_five_hour_over_threshold() {
        let now = OffsetDateTime::now_utc();
        let quota = Quota {
            five_hour: Some(QuotaWindow {
                utilization: 0.95,
                reset: Some(now + Duration::hours(1)),
            }),
            ..Quota::default()
        };
        assert!(quota.is_near(0.90, now));
    }

    #[test]
    fn governing_weekly_reset_hides_past_reset() {
        let now = OffsetDateTime::now_utc();
        let quota = Quota {
            seven_day: Some(QuotaWindow {
                utilization: 0.5,
                reset: Some(now - Duration::minutes(5)),
            }),
            ..Quota::default()
        };
        assert_eq!(quota.governing_weekly_reset(now), None);
    }

    /// Finding #4/#8: a high utilization learned WITHOUT a reset must not pin the
    /// account out of rotation forever. `update_from_headers` synthesizes a
    /// bounded reset, so the window gates now but reads fresh once the synthesized
    /// window (5h) has elapsed — clearable, never permanent.
    #[test]
    fn resetless_high_util_window_is_bounded_not_permanent() {
        let mut quota = Quota::default();
        // Utilization with NO reset header.
        let headers = TestHeaders::new(&[("anthropic-ratelimit-unified-5h-utilization", "0.95")]);
        assert!(quota.update_from_headers(&headers));

        let now = OffsetDateTime::now_utc();
        // A reset was synthesized in the future, so it gates right now …
        assert!(
            quota.is_near(0.90, now),
            "resetless high util gates immediately"
        );
        assert!(
            quota.five_hour.unwrap().reset.is_some(),
            "a bounded reset was synthesized"
        );
        // … but clears once the synthesized 5h window has elapsed — not forever.
        let after_window = now + Duration::hours(6);
        assert!(
            !quota.is_near(0.90, after_window),
            "resetless window must clear after its bounded lifetime, never pin forever"
        );
    }

    /// The zero-spend probe must NOT fabricate a 5h window on an idle account.
    ///
    /// Measured live 2026-07-31: every probe sweep stamped idle accounts with a
    /// reset exactly `now + 5h`, and `warm_targets` reads a live 5h reset as
    /// "already warm", so keep-warm had 0 eligible targets of 13 while 7
    /// accounts carried a future reset at 0.00 utilization. The probe starts no
    /// window — only a served request does — so a zero bucket must leave the
    /// reset `None`.
    #[test]
    fn probed_zero_bucket_does_not_fabricate_a_window() {
        let mut quota = Quota::default();
        quota.apply_usage(&crate::probe::Usage {
            five_hour: Some(crate::probe::UsageBucket {
                utilization: Some(0.0),
                reset_at_ms: None,
            }),
            ..Default::default()
        });

        let now = OffsetDateTime::now_utc();
        let window = quota.five_hour.expect("the bucket was applied");
        assert_eq!(
            window.reset, None,
            "a zero-utilization probe must not synthesize a reset"
        );
        assert_eq!(
            window.live_reset(now),
            None,
            "and must therefore not read as a live window to warm_targets"
        );
    }

    /// The guard above must not weaken bug #4/#5 on the path it was written for:
    /// a NON-zero probed utilization with no reset still gets a bounded window,
    /// so it gates now and clears later rather than pinning the account forever.
    #[test]
    fn probed_nonzero_bucket_without_reset_still_gets_a_bounded_window() {
        let mut quota = Quota::default();
        quota.apply_usage(&crate::probe::Usage {
            five_hour: Some(crate::probe::UsageBucket {
                utilization: Some(0.95),
                reset_at_ms: None,
            }),
            ..Default::default()
        });

        let now = OffsetDateTime::now_utc();
        assert!(
            quota.five_hour.expect("applied").reset.is_some(),
            "a resetless HIGH utilization still needs its bounded reset"
        );
        assert!(quota.is_near(0.90, now), "and gates immediately");
        assert!(
            !quota.is_near(0.90, now + Duration::hours(6)),
            "clearing after one bounded window, never pinning forever"
        );
    }

    /// Finding #3: a spent standard (API-key) limit must read fresh once its
    /// `standard_reset` has passed, or the account is pinned out permanently
    /// (no OAuth probe re-reads API-key limits).
    #[test]
    fn standard_limit_reads_fresh_after_reset() {
        let now = OffsetDateTime::now_utc();
        let spent = Quota {
            tokens_limit: Some(1000),
            tokens_remaining: Some(0),
            standard_reset: Some(now - Duration::hours(1)),
            ..Quota::default()
        };
        assert!(
            !spent.is_near(0.90, now),
            "a standard window past its reset must not keep gating"
        );

        // Before the reset the same spent window still gates.
        let unexpired = Quota {
            standard_reset: Some(now + Duration::hours(1)),
            ..spent
        };
        assert!(unexpired.is_near(0.90, now));
    }

    /// Per-model routing: `model_weekly_exhausted` gates on the model-scoped
    /// `7d_oi` (Fable) bucket. True when the bucket is at/over threshold with a
    /// future reset; false when the bucket was never learned.
    #[test]
    fn model_weekly_exhausted_true_when_oi_over_threshold() {
        let now = OffsetDateTime::now_utc();
        let quota = Quota {
            seven_day_oi: Some(QuotaWindow {
                utilization: 0.95,
                reset: Some(now + Duration::hours(2)),
            }),
            ..Quota::default()
        };
        assert!(quota.model_weekly_exhausted(0.90, now));
    }

    #[test]
    fn model_weekly_exhausted_false_when_oi_absent() {
        let now = OffsetDateTime::now_utc();
        assert!(!Quota::default().model_weekly_exhausted(0.90, now));
    }

    /// Bug #2 applies here too: a `7d_oi` bucket past its reset reads as a fresh
    /// `0.0`, so an exhausted-then-reset Fable bucket must NOT keep gating.
    #[test]
    fn model_weekly_exhausted_reads_fresh_after_reset() {
        let now = OffsetDateTime::now_utc();
        let quota = Quota {
            seven_day_oi: Some(QuotaWindow {
                utilization: 0.95,
                reset: Some(now - Duration::hours(1)), // reset already passed
            }),
            ..Quota::default()
        };
        assert!(!quota.model_weekly_exhausted(0.90, now));
    }

    /// Regression guard: `is_near` must keep IGNORING the `7d_oi` bucket, so a
    /// full Fable weekly never gates the shared (non-Fable) rotation. Only the
    /// new `model_weekly_exhausted` — consulted for Fable requests alone — sees it.
    #[test]
    fn is_near_still_ignores_seven_day_oi() {
        let now = OffsetDateTime::now_utc();
        let quota = Quota {
            seven_day_oi: Some(QuotaWindow {
                utilization: 0.95,
                reset: Some(now + Duration::hours(2)),
            }),
            ..Quota::default()
        };
        assert!(!quota.is_near(0.90, now));
    }
}
