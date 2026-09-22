//! No address is ever learned from a carrier's socket.
//!
//! # The shape of the risk
//!
//! `serve_stream` hands every CONTROL frame the socket it was accepted from,
//! `from: SocketAddr`. When the stream arrived through a real TCP connection
//! from the peer itself, that socket is evidence: its host is where the peer
//! actually is. When the frame instead travelled inside a carry, one Mac
//! forwarding for another, the accepted socket is the CARRIER's connection to
//! this node, and taking it as the peer's own address teaches this node to
//! aim a future punch, an observation register entry, or a reflexive
//! `observed_you_at` at a Mac that is not the peer at all.
//!
//! A carrier cannot stamp the inner header itself: a blind forward writes no
//! header of its own (`src/peer/tunnel.rs`'s module doc on that point). The
//! origin stamps `via`, and the origin is the only party that knows which
//! carrier it went through. So a non-empty inner `via` is this node's one
//! signal that the socket in front of it is not the peer's.
//!
//! # Driven over an in-memory duplex, not a socket
//!
//! `from` is the whole point of the test, and this box has one address
//! (`tests/peer_reach.rs`'s `offer_from` explains why a duplex stands in for
//! a socket for exactly this reason). Everything under the call is shipped
//! code: `listener::serve_accepted` is what the real accept loop calls.

use std::net::SocketAddr;
use std::time::Duration;

use tcr_peer_wire::{Caps, Control, Hello, PeerId, StreamHeader, StreamKind};
use teamclaude_rs::peer::config::{self as peer_config, PeerFile, PeerRow};
use teamclaude_rs::peer::id::NodeKey;
use teamclaude_rs::peer::listener::{self, SessionContext};
use teamclaude_rs::peer::noise::{self, Handshake};
use teamclaude_rs::peer::reach;
use teamclaude_rs::peer::serve;

/// A scratch directory named after this process, thread and a caller tag, so
/// two tests in this binary never collide on one path.
fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tcr-peer-carried-control-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the scratch dir");
    dir
}

/// The target: a pinned row for `peer` already on its file, so the ordinary
/// CONTROL gate admits the frame this test sends.
struct Target {
    peers: std::path::PathBuf,
    state: std::path::PathBuf,
    key: NodeKey,
}

impl Target {
    fn new(tag: &str, peer: PeerId) -> Self {
        let dir = scratch(tag);
        let peers = dir.join("tcr-peers.json");
        let state = dir.join("peer-state.json");
        let row = PeerRow {
            node: peer,
            label: "the-far-side".to_string(),
            endpoints: Vec::new(),
            added_at: 0,
            allow: peer_config::Allow::default(),
            lend: Vec::new(),
            rendezvous_secret: None,
            sees_us_at: None,
        };
        let file = PeerFile {
            peers: vec![row],
            ..PeerFile::default()
        };
        peer_config::save(&peers, &file).expect("write the peers file");
        let key = NodeKey::load_or_mint(&dir).expect("mint a node key");
        Self { peers, state, key }
    }

    fn context(&self) -> SessionContext {
        SessionContext::new(&self.key, &self.peers, &self.state)
    }

    fn file(&self) -> PeerFile {
        peer_config::read_or_default(&self.peers).expect("the peers file reads")
    }
}

/// Drive one CONTROL `Hello` at `target` over a duplex, as if it had been
/// accepted from `from`, and report the target's answering `Hello`.
///
/// `via` is the inner header's own `via`: empty for a direct arrival, naming
/// a carrier for a carried one. The exchange is otherwise the one
/// `tests/peer_probe.rs`'s `open_control` drives over a real socket:
/// `noise::dial_handshake` for the `IK` return, then `serve::send_control`
/// for the header and for the frame.
async fn hello_from(
    target: &Target,
    peer: &NodeKey,
    from: SocketAddr,
    via: Vec<PeerId>,
    addrs: Vec<String>,
) -> Hello {
    use tokio::time::timeout;

    let context = target.context();
    let (theirs, ours) = tokio::io::duplex(8192);
    let bind: SocketAddr = "0.0.0.0:7755".parse().expect("the literal parses");
    let served =
        tokio::spawn(async move { listener::serve_accepted(ours, &context, &from, bind).await });

    let mut theirs = theirs;
    let remote = target.key.id().0;
    let mut session = noise::dial_handshake(
        &mut theirs,
        peer.secret_bytes(),
        Handshake::Return,
        Some(&remote),
        None,
    )
    .await
    .expect("the IK return handshake completes against the pinned key");

    let header = StreamHeader {
        kind: StreamKind::Control,
        target: None,
        via,
        hops_remaining: 1,
        request_id: 7,
    };
    serve::send_control(&mut theirs, &mut session, &header)
        .await
        .expect("the header writes");

    let hello = Control::Hello(Hello {
        proto: tcr_peer_wire::PROTO_VERSION,
        node: peer.id(),
        label: "far-side".to_string(),
        seq: 1,
        caps: Caps::default(),
        addrs,
        ttl_s: 60,
        lendable: None,
        hops_to_egress: None,
        briefs: None,
        build_sha: None,
        boot_id: None,
        observed_you_at: None,
    });
    serve::send_control(&mut theirs, &mut session, &hello)
        .await
        .expect("the hello writes");

    let answer: Control = timeout(
        Duration::from_secs(5),
        serve::recv_control(&mut theirs, &mut session),
    )
    .await
    .expect("the target answers within the deadline")
    .expect("the answer decodes");
    drop(theirs);
    let _ = timeout(Duration::from_secs(5), served).await;

    let Control::Hello(answered) = answer else {
        panic!("the target answered a Hello with {answer:?}, not a Hello");
    };
    answered
}

