//! The proxy booted as a **library**, with no `tcr` process anywhere.
//!
//! This is the gate for the `run_server` → [`teamclaude_rs::server::serve`]
//! extraction. Before it, starting the proxy required `main`: the boot path
//! called `std::process::exit` in its middle and handed its background tasks to
//! "the process is about to die anyway", so nothing but the binary could run one
//! and nothing at all could stop one. The claims under test are exactly those
//! two — that a caller can bind, serve a real request, and then get every task
//! it started back *stopped*.
//!
//! # Port 0, never 3456
//!
//! A real proxy is very likely serving on the configured port on this machine,
//! and binding it here would fight it (and a takeover wipes its session→account
//! pin map, the most expensive event in this system). Every test in this file
//! binds port 0 — the kernel's free ephemeral port — and asserts it did not land
//! on the configured default. Nothing here signals, restarts or probes a live
//! proxy.

use std::time::Duration;

use teamclaude_rs::config::Config;
use teamclaude_rs::proxy::STATUS_PATH;
use teamclaude_rs::server::{serve, ServeOptions, TlsSetup};

/// The port `tcr` uses by default. Named here so the assertion below says what
/// it is protecting rather than showing a bare number.
const LIVE_PROXY_PORT: u16 = 3456;

/// A config that binds an ephemeral port and enables exactly the two background
/// loops this file counts: the session-affinity flusher and the quota prober.
/// `warmupSeconds` stays 0, so the keep-warm loop is not spawned at all (it
/// spends real quota; it ships dark).
///
/// No accounts: the rotation never has anything to pick, which is deliberate —
/// nothing in this file may reach Anthropic.
fn test_config() -> Config {
    serde_json::from_str(
        r#"{
            "proxy": { "port": 0 },
            "sessionAffinity": true,
            "quotaProbeSeconds": 3600,
            "warmupSeconds": 0,
            "accounts": []
        }"#,
    )
    .expect("the inline test config parses")
}

/// A disposable path for the affinity pin cache. The real one is a live cache
/// belonging to the running proxy and must never be written by a test.
fn scratch_affinity_path(tag: &str) -> std::path::PathBuf {
    let unique = format!(
        "tcr-serve-test-{}-{tag}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    );
    std::env::temp_dir().join(unique).join("affinity.json")
}

fn options(tag: &str) -> ServeOptions {
    ServeOptions {
        config: test_config(),
        // No persist path: this test may not write a config file anywhere.
        persist_path: None,
        // Belt and braces with the config's own 0 — whichever is read, it is not
        // the live proxy's port.
        port: Some(0),
        // NEVER true here: `--replace` is what signals an incumbent.
        replace: false,
        affinity_path: scratch_affinity_path(tag),
        // Loading the MITM material mints/reads a CA on disk. Base-URL mode is
        // all this file exercises, so do not touch it.
        tls: TlsSetup::Disabled,
    }
}

/// Is anything still accepting on `port`? Polled, because the listener is closed
/// asynchronously with the accept task returning.
async fn port_refuses_within(port: u16, budget: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
            Err(_) => return true,
            Ok(_) if tokio::time::Instant::now() >= deadline => return false,
            Ok(_) => tokio::time::sleep(Duration::from_millis(25)).await,
        }
    }
}

/// THE PHASE 3 GATE: bind, serve a real request, shut down, and prove the tasks
/// stopped — all without a `tcr` process.
///
/// The request is `GET /_tcr/status`, which the proxy answers itself: it is the
/// one route that never forwards upstream, so a real HTTP round trip crosses the
/// real hybrid listener (peek → base-URL → router) and reaches the real handler
/// without any network egress. With no `proxy.apiKey` configured the endpoint's
/// key gate does not apply, and the loopback origin it does require is proven by
/// the `ClientAddr` the listener injects — so a 200 here is also evidence the
/// connection went through the production accept path and not some shortcut.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_library_can_start_serve_and_stop_the_proxy_with_no_binary() {
    let handle = serve(options("gate"))
        .await
        .expect("binding an ephemeral port cannot fail")
        .expect_started();

    let addr = handle.addr();
    assert_ne!(addr.port(), 0, "the bound port must be resolved, not 0");
    assert_ne!(
        addr.port(),
        LIVE_PROXY_PORT,
        "a test must never bind the port a live proxy serves on"
    );
    assert!(addr.ip().is_loopback(), "the proxy binds loopback only");

    // The two loops this config enables: affinity flush + quota probe. The
    // keep-warm loop is off, so it must NOT have been spawned.
    assert_eq!(
        handle.background_task_count(),
        2,
        "expected the affinity flusher and the quota prober, and no keep-warm loop"
    );

    // One real request, over a real socket, through the real listener.
    let client = reqwest::Client::builder()
        // HTTP_PROXY very commonly points AT tcr; routing this through it would
        // send the request to the live proxy instead of ours.
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("http client");
    let response = client
        .get(format!("http://{addr}{STATUS_PATH}"))
        .send()
        .await
        .expect("the library-started proxy answered nothing");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::OK,
        "the status route is answered locally by the proxy itself"
    );
    let body: serde_json::Value = response.json().await.expect("a JSON status payload");
    assert_eq!(
        body["kind"],
        teamclaude_rs::status::STATUS_KIND,
        "the payload came from this proxy's own status handler: {body}"
    );

    // Shutdown must COMPLETE, not merely be called: every task is joined here,
    // so a loop that ignored the shutdown signal hangs this await and the
    // timeout fails the test. That is the assertion — `shutdown()` returning is
    // itself the proof the tasks ended.
    let report = tokio::time::timeout(Duration::from_secs(10), handle.shutdown())
        .await
        .expect("shutdown did not finish: a background task ignored the shutdown signal");
    assert_eq!(
        report.tasks_joined, 3,
        "the accept loop plus both background loops must be joined"
    );
    assert_eq!(
        report.affinity_pins_written,
        Some(0),
        "a clean shutdown owes the next boot its final pin write, even when empty"
    );

    // Independent of our own bookkeeping: the accept loop really is gone, so the
    // port refuses. (`shutdown` drops the listener with the loop.)
    assert!(
        port_refuses_within(addr.port(), Duration::from_secs(5)).await,
        "something is still accepting on :{} after shutdown",
        addr.port()
    );
}

/// A library caller that simply DROPS the handle must not leak the accept loop.
///
/// `run_server` never needed this — the process exited — which is exactly why it
/// could not be embedded. Dropping is the failure mode an embedder hits first
/// (an early return, a panic, a `?`), so it gets its own test.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_handle_stops_the_accept_loop() {
    let handle = serve(options("drop"))
        .await
        .expect("binding an ephemeral port cannot fail")
        .expect_started();
    let port = handle.addr().port();
    assert_ne!(port, LIVE_PROXY_PORT);

    // It is serving right now.
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok(),
        "the proxy should be accepting before the handle is dropped"
    );

    drop(handle);

    assert!(
        port_refuses_within(port, Duration::from_secs(5)).await,
        "the accept loop outlived its handle: :{port} is still accepting"
    );
}
