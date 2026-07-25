//! Workload scenario tests — real Claude Code traffic shapes driven through the
//! real selector, with **prompt-cache continuity** as the oracle.
//!
//! ## Why this file exists
//!
//! The inline unit suite passed in full while the live proxy destroyed 76.7% of
//! all prompt-cache creation: 13,033 mid-session cold starts and 39.4% of
//! requests switching account mid-conversation over 7 days. Unit tests of
//! `select()` cannot catch that class — each one asks "given this state, is the
//! next pick right?", while the damage is a property of a *sequence* of picks
//! under a *concurrent* arrival pattern. So these tests do not assert on one
//! call; they replay a workload and assert on the shape of the whole serve
//! history.
//!
//! ## The metric
//!
//! For one lineage (= one Claude Code conversation, keyed on the stable client
//! identity the proxy pins on), `switches` counts consecutive serves that landed
//! on *different* accounts, and
//!
//! ```text
//! continuity = 1.0 - switches / pairs      (pairs = requests - 1)
//! ```
//!
//! Anthropic's prompt cache is **per account**, so every switch re-creates the
//! whole conversation prefix upstream. `continuity == 1.0` means the whole
//! conversation was served warm.
//!
//! One deliberate refinement over raw `continuity`: a session that is *diverted*
//! for a single request by the soft pacing gate and then comes straight back
//! scores *worse* on `continuity` than one that is durably re-keyed and never
//! returns — the second serve back home counts as a second switch. That ranking
//! is backwards for cache economics (coming home is a cache HIT), so scenarios
//! that legitimately divert also assert on [`Fleet::home_share`] and on the pin
//! itself (the account the next select returns once the burst drains), which is
//! the invariant the soft-divert fix actually guarantees.
//!
//! ## Determinism
//!
//! Every scenario passes an explicit `now` built from a fixed epoch and advances
//! it by hand. No sleeps, no randomness, no wall-clock reads on any asserted
//! path. `PacingConfig::min_spacing_ms` is deliberately left unset everywhere
//! because it is evaluated against `last_served_ms`, which `enter_in_flight`
//! stamps from the real clock — the one place simulated time and wall time would
//! meet.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use teamclaude_rs::config::{Account, Config, PacingConfig, ProxyConfig, ThrottleConfig};
use teamclaude_rs::manager::{InFlightGuard, Manager};
use teamclaude_rs::oauth::{OAuthError, RefreshFuture, TokenRefresher};
use teamclaude_rs::probe::{ProbeError, ProbeFuture, Usage, UsageBucket, UsageProber};
use teamclaude_rs::warmer::{AccountWarmer, WarmError, WarmFuture};
use time::{Duration, OffsetDateTime};

// ---------------------------------------------------------------------------
// Part A — the harness
// ---------------------------------------------------------------------------

/// The in-flight cap these scenarios exercise: three requests may be in flight on
/// one account before it yields. This is the harness's own choice, NOT the
/// production default — `config::default_pacing` ships pacing OFF (no cap). The
/// scenarios build their `pacing(CAP)` explicitly so the capped path stays covered
/// on purpose; the burst scenarios are calibrated against this number, so it is
/// named once here.
const CAP: u32 = 3;

/// The default soft switch threshold (`config::default_switch_threshold`).
const SWITCH_THRESHOLD: f64 = 0.95;

/// Metric key under which identity-less (unpinned) traffic is recorded. Never
/// passed to `select` as an affinity — those calls pass `None`, which is the
/// whole point of scenario 6.
const ANON_KEY: u64 = 0;

/// A fixed simulation epoch (2026-01-01T00:00:00Z). Every scenario advances
/// `now` from here by hand, so a run is identical on every machine and clock.
fn t0() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_767_225_600).expect("fixed epoch is valid")
}

fn to_ms(at: OffsetDateTime) -> i64 {
    (at.unix_timestamp_nanos() / 1_000_000) as i64
}

/// Mirrors the inline unit helper of the same name (`src/manager/mod.rs`): one
/// config account with a priority and a token that is fresh far into the future.
fn account(name: &str, priority: i64) -> Account {
    Account {
        name: name.to_string(),
        account_type: "oauth".to_string(),
        account_uuid: None,
        org_uuid: None,
        org_name: None,
        access_token: format!("at-{name}"),
        refresh_token: Some(format!("rt-{name}")),
        expires_at: Some(to_ms(t0()) + 3_600_000),
        priority: Some(priority),
        switch_threshold: None,
        disabled: None,
        extra: serde_json::Map::new(),
    }
}

