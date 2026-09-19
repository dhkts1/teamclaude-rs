//! **A node learns it may borrow when the peers file changes, not at boot.**
//!
//! `tcr peer allow <peer> disclose on` says the grant is in effect now, because
//! the peer listener re-reads the peers file whenever its mtime moves. The
//! borrowing side did not keep that promise: the peer-lease provider was read
//! out of the peers file once, on the way up, and a node that booted with
//! nobody to borrow from answered every later request with the exhausted 429
//! however many peers it was then paired with. Only a restart changed it.
//!
//! This file is that story as one test, from outside the process that has to
//! keep the promise.
//!
//! # Its own test binary, deliberately
//!
//! `fallback::PROVIDER` is process-wide and installs once, so a test that
//! installs one decides the answer for every other test sharing its binary.
//! This file holds exactly one test for that reason, and any test added here
//! has to be read against that constraint first.
//!
//! # Nothing here touches the live proxy or the operator's own files
//!
//! The proxy binds `127.0.0.1:0`, so the kernel picks a port and it cannot be
//! `3456`. The config and the peers file are in a fresh `tempfile::tempdir`,
//! the peers file names no `listen` address so no peer port is opened, and no
//! account is configured, so nothing here can reach Anthropic or spend quota.
//! The one other socket is a stand-in lender on a kernel port that accepts a
//! connection and closes it.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use teamclaude_rs::config::Config;
use teamclaude_rs::peer::config as peer_config;
use teamclaude_rs::server::{serve, IncumbentPolicy, ServeOptions, ServeOutcome, TlsSetup};
use teamclaude_rs::singleton::ProxyHost;

/// The port `tcr` uses by default. Named so the assertion below says what it is
/// protecting rather than showing a bare number.
const LIVE_PROXY_PORT: u16 = 3456;

/// An obviously fake node id for the stand-in lender.
const LENDER_NODE: tcr_peer_wire::PeerId = tcr_peer_wire::PeerId([9_u8; 32]);

/// A peers file holding one pinned peer, with `disclose` either granted or not.
///
/// `disclose` is the whole difference between the two states this test moves
/// between: it is what says "this peer may read my requests in full", which is
/// what makes THIS node able to borrow from it. The address is the other half,
/// and both rows carry it, so the only thing that changes between the boot read
/// and the re-read is the grant.
fn peers_file(lender_addr: std::net::SocketAddr, disclose: bool) -> peer_config::PeerFile {
    peer_config::PeerFile {
        peers: vec![peer_config::PeerRow {
            node: LENDER_NODE,
            label: "lending-mac".to_string(),
            endpoints: vec![peer_config::Endpoint::direct(
                lender_addr,
                0,
                peer_config::EndpointSource::Paired,
            )],
            added_at: 0,
            rendezvous_secret: None,
            sees_us_at: None,
            allow: peer_config::Allow {
                relay: false,
                gateway: false,
                carry: false,
                inspect: false,
                allow_disclose: disclose,
                accept_move: false,
                control: peer_config::ControlGrants::default(),
            },
            lend: Vec::new(),
        }],
        ..peer_config::PeerFile::default()
    }
}

/// No accounts, an ephemeral port, both timer loops off.
fn test_config() -> Config {
    serde_json::from_str(
        r#"{
            "proxy": { "port": 0 },
            "quotaProbeSeconds": 0,
            "warmupSeconds": 0,
            "accounts": []
        }"#,
    )
    .expect("the inline test config parses")
}

fn options(config_path: std::path::PathBuf) -> ServeOptions {
    ServeOptions {
        config: test_config(),
        // What resolves the peers file beside it: see
        // `server::peers_file_beside_config`.
        persist_path: Some(config_path),
        port: Some(0),
        // The only policy that signals nothing. A test that could reach
        // `takeover_port` could SIGKILL the developer's live proxy.
        incumbent: IncumbentPolicy::never_signal(),
        affinity_path: None,
        wire_sessions_path: None,
        usage_dir: None,
        // Loading the MITM material mints or reads a CA on disk.
        tls: TlsSetup::Disabled,
        host: ProxyHost::Cli,
        owner_dir: None,
        inherited_listener: None,
    }
}

