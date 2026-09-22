//! Property tests over the main crate's peer-mesh functions. The wire
//! crate's own encode/decode and parse/display pairs live in
//! `crates/tcr-peer-wire/tests/props.rs`.

use std::env;
use std::sync::Arc;

use proptest::prelude::*;
use tcr_peer_wire::LeaseRefusal;
use teamclaude_rs::config::{Account, Config, PacingConfig, ProxyConfig, ThrottleConfig};
use teamclaude_rs::manager::Manager;
use teamclaude_rs::oauth::{OAuthError, RefreshFuture, TokenRefresher};
use teamclaude_rs::peer::discovery;
use teamclaude_rs::peer::lease::schedule_refusal;
use teamclaude_rs::peer::listener::{Admission, KNOCK_BURST, KNOCK_INTERVAL_MS};
use teamclaude_rs::peer::schedule::{Hhmm, Schedule};
use teamclaude_rs::peer::serve::relay_path_is_routable;
use teamclaude_rs::probe::{ProbeError, ProbeFuture, UsageProber};
use teamclaude_rs::warmer::{AccountWarmer, WarmError, WarmFuture};

fn cases() -> u32 {
    env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256)
}

// ---------------------------------------------------------------------------
// Item 2: `relay_path_is_routable` refuses every traversal spelling
// ---------------------------------------------------------------------------

/// An ordinary, safe path segment: lower-case alphanumeric only, so it can
/// never accidentally spell a traversal token (no `.`, `%`, `\`, `/`).
fn safe_segment() -> impl Strategy<Value = String> {
    "[a-z0-9]{1,6}"
}

/// Every spelling the module doc names as a traversal or escape, USED AS ONE
/// WHOLE SEGMENT between slashes: a literal `..`, its `%2e` percent-encoding
/// (both cases, since the check is case-insensitive), the escaped path
/// separators `%2f`/`%2F`/`%5c`, and a raw backslash.
///
/// `//` (protocol-relative) is deliberately NOT in this set: the function
/// only refuses it as the path's own PREFIX (`path.starts_with("//")`), an
/// internal empty segment from `//` elsewhere is not a dot-segment and is not
/// refused, so it is tested separately below by construction
/// (`relay_path_refuses_protocol_relative_prefix`) rather than folded into
/// this per-segment grammar, where it would assert a refusal the function
/// does not make and turn a real property into a false failure.
fn danger_token() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        Just(".."),
        Just("%2e%2e"),
        Just("%2E%2E"),
        Just("%2f"),
        Just("%2F"),
        Just("%5c"),
        Just("%5C"),
        Just("\\"),
    ]
}

