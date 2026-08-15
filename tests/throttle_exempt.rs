//! Behavioural coverage for the `throttleExemptNoise` knob
//! (`Manager::throttle_exempt_noise_enabled`, `src/manager/throttle.rs`).
//!
//! Structure mirrors `tests/hop_overhead.rs::throttle_burst_cost`: a real
//! `mitm::serve` listener in front of a real `Manager`, a fake upstream that
//! counts hits, and a concurrent burst measured end-to-end. Unlike that file
//! these are ASSERTIONS, not timing measurements — every test uses a
//! deliberately wide margin (the throttle's own spacing is 350ms and this
//! never asserts anything finer than 250ms either side of it), so these are
//! NOT `#[ignore]`d.
//!
//! Three behaviours, one per test:
//!
//! 1. Knob OFF (default) — a burst of `Noise`-classified requests
//!    (`/api/event_logging/v2/batch`) still pays the fleet-wide GCRA, exactly
//!    as before this change. Current behaviour preserved.
//! 2. Knob ON — the SAME burst of `Noise` requests skips the GCRA entirely:
//!    all admit near-instantly instead of queueing behind `burst=4`.
//! 3. Knob ON, query-string trap — `/v1/messages?beta=true` (the shape live
//!    traffic actually sends) must still classify as `Inference`, not
//!    `Noise`: a burst against it is throttled exactly like case 1, proving
//!    `classify_request` is being fed `uri.path()` and not the raw target.

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
// Stubs — nothing here ever reaches the network. Verbatim from hop_overhead.rs.
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

fn account(i: usize) -> Account {
    Account {
        name: format!("acct{i}"),
        account_type: "oauth".to_string(),
        account_uuid: Some(fake_uuid(i)),
        org_uuid: None,
        org_name: None,
        access_token: format!("at-fake-{i}"),
        refresh_token: Some(format!("rt-fake-{i}")),
        expires_at: Some(teamclaude_rs::now_ms() + 3_600_000),
        priority: Some(i as i64),
        switch_threshold: None,
        disabled: None,
        extra: serde_json::Map::new(),
    }
}

/// Mirrors `hop_overhead.rs::config`, plus the knob under test in `extra`.
fn config(upstream: &str, throttle: ThrottleConfig, throttle_exempt_noise: bool) -> Config {
    let mut extra = serde_json::Map::new();
    extra.insert("sessionAffinity".to_string(), serde_json::Value::Bool(true));
    extra.insert(
        "throttleExemptNoise".to_string(),
        serde_json::Value::Bool(throttle_exempt_noise),
    );
    Config {
        proxy: ProxyConfig {
            port: 0,
            api_key: None,
            extra: serde_json::Map::new(),
        },
        upstream: upstream.to_string(),
        switch_threshold: 0.98,
        pacing: PacingConfig::default(),
        throttle,
        lock_account: None,
        control_account: None,
        control_reserve: 0.05,
        http1_only: false,
        accounts: (0..POOL).map(account).collect(),
        extra,
    }
}

fn manager(upstream: &str, throttle: ThrottleConfig, throttle_exempt_noise: bool) -> Arc<Manager> {
    Manager::new(
        config(upstream, throttle, throttle_exempt_noise),
        Arc::new(NeverRefreshes),
        Arc::new(NeverProbes),
        Arc::new(NeverWarms),
        None,
    )
}