/// **A grant that lands after boot is honoured without a restart.**
///
/// The instrument is a socket, not a log line and not the `OnceLock`: the
/// provider's first act on a dry-fleet ask is to dial the lender's address, so
/// a stand-in lender that accepts and hangs up is proof that the dry-fleet arm
/// reached a provider and that the provider read the peers file. It hangs up
/// rather than answering, which fails the borrower's handshake and returns the
/// request to the last rung of the ladder, the 429 this proxy always had. What
/// is being proved here is that another Mac was asked at all.
///
/// Three moments, in order:
///
/// 1. booted beside a peers file whose only row has `disclose` OFF, no
///    provider is installed. This is also the positive control for step 3: it
///    says the provider that exists at the end was not there at the start, so
///    the dial cannot be an artefact of the boot read;
/// 2. the peers file gains the grant, with nothing restarted and nothing
///    signalled. This is what `tcr peer allow <peer> disclose on` writes;
/// 3. one request at a fleet with no account: the stand-in lender is dialled,
///    and the installed provider is the peer-lease one.
///
/// Watched red against the code before the late install: step 3 fails with no
/// connection at the stand-in lender and `configured_provider()` still `None`,
/// which is the reported defect exactly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_disclose_grant_written_after_boot_is_borrowed_on_without_a_restart() {
    let home = tempfile::tempdir().expect("a temp home");
    let config_path = home.path().join("teamclaude.json");
    std::fs::write(&config_path, br#"{"accounts": []}"#).expect("the temp config writes");
    let peers_path = home.path().join("tcr-peers.json");

    // The stand-in lender: a socket that accepts, counts, and closes.
    let lender = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the stand-in lender");
    let lender_addr = lender.local_addr().expect("the stand-in lender's addr");
    let dialled = Arc::new(AtomicUsize::new(0));
    let counter = dialled.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = lender.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            drop(stream);
        }
    });

    // Moment 1: a pinned peer, an address, and no disclosure.
    peer_config::save(&peers_path, &peers_file(lender_addr, false))
        .expect("the temp peers file writes");

    let ServeOutcome::Started(mut handle) = serve(options(config_path))
        .await
        .expect("the library boots the proxy")
    else {
        panic!("this test may not run against an incumbent proxy");
    };
    assert_ne!(handle.addr().port(), LIVE_PROXY_PORT);
    assert!(
        teamclaude_rs::fallback::configured_provider().is_none(),
        "a peers file whose only row may not be disclosed to describes nobody to borrow from, \
         so the boot read must install no provider"
    );
    assert_eq!(
        dialled.load(Ordering::SeqCst),
        0,
        "nothing may have been dialled before the grant"
    );

    // Moment 2: the grant, written while the proxy runs. The pause is for the
    // mtime and nothing else: the re-read is gated on the file's mtime moving,
    // and a write inside the same clock tick as the boot read is a write the
    // stat cannot see.
    tokio::time::sleep(Duration::from_millis(20)).await;
    peer_config::save(&peers_path, &peers_file(lender_addr, true))
        .expect("the grant writes into the peers file");

    // Moment 3: one POST at a fleet with no account, which is the only
    // condition under which a provider is consulted at all.
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("a loopback client");
    let answered = client
        .post(format!("http://{}/v1/messages", handle.addr()))
        .header("content-type", "application/json")
        .body(r#"{"model":"claude-sonnet-4-5","messages":[]}"#)
        .send()
        .await
        .expect("the proxy answered");
    assert_eq!(
        answered.status().as_u16(),
        429,
        "a stand-in lender that hangs up cannot serve the request, so the ladder still ends at \
         the honest 429"
    );

    assert_eq!(
        dialled.load(Ordering::SeqCst),
        1,
        "the request after the grant must have dialled the lender, with no restart in between: \
         this is the whole claim"
    );
    let provider = teamclaude_rs::fallback::configured_provider()
        .expect("the grant installed a provider after boot");
    assert_eq!(
        provider.name(),
        "peer-lease",
        "the provider installed after boot is the peer-lease one"
    );

    handle.shutdown().await;
}