#[derive(Debug, Clone)]
enum Token {
    Safe(String),
    Danger(&'static str),
}

fn token() -> impl Strategy<Value = Token> {
    prop_oneof![
        4 => safe_segment().prop_map(Token::Safe),
        1 => danger_token().prop_map(Token::Danger),
    ]
}

/// Builds `/tok1/tok2/...` from the grammar above and says, from the grammar
/// alone (never from reading `relay_path_is_routable`'s own source), whether
/// any token is a traversal spelling.
fn routable_candidate() -> impl Strategy<Value = (String, bool)> {
    prop::collection::vec(token(), 1..6).prop_map(|tokens| {
        let has_danger = tokens.iter().any(|t| matches!(t, Token::Danger(_)));
        let body = tokens
            .iter()
            .map(|t| match t {
                Token::Safe(s) => s.as_str(),
                Token::Danger(d) => d,
            })
            .collect::<Vec<_>>()
            .join("/");
        (format!("/{body}"), has_danger)
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    /// A path built purely from safe segments is accepted, rooted, and
    /// contains no traversal spelling; a path carrying at least one traversal
    /// token from the grammar is refused. Neither arm reads
    /// `relay_path_is_routable`'s own implementation: the grammar alone
    /// decides which arm a candidate falls in.
    #[test]
    fn relay_path_refuses_every_traversal_spelling((path, has_danger) in routable_candidate()) {
        let accepted = relay_path_is_routable(&path);
        if has_danger {
            prop_assert!(!accepted, "{path:?} carries a traversal spelling and must be refused");
        } else {
            prop_assert!(accepted, "{path:?} is built from safe segments only and must be accepted");
            prop_assert!(path.starts_with('/'), "an accepted path must be rooted: {path:?}");
            prop_assert!(!path.contains(".."), "an accepted path must carry no .. segment: {path:?}");
        }
    }

    /// A path whose own prefix is `//` (protocol-relative: rooted AND naming
    /// an authority) is refused, whatever safe body follows it.
    #[test]
    fn relay_path_refuses_protocol_relative_prefix(body in prop::collection::vec(safe_segment(), 0..4)) {
        let path = format!("//{}", body.join("/"));
        prop_assert!(!relay_path_is_routable(&path), "{path:?} is protocol-relative and must be refused");
    }
}

// ---------------------------------------------------------------------------
// Item 3: `Admission::take_knock_token` never over-grants one address
// ---------------------------------------------------------------------------

/// One step: knock from one of four addresses, at a timestamp that only ever
/// moves forward from the previous step (a bucket refills from ELAPSED time,
/// so a clock running backwards is not a case this invariant is about).
#[derive(Debug, Clone, Copy)]
struct Knock {
    address_index: u8,
    advance_ms: i64,
}

fn knock_sequence() -> impl Strategy<Value = Vec<Knock>> {
    prop::collection::vec(
        (0u8..4, 0i64..(KNOCK_INTERVAL_MS * 3)).prop_map(|(address_index, advance_ms)| Knock {
            address_index,
            advance_ms,
        }),
        1..200,
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    /// However a sequence of up to 200 knocks is spread across four
    /// addresses and however time advances between them, no single address is
    /// ever granted more than `KNOCK_BURST` tokens inside any
    /// `KNOCK_INTERVAL_MS`-wide window: counted from the ACCEPTED sequence
    /// (every step whose token was actually granted), never from the
    /// bucket's own internal state.
    #[test]
    fn knock_bucket_never_exceeds_its_burst_per_window(steps in knock_sequence()) {
        let mut admission = Admission::new();
        let mut now_ms: i64 = 1_000_000; // arbitrary epoch, strictly non-decreasing below
        // grants[addr] = every timestamp (ms) that address was granted a token.
        let mut grants: [Vec<i64>; 4] = Default::default();

        for step in steps {
            now_ms += step.advance_ms;
            let addr = format!("10.0.0.{}", step.address_index);
            if admission.take_knock_token(&addr, now_ms) {
                grants[step.address_index as usize].push(now_ms);
            }
        }

        for timestamps in &grants {
            for &start in timestamps {
                let in_window = timestamps
                    .iter()
                    .filter(|&&t| t >= start && t < start + KNOCK_INTERVAL_MS)
                    .count();
                prop_assert!(
                    in_window <= KNOCK_BURST as usize,
                    "an address received {in_window} tokens inside one {KNOCK_INTERVAL_MS}ms window, \
                     more than KNOCK_BURST ({KNOCK_BURST}): grants at {timestamps:?}"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Item 4: `Manager::lendable_fraction` is bounded and monotone in its scope
// ---------------------------------------------------------------------------

/// `f` must stay bound by `MAX_LEND_FRACTION` "(read the
/// constant)". That constant (`peer::config`, value `0.5`) is the ceiling on
/// ONE GRANT's fraction, applied where a grant is written and where one is read
/// back off disk (the property at the end of this file): a different quantity
/// from the one this property tests.
/// `lendable_fraction` itself is not clamped to it, it folds
/// `(guard - utilization).max(0.0)` over eligible accounts, where
/// `guard = (switch_threshold - control_reserve).max(0.0)`
/// (`effective_threshold`, `src/manager/select.rs:248`): so with this test
/// fleet's `SWITCH_THRESHOLD = 0.9` and `CONTROL_RESERVE = 0.05`, `guard` is
/// `0.85`, well above `0.5`. Asserting `f <= 0.5` against the real function
/// would be a false failure, not a caught bug: confirmed by hand: a
/// zero-utilization single-account fleet with these thresholds returns `0.85`.
/// So this property bounds `f` by `GUARD`, the bound `lendable_fraction`
/// actually promises, and separately asserts the monotonicity half exactly as
/// asked: `f(subset scope) <= f(All)`, which holds because every account
/// `scope_covers(&All, ..)` covers by construction (`src/peer/lease.rs:1702`)
/// and the fold is a maximum, so restricting the account set a maximum runs
/// over can never raise it.
const SWITCH_THRESHOLD: f64 = 0.9;
const CONTROL_RESERVE: f64 = 0.05;
const GUARD: f64 = SWITCH_THRESHOLD - CONTROL_RESERVE;

struct NeverRefreshes;
impl TokenRefresher for NeverRefreshes {
    fn refresh(&self, _refresh_token: String) -> RefreshFuture {
        Box::pin(async { Err(OAuthError::Transient("no refresher in prop tests".into())) })
    }
}

struct NeverProbes;
impl UsageProber for NeverProbes {
    fn probe(&self, _access_token: String) -> ProbeFuture {
        Box::pin(async {
            Err(ProbeError {
                status: None,
                message: "no prober in prop tests".into(),
                retry_after_secs: None,
            })
        })
    }
}

struct NeverWarms;
impl AccountWarmer for NeverWarms {
    fn warm(&self, _access_token: String, _upstream: String) -> WarmFuture {
        Box::pin(async {
            Err(WarmError {
                status: None,
                message: "no warmer in prop tests".into(),
            })
        })
    }
}

/// One fake account, obviously-fake credentials: this repository is public.
/// Every other account in a fleet is odd-indexed into `work`, so a `Group`
/// scope test always has a real, non-empty partition to compare against `All`.
fn fake_account(index: usize) -> Account {
    Account {
        name: format!("acct{index}@example.com"),
        account_type: "oauth".to_string(),
        account_uuid: Some(format!("1111111{index}-1111-1111-1111-111111111111")),
        org_uuid: None,
        org_name: None,
        access_token: format!("at-{index}"),
        refresh_token: Some(format!("rt-{index}")),
        expires_at: Some(4_102_444_800_000),
        priority: Some(0),
        switch_threshold: None,
        disabled: None,
        groups: if index.is_multiple_of(2) {
            Some(vec!["work".to_string()])
        } else {
            None
        },
        organization_type: None,
        rate_limit_tier: None,
        seat_tier: None,
        egress: teamclaude_rs::config::Egress::Local,
        egress_strict: false,
        extra: serde_json::Map::new(),
    }
}

fn fleet(accounts: Vec<Account>) -> Arc<Manager> {
    let config = Config {
        quarantined_accounts: Vec::new(),
        migrated_legacy_throttle: false,
        renamed_accounts: Vec::new(),
        rename_write_error: None,
        proxy: ProxyConfig::default(),
        upstream: "http://127.0.0.1:1".to_string(),
        switch_threshold: SWITCH_THRESHOLD,
        fable_weekly_threshold: None,
        pacing: PacingConfig {
            max_in_flight_per_account: None,
            min_spacing_ms: None,
        },
        account_throttle: ThrottleConfig::default(),
        fleet_throttle: ThrottleConfig::default(),
        lock_account: None,
        control_account: None,
        control_reserve: CONTROL_RESERVE,
        control_pooled: false,
        control_identity: Default::default(),
        reset_urgency_tier_hours: 24,
        http1_only: false,
        accounts,
        group_settings: std::collections::HashMap::new(),
        pricing: Default::default(),
        usage_retention_days: 90,
        extra: serde_json::Map::new(),
    };
    Manager::new(
        config,
        Arc::new(NeverRefreshes),
        Arc::new(NeverProbes),
        Arc::new(NeverWarms),
        None,
    )
}

/// A random fleet of 1..6 accounts, each with a random 5-hour utilization in
/// `[0.0, 1.0]`, and one scope to test: `All`, the `work` group (every
/// even-indexed account), or a random non-empty subset of the fleet's names.
fn fleet_and_scope() -> impl Strategy<Value = (Vec<f64>, tcr_peer_wire::LendScope)> {
    (1usize..6, prop::collection::vec(0.0f64..1.0, 1..6)).prop_flat_map(|(n, utilizations)| {
        let utilizations = utilizations.into_iter().take(n).collect::<Vec<_>>();
        let n = utilizations.len();
        let names: Vec<String> = (0..n).map(|i| format!("acct{i}@example.com")).collect();
        let scope_strategy = prop_oneof![
            Just(tcr_peer_wire::LendScope::All),
            Just(tcr_peer_wire::LendScope::Group("work".to_string())),
            prop::collection::vec(0..n, 1..=n).prop_map(move |indices| {
                let mut labels: Vec<String> =
                    indices.into_iter().map(|i| names[i].clone()).collect();
                labels.sort();
                labels.dedup();
                tcr_peer_wire::LendScope::Accounts(labels)
            }),
        ];
        (Just(utilizations), scope_strategy)
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    /// `0.0 <= lendable_fraction(scope, ..) <= GUARD` for any scope, and
    /// `lendable_fraction(scope, ..) <= lendable_fraction(All, ..)`: a
    /// restricted scope can never see MORE headroom than the whole fleet.
    #[test]
    fn lendable_fraction_is_bounded_and_monotone_in_scope((utilizations, scope) in fleet_and_scope()) {
        let accounts: Vec<Account> = (0..utilizations.len()).map(fake_account).collect();
        let manager = fleet(accounts);
        let now = time::OffsetDateTime::now_utc();
        for (idx, utilization) in utilizations.iter().enumerate() {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                "anthropic-ratelimit-unified-5h-utilization",
                format!("{utilization}").parse().expect("a header value"),
            );
            let reset = (now + time::Duration::hours(2)).unix_timestamp();
            headers.insert(
                "anthropic-ratelimit-unified-5h-reset",
                reset.to_string().parse().expect("a header value"),
            );
            manager.update_quota(idx, &headers);
        }

        let f_scope = manager.lendable_fraction(&scope, tcr_peer_wire::Window::FiveHour, now);
        let f_all = manager.lendable_fraction(&tcr_peer_wire::LendScope::All, tcr_peer_wire::Window::FiveHour, now);

        prop_assert!(f_scope >= 0.0, "lendable_fraction must never be negative: {f_scope}");
        prop_assert!(f_scope <= GUARD + f64::EPSILON, "lendable_fraction {f_scope} exceeds the guard {GUARD}");
        prop_assert!(f_scope <= f_all + f64::EPSILON, "scope={scope:?} lent {f_scope} > All's {f_all}");
    }
}

// ---------------------------------------------------------------------------
// Item 5: `sanitize_name` is an oracle over id-shaped and hostile input
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    /// A name embedding an `@` (an email shape) is always refused, whatever
    /// surrounds it.
    #[test]
    fn sanitize_name_refuses_any_embedded_at(prefix in "[a-zA-Z0-9]{0,8}", suffix in "[a-zA-Z0-9]{0,8}") {
        let name = format!("{prefix}@{suffix}");
        prop_assert_eq!(discovery::sanitize_name(&name), None);
    }

    /// A name that IS a canonical UUID shape (`8-4-4-4-12` hex, matched
    /// against the WHOLE trimmed label: `is_uuid_shape`, `crates/tcr-peer-wire`)
    /// is always refused. Not tested with surrounding text: a prefix or
    /// suffix changes a group's length and stops the shape from matching at
    /// all, which is a fact about `is_uuid_shape`'s own contract (an exact
    /// match on the whole label), not a gap this oracle needs to cover.
    #[test]
    fn sanitize_name_refuses_the_canonical_uuid_shape(
        a in "[0-9a-f]{8}", b in "[0-9a-f]{4}", c in "[0-9a-f]{4}", d in "[0-9a-f]{4}", e in "[0-9a-f]{12}",
    ) {
        let name = format!("{a}-{b}-{c}-{d}-{e}");
        prop_assert_eq!(discovery::sanitize_name(&name), None);
    }

    /// A name embedding `PeerId::display`'s own `tcr-` shape (`tcr-` plus ten
    /// alphanumeric characters) is always refused: an id-shaped "name" is
    /// what testing found slipping past the old denylist.
    #[test]
    fn sanitize_name_refuses_any_embedded_tcr_prefix(rest in "[A-Za-z0-9]{10}") {
        let name = format!("tcr-{rest}");
        prop_assert_eq!(discovery::sanitize_name(&name), None);
    }

    /// Any name `sanitize_name` accepts is returned byte-for-byte unchanged
    /// (once trimmed: the sanitizer's own documented trim) and is at most
    /// `MAX_LABEL_BYTES` long, the cap `sanitize_label` enforces.
    #[test]
    fn sanitize_name_returns_accepted_names_unchanged_and_within_the_cap(raw in ".{0,80}") {
        if let Some(accepted) = discovery::sanitize_name(&raw) {
            prop_assert_eq!(&accepted, raw.trim());
            prop_assert!(accepted.len() <= tcr_peer_wire::MAX_LABEL_BYTES);
        }
    }
}

// ---------------------------------------------------------------------------
// `Schedule::contains` agrees with an
// independently-built, brute-force minute walk over a whole week
// ---------------------------------------------------------------------------

fn hhmm_strategy() -> impl Strategy<Value = Hhmm> {
    (0u8..24, 0u8..60).prop_map(|(h, m)| Hhmm::new(h, m).expect("h<24, m<60 by construction"))
}

fn weekday_strategy() -> impl Strategy<Value = time::Weekday> {
    prop_oneof![
        Just(time::Weekday::Monday),
        Just(time::Weekday::Tuesday),
        Just(time::Weekday::Wednesday),
        Just(time::Weekday::Thursday),
        Just(time::Weekday::Friday),
        Just(time::Weekday::Saturday),
        Just(time::Weekday::Sunday),
    ]
}

fn schedule_strategy() -> impl Strategy<Value = Schedule> {
    (
        prop::option::of((hhmm_strategy(), hhmm_strategy())),
        prop::option::of(prop::collection::hash_set(weekday_strategy(), 1..7)),
    )
        .prop_map(|(between, days)| Schedule { between, days })
}

/// Minute `0` is Monday 00:00. Marks every minute of the week the window
/// covers, built by WALKING FORWARD from each allowed day's start minute for
/// the window's own duration, a different construction from
/// `Schedule::contains`'s comparison-based arithmetic, so an off-by-one in
/// one does not reappear in the other by copy-paste.
fn brute_force_open_minutes(schedule: &Schedule) -> [bool; 10_080] {
    let mut open = [false; 10_080];
    let days_of_week = [
        time::Weekday::Monday,
        time::Weekday::Tuesday,
        time::Weekday::Wednesday,
        time::Weekday::Thursday,
        time::Weekday::Friday,
        time::Weekday::Saturday,
        time::Weekday::Sunday,
    ];
    let day_allowed =
        |d: time::Weekday| schedule.days.as_ref().is_none_or(|days| days.contains(&d));
    match schedule.between {
        None => {
            for day in days_of_week {
                if day_allowed(day) {
                    let base = day.number_days_from_monday() as usize * 1_440;
                    for minute in &mut open[base..base + 1_440] {
                        *minute = true;
                    }
                }
            }
        }
        Some((start, end)) => {
            let (start_m, end_m) = (start.minute_of_day() as usize, end.minute_of_day() as usize);
            let duration = match start_m.cmp(&end_m) {
                std::cmp::Ordering::Less => end_m - start_m,
                std::cmp::Ordering::Greater => (1_440 - start_m) + end_m,
                std::cmp::Ordering::Equal => 0,
            };
            for day in days_of_week {
                if !day_allowed(day) || duration == 0 {
                    continue;
                }
                let start_abs = day.number_days_from_monday() as usize * 1_440 + start_m;
                for offset in 0..duration {
                    open[(start_abs + offset) % 10_080] = true;
                }
            }
        }
    }
    open
}

proptest! {
    // 10,080 `contains()` calls per case (one per minute of a week), so the
    // case count is bounded well below the shared `cases()` default rather
    // than reused from it.
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// For every minute of an arbitrary week, `Schedule::contains_at_local`
    /// agrees with the brute-force walk above, including every midnight
    /// crossing, every day-of-week boundary, and the zero-width `start ==
    /// end` case.
    ///
    /// Against `contains_at_local`, not `contains`: the brute-force model
    /// reads `instant`'s own hour/minute/weekday fields directly, the exact
    /// contract `contains_at_local` documents. `contains` first converts
    /// through the HOST's ambient local offset (`UtcOffset::local_offset_at`),
    /// which shifts the weekday near a UTC day boundary on any host that is
    /// not at UTC+0, caught by running this property against `contains` on a
    /// host two hours ahead: `between: None, days: {Monday}` disagreed at
    /// week-minute 1320 (Monday 22:00 UTC), because converting to local rolled
    /// that instant into Tuesday.
    #[test]
    fn contains_agrees_with_a_brute_force_week_walk(schedule in schedule_strategy()) {
        // 2000-01-03 is a Monday (Jan 1 2000 was a Saturday); the anchor is
        // pinned in a test assertion rather than trusted from a comment.
        let monday = time::macros::datetime!(2000-01-03 00:00 UTC);
        prop_assert_eq!(monday.weekday(), time::Weekday::Monday);

        let open = brute_force_open_minutes(&schedule);
        for (week_minute, expected) in open.iter().enumerate() {
            let instant = monday + time::Duration::minutes(week_minute as i64);
            prop_assert_eq!(
                schedule.contains_at_local(instant),
                *expected,
                "week-minute {} ({}) disagreed with the brute-force walk",
                week_minute,
                instant
            );
        }
    }
}

/// The property red on a seeded off-by-one: a window whose END is treated as
/// INCLUSIVE (`minute <= end_m` instead of `minute < end_m`) disagrees with
/// the brute-force walk at the boundary minute itself.
#[test]
fn contains_disagrees_with_brute_force_under_a_seeded_off_by_one() {
    fn contains_with_inclusive_end_bug(schedule: &Schedule, now: time::OffsetDateTime) -> bool {
        let minute = u16::from(now.hour()) * 60 + u16::from(now.minute());
        let Some((start, end)) = schedule.between else {
            return true;
        };
        let (start_m, end_m) = (start.minute_of_day(), end.minute_of_day());
        minute >= start_m && minute <= end_m // bug: should be `<`
    }

    let schedule = Schedule {
        between: Some((Hhmm::new(9, 0).unwrap(), Hhmm::new(17, 0).unwrap())),
        days: None,
    };
    let at_the_boundary = time::macros::datetime!(2026-01-05 17:00 UTC);
    assert!(
        !schedule.contains(at_the_boundary),
        "the real function excludes the end minute"
    );
    assert!(
        contains_with_inclusive_end_bug(&schedule, at_the_boundary),
        "the seeded bug must include it, proving this test would have caught it"
    );
}

// ---------------------------------------------------------------------------
// `schedule_refusal` at the window boundary
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    /// No schedule at all never refuses, for any instant.
    #[test]
    fn schedule_refusal_is_none_without_a_schedule(week_minute in 0i64..10_080) {
        let monday = time::macros::datetime!(2000-01-03 00:00 UTC);
        let now = monday + time::Duration::minutes(week_minute);
        prop_assert_eq!(schedule_refusal(None, now), None);
    }
}

/// Decision row 14's own example: the same grant, `between (22:00, 08:00)`,
/// refused a minute before the window opens and permitted a minute after.
///
/// # Why the offset is written down rather than looked up
///
/// The decision is about a WALL-CLOCK reading, 21:59 on somebody's kitchen
/// clock, so the boundary is asserted at an offset this test names, against
/// `Schedule::contains_at_local`, which takes an already-local instant. Every
/// offset in the list is checked, so the boundary holds the same way east and
/// west of UTC and on a half-hour offset.
///
/// An earlier version asked `schedule_refusal` (which goes through
/// `Schedule::contains`, which looks up THIS HOST's offset) and tried to
/// compensate by stamping the fixture with a looked-up offset of its own. That
/// works only while the two lookups agree, and on a host that observes
/// daylight saving they do not: an offset read for "now" is the wrong one for
/// a fixture dated in January, and the test failed with `left:
/// Some(OutsideSchedule), right: None` on the `after` assertion. A test of
/// this module's arithmetic should not be able to fail for a reason that lives
/// in `localtime_r`.
///
/// The host-offset path is still covered, below, by asking for a verdict
/// rather than for a boundary.
#[test]
fn schedule_refusal_flips_at_the_boundary_2159_vs_2201() {
    let schedule = Schedule {
        between: Some((Hhmm::new(22, 0).unwrap(), Hhmm::new(8, 0).unwrap())),
        days: None,
    };
    for (hours, minutes) in [(0, 0), (2, 0), (-8, 0), (5, 30), (13, 0)] {
        let offset = time::UtcOffset::from_hms(hours, minutes, 0).expect("a real offset");
        let before = time::macros::datetime!(2026-01-05 21:59 UTC).replace_offset(offset);
        let after = time::macros::datetime!(2026-01-05 22:01 UTC).replace_offset(offset);
        assert!(
            !schedule.contains_at_local(before),
            "21:59 is outside a 22:00 window at {offset}"
        );
        assert!(
            schedule.contains_at_local(after),
            "22:01 is inside it at {offset}"
        );
    }
}

/// `schedule_refusal` answers the refusal for a shut window and nothing for an
/// open one, at whatever offset the host running this is in.
///
/// The pair is built AROUND the instant this runs, the window that contains
/// now, and the window three hours from now, so the verdict is the same at
/// every hour and in every timezone, and the host's own `local_offset_at` is
/// on the path rather than stubbed out. The exact minute the verdict flips is
/// the test above; this one is that the host path reaches the same answer.
#[test]
fn schedule_refusal_uses_this_hosts_own_clock() {
    let now_utc = time::OffsetDateTime::now_utc();
    let offset = time::UtcOffset::local_offset_at(now_utc).unwrap_or(time::UtcOffset::UTC);
    let local = now_utc.to_offset(offset);
    let hour = |shift: i16| -> u8 { ((i16::from(local.hour()) + shift).rem_euclid(24)) as u8 };

    let open = Schedule {
        between: Some((
            Hhmm::new(hour(-3), local.minute()).expect("a real time of day"),
            Hhmm::new(hour(3), local.minute()).expect("a real time of day"),
        )),
        days: None,
    };
    let shut = Schedule {
        between: Some((
            Hhmm::new(hour(3), local.minute()).expect("a real time of day"),
            Hhmm::new(hour(6), local.minute()).expect("a real time of day"),
        )),
        days: None,
    };
    assert_eq!(schedule_refusal(Some(&open), now_utc), None);
    assert_eq!(
        schedule_refusal(Some(&shut), now_utc),
        Some(LeaseRefusal::OutsideSchedule)
    );
}

// ---------------------------------------------------------------------------
// A grant's fraction is inside the ceiling WHEREVER it came from
// ---------------------------------------------------------------------------

// Every fraction that comes off disk is inside `[0, MAX_LEND_FRACTION]`.
//
// The ceiling used to be enforced in one place only, `tcr peer lend`'s own argv
// handling, so it held for a file this build had just written and for nothing
// else: a grant written by an older build, edited by hand, or copied from
// another Mac reached `clamp_to_grant`'s
// `wanted.min(grant.fraction).min(lendable)` with any value at all, and the
// field's own doc said "Clamped" while nothing on the read path clamped it.
//
// Watched red by deleting the `deserialize_with` on `LendGrant::fraction`:
// `0.9` reads back as `0.9`.
proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    #[test]
    fn a_fraction_read_off_disk_is_inside_the_ceiling(raw in -1000.0_f64..1000.0) {
        let grant: teamclaude_rs::peer::config::LendGrant = serde_json::from_str(&format!(
            r#"{{"window":"7d","fraction":{raw},"ttlS":300,"maxInflight":2}}"#
        ))
        .expect("a grant with a finite fraction parses");
        prop_assert!(
            (0.0..=teamclaude_rs::peer::config::MAX_LEND_FRACTION).contains(&grant.fraction),
            "a hand-written {raw} reached the sizing arithmetic as {}",
            grant.fraction
        );
    }
}

/// The two ends of the same rule, spelled out: what an operator's own
/// over-large grant reads back as, and that an ordinary one is untouched.
#[test]
fn an_over_large_fraction_reads_back_at_the_ceiling() {
    let read = |raw: &str| -> f64 {
        serde_json::from_str::<teamclaude_rs::peer::config::LendGrant>(&format!(
            r#"{{"window":"7d","fraction":{raw},"ttlS":300,"maxInflight":2}}"#
        ))
        .expect("the grant parses")
        .fraction
    };
    assert_eq!(read("0.9"), teamclaude_rs::peer::config::MAX_LEND_FRACTION);
    assert_eq!(read("-0.5"), 0.0);
    assert_eq!(
        read("0.2"),
        0.2,
        "a fraction inside the ceiling is untouched"
    );
}
