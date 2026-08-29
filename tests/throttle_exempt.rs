//! Behavioural coverage for the `throttleExemptNoise` knob
//! (`Manager::throttle_exempt_noise_enabled`, `src/manager/throttle.rs`).
//!
//! A real `mitm::serve` listener in front of a real `Manager`, a fake
//! upstream that counts hits AND records which account served each one, and a
//! concurrent burst measured end-to-end.
//! These are ASSERTIONS, not timing measurements — every test uses a
//! deliberately wide margin (the throttle's own spacing is 350ms and this
//! never asserts anything finer than 100ms either side of it), so these are
//! NOT `#[ignore]`d.
//!
//! # Where a burst LANDS is the independent variable
//!
//! The throttle is two buckets: a per-ORGANIZATION one (the real limiter) and a
//! looser fleet-wide ceiling. So a burst's cost depends on where it lands, and
//! every test here controls that through the `user_id` in the request body —
//! `stable_session_key` hashes it into an affinity pin, so distinct ids
//! ([`spread_uids`]) scatter across orgs and a repeated id ([`same_uids`])
//! concentrates on one. A test that spreads when it meant to concentrate asserts
//! nothing, because spreading is free by design.
//!
//! ## The exemption behaviours
//!
//! 1. Knob OFF (default) — a CONCENTRATED burst of `Noise`-classified requests
//!    (`/api/event_logging/v2/batch`) still pays its org's GCRA.
//! 2. Knob ON — the same concentrated burst skips the PER-ORG bucket. It still
//!    pays the fleet ceiling; these tests set that ceiling loose enough not to
//!    bind, so the timing attributes cleanly to the per-org bucket.
//! 3. Knob ON, exempt-prefix boundary — `classify_request` (`src/manager/select.rs`)
//!    matches `Noise` by PREFIX, so `/api/event_logging/v2/batch` (already
//!    covered by case 2) and the shorter `/api/event_logging` root both
//!    classify as `Noise` and skip the per-org GCRA the same way.
//! 4. Knob ON, exempt-prefix miss — `/api/event_loggingXYZ` still
//!    `starts_with("/api/event_logging")`, so `classify_request` returns
//!    `Noise` for it too and it is exempt exactly like case 3, even though
//!    it is not a real event-logging path. This asserts what the code
//!    ACTUALLY does today (prefix match, no trailing `/` or path-boundary
//!    check), not the tighter behaviour a reader might assume from the name
//!    "prefix". If this ever starts failing, `classify_request`'s matching
//!    got stricter and this comment (and case 3's) should be revisited.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use teamclaude_rs::config::{Account, Config, PacingConfig, ProxyConfig, ThrottleConfig};
use teamclaude_rs::manager::Manager;
use teamclaude_rs::oauth::{OAuthError, RefreshFuture, TokenRefresher};
use teamclaude_rs::probe::{ProbeError, ProbeFuture, UsageProber};
use teamclaude_rs::warmer::{AccountWarmer, WarmError, WarmFuture};

// ---------------------------------------------------------------------------
// Stubs — nothing here ever reaches the network.
// ---------------------------------------------------------------------------

struct NeverRefreshes;
impl TokenRefresher for NeverRefreshes {
    fn refresh(&self, _refresh_token: String) -> RefreshFuture {
        Box::pin(async {
            Err(OAuthError::Transient(
                "no refresher in throttle tests".into(),
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
                message: "no prober in throttle tests".into(),
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
                message: "no warmer in throttle tests".into(),
            })
        })
    }
}

// ---------------------------------------------------------------------------
// Fleet
// ---------------------------------------------------------------------------

const POOL: usize = 13;

fn fake_uuid(i: usize) -> String {
    format!("{:08}-1111-1111-1111-111111111111", i)
}

/// Distinct from [`fake_uuid`] so a bucket key built from the wrong field is
/// visible rather than silently plausible.
fn fake_org_uuid(i: usize) -> String {
    format!("{:08}-2222-2222-2222-222222222222", i)
}

