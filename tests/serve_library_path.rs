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
use teamclaude_rs::server::{serve, AffinityFlush, IncumbentPolicy, ServeOptions, TlsSetup};
use teamclaude_rs::singleton::{ProxyHost, ProxyOwner};

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

/// A disposable DIRECTORY for the port claim, for the same reason as the pin
/// cache above: the real claim lives beside the live proxy's pin cache, is named
/// after its port, and `tcr login` reads it. The file name inside is `serve`'s to
/// choose, never a caller's.
fn scratch_owner_dir(tag: &str) -> std::path::PathBuf {
    let dir = scratch_affinity_path(tag)
        .parent()
        .expect("the scratch affinity path has a parent directory")
        .to_path_buf();
    std::fs::create_dir_all(&dir).expect("a scratch dir under the temp dir is creatable");
    dir
}

fn options(tag: &str) -> ServeOptions {
    ServeOptions {
        config: test_config(),
        // No persist path: this test may not write a config file anywhere.
        persist_path: None,
        // Belt and braces with the config's own 0 — whichever is read, it is not
        // the live proxy's port.
        port: Some(0),
        // The default, and the only policy that signals nothing: a test that
        // could reach `takeover_port` could SIGKILL the developer's live proxy.
        incumbent: IncumbentPolicy::never_signal(),
        affinity_path: Some(scratch_affinity_path(tag)),
        // In-memory usage only. The binary's ledger directory is shared, and a
        // test serving briefly must never append into the live proxy's day file
        // — the same hazard `affinity_path` is scratch-scoped for.
        usage_dir: None,
        // Loading the MITM material mints/reads a CA on disk. Base-URL mode is
        // all this file exercises, so do not touch it.
        tls: TlsSetup::Disabled,
        // A library caller, so the host is stated as such — but with no claim
        // directory this is recorded nowhere. The one test that DOES write a
        // claim overrides both fields together.
        host: ProxyHost::Cli,
        owner_dir: None,
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
    let mut handle = serve(options("gate"))
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
        report.tasks_aborted, 0,
        "no task should need the grace period's abort on a clean shutdown"
    );
    assert_eq!(
        report.affinity,
        AffinityFlush::Written(0),
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

/// A library caller that simply DROPS the handle must not leak the accept loop
/// **or any background loop**.
///
/// `run_server` never needed this — the process exited — which is exactly why it
/// could not be embedded. Dropping is the failure mode an embedder hits first
/// (an early return, a panic, a `?`), so it gets its own test.
///
/// # What this test can and cannot prove
///
/// The port refusing is NOT evidence about `impl Drop for ServerHandle`: the
/// handle owns the `watch::Sender`, so dropping it closes the channel and every
/// loop's `stop.changed()` returns `Err` — the accept loop would stop with the
/// `Drop` impl deleted entirely. What is added here is the *other* four fifths
/// of the claim: every spawned task holds an `Arc<Manager>` clone, so watching
/// the strong count fall back to the one this test holds proves the prober and
/// the flusher are gone too, not just the listener.
///
/// The `Drop` impl itself is pinned by the unit tests in `src/server.rs`, which
/// can hold a `Sender` clone (keeping the channel open) and so isolate `abort()`
/// as the only thing that can stop a task. Deleting `impl Drop` turns those red;
/// it cannot turn this one red, and pretending otherwise is what the first
/// version of this test did.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_handle_stops_the_accept_loop_and_every_background_loop() {
    let handle = serve(options("drop"))
        .await
        .expect("binding an ephemeral port cannot fail")
        .expect_started();
    let port = handle.addr().port();
    assert_ne!(port, LIVE_PROXY_PORT);
    assert_eq!(handle.background_task_count(), 2);

    // Every task `serve` spawned captured one of these.
    let manager = handle.manager().clone();
    let live_tasks = || std::sync::Arc::strong_count(&manager) - 1;
    assert!(
        live_tasks() >= 3,
        "expected the accept loop and both background loops to hold a manager clone, saw {}",
        live_tasks()
    );

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

    // Polled: aborting is asynchronous — the task's future (and with it the
    // manager clone) is dropped when the runtime next gets to it.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while live_tasks() > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        live_tasks(),
        0,
        "a task outlived the handle that owned it: {} manager clone(s) still held",
        live_tasks()
    );
}

