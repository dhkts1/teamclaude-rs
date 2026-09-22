//! Two Macs that both moved swap their current addresses through a friend.
//!
//! # What a hint is for
//!
//! `CollapseHint`'s own doc line is "You reached me through a middle hop; here
//! is how to reach me directly". Its frame has shipped since the wire was
//! written and has never had a production caller, so a pair whose addresses
//! both went stale had one repair: a person, pasting a link. The three tests
//! here are the automatic path: one carried frame, and the Mac that received it
//! now knows where to aim.
//!
//! # Why the frame is believed and the socket it arrived on is not
//!
//! The session carrying the hint has already proved the sender's pinned static
//! key, because a carried CONTROL stream is a fresh end to end Noise session
//! nested inside a TUNNEL (`Control`'s own doc in
//! `crates/tcr-peer-wire/src/lib.rs`). The transport socket under it is the
//! CARRIER's connection to this node and proves nothing about who is behind
//! it, which is the rule the previous unit shipped. So the address in the
//! frame is routing advice from a proved sender, and the socket is a log
//! field.
//!
//! # What these were red for before the arm existed
//!
//! All three, measured against the commit this branch was cut from, close with
//! one line: `peer control: CollapseHint(CollapseHint { .. }) is not answered
//! by this build (the lease lifecycle is phase 4)`. That is the `other =>
//! bail!` arm, and it is why two of the three fail on the answer and the third
//! on the refusal's wording.
//!
//! `a_relayed_hint_teaches_where_to_aim` fails on the VALUE rather than on
//! `PeerAddressUnknown`: `left: 198.51.100.50, right: 192.0.2.77`, the stale
//! address the register was seeded with. The seeding is deliberate, so that
//! the address a punch would have used is a real value and not an absence, and
//! it means the base answers the wrong address rather than refusing to answer.
//! Both are red; only one of them is the interesting red, because a fix that
//! merely made `punch_target` answer something would pass the other.
//!
//! # Driven over an in-memory duplex, not a socket
//!
//! The instrument is `tests/peer_carried_control.rs`'s, copied rather than
//! reinvented: `tokio::io::duplex` against `listener::serve_accepted`, the
//! asker's half hand-driven with `noise::dial_handshake` plus
//! `serve::send_control`, and a header whose `via` names a carrier. The
//! accepting address is the whole point of the fixture and this box has one
//! address, so a duplex stands in for the socket.

use std::net::SocketAddr;
use std::time::Duration;

use tcr_peer_wire::{CollapseHint, Control, PeerId, StreamHeader, StreamKind};
use teamclaude_rs::peer::config::{self as peer_config, PeerFile, PeerRow};
use teamclaude_rs::peer::id::NodeKey;
use teamclaude_rs::peer::listener::{self, SessionContext};
use teamclaude_rs::peer::noise::{self, Handshake};
use teamclaude_rs::peer::reach;
use teamclaude_rs::peer::serve;

/// A carrier's own connection to this node, RFC 5737 TEST-NET-3. Every
/// exchange below is served from it, so an address learned from the socket
/// would be this one and nothing here may ever report it.
const CARRIER_SOCKET: &str = "203.0.113.9:41000";

/// A scratch directory named after this process, thread and a caller tag, so
/// two tests in this binary never collide on one path.
fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tcr-peer-aim-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the scratch dir");
    dir
}

/// The Mac that receives the hint: a pinned row for `peer` already on its
/// file, so the ordinary CONTROL gate admits the frame.
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
}

/// What one carried hint produced: the frame the target answered with, if it
/// answered at all, and how its session ended.
struct Exchanged {
    answer: Option<Control>,
    closed_with: String,
}