/// One account per org, EXCEPT account 0 which deliberately has no org at all.
///
/// Account 0 covers the `acct:<idx>` fallback branch of
/// `Manager::throttle_bucket_key`; every other account covers the `org:<uuid>`
/// branch. Before this change every fixture had `org_uuid: None`, so a per-org
/// test suite built on them would have exercised only the fallback and never the
/// path that actually ships.
fn account(i: usize) -> Account {
    Account {
        name: format!("acct{i}"),
        account_type: "oauth".to_string(),
        account_uuid: Some(fake_uuid(i)),
        org_uuid: (i > 0).then(|| fake_org_uuid(i)),
        org_name: None,
        access_token: format!("at-fake-{i}"),
        refresh_token: Some(format!("rt-fake-{i}")),
        expires_at: Some(teamclaude_rs::now_ms() + 3_600_000),
        // ALL accounts share one priority so they form a single rotation tier.
        //
        // This used to be `Some(i as i64)` — 13 distinct priorities, i.e. 13 tiers
        // of one. Rotation only happens WITHIN a tier, so every new session pinned
        // to account 0 and a burst of "distinct" sessions all landed on one
        // account. That was invisible while the throttle was fleet-wide (one
        // bucket either way) and became load-bearing the moment buckets were
        // keyed per-org: `spread_burst_across_orgs_is_not_paced` measured exactly
        // `(8-4)*350 = 1400ms`, the closed form for a single bucket.
        priority: Some(0),
        switch_threshold: None,
        disabled: None,
        groups: None,
        extra: serde_json::Map::new(),
    }
}

/// Plus the knob under test in `extra`.
fn config(
    upstream: &str,
    account_throttle: ThrottleConfig,
    fleet_throttle: ThrottleConfig,
    throttle_exempt_noise: bool,
) -> Config {
    let mut extra = serde_json::Map::new();
    extra.insert("sessionAffinity".to_string(), serde_json::Value::Bool(true));
    extra.insert(
        "throttleExemptNoise".to_string(),
        serde_json::Value::Bool(throttle_exempt_noise),
    );
    Config {
        quarantined_accounts: Vec::new(),
        migrated_legacy_throttle: false,
        proxy: ProxyConfig {
            port: 0,
            api_key: None,
            extra: serde_json::Map::new(),
        },
        upstream: upstream.to_string(),
        switch_threshold: 0.98,
        pacing: PacingConfig::default(),
        account_throttle,
        fleet_throttle,
        lock_account: None,
        control_account: None,
        control_reserve: 0.05,
        control_pooled: false,
        reset_urgency_tier_hours: 24,
        http1_only: false,
        accounts: (0..POOL).map(account).collect(),
        group_settings: std::collections::HashMap::new(),
        pricing: Default::default(),
        usage_retention_days: 90,
        extra,
    }
}

fn manager(
    upstream: &str,
    account_throttle: ThrottleConfig,
    fleet_throttle: ThrottleConfig,
    throttle_exempt_noise: bool,
) -> Arc<Manager> {
    Manager::new(
        config(
            upstream,
            account_throttle,
            fleet_throttle,
            throttle_exempt_noise,
        ),
        Arc::new(NeverRefreshes),
        Arc::new(NeverProbes),
        Arc::new(NeverWarms),
        None,
    )
}

/// A PER-ORG bucket tight enough that a burst landing on ONE org is visibly
/// paced: 4 admit instantly, then one per 350ms.
///
/// Deliberately `burst: 4` rather than the shipped default of 8, so a burst of
/// `BURST` (8) on a single org produces the same `(8-4)*350 = 1400ms` closed form
/// the floor/ceiling constants below were derived from. Testing the *mechanism*,
/// not the shipped tuning.
fn account_throttle_tight() -> ThrottleConfig {
    ThrottleConfig {
        min_spacing_ms: Some(350),
        burst: Some(4),
    }
}