/// Per-account pacing with the production cap and no min-spacing (see the module
/// header for why min-spacing stays off).
fn pacing(cap: u32) -> PacingConfig {
    PacingConfig {
        max_in_flight_per_account: Some(cap),
        min_spacing_ms: None,
    }
}

/// None of these are ever invoked: `select` and `enter_in_flight` touch neither
/// the network nor the token/probe/warm paths. They exist because `Manager::new`
/// takes them.
struct NeverRefreshes;
impl TokenRefresher for NeverRefreshes {
    fn refresh(&self, _refresh_token: String) -> RefreshFuture {
        Box::pin(async {
            Err(OAuthError::Transient(
                "no refresher in workload tests".into(),
            ))
        })
    }
}

struct NeverProbes;
impl UsageProber for NeverProbes {
    fn probe(&self, _access_token: String) -> ProbeFuture {
        Box::pin(async {
            Err(ProbeError {
                status: None,
                message: "no prober in workload tests".into(),
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
                message: "no warmer in workload tests".into(),
            })
        })
    }
}

/// One Claude Code conversation. A lineage issues requests as
/// `select(&tried, now, model, Some(key))` — the same shape the proxy uses once
/// it has resolved a stable client identity (`x-api-key` / `metadata.user_id`).
///
/// Subagents and workflow workers inherit their parent's identity, so a fan-out
/// is modelled as *many concurrent requests on one lineage*, not many lineages.
#[derive(Clone, Copy)]
struct Lineage {
    key: u64,
    name: &'static str,
}

/// A fleet of accounts plus the recorded serve history that the metric is
/// computed from.
struct Fleet {
    manager: Arc<Manager>,
    account_names: Vec<String>,
    /// Every `(lineage key, account index)` serve, in issue order.
    serves: Vec<(u64, usize)>,
    /// Lineage display names in first-registration order (report ordering).
    lineages: Vec<(u64, &'static str)>,
    next_key: u64,
}

impl Fleet {
    /// Build a manager over `accounts` (name, priority) with `pacing` and the
    /// default soft switch threshold. `config_path: None` makes every persist a
    /// no-op, so nothing can reach disk.
    fn new(accounts: &[(&str, i64)], pacing: PacingConfig) -> Self {
        let account_names = accounts.iter().map(|(n, _)| (*n).to_string()).collect();
        let config = Config {
            proxy: ProxyConfig::default(),
            upstream: "https://api.anthropic.com".to_string(),
            switch_threshold: SWITCH_THRESHOLD,
            pacing,
            // Inert: the global egress throttle is an async sleep on the send
            // path and has no bearing on selection.
            throttle: ThrottleConfig::default(),
            lock_account: None,
            accounts: accounts.iter().map(|(n, p)| account(n, *p)).collect(),
            extra: serde_json::Map::new(),
        };
        let manager = Manager::new(
            config,
            Arc::new(NeverRefreshes),
            Arc::new(NeverProbes),
            Arc::new(NeverWarms),
            None,
        );
        let mut fleet = Self {
            manager,
            account_names,
            serves: Vec::new(),
            lineages: Vec::new(),
            next_key: 1,
        };
        fleet.lineages.push((ANON_KEY, "anonymous"));
        fleet
    }

    /// A healthy same-priority fleet of `n` accounts at the production cap.
    fn healthy(n: usize) -> Self {
        const POOL: [&str; 6] = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot"];
        let accounts: Vec<(&str, i64)> = POOL[..n].iter().map(|n| (*n, 0)).collect();
        Self::new(&accounts, pacing(CAP))
    }

    /// Register a new conversation. Keys are handed out densely from 1 so
    /// `ANON_KEY` (0) can never collide with a real lineage.
    fn lineage(&mut self, name: &'static str) -> Lineage {
        let key = self.next_key;
        self.next_key += 1;
        self.lineages.push((key, name));
        Lineage { key, name }
    }