/// Drive one carried `CollapseHint` at `target` over a duplex, as if it had
/// been accepted from [`CARRIER_SOCKET`], and report what came back.
///
/// `via` names a carrier on every call here, which is what makes the socket
/// under the stream a carrier's rather than the peer's.
async fn hint_from(
    target: &Target,
    peer: &NodeKey,
    via: Vec<PeerId>,
    hint: CollapseHint,
) -> Exchanged {
    use tokio::time::timeout;

    let context = target.context();
    let (theirs, ours) = tokio::io::duplex(8192);
    let from: SocketAddr = CARRIER_SOCKET.parse().expect("the literal parses");
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
        request_id: 11,
    };
    serve::send_control(&mut theirs, &mut session, &header)
        .await
        .expect("the header writes");
    serve::send_control(&mut theirs, &mut session, &Control::CollapseHint(hint))
        .await
        .expect("the hint writes");

    // One bounded read, and a target that closed the session instead of
    // answering is an answer of `None` rather than a hang: that is exactly what
    // a build with no arm for this frame does, and the test has to be able to
    // say so.
    let answer = match timeout(
        Duration::from_secs(5),
        serve::recv_control::<_, Control>(&mut theirs, &mut session),
    )
    .await
    {
        Ok(Ok(frame)) => Some(frame),
        Ok(Err(_closed)) => None,
        Err(_elapsed) => None,
    };
    drop(theirs);
    let closed_with = match timeout(Duration::from_secs(5), served).await {
        Ok(Ok(Ok(()))) => String::new(),
        Ok(Ok(Err(err))) => format!("{err:#}"),
        Ok(Err(join)) => format!("the serving task panicked: {join}"),
        Err(_elapsed) => "the serving task never finished".to_string(),
    };
    Exchanged {
        answer,
        closed_with,
    }
}

/// **A relayed hint teaches where to aim.**
///
/// Both observation registers are seeded stale first, so the address the punch
/// would have used is a real value and not an absence, and the hint names a
/// third address that is neither the stale one nor the carrier's. The
/// assertion is on the VALUE `punch_target` answers with, because "it returned
/// Ok" is also true of the stale answer this exists to replace.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_relayed_hint_teaches_where_to_aim() {
    let peer = NodeKey::load_or_mint(&scratch("aim-peer")).expect("mint the peer's key");
    let carrier = PeerId([0x77_u8; 32]);
    let target = Target::new("aim-target", peer.id());

    let stale: SocketAddr = "198.51.100.50:7755".parse().expect("the literal parses");
    let fresh: SocketAddr = "192.0.2.77:7755".parse().expect("the literal parses");
    reach::remember_observed_peer(peer.id(), stale);
    reach::remember_observed_peer(target.key.id(), stale);
    // Without a pair secret every `punch_target` below is
    // `NoRendezvousSecret` whatever address is recorded, and the address is
    // what this test is about.
    reach::remember_port_secret(peer.id(), [0x41_u8; 32]);

    let exchanged = hint_from(
        &target,
        &peer,
        vec![carrier],
        CollapseHint {
            node: peer.id(),
            addrs: vec![fresh.to_string()],
            observed_rtt_ms: 0,
        },
    )
    .await;

    let aimed = reach::punch_target(&peer.id(), &[])
        .expect("a hint that named a dialable address leaves a punch with somewhere to aim");
    assert_eq!(
        aimed.0,
        fresh.ip(),
        "the punch must aim at the address the hint named, not {:?}; closed with {:?}",
        aimed.0,
        exchanged.closed_with
    );
    assert_ne!(
        aimed.0,
        stale.ip(),
        "the punch is still aiming at the stale address the register held before the hint"
    );
    let carrier_socket: SocketAddr = CARRIER_SOCKET.parse().expect("the literal parses");
    assert_ne!(
        aimed.0,
        carrier_socket.ip(),
        "the punch is aiming at the carrier's own socket, which proves nothing about the peer"
    );

    let answer = exchanged.answer.as_ref().unwrap_or_else(|| {
        panic!(
            "the target answered no hint at all; closed with {:?}",
            exchanged.closed_with
        )
    });
    let Control::CollapseHint(theirs) = answer else {
        panic!("the target answered a hint with {answer:?}, not a hint of its own");
    };
    assert_eq!(
        theirs.node,
        target.key.id(),
        "a hint is about its own sender, so the answer must name the answering Mac"
    );
}