/// A fleet ceiling loose enough that it never binds in these tests, so a timing
/// assertion attributes cleanly to the per-org bucket. `burst: 1024` swallows any
/// burst these tests fire.
fn fleet_throttle_loose() -> ThrottleConfig {
    ThrottleConfig {
        min_spacing_ms: Some(1),
        burst: Some(1024),
    }
}

/// Fully inert (all knobs `None`) — the `"{}"` escape hatch, per bucket.
fn throttle_off() -> ThrottleConfig {
    ThrottleConfig::default()
}

// ---------------------------------------------------------------------------
// Fake upstream — matches ANY path (event_logging, mcp-registry, v1/messages).
// ---------------------------------------------------------------------------

fn account_headers(resp: axum::http::response::Builder) -> axum::http::response::Builder {
    resp.header("anthropic-ratelimit-unified-status", "allowed")
        .header("anthropic-ratelimit-unified-5h-utilization", "0.42")
        .header("anthropic-ratelimit-unified-7d-utilization", "0.10")
        .header("anthropic-ratelimit-unified-7d-reset", "1800000000")
        .header("anthropic-ratelimit-requests-remaining", "7")
        .header("anthropic-organization-id", "org-00000000")
        .header("request-id", "req_0000000000000000")
        .header("anthropic-version", "2023-06-01")
}

/// Records which ACCOUNT served each request, by the bearer token the proxy
/// forwarded (`at-fake-<i>`, unique per fixture account).
///
/// This is the instrument that stops a spread/concentration test from passing for
/// the wrong reason. A timing assertion alone cannot distinguish "the burst spread
/// across buckets" from "the burst was fast for some unrelated reason" — and the
/// first version of `spread_burst_across_orgs_is_not_paced` failed for exactly
/// that class of hidden cause (every session pinned to one account because the
/// fixture gave all 13 accounts distinct priorities). Asserting on the account set
/// makes the premise explicit instead of assumed.
type ServedBy = Arc<std::sync::Mutex<Vec<String>>>;

async fn spawn_json_upstream() -> (String, Arc<AtomicUsize>) {
    let (base, hits, _served) = spawn_json_upstream_recording().await;
    (base, hits)
}

async fn spawn_json_upstream_recording() -> (String, Arc<AtomicUsize>, ServedBy) {
    let hits = Arc::new(AtomicUsize::new(0));
    let served: ServedBy = Arc::new(std::sync::Mutex::new(Vec::new()));
    let counter = hits.clone();
    let recorder = served.clone();
    let app = Router::new().fallback(any(move |req: axum::extract::Request| {
        let counter = counter.clone();
        let recorder = recorder.clone();
        async move {
            let token = req
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("<none>")
                .trim_start_matches("Bearer ")
                .to_string();
            let _ = axum::body::to_bytes(req.into_body(), 256 * 1024 * 1024).await;
            counter.fetch_add(1, Ordering::SeqCst);
            recorder
                .lock()
                .expect("served-by lock poisoned")
                .push(token);
            let body = br#"{"type":"message","usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}"#;
            account_headers(Response::builder().status(200))
                .header("content-type", "application/json")
                .body(Body::from(body.to_vec()))
                .expect("build upstream response")
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind json upstream");
    let addr = listener.local_addr().expect("json upstream addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), hits, served)
}

/// How many DISTINCT accounts served requests since `from` (an index into the
/// recorded log, so warmup traffic can be excluded from the count).
fn distinct_accounts_since(served: &ServedBy, from: usize) -> usize {
    let log = served.lock().expect("served-by lock poisoned");
    let mut seen: Vec<&String> = log.iter().skip(from).collect();
    seen.sort();
    seen.dedup();
    seen.len()
}

fn served_len(served: &ServedBy) -> usize {
    served.lock().expect("served-by lock poisoned").len()
}

async fn spawn_proxy(manager: Arc<Manager>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let addr = listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        teamclaude_rs::mitm::serve(listener, manager, None).await;
    });
    format!("http://{addr}")
}

fn body(user_id: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "model": "claude-opus-4-6",
        "max_tokens": 1024,
        "stream": false,
        "metadata": { "user_id": user_id },
        "messages": [{ "role": "user", "content": "hi" }],
    }))
    .expect("serialize body")
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build client")
}