/// The documented convenience constructor must not reach the binary's files.
///
/// `ServeOptions::new` used to point `affinity_path` at [the live proxy's shared
/// pin cache], so an embedder following the docs would atomically replace the
/// running proxy's session→account map with its own empty one at shutdown, and
/// the next boot would cold-start every session's prompt cache. Nothing guarded
/// it, because every test hand-built a scratch path instead.
///
/// Asserted as *values*, without serving: this test may not create a proxy that
/// could write anywhere.
///
/// [the live proxy's shared pin cache]: teamclaude_rs::affinity::default_path
#[test]
fn the_convenience_constructor_touches_nothing_outside_the_process() {
    let options = ServeOptions::new(test_config());

    assert_eq!(
        options.affinity_path, None,
        "ServeOptions::new must not adopt the live proxy's shared pin cache"
    );
    assert_eq!(
        options.persist_path, None,
        "ServeOptions::new must not adopt a config file to write back"
    );
    assert!(
        !options.incumbent.signals_anything(),
        "ServeOptions::new must not be able to signal whatever holds the port"
    );
    assert_eq!(
        options.owner_dir, None,
        "ServeOptions::new must not claim a port on disk; the real claim directory \
         is shared state holding the live proxy's own claim"
    );
}

/// THE PHASE 1 GATE, the half a unit test cannot reach: a **real** `serve` writes
/// its port claim after the bind and withdraws it on shutdown.
///
/// The claim is what makes a proxy identifiable when its process name is not
/// `tcr` — `teamclaude_rs::singleton` documents the silent OAuth token loss that
/// depends on it (`tcr login` stops refusing to run beside a live server, whose
/// next persist writes its boot-time single-use refresh tokens back over the fresh
/// ones). The file existing is therefore the behaviour, not an implementation
/// detail, and its CONTENTS are asserted field by field: a claim carrying the
/// wrong pid or port is ignored by every reader, which would look exactly like no
/// claim at all.
///
/// `host: Embedded` is used deliberately — that is the host the command-line
/// matcher cannot recognize, so it is the case where this file is the only
/// identity there is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serving_writes_the_port_claim_after_binding_and_withdraws_it_on_shutdown() {
    let owner_dir = scratch_owner_dir("claim");

    let mut handle = serve(ServeOptions {
        owner_dir: Some(owner_dir.clone()),
        host: ProxyHost::Embedded,
        ..options("claim")
    })
    .await
    .expect("binding an ephemeral port cannot fail")
    .expect_started();

    let addr = handle.addr();
    assert_ne!(
        addr.port(),
        LIVE_PROXY_PORT,
        "a test must never bind the port a live proxy serves on"
    );

    // THE NAME IS THE CONTRACT, and this is the case that used to break it: the
    // caller asked for port 0, so the port is only known after the bind. Every
    // reader resolves `proxy-owner-<port>.json` for the port it is asking about,
    // so a claim under any other name is consulted by nothing — silently. `serve`
    // is handed a directory and derives the name from the port it bound.
    let owner_path = teamclaude_rs::singleton::owner_path_in(&owner_dir, addr.port());
    assert!(
        owner_path.exists(),
        "the claim must be named after the port actually bound: {}",
        owner_path.display()
    );

    let written = std::fs::read_to_string(&owner_path).unwrap_or_else(|err| {
        panic!(
            "no port claim at {} after a successful bind: {err}",
            owner_path.display()
        )
    });
    let owner: ProxyOwner = serde_json::from_str(&written)
        .unwrap_or_else(|err| panic!("the claim is not a readable ProxyOwner: {err}: {written}"));
    assert_eq!(
        owner,
        ProxyOwner {
            pid: std::process::id(),
            port: addr.port(),
            sha: teamclaude_rs::build_info::SHA.to_string(),
            host: ProxyHost::Embedded,
        },
        "the claim must name THIS process, the port actually bound, this build and \
         the host the caller stated — a reader verifies pid and port before \
         believing any of it"
    );

    let report = tokio::time::timeout(Duration::from_secs(10), handle.shutdown())
        .await
        .expect("shutdown did not finish: a background task ignored the shutdown signal");
    assert_eq!(report.tasks_aborted, 0, "clean shutdown");

    assert!(
        !owner_path.exists(),
        "the claim must be withdrawn on shutdown; a proxy that has stopped \
         listening must not still be advertising the port: {}",
        owner_path.display()
    );
}

/// The default incumbent policy — what `..Default::default()`, a struct literal
/// with an omitted field, or a copied example gives you — signals nothing.
///
/// `replace: bool` made the SIGTERM-then-SIGKILL path one keystroke from every
/// caller; only a named constructor reaches it now.
#[test]
fn the_default_incumbent_policy_cannot_signal_anything() {
    assert!(!IncumbentPolicy::default().signals_anything());
    assert!(!IncumbentPolicy::never_signal().signals_anything());
    assert!(IncumbentPolicy::replace_legacy_js_only().signals_anything());
    assert!(IncumbentPolicy::kill_the_incumbent_proxy().signals_anything());
}