    /// One request on `lineage`, recorded. Panics (with the report) if the fleet
    /// refuses to serve — a dropped request is always a scenario bug here, since
    /// every scenario keeps at least one account servable.
    fn serve(&mut self, lineage: Lineage, now: OffsetDateTime) -> usize {
        self.serve_with_tried(lineage, &HashSet::new(), now)
    }

    /// One request on `lineage` that has already failed on the accounts in
    /// `tried` — the shape the proxy uses when it rotates after an upstream
    /// error.
    fn serve_with_tried(
        &mut self,
        lineage: Lineage,
        tried: &HashSet<usize>,
        now: OffsetDateTime,
    ) -> usize {
        match self.manager.select(tried, now, None, Some(lineage.key)) {
            Some(idx) => {
                self.serves.push((lineage.key, idx));
                idx
            }
            None => panic!(
                "fleet refused to serve lineage {}\n{}",
                lineage.name,
                self.report()
            ),
        }
    }

    /// One identity-less request: no affinity key, so the selector must route it
    /// without creating or touching a pin.
    fn serve_anon(&mut self, now: OffsetDateTime) -> usize {
        match self.manager.select(&HashSet::new(), now, None, None) {
            Some(idx) => {
                self.serves.push((ANON_KEY, idx));
                idx
            }
            None => panic!("fleet refused an identity-less request\n{}", self.report()),
        }
    }

    /// A request on `lineage` whose response is still streaming: the returned
    /// guard holds the account's `in_flight` slot until it is dropped. This is
    /// how a burst saturates an account — exactly as the proxy does when it
    /// moves the guard into an SSE body.
    fn stream(&mut self, lineage: Lineage, now: OffsetDateTime) -> InFlightGuard {
        let idx = self.serve(lineage, now);
        self.manager.enter_in_flight(idx)
    }

    /// As [`Self::stream`], for identity-less traffic.
    fn stream_anon(&mut self, now: OffsetDateTime) -> InFlightGuard {
        let idx = self.serve_anon(now);
        self.manager.enter_in_flight(idx)
    }

    /// Drive account `idx` over the soft switch threshold on its shared weekly
    /// (`7d`) window with a reset 48h out, through the same public path a
    /// background quota probe uses. Mirrors the inline unit helper
    /// `set_over_threshold` (`src/manager/mod.rs`); the reset is supplied
    /// explicitly so the fold is not clock-dependent.
    fn set_over_threshold(&self, idx: usize, util: f64, now: OffsetDateTime) {
        self.manager.apply_usage(
            idx,
            &Usage {
                seven_day: Some(UsageBucket {
                    utilization: Some(util),
                    reset_at_ms: Some(to_ms(now + Duration::hours(48))),
                }),
                ..Usage::default()
            },
        );
    }

    fn account_name(&self, idx: usize) -> &str {
        self.account_names
            .get(idx)
            .map_or("?", std::string::String::as_str)
    }

    /// The accounts that served `key`, in order.
    fn serves_of(&self, key: u64) -> Vec<usize> {
        self.serves
            .iter()
            .filter(|(k, _)| *k == key)
            .map(|(_, idx)| *idx)
            .collect()
    }

    /// Consecutive serves of one lineage that landed on different accounts —
    /// each one is a prompt-cache prefix re-created upstream.
    fn switches(&self, key: u64) -> usize {
        self.serves_of(key)
            .windows(2)
            .filter(|w| w[0] != w[1])
            .count()
    }

    /// `1.0 - switches / pairs`; `1.0` for a lineage with fewer than 2 requests.
    fn continuity(&self, key: u64) -> f64 {
        let serves = self.serves_of(key);
        if serves.len() < 2 {
            return 1.0;
        }
        let pairs = serves.len() - 1;
        1.0 - (self.switches(key) as f64 / pairs as f64)
    }

    /// The account a lineage started on — the one holding its warm prefix.
    fn home(&self, key: u64) -> usize {
        *self
            .serves_of(key)
            .first()
            .unwrap_or_else(|| panic!("lineage {key} never issued a request"))
    }

    /// Fraction of a lineage's requests served by its home account. Unlike
    /// [`Self::continuity`] this does not punish a divert-and-return, so it is
    /// the right reading where the soft pacing gate is expected to fire.
    fn home_share(&self, key: u64) -> f64 {
        let serves = self.serves_of(key);
        if serves.is_empty() {
            return 1.0;
        }
        let home = serves[0];
        serves.iter().filter(|&&idx| idx == home).count() as f64 / serves.len() as f64
    }