async fn post(cli: &reqwest::Client, base: &str, path: &str, payload: &[u8]) -> (u16, Duration) {
    let start = Instant::now();
    let resp = cli
        .post(format!("{base}{path}"))
        .header("content-type", "application/json")
        .body(payload.to_vec())
        .send()
        .await
        .expect("request sent");
    let status = resp.status().as_u16();
    let _ = resp.bytes().await.expect("read body");
    (status, start.elapsed())
}

/// One concurrent POST per entry in `uids`; returns the wall-clock elapsed for
/// the LAST to complete, relative to a shared start instant.
///
/// The `user_id` in each body is what `stable_session_key` hashes into an
/// affinity pin, so `uids` is how a test chooses WHERE the burst lands: distinct
/// ids spread across accounts (and therefore across per-org buckets), a repeated
/// id concentrates on one.
///
/// Every DISTINCT id is warmed serially first, then the bucket is left idle long
/// enough to refill. Warming each session rather than just the route is
/// load-bearing: an unpinned session picks its account during `select`, so firing
/// N concurrent first-requests would race selection and could scatter a burst the
/// test meant to concentrate (or vice versa). After warmup every pin exists and
/// the timed burst is deterministic.
async fn burst_last_elapsed_uids(proxy: &str, path: &str, uids: &[String]) -> Duration {
    let cli = client();
    let mut distinct: Vec<&String> = uids.iter().collect();
    distinct.sort();
    distinct.dedup();
    for uid in distinct {
        let (s, _) = post(&cli, proxy, path, &body(uid)).await;
        assert_eq!(s, 200, "warmup request must serve 200");
    }
    tokio::time::sleep(Duration::from_millis(2_000)).await;

    let start = Instant::now();
    let mut handles = Vec::with_capacity(uids.len());
    for uid in uids {
        let cli = client();
        let proxy = proxy.to_string();
        let path = path.to_string();
        let payload = body(uid);
        handles.push(tokio::spawn(async move {
            let (status, _) = post(&cli, &proxy, &path, &payload).await;
            assert_eq!(status, 200, "burst request must serve 200");
            start.elapsed()
        }));
    }
    let mut last = Duration::ZERO;
    for h in handles {
        let elapsed = h.await.expect("burst task");
        if elapsed > last {
            last = elapsed;
        }
    }
    last
}

/// `n` sessions, all DIFFERENT — affinity spreads them across accounts, so each
/// lands in its own per-org bucket.
fn spread_uids(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("user_burst_{i}")).collect()
}

/// `n` requests from ONE session — all pinned to a single account, so they all
/// contend for the same per-org bucket.
fn same_uids(n: usize) -> Vec<String> {
    vec!["user_solo".to_string(); n]
}

/// The tuning this actually SHIPS with (`default_account_throttle`).
fn account_throttle_shipped() -> ThrottleConfig {
    ThrottleConfig {
        min_spacing_ms: Some(350),
        burst: Some(8),
    }
}

/// The ceiling this actually SHIPS with (`default_fleet_throttle`).
fn fleet_throttle_shipped() -> ThrottleConfig {
    ThrottleConfig {
        min_spacing_ms: Some(100),
        burst: Some(16),
    }
}

const BURST: usize = 8;
/// `(BURST - burst) * spacing_ms` = `(8 - 4) * 350` = 1400ms — the minimum
/// wall-clock a fully-throttled burst of 8 must take for its last request.
/// Asserted with a wide margin below the closed form so scheduler jitter on a
/// loaded CI box cannot flake it.
const THROTTLED_FLOOR: Duration = Duration::from_millis(900);
/// Upper bound for a burst that skips the GCRA entirely. `THROTTLED_FLOOR`
/// (900ms) vs. an actually-exempt burst (~0ms) is a ~900ms discriminating
/// gap, so 800ms buys 2.7x headroom against CI flake (`cargo test --all
/// --locked`, debug build, 2-vCPU runners, three 4-worker tokio runtimes
/// contending) while still catching a real regression: it costs zero
/// discriminating power relative to 300ms because nothing exempt should ever
/// approach even a fraction of one 350ms throttle tick.
const EXEMPT_CEILING: Duration = Duration::from_millis(800);