/// The LIVE throttle setting from `~/.config/teamclaude.json` — 4 admit
/// instantly, then one per 350ms.
fn throttle_live() -> ThrottleConfig {
    ThrottleConfig {
        min_spacing_ms: Some(350),
        burst: Some(4),
    }
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

async fn spawn_json_upstream() -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let app = Router::new().fallback(any(move |req: axum::extract::Request| {
        let counter = counter.clone();
        async move {
            let _ = axum::body::to_bytes(req.into_body(), 256 * 1024 * 1024).await;
            counter.fetch_add(1, Ordering::SeqCst);
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
    (format!("http://{addr}"), hits)
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

/// Fires `BURST` concurrent POSTs to `path` and returns the wall-clock elapsed
/// for the LAST one to complete, relative to a shared start instant. Warms the
/// route with one request first (route/pool build), then sleeps long enough
/// for the GCRA bucket to fully refill (`burst * spacing` = 1.4s) before the
/// timed burst, exactly as `hop_overhead.rs::throttle_burst_cost` does.
async fn burst_last_elapsed(proxy: &str, path: &str, n: usize) -> Duration {
    let cli = client();
    let (s, _) = post(&cli, proxy, path, &body("user_warmup")).await;
    assert_eq!(s, 200, "warmup request must serve 200");
    tokio::time::sleep(Duration::from_millis(2_000)).await;

    let start = Instant::now();
    let mut handles = Vec::with_capacity(n);
    for i in 0..n {
        let cli = client();
        let proxy = proxy.to_string();
        let path = path.to_string();
        let payload = body(&format!("user_burst_{i}"));
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

const BURST: usize = 8;
/// `(BURST - burst) * spacing_ms` = `(8 - 4) * 350` = 1400ms — the minimum
/// wall-clock a fully-throttled burst of 8 must take for its last request.
/// Asserted with a wide margin below the closed form so scheduler jitter on a
/// loaded CI box cannot flake it.
const THROTTLED_FLOOR: Duration = Duration::from_millis(900);
/// Upper bound for a burst that skips the GCRA entirely — no request should
/// ever wait anywhere near one throttle tick.
const EXEMPT_CEILING: Duration = Duration::from_millis(300);

/// Case 1: knob OFF (default) — `Noise` traffic still pays the throttle.
/// Current behaviour preserved.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn noise_burst_throttled_when_knob_off() {
    let (upstream, _hits) = spawn_json_upstream().await;
    let mgr = manager(&upstream, throttle_live(), false);
    let proxy = spawn_proxy(mgr).await;

    let last = burst_last_elapsed(&proxy, "/api/event_logging/v2/batch", BURST).await;
    assert!(
        last >= THROTTLED_FLOOR,
        "knob OFF: a Noise burst must still pay the fleet-wide GCRA — \
         last request completed in {last:?}, expected >= {THROTTLED_FLOOR:?}"
    );
}

/// Case 2: knob ON — the SAME `Noise` burst skips the GCRA entirely.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn noise_burst_exempt_when_knob_on() {
    let (upstream, _hits) = spawn_json_upstream().await;
    let mgr = manager(&upstream, throttle_live(), true);
    let proxy = spawn_proxy(mgr).await;

    let last = burst_last_elapsed(&proxy, "/api/event_logging/v2/batch", BURST).await;
    assert!(
        last <= EXEMPT_CEILING,
        "knob ON: a Noise burst must skip the GCRA — \
         last request completed in {last:?}, expected <= {EXEMPT_CEILING:?}"
    );
}

/// Case 3: knob ON, query-string trap. `/v1/messages?beta=true` — the shape
/// live traffic sends — must classify as `Inference`, never `Noise`, so a
/// burst against it is throttled exactly like case 1 even with the knob on.
/// This is the test that catches passing the raw target instead of
/// `uri.path()` into `classify_request`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inference_query_string_not_exempt_when_knob_on() {
    let (upstream, _hits) = spawn_json_upstream().await;
    let mgr = manager(&upstream, throttle_live(), true);
    let proxy = spawn_proxy(mgr).await;

    let last = burst_last_elapsed(&proxy, "/v1/messages?beta=true", BURST).await;
    assert!(
        last >= THROTTLED_FLOOR,
        "knob ON: `/v1/messages?beta=true` must still classify as Inference \
         and pay the GCRA — last request completed in {last:?}, expected >= {THROTTLED_FLOOR:?} \
         (a failure here means the raw target, not uri.path(), reached classify_request)"
    );
}