    /// How many distinct accounts a lineage was served by — the number of
    /// upstream caches its conversation prefix was created in.
    fn distinct_accounts(&self, key: u64) -> usize {
        let mut seen: Vec<usize> = self.serves_of(key);
        seen.sort_unstable();
        seen.dedup();
        seen.len()
    }

    /// Combined continuity over every real (non-anonymous) lineage: total
    /// switches over total pairs.
    fn overall_continuity(&self) -> f64 {
        let (switches, pairs) = self.lineages.iter().filter(|(k, _)| *k != ANON_KEY).fold(
            (0usize, 0usize),
            |(s, p), (key, _)| {
                let n = self.serves_of(*key).len();
                (s + self.switches(*key), p + n.saturating_sub(1))
            },
        );
        if pairs == 0 {
            return 1.0;
        }
        1.0 - (switches as f64 / pairs as f64)
    }

    /// One greppable line per lineage that issued anything:
    /// `lineage=<name> requests=<n> switches=<n> continuity=<0.00-1.00> home=<account> path=<a>b>a>`
    fn report(&self) -> String {
        let mut counts: HashMap<u64, usize> = HashMap::new();
        for (key, _) in &self.serves {
            *counts.entry(*key).or_insert(0) += 1;
        }
        let mut out = String::new();
        for (key, name) in &self.lineages {
            let serves = self.serves_of(*key);
            if serves.is_empty() {
                continue;
            }
            let path: Vec<&str> = serves.iter().map(|&idx| self.account_name(idx)).collect();
            out.push_str(&format!(
                "lineage={} requests={} switches={} continuity={:.2} home={} path={}\n",
                name,
                serves.len(),
                self.switches(*key),
                self.continuity(*key),
                self.account_name(serves[0]),
                path.join(">"),
            ));
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Part B — the scenarios
// ---------------------------------------------------------------------------

/// **1. One conversation, 50 turns, a healthy fleet.** The baseline every other
/// scenario degrades from: a lone session's warm cache is never moved, so it is
/// served by exactly one account for its whole life.
#[test]
fn solo_session_stays_on_one_account() {
    let mut fleet = Fleet::healthy(3);
    let session = fleet.lineage("solo");
    let mut now = t0();

    for _ in 0..50 {
        fleet.serve(session, now);
        now += Duration::seconds(1);
    }

    assert_eq!(
        fleet.distinct_accounts(session.key),
        1,
        "a lone session must never be moved\n{}",
        fleet.report()
    );
    assert_eq!(fleet.switches(session.key), 0, "\n{}", fleet.report());
    assert!(
        (fleet.continuity(session.key) - 1.0).abs() < f64::EPSILON,
        "\n{}",
        fleet.report()
    );
}

/// **2. A subagent fan-out.** Five workers run concurrently under the PARENT's
/// lineage key — real behaviour, because subagents inherit `metadata.user_id`
/// — each holding a streaming response. With the cap at 3 some workers MUST be
/// diverted; that is pacing working. What must NOT happen is the parent's pin
/// moving: once the burst drains, the next turn has to land back on the account
/// holding the conversation's warm prefix.
///
/// This is the load-bearing one. A soft gate may divert a request; it may never
/// re-key a session.
#[test]
fn subagent_fanout_does_not_rekey_the_parent() {
    let mut fleet = Fleet::healthy(3);
    let parent = fleet.lineage("parent");
    let mut now = t0();

    // The parent's first turn establishes the pin (and the warm cache).
    let home = fleet.serve(parent, now);
    now += Duration::seconds(1);

    // Five subagents fan out concurrently on the parent's identity.
    let mut streams = Vec::new();
    for _ in 0..5 {
        streams.push(fleet.stream(parent, now));
        now += Duration::milliseconds(50);
    }

    let fanout: Vec<usize> = fleet.serves_of(parent.key)[1..].to_vec();
    assert_eq!(
        fanout.iter().filter(|&&idx| idx == home).count(),
        CAP as usize,
        "exactly the cap's worth of workers should fit on the pinned account\n{}",
        fleet.report()
    );
    assert!(
        fanout.iter().any(|&idx| idx != home),
        "over-cap workers must be diverted off the pin — that is pacing working\n{}",
        fleet.report()
    );

    // The streams complete.
    drop(streams);
    now += Duration::seconds(1);

    assert_eq!(
        fleet.serve(parent, now),
        home,
        "the parent's next turn must land on its ORIGINAL account: a soft pacing \
         divert must not have re-keyed the session\n{}",
        fleet.report()
    );
}

/// **3. A workflow fan-out at 16 concurrent**, same inherited identity. Asserts
/// the pin survives well past the cap, not merely at cap+1 — past the point
/// where every account is saturated and selection falls through to the
/// least-loaded soft fallback.
#[test]
fn workflow_fanout_16_concurrent_does_not_rekey() {
    let mut fleet = Fleet::healthy(4);
    let workflow = fleet.lineage("workflow");
    let mut now = t0();

    let home = fleet.serve(workflow, now);
    now += Duration::seconds(1);

    let mut streams = Vec::new();
    for _ in 0..16 {
        streams.push(fleet.stream(workflow, now));
        now += Duration::milliseconds(25);
    }

    // 4 accounts x cap 3 = 12 paced slots; the last 4 are served by the soft
    // fallback (pacing may spread a request, never drop one).
    assert_eq!(
        fleet.serves_of(workflow.key).len(),
        17,
        "every fan-out request must be served\n{}",
        fleet.report()
    );

    drop(streams);
    now += Duration::seconds(1);

    assert_eq!(
        fleet.serve(workflow, now),
        home,
        "a 16-wide burst must not move the pin\n{}",
        fleet.report()
    );
    assert_eq!(
        fleet.serve(workflow, now + Duration::seconds(1)),
        home,
        "and it must stay put on the turn after that\n{}",
        fleet.report()
    );
}

/// **4. Two sessions stacked on one account.** Both start on the same account
/// (the second is offline when they arrive), then the fleet recovers. The
/// load-balancing migration is *allowed* to move one of them once — that is the
/// balance it exists for — but it must then converge, not ping-pong the pair
/// back and forth for the rest of their lives.
#[test]
fn two_sessions_on_one_account_do_not_pingpong() {
    let mut fleet = Fleet::new(&[("alpha", 0), ("bravo", 0)], pacing(CAP));
    let first = fleet.lineage("session-a");
    let second = fleet.lineage("session-b");
    let mut now = t0();

    // Both sessions arrive while only one account is up, so both pin to it.
    fleet.manager.set_disabled(1, true);
    assert_eq!(fleet.serve(first, now), 0);
    assert_eq!(fleet.serve(second, now), 0);
    fleet.manager.set_disabled(1, false);
    now += Duration::seconds(1);

    for _ in 0..40 {
        fleet.serve(first, now);
        now += Duration::seconds(1);
        fleet.serve(second, now);
        now += Duration::seconds(1);
    }

    for session in [first, second] {
        assert!(
            fleet.switches(session.key) <= 1,
            "{} moved {} times — a rebalance is one move, not a ping-pong\n{}",
            session.name,
            fleet.switches(session.key),
            fleet.report()
        );
    }
    assert!(
        fleet.overall_continuity() >= 0.9,
        "combined continuity {:.3} fell below 0.9\n{}",
        fleet.overall_continuity(),
        fleet.report()
    );
    // Load-balancing migration ships OFF, so the pair deliberately does NOT
    // rebalance onto one account each: a session moves only at start or on a HARD
    // failure, and stacking two warm sessions on one healthy account costs nothing
    // while moving either one costs a full prompt-cache re-creation. Both keeping
    // their original pin is the CORRECT outcome, and it is strictly stronger than
    // the convergence this test originally asserted.
    for session in [first, second] {
        assert_eq!(
            fleet.switches(session.key),
            0,
            "{} left its warm pin while load-balancing migration is disabled\n{}",
            session.name,
            fleet.report()
        );
    }
    // The converge-one-per-account behaviour still exists behind
    // `loadBalanceMigration: true`; it is covered by
    // `migration_is_off_by_default_and_pin_is_honoured` in src/manager/mod.rs.
}

/// **5. An account crosses its quota threshold mid-run.** Two crossings, and only
/// one of them is a reason to move:
///
/// * **the SOFT switch threshold** is our own utilization arithmetic, computed from
///   headers that can be stale by minutes. Anthropic keeps answering 200s for
///   accounts it benches (one observed reading 100% weekly while still serving), so
///   it is a rotation hint for UNPINNED picks — the pinned sessions keep serving
///   from it and keep their prompt caches warm: `continuity == 1.0`.
/// * **a live 429 hold** is upstream's own verdict. That is HARD: the pinned
///   sessions move, exactly once, and stay moved.
///
/// The session on the healthy account must not be perturbed by either — a quota
/// event on a neighbour is never a reason to throw away a warm cache.
#[test]
fn quota_crossing_moves_only_the_affected_sessions() {
    let mut fleet = Fleet::new(&[("alpha", 0), ("bravo", 0)], pacing(CAP));
    let doomed_a = fleet.lineage("on-alpha-1");
    let healthy = fleet.lineage("on-bravo");
    let doomed_b = fleet.lineage("on-alpha-2");
    let mut now = t0();

    // LRU rotation lands these as alpha / bravo / alpha.
    assert_eq!(fleet.serve(doomed_a, now), 0);
    assert_eq!(fleet.serve(healthy, now), 1);
    assert_eq!(fleet.serve(doomed_b, now), 0);
    now += Duration::seconds(1);

    // Warm turns while everything is healthy — nobody moves.
    for _ in 0..5 {
        for session in [doomed_a, healthy, doomed_b] {
            fleet.serve(session, now);
            now += Duration::seconds(1);
        }
    }
    for session in [doomed_a, healthy, doomed_b] {
        assert_eq!(
            fleet.switches(session.key),
            0,
            "{} moved while the fleet was healthy\n{}",
            session.name,
            fleet.report()
        );
    }

    // Phase A — alpha crosses the SOFT switch threshold. Nobody moves: the pinned
    // sessions are served by alpha anyway, and upstream gets to be the oracle.
    fleet.set_over_threshold(0, 0.99, now);

    for _ in 0..5 {
        for session in [doomed_a, healthy, doomed_b] {
            fleet.serve(session, now);
            now += Duration::seconds(1);
        }
    }

    for session in [doomed_a, doomed_b] {
        assert_eq!(
            fleet.serves_of(session.key).last(),
            Some(&0),
            "{} must still be served by its over-threshold pin — the SOFT \
             threshold is a rotation hint, not proof the account is gone\n{}",
            session.name,
            fleet.report()
        );
        assert!(
            (fleet.continuity(session.key) - 1.0).abs() < f64::EPSILON,
            "{} lost cache continuity to a SOFT gate\n{}",
            session.name,
            fleet.report()
        );
    }
    // A session arriving cold still avoids the over-threshold account: the
    // threshold keeps steering the picks that have no warm cache to lose.
    assert_eq!(
        fleet.serve_anon(now),
        1,
        "unpinned traffic must still route away from the over-threshold \
         account\n{}",
        fleet.report()
    );
    now += Duration::seconds(1);

    // Phase B — alpha 429s for real, which arms a live hold. THAT is hard.
    fleet.manager.mark_rate_limited(0, 60);

    for _ in 0..5 {
        for session in [doomed_a, healthy, doomed_b] {
            fleet.serve(session, now);
            now += Duration::seconds(1);
        }
    }

    for session in [doomed_a, doomed_b] {
        assert_eq!(
            fleet.switches(session.key),
            1,
            "{} must re-pin exactly once off the held account — and only on the \
             HARD gate, never on the soft crossing before it\n{}",
            session.name,
            fleet.report()
        );
        assert_eq!(
            fleet.serves_of(session.key).last(),
            Some(&1),
            "{} must end up on the healthy account\n{}",
            session.name,
            fleet.report()
        );
    }
    assert_eq!(
        fleet.switches(healthy.key),
        0,
        "the session on the healthy account must not be perturbed by a \
         neighbour's quota event\n{}",
        fleet.report()
    );
    assert!(
        (fleet.continuity(healthy.key) - 1.0).abs() < f64::EPSILON,
        "\n{}",
        fleet.report()
    );
}

/// **6. Identity-less traffic must never take a pin.** Requests with no stable
/// client identity route by rotation and leave the affinity map untouched.
///
/// The guard is behavioural, because the pin map is private. A ghost pin — the
/// `proxy.rs` bug, where a request with no identity still got a synthetic key —
/// makes identity-less traffic STICK to one account instead of rotating, and the
/// phantom pins inflate the per-account pinned-session counts that drive the
/// load-balancing migration, dragging real warm sessions around for "balance"
/// against sessions that do not exist. Rotation is the observable end of that.
///
/// Three accounts, not two: the pin fast-path re-stamps the pinned account on
/// every select (deliberately — so other sessions' LRU steers away from it),
/// which in a two-account fleet would deterministically funnel all identity-less
/// traffic to the one remaining account and make rotation unobservable.
#[test]
fn identityless_requests_never_take_a_pin() {
    let mut fleet = Fleet::new(&[("alpha", 0), ("bravo", 0), ("charlie", 0)], pacing(CAP));
    let real = fleet.lineage("real-session");
    let mut now = t0();

    fleet.serve(real, now);
    now += Duration::seconds(1);

    for _ in 0..20 {
        fleet.serve_anon(now);
        now += Duration::seconds(1);
        fleet.serve(real, now);
        now += Duration::seconds(1);
    }

    assert!(
        fleet.distinct_accounts(ANON_KEY) >= 2,
        "identity-less requests must rotate across the fleet — sticking to one \
         account is the signature of a pin\n{}",
        fleet.report()
    );
    assert_eq!(
        fleet.switches(real.key),
        0,
        "identity-less traffic must not perturb a pinned session\n{}",
        fleet.report()
    );
    assert!(
        (fleet.continuity(real.key) - 1.0).abs() < f64::EPSILON,
        "\n{}",
        fleet.report()
    );
    assert_eq!(
        fleet.serves_of(real.key).last(),
        Some(&fleet.home(real.key)),
        "the pinned session must end where it started\n{}",
        fleet.report()
    );
}

/// **7. The production trigger: a busy neighbour must not re-key a quiet
/// session.** `in_flight` is an ACCOUNT-wide aggregate, so a neighbour's
/// concurrency — here identity-less traffic, which takes no pin and so cannot
/// trip the load-balancing migration — is what the quiet session's pin is
/// measured against.
///
/// Two phases:
/// * **busy, under the cap** — the pin is still usable, so the quiet session is
///   served warm every turn: `continuity == 1.0`.
/// * **saturated, at the cap** — the pin soft-fails, so that ONE request is
///   diverted (pacing working), and then the very next turn must come home.
///   Before the soft-divert fix the divert re-keyed the session durably: it
///   never came home, and every later turn paid a cold cache.
///
/// Pacing is the soft gate that still diverts. The other one — our own utilization
/// threshold — does not: scenario 5 covers a pin served straight through it.
#[test]
fn busy_neighbour_does_not_rekey_a_quiet_session() {
    // `hot` sorts first on priority, so both the quiet session and the
    // identity-less neighbour land there while it has room.
    let mut fleet = Fleet::new(&[("hot", 0), ("cold", 10)], pacing(CAP));
    let quiet = fleet.lineage("quiet");
    let mut now = t0();

    let home = fleet.serve(quiet, now);
    assert_eq!(home, 0, "the quiet session pins to the priority-0 account");
    now += Duration::seconds(1);

    // Phase 1 — the neighbour is busy but leaves a slot.
    for _ in 0..4 {
        let neighbour: Vec<InFlightGuard> = (0..CAP - 1)
            .map(|_| {
                let guard = fleet.stream_anon(now);
                now += Duration::milliseconds(10);
                guard
            })
            .collect();
        assert_eq!(
            fleet.serve(quiet, now),
            home,
            "a busy-but-not-saturated neighbour must not move a quiet session\n{}",
            fleet.report()
        );
        drop(neighbour);
        now += Duration::seconds(1);
    }
    assert!(
        (fleet.continuity(quiet.key) - 1.0).abs() < f64::EPSILON,
        "phase 1 continuity must be 1.0\n{}",
        fleet.report()
    );

    // Phase 2 — the neighbour saturates the account.
    const SATURATED_ROUNDS: usize = 4;
    for _ in 0..SATURATED_ROUNDS {
        let neighbour: Vec<InFlightGuard> = (0..CAP)
            .map(|_| {
                let guard = fleet.stream_anon(now);
                now += Duration::milliseconds(10);
                guard
            })
            .collect();
        assert_ne!(
            fleet.serve(quiet, now),
            home,
            "a saturated pin must DIVERT the request — that is the soft gate \
             doing its job\n{}",
            fleet.report()
        );
        drop(neighbour);
        now += Duration::seconds(1);
        assert_eq!(
            fleet.serve(quiet, now),
            home,
            "and the very next turn must come HOME: a soft gate may divert a \
             request, it may never re-key a session\n{}",
            fleet.report()
        );
        now += Duration::seconds(1);
    }

    assert_eq!(
        fleet.distinct_accounts(quiet.key),
        2,
        "the quiet session should only ever have touched its home and the \
         divert target\n{}",
        fleet.report()
    );
    let served = fleet.serves_of(quiet.key).len();
    let expected_home_share = (served - SATURATED_ROUNDS) as f64 / served as f64;
    assert!(
        (fleet.home_share(quiet.key) - expected_home_share).abs() < 1e-9,
        "every turn except the {SATURATED_ROUNDS} paced diverts must be served \
         by the home account (home_share {:.3}, expected {expected_home_share:.3})\n{}",
        fleet.home_share(quiet.key),
        fleet.report()
    );
}

// ---------------------------------------------------------------------------
// Part C — scenarios that fail today: the executable spec for the next fix
// ---------------------------------------------------------------------------

/// **8. A transient upstream failure must divert, never re-key.** When a 429 or
/// a transport blip fails a request on the pinned account, the proxy retries
/// with that account in `tried`. `select` then falls through to the normal pick
/// and writes the fall-through account in as the session's new pin — durably.
/// The account was never gone; one request failed on it.
///
/// This is the same invariant the soft-pacing fix established (`keep_pin` in
/// `src/manager/select.rs`), reached through a different trigger: `tried` is a
/// per-REQUEST fact, so like a soft gate it may divert a request, but it may not
/// re-key a session.
#[test]
fn transient_429_does_not_rekey_a_session() {
    let mut fleet = Fleet::new(&[("alpha", 0), ("bravo", 0)], pacing(CAP));
    let session = fleet.lineage("blipped");
    let mut now = t0();

    let home = fleet.serve(session, now);
    now += Duration::seconds(1);

    // The turn that blips: upstream fails on the pin, the proxy retries with it
    // in `tried`. Serving elsewhere is correct — this ONE request must go
    // somewhere.
    let mut tried = HashSet::new();
    tried.insert(home);
    let diverted = fleet.serve_with_tried(session, &tried, now);
    assert_ne!(
        diverted, home,
        "the retry must not re-use the failed account"
    );
    now += Duration::seconds(1);

    // The blip is over — the account was never actually gone.
    for _ in 0..5 {
        assert_eq!(
            fleet.serve(session, now),
            home,
            "a transient failure must not have moved the pin\n{}",
            fleet.report()
        );
        now += Duration::seconds(1);
    }
}

/// **9. Migration must never drag a session down a priority tier.** The
/// load-balancing migration in `src/manager/select.rs` ranks candidates on
/// `cand_key = (pinned_count, in_flight, last_selected_seq)` — `priority` is
/// absent — while `pick_eligible` sorts priority FIRST. So an account that
/// normal selection would never choose while a priority-0 account is healthy can
/// still win the migration and take a warm session with it (observed 544x in one
/// day).
///
/// Balance is a tiebreak WITHIN a tier, never a reason to cross one.
#[test]
fn migration_never_moves_a_session_to_a_lower_priority_tier() {
    let mut fleet = Fleet::new(&[("primary", 0), ("overflow", 10)], pacing(CAP));
    let first = fleet.lineage("tier-a");
    let second = fleet.lineage("tier-b");
    let mut now = t0();

    // Both sessions arrive while the overflow account is offline, so both pin to
    // the primary (priority-0) account.
    fleet.manager.set_disabled(1, true);
    assert_eq!(fleet.serve(first, now), 0);
    assert_eq!(fleet.serve(second, now), 0);
    fleet.manager.set_disabled(1, false);
    now += Duration::seconds(1);

    for _ in 0..10 {
        for session in [first, second] {
            fleet.serve(session, now);
            now += Duration::seconds(1);
        }
    }

    for session in [first, second] {
        assert_eq!(
            fleet.distinct_accounts(session.key),
            1,
            "{} was migrated off the healthy priority-0 account onto a lower \
             tier purely for balance\n{}",
            session.name,
            fleet.report()
        );
    }
}