/// Case 1: knob OFF (default) — `Noise` traffic still pays the per-org bucket.
/// Current behaviour preserved.
///
/// The burst CONCENTRATES on one session (`same_uids`) so it all lands in a
/// single per-org bucket. Since the throttle became per-org, a spread burst is
/// free by design, so spreading here would assert nothing about the exemption.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn noise_burst_throttled_when_knob_off() {
    let (upstream, _hits) = spawn_json_upstream().await;
    let mgr = manager(
        &upstream,
        account_throttle_tight(),
        fleet_throttle_loose(),
        false,
    );
    let proxy = spawn_proxy(mgr).await;

    let last =
        burst_last_elapsed_uids(&proxy, "/api/event_logging/v2/batch", &same_uids(BURST)).await;
    assert!(
        last >= THROTTLED_FLOOR,
        "knob OFF: a Noise burst on ONE org must still pay that org's GCRA — \
         last request completed in {last:?}, expected >= {THROTTLED_FLOOR:?}"
    );
}

/// Case 2: knob ON — the SAME concentrated `Noise` burst skips the per-org
/// bucket. It still pays the fleet ceiling, which is deliberately loose enough
/// here (`fleet_throttle_loose`) not to bind, so the timing attributes cleanly to
/// the per-org bucket being skipped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn noise_burst_exempt_when_knob_on() {
    let (upstream, _hits) = spawn_json_upstream().await;
    let mgr = manager(
        &upstream,
        account_throttle_tight(),
        fleet_throttle_loose(),
        true,
    );
    let proxy = spawn_proxy(mgr).await;

    let last =
        burst_last_elapsed_uids(&proxy, "/api/event_logging/v2/batch", &same_uids(BURST)).await;
    assert!(
        last <= EXEMPT_CEILING,
        "knob ON: a Noise burst must skip the per-org GCRA — \
         last request completed in {last:?}, expected <= {EXEMPT_CEILING:?}"
    );
}

/// Case 3: knob ON, bare-prefix boundary. `/api/event_logging` (no trailing
/// segment at all, unlike case 2's `/api/event_logging/v2/batch`) still
/// `starts_with("/api/event_logging")`, so `classify_request` returns `Noise`
/// for it too and the burst is exempt just like case 2.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exempt_prefix_root_when_knob_on() {
    let (upstream, _hits) = spawn_json_upstream().await;
    let mgr = manager(
        &upstream,
        account_throttle_tight(),
        fleet_throttle_loose(),
        true,
    );
    let proxy = spawn_proxy(mgr).await;

    let last = burst_last_elapsed_uids(&proxy, "/api/event_logging", &same_uids(BURST)).await;
    assert!(
        last <= EXEMPT_CEILING,
        "knob ON: `/api/event_logging` (bare prefix root) classifies as Noise \
         and must skip the GCRA — last request completed in {last:?}, expected <= {EXEMPT_CEILING:?}"
    );
}