/// **A carried Hello teaches nothing about where the peer is.**
///
/// The socket this exchange is served from is `203.0.113.9:41000`
/// (RFC 5737), which stands in for a carrier's own connection to this node.
/// `via` names a carrier, so nothing here belongs to the peer.
///
/// Red on the base with a message shaped like "a carried Hello taught the
/// carrier's socket as the peer's address": before this fix,
/// `reach::observed_peer_for` held `203.0.113.9:41000` for the peer, the row
/// gained an endpoint at `203.0.113.9:<port>`, and the answer's
/// `observed_you_at` named the carrier's socket back to the peer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn carried_hello_teaches_no_address() {
    let peer = NodeKey::load_or_mint(&scratch("carried-peer")).expect("mint the peer's key");
    let carrier = PeerId([0x77_u8; 32]);
    let target = Target::new("carried-target", peer.id());

    let carrier_socket: SocketAddr = "203.0.113.9:41000".parse().expect("the literal parses");
    // A wildcard bind: not itself dialable, so a correct combine adds nothing
    // for it, and a combine that wrongly paired it with the carrier's socket
    // host would add `203.0.113.9:<this port>`, which is exactly the wrong
    // answer this test exists to catch.
    let answered = hello_from(
        &target,
        &peer,
        carrier_socket,
        vec![carrier],
        vec!["0.0.0.0:9999".to_string()],
    )
    .await;

    assert_eq!(
        reach::observed_peer_for(&peer.id()),
        None,
        "a carried Hello taught the carrier's socket as the peer's address: recorded {:?}, \
         carrier {carrier_socket}",
        reach::observed_peer_for(&peer.id())
    );

    let row = target
        .file()
        .peers
        .into_iter()
        .find(|row| row.node == peer.id())
        .expect("the pinned row is still there");
    assert!(
        row.endpoints.is_empty(),
        "a carried Hello wrote an endpoint from the carrier's socket: {:?}",
        row.endpoints
    );

    assert_eq!(
        answered.observed_you_at, None,
        "a carried Hello was answered as if this node saw the peer at the carrier's socket: {:?}",
        answered.observed_you_at
    );
}

/// **The control this test stands on.** The identical exchange with an empty
/// `via`, so this fixture and its harness really do teach an address when
/// nothing is carried. Without this, a fix that learns nothing EVER would
/// pass `carried_hello_teaches_no_address` for the wrong reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_direct_hello_still_teaches_the_address() {
    let peer = NodeKey::load_or_mint(&scratch("direct-peer")).expect("mint the peer's key");
    let target = Target::new("direct-target", peer.id());

    let direct_socket: SocketAddr = "203.0.113.11:41000".parse().expect("the literal parses");
    // The same wildcard-bind announcement the carried test uses, so the two
    // tests differ only in `via`. The combine pairs this connection's host
    // with the announced port 9999, which is why the endpoint below is
    // `203.0.113.11:9999` and not `direct_socket` itself: `direct_socket`'s
    // own port is this one connection's ephemeral source port, never a port
    // to dial, direct arrival or not.
    let answered = hello_from(
        &target,
        &peer,
        direct_socket,
        Vec::new(),
        vec!["0.0.0.0:9999".to_string()],
    )
    .await;

    assert_eq!(
        reach::observed_peer_for(&peer.id()),
        Some(direct_socket),
        "a direct Hello must still teach the register the socket it arrived on"
    );

    let row = target
        .file()
        .peers
        .into_iter()
        .find(|row| row.node == peer.id())
        .expect("the pinned row is still there");
    assert_eq!(
        row.endpoints.len(),
        1,
        "a direct Hello must still write the endpoint it was reached over: {:?}",
        row.endpoints
    );
    let expected: SocketAddr = "203.0.113.11:9999".parse().expect("the literal parses");
    assert_eq!(row.endpoints[0].direct_addr(), Some(expected));

    assert_eq!(
        answered.observed_you_at.as_deref(),
        Some(direct_socket.to_string().as_str()),
        "a direct Hello must still be answered with where this node saw the peer"
    );
}
