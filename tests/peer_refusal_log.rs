//! The AUTHENTICATED refusal log is bounded like the
//! unauthenticated one.
//!
//! # Why this is its own test binary
//!
//! It installs a process-wide `tracing` subscriber
//! (`set_global_default`), and a subscriber is shared by every test in a
//! binary: so a `matching("peer connection closed")` count here would include
//! the lines other tests' listeners wrote, concurrently, about their own
//! peers. Measured while writing it: run inside `tests/peer_pairing.rs` this
//! counted THREE lines from three different source ports, none of which this
//! test produced. A count that includes another test's events is not a
//! measurement of the bound.
//!
//! Everything binds `127.0.0.1:0` with temp files at 0600 and reads no real
//! config. Nothing here touches the proxy on `127.0.0.1:3456`.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tcr_peer_wire::PeerId;
use teamclaude_rs::peer::config::{self, PeerFile};
use teamclaude_rs::peer::id::NodeKey;
use teamclaude_rs::peer::listener::{self, SessionContext};
use teamclaude_rs::peer::noise::{self, Handshake};
use tokio::net::TcpStream;

/// A scratch directory named after this process and thread, so five lanes
/// running at once never collide on one path.
fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tcr-peer-refusal-log-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the scratch dir");
    dir
}

/// Bind `127.0.0.1:0` and start the SHIPPED accept loop on it.
async fn serve(context: SessionContext) -> SocketAddr {
    let listener = listener::bind("127.0.0.1:0".parse().expect("a loopback address"))
        .await
        .expect("bind a kernel-chosen port on loopback");
    let addr = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let _ = listener::serve_on_with(listener, context).await;
    });
    addr
}

// ---------------------------------------------------------------------------
// Item 5: the authenticated refusal log is bounded too
// ---------------------------------------------------------------------------

/// One captured tracing event.
#[derive(Debug, Clone)]
struct CapturedEvent {
    message: String,
    fields: BTreeMap<String, String>,
}

#[derive(Clone, Default)]
struct CaptureLayer {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

struct FieldVisitor(BTreeMap<String, String>);

impl tracing::field::Visit for FieldVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_string(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CaptureLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = FieldVisitor(BTreeMap::new());
        event.record(&mut visitor);
        let message = visitor
            .0
            .get("message")
            .cloned()
            .unwrap_or_else(|| "<no message field>".to_string());
        self.events
            .lock()
            .expect("capture lock")
            .push(CapturedEvent {
                message,
                fields: visitor.0,
            });
    }
}

impl CaptureLayer {
    fn matching(&self, needle: &str) -> Vec<CapturedEvent> {
        self.events
            .lock()
            .expect("capture lock")
            .iter()
            .filter(|event| event.message.contains(needle))
            .cloned()
            .collect()
    }
}

/// **A pinned peer in a reconnect loop writes ONE log line, not one per
/// attempt.**
///
/// This used to log every AUTHENTICATED failure unconditionally, on the
/// reasoning that a pinned peer's stream ending is always worth a line. That is
/// true of the first one and false of the thousandth: a peer looping is a peer
/// filling a disk, and `tcr peer forget` is not the remedy an operator reaches
/// for when the log is what is broken.
///
/// Twenty authenticated failures in a row, from a pinned peer that completes
/// its `IK` handshake and then sends a frame that is not a stream header. The
/// assertion is on the number of `peer connection closed` lines, and the
/// suppressed count on the one line that IS written is what makes the bound
/// honest rather than a silence.
///
/// This test owns the process-wide tracing subscriber, so it is the only
/// log-capture test in this file.
///
/// Watched red: restore the unconditional `tracing::warn!` on the
/// `failure.authenticated` arm of `serve_on_with` and this fails with 20 lines.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_authenticated_reconnect_loop_writes_one_log_line() {
    let capture = CaptureLayer::default();
    {
        use tracing_subscriber::layer::SubscriberExt as _;
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        tracing::subscriber::set_global_default(subscriber)
            .expect("no other default is set in this test binary");
    }

    let dir = scratch("auth-refusal-log");
    let peers = dir.join("tcr-peers.json");
    let state = dir.join("peer-state.json");
    let (peer_secret, peer_public) = noise::generate_static().expect("a peer keypair");
    let key = NodeKey::load_or_mint(&dir).expect("mint a node key");
    config::save(
        &peers,
        &PeerFile {
            peers: vec![config::PeerRow {
                node: PeerId(peer_public),
                label: "laptop-2".to_string(),
                endpoints: Vec::new(),
                added_at: 0,
                rendezvous_secret: None,
                sees_us_at: None,
                allow: config::Allow::default(),
                lend: Vec::new(),
            }],
            ..PeerFile::default()
        },
    )
    .expect("write the peers file");

    let responder_public = key.id().0;
    let addr = serve(SessionContext::new(&key, &peers, &state)).await;

    const ATTEMPTS: usize = 20;
    for _ in 0..ATTEMPTS {
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        let mut session = noise::dial_handshake(
            &mut stream,
            &peer_secret,
            Handshake::Return,
            Some(&responder_public),
            None,
        )
        .await
        .expect("a pinned peer authenticates");
        // Authenticated, and then a frame that is not a stream header: the
        // listener closes the connection and used to log it.
        noise::send_encrypted(&mut stream, &mut session.transport, b"not a header")
            .await
            .expect("the frame is written");
        // Give the listener's task time to run its refusal path before the next
        // attempt, so the count below is over twenty decisions and not one.
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let lines = capture.matching("peer connection closed");
    assert_eq!(
        lines.len(),
        1,
        "a pinned peer failing {ATTEMPTS} times must write ONE line, and it wrote {}: {:?}",
        lines.len(),
        lines
            .iter()
            .map(|event| event.fields.clone())
            .collect::<Vec<_>>()
    );
    // The bound must not turn a flood into an absence of evidence, so the one
    // line carries a suppressed count. It is 0 on the FIRST line by
    // construction (nothing had been silenced before it), and what proves the
    // counting works is the field being present at all plus
    // `an_unauthenticated_flood_writes_one_log_line_and_then_counts` in
    // `tests/peer_noise.rs`, which drives the count directly.
    assert!(
        lines[0].fields.contains_key("suppressed"),
        "the line has to carry what it stood for, or the bound is a silence: {:?}",
        lines[0].fields
    );
}