/// Case 4: knob ON, prefix overreach. `classify_request` matches `Noise` with
/// a plain `str::starts_with` — no trailing `/` or path-segment boundary
/// check — so `/api/event_loggingXYZ` and `/mcp-registry-anything` (neither a
/// real event-logging or mcp-registry route) ALSO `starts_with` their
/// respective prefixes and classify as `Noise`. This asserts what the code
/// actually does today: both are exempt from the GCRA, same as a genuine
/// `/api/event_logging/...` path. If this starts failing, `classify_request`
/// grew a path-boundary check and this comment is stale.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exempt_prefix_overreach_when_knob_on() {
    let (upstream, _hits) = spawn_json_upstream().await;
    let mgr = manager(
        &upstream,
        account_throttle_tight(),
        fleet_throttle_loose(),
        true,
    );
    let proxy = spawn_proxy(mgr).await;

    let last_logging =
        burst_last_elapsed_uids(&proxy, "/api/event_loggingXYZ", &same_uids(BURST)).await;
    assert!(
        last_logging <= EXEMPT_CEILING,
        "knob ON: `/api/event_loggingXYZ` prefix-matches `/api/event_logging` \
         and classifies as Noise (no path-boundary check) — last request completed \
         in {last_logging:?}, expected <= {EXEMPT_CEILING:?}"
    );

    let last_mcp =
        burst_last_elapsed_uids(&proxy, "/mcp-registry-anything", &same_uids(BURST)).await;
    assert!(
        last_mcp <= EXEMPT_CEILING,
        "knob ON: `/mcp-registry-anything` prefix-matches `/mcp-registry` \
         and classifies as Noise (no path-boundary check) — last request completed \
         in {last_mcp:?}, expected <= {EXEMPT_CEILING:?}"
    );
}

// ---------------------------------------------------------------------------
// Per-org bucketing. These are the tests that carry the behavioural claim of the
// split: capacity scales WITH the account pool, while any single organization is
// paced exactly as tightly as before.
// ---------------------------------------------------------------------------

/// The headline: a burst SPREAD across sessions lands in different per-org
/// buckets and is not paced at all.
///
/// Under the old single fleet-wide bucket this exact burst cost
/// `(8-4)*350 = 1400ms` on its last request. Contrast
/// `concentrated_burst_on_one_org_is_still_paced`, which fires the same width at
/// the same settings and DOES pay — the only difference is where it lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spread_burst_across_orgs_is_not_paced() {
    let (upstream, _hits, served) = spawn_json_upstream_recording().await;
    let mgr = manager(
        &upstream,
        account_throttle_tight(),
        fleet_throttle_loose(),
        false,
    );
    let proxy = spawn_proxy(mgr).await;

    let last = burst_last_elapsed_uids(&proxy, "/v1/messages", &spread_uids(BURST)).await;

    // Prove the PREMISE before trusting the timing. A fast burst only means
    // "spread across buckets" if it actually reached several accounts; without
    // this the test would go green for any reason the proxy happened to be quick.
    // >= 4 distinct out of 8 bounds each bucket at 2 requests, comfortably inside
    // the burst-4 budget, so the timing assertion below is attributable.
    let distinct = distinct_accounts_since(&served, 0);
    assert!(
        distinct >= 4,
        "premise failed, not the behaviour: a {BURST}-wide burst of DISTINCT \
         sessions reached only {distinct} account(s), so this never tested \
         per-org spreading at all. Check that the fixture's accounts share a \
         priority tier — rotation only happens within a tier."
    );

    assert!(
        last <= EXEMPT_CEILING,
        "a {BURST}-wide burst spread across {distinct} orgs must not be paced — \
         each org's bucket sees at most a request or two, well inside its burst \
         budget. Last request completed in {last:?}, expected <= {EXEMPT_CEILING:?}"
    );
}

/// The regression guard: per-identity protection must SURVIVE the split.
///
/// If this ever goes green while `spread_burst_across_orgs_is_not_paced` also
/// passes, per-org pacing has silently stopped working and every organization is
/// unthrottled.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concentrated_burst_on_one_org_is_still_paced() {
    let (upstream, _hits, served) = spawn_json_upstream_recording().await;
    let mgr = manager(
        &upstream,
        account_throttle_tight(),
        fleet_throttle_loose(),
        false,
    );
    let proxy = spawn_proxy(mgr).await;

    let last = burst_last_elapsed_uids(&proxy, "/v1/messages", &same_uids(BURST)).await;

    // The mirror of the premise check in `spread_burst_across_orgs_is_not_paced`:
    // this burst must have CONCENTRATED, or the floor below proves nothing.
    let distinct = distinct_accounts_since(&served, 0);
    assert_eq!(
        distinct,
        1,
        "premise failed, not the behaviour: one session must pin to exactly one \
         account, but {} requests reached {distinct} accounts. If selection \
         migrated the pin mid-burst this test is no longer measuring one org.",
        served_len(&served)
    );

    assert!(
        last >= THROTTLED_FLOOR,
        "a {BURST}-wide burst pinned to ONE session (and therefore one org) must \
         still pay `(8-4)*350 = 1400ms` — this is the per-identity protection the \
         split must not have loosened. Last request completed in {last:?}, \
         expected >= {THROTTLED_FLOOR:?}"
    );
}