/// **The arm filters rather than accepting whatever a frame carries.**
///
/// Three claims, none of them dialable: a wildcard bind address, a string that
/// is not an address at all, and an unspecified address on port zero.
/// `is_dialable_claim`'s own measured case is a peer honestly naming its own
/// configured listen socket, so this is the ordinary frame and not a hostile
/// one. The register must still hold the value it was seeded with, and the
/// session must still be answered: a peer whose addresses are all
/// unannounceable has not said anything wrong.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hint_with_no_dialable_address_leaves_the_stale_one_in_place() {
    let peer = NodeKey::load_or_mint(&scratch("filter-peer")).expect("mint the peer's key");
    let carrier = PeerId([0x78_u8; 32]);
    let target = Target::new("filter-target", peer.id());

    let stale: SocketAddr = "198.51.100.51:7755".parse().expect("the literal parses");
    reach::remember_observed_peer(peer.id(), stale);

    let exchanged = hint_from(
        &target,
        &peer,
        vec![carrier],
        CollapseHint {
            node: peer.id(),
            addrs: vec![
                "0.0.0.0:7755".to_string(),
                "not-an-address".to_string(),
                ":::0".to_string(),
            ],
            observed_rtt_ms: 0,
        },
    )
    .await;

    assert_eq!(
        reach::observed_peer_for(&peer.id()),
        Some(stale),
        "a hint with nothing dialable in it overwrote the address the register already held"
    );

    let answer = exchanged
        .answer
        .as_ref()
        .unwrap_or_else(|| panic!("the target closed the session instead of answering a peer whose claims were merely undialable; closed with {:?}", exchanged.closed_with));
    let Control::CollapseHint(theirs) = answer else {
        panic!("the target answered a hint with {answer:?}, not a hint of its own");
    };
    assert_eq!(
        theirs.node,
        target.key.id(),
        "the answer must name the answering Mac even when the asker's own claims were dropped"
    );
}

/// **A hint about a third Mac is refused, and the refusal names the frame.**
///
/// A hint is about its sender. A third party's address is `NeighborBrief`'s
/// job and travels under a grant, so believing this one would be transitive
/// trust with no grant behind it. Nothing may be recorded for either Mac, and
/// the close has to say which frame it refused: a reader of one log line
/// otherwise cannot tell this apart from any other unanswered frame.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hint_about_a_third_mac_is_refused() {
    let peer = NodeKey::load_or_mint(&scratch("third-peer")).expect("mint the peer's key");
    let carrier = PeerId([0x79_u8; 32]);
    let third = PeerId([0x33_u8; 32]);
    let target = Target::new("third-target", peer.id());

    let fresh: SocketAddr = "192.0.2.90:7755".parse().expect("the literal parses");
    let exchanged = hint_from(
        &target,
        &peer,
        vec![carrier],
        CollapseHint {
            node: third,
            addrs: vec![fresh.to_string()],
            observed_rtt_ms: 0,
        },
    )
    .await;

    assert_eq!(
        reach::observed_peer_for(&third),
        None,
        "a hint naming a third Mac wrote that Mac's address into the register"
    );
    assert_eq!(
        reach::observed_peer_for(&peer.id()),
        None,
        "a hint naming a third Mac wrote an address for its own sender instead"
    );
    assert!(
        exchanged.closed_with.contains("CollapseHint"),
        "the refusal must name the frame it refused: {:?}",
        exchanged.closed_with
    );
    assert!(
        exchanged.closed_with.contains("about its own sender"),
        "the refusal must say why a hint about another Mac is not believed: {:?}",
        exchanged.closed_with
    );
    assert!(
        exchanged.answer.is_none(),
        "a refused hint must not be answered with one: {:?}",
        exchanged.answer
    );
}

/// **The wire did not move.**
///
/// Asserted here rather than argued in a commit body: the three keys are the
/// ones `tests/peer_wire.rs` already pins for this frame, and a build that
/// renamed or added one would make every older peer's hint unreadable. Green
/// on the base by design, because this unit changes no field.
#[test]
fn a_collapse_hint_still_carries_the_three_keys_the_wire_already_pins() {
    let hint = CollapseHint {
        node: PeerId([0x0a_u8; 32]),
        addrs: vec!["192.0.2.5:7755".to_string()],
        observed_rtt_ms: 0,
    };
    let value = serde_json::to_value(Control::CollapseHint(hint.clone()))
        .expect("a Control::CollapseHint serializes");
    let object = value
        .as_object()
        .expect("an internally tagged Control serializes as an object");
    let mut keys: Vec<&str> = object
        .keys()
        .map(|key| key.as_str())
        .filter(|key| *key != "type")
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["addrs", "node", "observedRttMs"],
        "the hint's keys moved: {value}"
    );

    let back: Control = serde_json::from_value(value).expect("the frame reads back");
    assert_eq!(
        back,
        Control::CollapseHint(hint),
        "a hint this build wrote did not read back as the value it was"
    );
}