/// The fleet ceiling must actually limit. A ceiling that never fires is
/// indistinguishable from an absent one — and "the burst was fast" is exactly
/// what a silently-inert ceiling looks like.
///
/// Per-org is switched OFF here so the only thing that can pace this burst is the
/// ceiling; the timing therefore attributes to it and nothing else.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fleet_ceiling_paces_a_spread_burst_when_it_is_the_only_bucket() {
    let (upstream, _hits) = spawn_json_upstream().await;
    let mgr = manager(
        &upstream,
        throttle_off(),
        // Same numbers as `account_throttle_tight`, so the SAME closed form and
        // the same floor constant apply: (8-4)*350 = 1400ms.
        account_throttle_tight(),
        false,
    );
    let proxy = spawn_proxy(mgr).await;

    let last = burst_last_elapsed_uids(&proxy, "/v1/messages", &spread_uids(BURST)).await;
    assert!(
        last >= THROTTLED_FLOOR,
        "the fleet ceiling must pace a burst even when it is SPREAD across orgs — \
         that is the whole point of having a ceiling above the per-org buckets. \
         Last request completed in {last:?}, expected >= {THROTTLED_FLOOR:?}"
    );
}

/// Both buckets inert (the `"accountThrottle": {}` + `"fleetThrottle": {}`
/// escape hatch) means genuinely no pacing. This is the control for the two
/// floor-asserting tests above: without it, they would still pass if the proxy
/// were slow for some reason entirely unrelated to throttling.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn both_buckets_off_is_unpaced() {
    let (upstream, _hits) = spawn_json_upstream().await;
    let mgr = manager(&upstream, throttle_off(), throttle_off(), false);
    let proxy = spawn_proxy(mgr).await;

    let last = burst_last_elapsed_uids(&proxy, "/v1/messages", &same_uids(BURST)).await;
    assert!(
        last <= EXEMPT_CEILING,
        "with both buckets inert nothing may pace a burst, not even one \
         concentrated on a single org — last request completed in {last:?}, \
         expected <= {EXEMPT_CEILING:?}"
    );
}

// ---------------------------------------------------------------------------
// The SHIPPED tuning, both buckets live at once.
//
// Every other test in this file either disables one bucket or sets the ceiling
// to `burst: 1024`, which is deliberate — it makes each timing assertion
// attributable to one mechanism. The cost is that the composition that actually
// runs in production is never exercised. These close that gap.
// ---------------------------------------------------------------------------

/// Honest documentation, in executable form, of what shipping `burst: 8` does to
/// a burst concentrated on ONE organization: it passes untaxed.
///
/// This is a REAL loosening of instantaneous per-identity burst, 4 -> 8. Under
/// the pre-split fleet-wide bucket this same 8-wide burst paid
/// `(8-4)*350 = 1400ms`. Sustained per-identity rate is unchanged (one send per
/// 350ms either way) — only the burst budget grew, deliberately, so a single
/// Claude Code turn (inference + `count_tokens` + telemetry, ~4-6 requests) is
/// never taxed mid-turn.
///
/// If someone later reverts the burst to 4, this test SHOULD fail. That is the
/// point: the loosening is a decision on the record, not an accident.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shipped_tuning_passes_one_turn_untaxed() {
    let (upstream, _hits, served) = spawn_json_upstream_recording().await;
    let mgr = manager(
        &upstream,
        account_throttle_shipped(),
        fleet_throttle_shipped(),
        false,
    );
    let proxy = spawn_proxy(mgr).await;

    let last = burst_last_elapsed_uids(&proxy, "/v1/messages", &same_uids(BURST)).await;

    let distinct = distinct_accounts_since(&served, 0);
    assert_eq!(distinct, 1, "premise: this burst must land on one org");

    assert!(
        last <= EXEMPT_CEILING,
        "at the SHIPPED burst of 8, an {BURST}-wide burst on one org is untaxed \
         (it cost 1400ms before the split). Last request completed in {last:?}, \
         expected <= {EXEMPT_CEILING:?}"
    );
}

/// ...and beyond one turn's worth, the shipped per-org bucket still paces.
///
/// This is the regression guard that `concentrated_burst_on_one_org_is_still_paced`
/// cannot be: that one dials burst back to 4 to reproduce the old closed form, so
/// it proves the MECHANISM paces without proving the SHIPPED tuning does. This
/// runs the real numbers: 12 concurrent on one org must pay `(12-8)*350 = 1400ms`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shipped_tuning_still_paces_beyond_one_turn() {
    const WIDE: usize = 12;
    let (upstream, _hits, served) = spawn_json_upstream_recording().await;
    let mgr = manager(
        &upstream,
        account_throttle_shipped(),
        fleet_throttle_shipped(),
        false,
    );
    let proxy = spawn_proxy(mgr).await;

    let last = burst_last_elapsed_uids(&proxy, "/v1/messages", &same_uids(WIDE)).await;

    let distinct = distinct_accounts_since(&served, 0);
    assert_eq!(distinct, 1, "premise: this burst must land on one org");

    assert!(
        last >= THROTTLED_FLOOR,
        "at the SHIPPED tuning a {WIDE}-wide burst on ONE org must still pay \
         (12-8)*350 = 1400ms — per-identity pacing is loosened, not removed. \
         Last request completed in {last:?}, expected >= {THROTTLED_FLOOR:?}"
    );
}

/// The shipped fleet ceiling must be able to FIRE, and this test must be able to
/// TELL that it fired. Gil's call: a ceiling set so far above traffic that it can
/// never bind is decoration, not insurance.
///
/// Per-org is off so only the ceiling can pace this, and the burst is spread so no
/// single org bucket would bind anyway. At `burst: 16` a 32-wide runaway must pay
/// `(32-16)*100 = 1600ms`.
///
/// **The floor is set to discriminate, not merely to be exceeded.** An earlier
/// version fired 24 wide and asserted `>= 500ms`, which passed at `burst: 24` too
/// — i.e. it would have gone green with the ceiling that CANNOT fire, measuring
/// concurrency overhead rather than pacing. 32 wide against a 1600ms closed form
/// leaves the old value (`(32-24)*100 = 800ms`) below the floor, so the two are
/// actually distinguishable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shipped_fleet_ceiling_can_actually_fire() {
    const RUNAWAY: usize = 32;
    let (upstream, _hits, served) = spawn_json_upstream_recording().await;
    let mgr = manager(&upstream, throttle_off(), fleet_throttle_shipped(), false);
    let proxy = spawn_proxy(mgr).await;

    let last = burst_last_elapsed_uids(&proxy, "/v1/messages", &spread_uids(RUNAWAY)).await;

    let distinct = distinct_accounts_since(&served, 0);
    assert!(
        distinct >= 4,
        "premise: a runaway must be spread, or the per-org bucket would be the \
         thing under test; reached {distinct} account(s)"
    );

    assert!(
        last >= Duration::from_millis(1_200),
        "the shipped fleet ceiling must engage on a {RUNAWAY}-wide runaway — \
         (32-16)*100 = 1600ms. A ceiling that cannot fire is indistinguishable \
         from an absent one. Last request completed in {last:?}"
    );
}
