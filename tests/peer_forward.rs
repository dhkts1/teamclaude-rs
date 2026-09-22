//! One Mac splicing a pinned friend onward to
//! another pinned friend, and the four things that stop it being an open relay.
//!
//! # What is real here and what is a stand-in
//!
//! Real: the Noise handshakes (two of them, nested), the sessions, the
//! production authorization gate (`listener::peer_stream_gate_rows`), the
//! production forwarding decision (`tunnel::authorize_forward`), the production
//! forwarder (`tunnel::handle_forward_on`), the production peers file and store,
//! and a target that really runs a responder handshake and really decrypts a
//! frame.
//!
//! One stand-in, named at its call site: the forwarder's accept loop, which is
//! the listener's `StreamKind::Tunnel` arm. The relay header used to be a
//! second one: `TunnelTarget::Peer` was a newtype variant of an internally
//! tagged enum and serde would not write it, so no requester could name a
//! target and the id was rebuilt on the forwarder, and forward-dial support
//! made the variant a struct variant, so it travels now and
//! `tests/peer_egress.rs`'s `a_relay_target_travels_on_the_wire` is where that
//! is on the record.
//!
//! # The requester's half, added for forward-dial
//!
//! Everything above the requester's-half banner near the end of this file
//! measures a Mac that AGREES to carry. The cases under it drive the
//! production dial that ASKS one to, `serve::dial_peer_reaching`, which is
//! the half that did not exist: no code anywhere opened a TUNNEL whose target
//! was a peer, so every gate above was reachable only by a test.
//!
//! # House rules this file is built to
//!
//! Every socket binds `127.0.0.1:0` (kernel-chosen), every file is under a
//! process-and-thread-unique scratch directory, no account or address is real,
//! and nothing touches the proxy on `127.0.0.1:3456`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tcr_peer_wire::{PeerId, StreamHeader, StreamKind, TunnelTarget};
use teamclaude_rs::peer::config::{
    self, Allow, Endpoint, EndpointSource, PeerFile, PeerRow, PeerStore,
};
use teamclaude_rs::peer::listener;
use teamclaude_rs::peer::noise::{self, Handshake};
use teamclaude_rs::peer::tunnel::{self, Carry, Forward, NoiseStream, OriginRoute, TunnelBudget};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

/// A fixed instant, so nothing here reads the clock.
const NOW_MS: i64 = 1_767_225_600_000;

/// A scratch directory named after this process and thread, so five lanes
/// running at once never collide on one path.
fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tcr-peer-forward-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the scratch dir");
    dir
}

/// One machine's key material, as a test holds it.
struct Node {
    secret: [u8; 32],
    id: PeerId,
}

fn node() -> Node {
    let (secret, public) = noise::generate_static().expect("mint a static keypair");
    Node {
        secret,
        id: PeerId(public),
    }
}

/// A listener on a kernel-chosen loopback port.
async fn loopback() -> TcpListener {
    TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback listener")
}

/// A pinned row for `peer`, with `relay` granted or not.
///
/// `addrs` stays a list of strings because that is what a caller here writes,
/// one literal socket address, or none at all. The row's own
/// field is [`Endpoint`], so the conversion happens once, here, rather than at
/// fourteen call sites: each address becomes a `Direct` endpoint observed at
/// [`NOW_MS`] with source `Paired`, which is what a pin writes. A caller
/// passing something that is not a socket address is a mistake in the test, so
/// it panics naming the value rather than silently dropping it.
fn row(peer: &PeerId, label: &str, addrs: Vec<String>, relay: bool) -> PeerRow {
    let endpoints = addrs
        .iter()
        .map(|addr| {
            let parsed = addr
                .parse()
                .unwrap_or_else(|err| panic!("{addr:?} is not a socket address: {err}"));
            Endpoint::direct(parsed, NOW_MS, EndpointSource::Paired)
        })
        .collect();
    PeerRow {
        node: *peer,
        label: label.to_string(),
        endpoints,
        added_at: NOW_MS,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: Allow {
            relay,
            ..Allow::default()
        },
        lend: Vec::new(),
    }
}

/// A real [`PeerStore`] over a real peers file in a scratch directory.
fn store(tag: &str, max_hops: u8, rows: Vec<PeerRow>) -> PeerStore {
    let path = scratch(tag).join("tcr-peers.json");
    config::save(
        &path,
        &PeerFile {
            max_hops,
            peers: rows,
            ..PeerFile::default()
        },
    )
    .expect("write the peers file");
    PeerStore::open(&path).expect("open the peers file")
}

/// The header the cases in this section write by hand, with no target on it.
///
/// A header with `target: None` is not what a real requester sends any more
/// ([`tunnel::open_forward_to`] names the Mac, and the client cases at the end
/// of this file drive it); it is what these cases want, because each of them
/// varies `hops_remaining` or `via` against a target [`forwarder_on`] supplies,
/// so the field under test is the only one that moves.
fn forward_header(hops_remaining: u8, via: Vec<PeerId>) -> StreamHeader {
    StreamHeader {
        kind: StreamKind::Tunnel,
        target: None,
        via,
        hops_remaining,
        request_id: 11,
    }
}

// ---------------------------------------------------------------------------
// Item 1: the decision, every refusal of it, before a socket exists
// ---------------------------------------------------------------------------

/// Everything [`tunnel::authorize_forward`] needs, so each case below changes
/// exactly one thing.
struct Case {
    requester: PeerId,
    target: TunnelTarget,
    route: OriginRoute,
    node: PeerId,
    hops_remaining: u8,
    via: Vec<PeerId>,
    store: PeerStore,
}

fn decide(case: &Case) -> anyhow::Result<tunnel::ForwardPlan> {
    let budget = Mutex::new(TunnelBudget::new());
    tunnel::authorize_forward(
        &Carry {
            route: case.route,
            peer: case.requester,
            target: &case.target,
            hosts: teamclaude_rs::peer::egress::PEER_EGRESS_HOSTS,
            cap_bytes: 1024 * 1024,
            budget: &budget,
            now_ms: NOW_MS,
        },
        &Forward {
            store: &case.store,
            node: case.node,
            hops_remaining: case.hops_remaining,
            via: &case.via,
        },
    )
}

/// The case every refusal below is one edit away from: A is pinned with
/// `relay`, B is pinned, this Mac's `maxHops` is the default 1, one hop is
/// left.
fn granted(tag: &str, a: &PeerId, b: &PeerId, me: &PeerId) -> Case {
    Case {
        requester: *a,
        target: TunnelTarget::Peer { node: *b },
        route: OriginRoute::Peer(*b),
        node: *me,
        hops_remaining: 1,
        via: Vec::new(),
        store: store(
            tag,
            1,
            vec![
                row(a, "requester", Vec::new(), true),
                row(b, "target", vec!["127.0.0.1:1".to_string()], false),
            ],
        ),
    }
}

#[test]
fn a_granted_forward_to_a_pinned_target_is_agreed_and_spends_one_hop() {
    let (a, b, me) = (node(), node(), node());
    let plan = decide(&granted("granted", &a.id, &b.id, &me.id)).expect("the forward is agreed");
    assert_eq!(
        plan.target.node, b.id,
        "the row dialled must be the target's"
    );
    assert_eq!(
        plan.onward_hops, 0,
        "one forward hop is the default budget, so nothing is left after this one"
    );
}

#[test]
fn a_requester_without_the_relay_grant_is_refused() {
    let (a, b, me) = (node(), node(), node());
    let mut case = granted("no-grant", &a.id, &b.id, &me.id);
    case.store = store(
        "no-grant-rows",
        1,
        vec![
            row(&a.id, "requester", Vec::new(), false),
            row(&b.id, "target", vec!["127.0.0.1:1".to_string()], false),
        ],
    );
    let text = format!(
        "{:#}",
        decide(&case).expect_err("a bare pin may not forward")
    );
    assert!(
        text.contains("allow.relay"),
        "the refusal must name the missing grant: {text}"
    );

    // And the production gate refuses the same frame one step earlier, so the
    // decision is not the only thing standing between a bare pin and a
    // forward.
    let header = StreamHeader {
        target: Some(TunnelTarget::Peer { node: b.id }),
        ..forward_header(1, Vec::new())
    };
    let bare = row(&a.id, "requester", Vec::new(), false);
    let refusal = teamclaude_rs::peer::listener::peer_stream_gate_rows(&header, Some(&bare))
        .expect_err("the gate refuses a relay target under a bare pin");
    assert!(
        format!("{refusal:?}").contains("NotGranted"),
        "the gate's refusal must be the missing grant: {refusal:?}"
    );
}

#[test]
fn a_target_this_mac_never_pinned_is_refused() {
    let (a, b, me) = (node(), node(), node());
    let unpinned = node();
    let mut case = granted("unpinned", &a.id, &b.id, &me.id);
    case.target = TunnelTarget::Peer { node: unpinned.id };
    case.route = OriginRoute::Peer(unpinned.id);
    let text = format!(
        "{:#}",
        decide(&case).expect_err("a target with no row may not be dialled")
    );
    assert!(
        text.contains("has not pinned"),
        "the refusal must be about the missing row: {text}"
    );
}

#[test]
fn a_frame_with_no_hops_left_is_refused() {
    let (a, b, me) = (node(), node(), node());
    let mut case = granted("hops", &a.id, &b.id, &me.id);
    case.hops_remaining = 0;
    let text = format!(
        "{:#}",
        decide(&case).expect_err("a frame at zero hops may not be forwarded")
    );
    assert!(
        text.contains("no hops left"),
        "the refusal must be about the hop budget: {text}"
    );
}

#[test]
fn max_hops_zero_disables_forwarding_entirely() {
    let (a, b, me) = (node(), node(), node());
    let mut case = granted("max-hops", &a.id, &b.id, &me.id);
    case.store = store(
        "max-hops-rows",
        0,
        vec![
            row(&a.id, "requester", Vec::new(), true),
            row(&b.id, "target", vec!["127.0.0.1:1".to_string()], false),
        ],
    );
    let text = format!(
        "{:#}",
        decide(&case).expect_err("`maxHops` 0 refuses every forward")
    );
    assert!(
        text.contains("maxHops` is 0"),
        "the refusal must name the operator's setting: {text}"
    );
}

#[test]
fn a_frame_that_already_passed_through_this_mac_is_refused() {
    let (a, b, me) = (node(), node(), node());
    let mut case = granted("cycle", &a.id, &b.id, &me.id);
    case.via = vec![node().id, me.id];
    let text = format!("{:#}", decide(&case).expect_err("a cycle is refused"));
    assert!(
        text.contains("cycle"),
        "the refusal must be about the loop: {text}"
    );
}

#[test]
fn a_route_that_names_another_mac_than_the_header_is_refused() {
    let (a, b, me) = (node(), node(), node());
    let elsewhere = node();
    let mut case = granted("mismatch", &a.id, &b.id, &me.id);
    case.route = OriginRoute::Peer(elsewhere.id);
    let text = format!("{:#}", decide(&case).expect_err("a mismatch is refused"));
    assert!(
        text.contains("the route names"),
        "the refusal must name both halves: {text}"
    );

    // An origin target on this path, and a peer route on the gateway path, are
    // the same mistake from the other side: neither one picks a socket.
    let mut origin = granted("mismatch-origin", &a.id, &b.id, &me.id);
    origin.target = TunnelTarget::Origin {
        host: "api.anthropic.com".to_string(),
        port: 443,
    };
    let text = format!(
        "{:#}",
        decide(&origin).expect_err("an origin target is not forwarded")
    );
    assert!(
        text.contains("handle_tunnel_on"),
        "the refusal must send the caller to the gateway handler: {text}"
    );
}

#[test]
fn a_forward_back_to_the_requester_or_to_this_mac_is_refused() {
    let (a, me) = (node(), node());
    // To this Mac itself.
    let mut to_self = granted("self", &a.id, &me.id, &me.id);
    to_self.store = store(
        "self-rows",
        1,
        vec![
            row(&a.id, "requester", Vec::new(), true),
            row(&me.id, "me", vec!["127.0.0.1:1".to_string()], false),
        ],
    );
    let text = format!(
        "{:#}",
        decide(&to_self).expect_err("forwarding to itself is refused")
    );
    assert!(
        text.contains("forward to itself"),
        "the refusal must say so: {text}"
    );

    // Back out the link it arrived on.
    let mut back = granted("ingress", &a.id, &a.id, &me.id);
    back.store = store(
        "ingress-rows",
        1,
        vec![row(
            &a.id,
            "requester",
            vec!["127.0.0.1:1".to_string()],
            true,
        )],
    );
    let text = format!(
        "{:#}",
        decide(&back).expect_err("forwarding back out the ingress link is refused")
    );
    assert!(
        text.contains("back out the link"),
        "the refusal must say so: {text}"
    );
}

// ---------------------------------------------------------------------------
// The forwarder, as the listener's TUNNEL arm will run it
// ---------------------------------------------------------------------------

/// What the forwarder under test is, on one Mac.
///
/// A struct rather than eight arguments, and it holds the budget behind an
/// `Arc` so the TEST can read the ledger the forwarder charged.
struct Forwarder {
    secret: [u8; 32],
    node: PeerId,
    store: PeerStore,
    target: PeerId,
    cap_bytes: u64,
    budget: Arc<Mutex<TunnelBudget>>,
}

/// One forward, as `src/peer/listener.rs`'s `StreamKind::Tunnel` arm will run
/// it: accept the session, read the header, run the PRODUCTION gate, then hand
/// the stream to the PRODUCTION forwarder.
///
/// Test-only in its accept loop, and in one fallback: a requester that sent no
/// target at all gets [`Forwarder::target`] put in on this side. That used to
/// be every requester, because `TunnelTarget::Peer` could not be serialized;
/// since it now travels serialized, so a client written against the
/// production dial exercises the real field and only the hand-rolled
/// [`forward_header`] cases still need the fallback. Every other field the
/// gate and the forwarder read , `hops_remaining`, `via`, the kind, and the
/// peer the HANDSHAKE proved, is the requester's own.
fn forwarder_on(
    listener: TcpListener,
    forwarder: Forwarder,
) -> tokio::task::JoinHandle<anyhow::Result<(u64, u64)>> {
    forwarder_rounds_on(listener, forwarder, 1)
}

/// [`forwarder_on`] for a caller that is dialled more than once, answering the
/// LAST forward's byte counts.
///
/// **A real forwarder serves many streams, and this fixture used to serve
/// exactly one.** That arity was invisible at the call site and it was the
/// whole of a later failure: a caller that now opens one carried stream before
/// the one it is measuring found nothing accepting the second, and read that
/// as the carry itself refusing. Making the count an argument puts the fact
/// where a reader of the test can see it.
fn forwarder_rounds_on(
    listener: TcpListener,
    forwarder: Forwarder,
    rounds: usize,
) -> tokio::task::JoinHandle<anyhow::Result<(u64, u64)>> {
    tokio::spawn(async move {
        let mut last = (0, 0);
        for _ in 0..rounds {
            let (stream, _) = listener.accept().await?;
            last = forward_one(stream, &forwarder).await?;
        }
        Ok(last)
    })
}

/// One accepted connection, forwarded. The body of [`forwarder_on`], lifted
/// out so that how MANY connections are served is the spawner's business and
/// not something baked into the accept.
async fn forward_one(stream: TcpStream, forwarder: &Forwarder) -> anyhow::Result<(u64, u64)> {
    let mut stream = stream;
    let pin_rows = forwarder.store.peers();
    let mut session = noise::accept_handshake(
        &mut stream,
        &forwarder.secret,
        Handshake::Return,
        &[],
        move |remote| noise::pin_check_rows(remote, &pin_rows),
    )
    .await?;
    let frame = noise::recv_encrypted(&mut stream, &mut session.transport).await?;
    let wire: StreamHeader = serde_json::from_slice(&frame)?;
    let target = wire.target.clone().unwrap_or(TunnelTarget::Peer {
        node: forwarder.target,
    });
    let header = StreamHeader {
        target: Some(target.clone()),
        ..wire
    };
    let row = forwarder.store.row(&session.peer);
    listener::peer_stream_gate_rows(&header, row.as_ref()).map_err(anyhow::Error::new)?;
    // The peer charged is the one the HANDSHAKE proved, read off the
    // session, never anything the header said about itself.
    let peer = session.peer;
    tunnel::handle_forward_on(
        stream,
        session,
        Carry {
            route: OriginRoute::Peer(forwarder.target),
            peer,
            target: &target,
            hosts: teamclaude_rs::peer::egress::PEER_EGRESS_HOSTS,
            cap_bytes: forwarder.cap_bytes,
            budget: &forwarder.budget,
            now_ms: NOW_MS,
        },
        Forward {
            store: &forwarder.store,
            node: forwarder.node,
            hops_remaining: header.hops_remaining,
            via: &header.via,
        },
    )
    .await
}

/// A target that counts what reached it, so "refused" can be measured as *the
/// target was never dialled* instead of as an error string.
#[derive(Clone, Default)]
struct Target {
    connections: Arc<AtomicUsize>,
    bytes: Arc<AtomicUsize>,
}

/// Accept forever, count every connection and byte, and answer `reply` once per
/// connection.
///
/// It speaks nothing: a forwarder splices raw bytes and has no business knowing
/// what its target's protocol is, so a target that answers a fixed string is a
/// truthful stand-in for every case except the end-to-end one below, which
/// runs a real responder handshake instead.
fn target_on(listener: TcpListener, reply: Vec<u8>) -> Target {
    let target = Target::default();
    let watch = target.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            watch.connections.fetch_add(1, Ordering::SeqCst);
            let seen = Arc::clone(&watch.bytes);
            let reply = reply.clone();
            tokio::spawn(async move {
                let mut buf = vec![0_u8; 4096];
                let mut answered = false;
                loop {
                    match socket.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(read) => {
                            seen.fetch_add(read, Ordering::SeqCst);
                            if !answered {
                                answered = true;
                                if socket.write_all(&reply).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                }
            });
        }
    });
    target
}

/// The requester's half: the outer session to the forwarder, the header, and
/// the plaintext stream the forwarder will splice onward.
async fn ask_to_forward(
    forwarder_addr: std::net::SocketAddr,
    requester: &Node,
    forwarder: &Node,
    header: &StreamHeader,
) -> NoiseStream {
    let mut stream = TcpStream::connect(forwarder_addr)
        .await
        .expect("dial the forwarder");
    let mut session = noise::dial_handshake(
        &mut stream,
        &requester.secret,
        Handshake::Return,
        Some(&forwarder.id.0),
        None,
    )
    .await
    .expect("the outer handshake completes");
    noise::send_encrypted(
        &mut stream,
        &mut session.transport,
        &serde_json::to_vec(header).expect("the header serializes"),
    )
    .await
    .expect("the header is written");
    NoiseStream::start(stream, session)
}

// ---------------------------------------------------------------------------
// Item 2: forwarded bytes are the requester's, out of the same hour
// ---------------------------------------------------------------------------

/// Both directions of a forward land on the REQUESTER's hour, in the same
/// ledger a carry draws on.
///
/// This is the only hard ceiling on what a transitive forward grant costs
/// (`Allow::relay`'s doc-comment): the node two hops out spends the grantee's
/// hour, so a forward that charged nobody would make the grant unbounded.
#[tokio::test]
async fn forwarded_bytes_are_charged_to_the_requester() {
    let (a, b, c) = (node(), node(), node());
    let target_listener = loopback().await;
    let target_addr = target_listener.local_addr().expect("the target address");
    let target = target_on(target_listener, vec![7_u8; 16]);

    let budget = Arc::new(Mutex::new(TunnelBudget::new()));
    let forwarder_listener = loopback().await;
    let forwarder_addr = forwarder_listener
        .local_addr()
        .expect("the forwarder address");
    let task = forwarder_on(
        forwarder_listener,
        Forwarder {
            secret: c.secret,
            node: c.id,
            store: store(
                "charge",
                1,
                vec![
                    row(&a.id, "requester", Vec::new(), true),
                    row(&b.id, "target", vec![target_addr.to_string()], false),
                ],
            ),
            target: b.id,
            cap_bytes: 1024 * 1024,
            budget: Arc::clone(&budget),
        },
    );

    let mut asked = ask_to_forward(forwarder_addr, &a, &c, &forward_header(1, Vec::new())).await;
    asked
        .write_all(&[3_u8; 32])
        .await
        .expect("32 bytes go up the tunnel");
    let mut back = [0_u8; 16];
    asked
        .read_exact(&mut back)
        .await
        .expect("16 bytes come back down");
    asked.finish().await.expect("the requester's pump ends");

    let (up, down) = task
        .await
        .expect("the forwarder task joins")
        .expect("the forward completes");
    assert_eq!((up, down), (32, 16), "the meter is the two byte counts");
    assert_eq!(
        target.connections.load(Ordering::SeqCst),
        1,
        "the target was dialled exactly once"
    );

    let mut ledger = budget.lock().expect("the ledger lock");
    assert_eq!(
        ledger.spent_by(&a.id, NOW_MS),
        48,
        "both directions are charged to the requester"
    );
    assert_eq!(
        ledger.spent_by(&b.id, NOW_MS),
        0,
        "nothing is charged to the target: it never asked for anything"
    );
    assert_eq!(
        ledger.reserved_by(&a.id),
        0,
        "the reservation is released when the forward ends"
    );
}

/// A requester that has spent its hour is refused before the target is dialled.
///
/// The assertion that matters is the connection count, not the error: a cap
/// that refused only after opening the onward socket would still let a peer
/// that has gone wrong knock on a friend's Mac all hour.
#[tokio::test]
async fn a_requester_over_its_hour_is_refused_before_the_target_is_dialled() {
    let (a, b, c) = (node(), node(), node());
    let target_listener = loopback().await;
    let target_addr = target_listener.local_addr().expect("the target address");
    let target = target_on(target_listener, vec![7_u8; 16]);

    let budget = Arc::new(Mutex::new(TunnelBudget::new()));
    budget
        .lock()
        .expect("the ledger lock")
        .charge(&a.id, 4096, NOW_MS);
    let forwarder_listener = loopback().await;
    let forwarder_addr = forwarder_listener
        .local_addr()
        .expect("the forwarder address");
    let task = forwarder_on(
        forwarder_listener,
        Forwarder {
            secret: c.secret,
            node: c.id,
            store: store(
                "spent",
                1,
                vec![
                    row(&a.id, "requester", Vec::new(), true),
                    row(&b.id, "target", vec![target_addr.to_string()], false),
                ],
            ),
            target: b.id,
            cap_bytes: 4096,
            budget: Arc::clone(&budget),
        },
    );

    let mut asked = ask_to_forward(forwarder_addr, &a, &c, &forward_header(1, Vec::new())).await;
    // Written and ignored: the refusal happens before anything is read.
    let _written = asked.write_all(&[3_u8; 32]).await;
    let refusal = task
        .await
        .expect("the forwarder task joins")
        .expect_err("a spent hour refuses the forward");
    let text = format!("{refusal:#}");
    assert!(
        text.contains("carried bytes this hour"),
        "the refusal must be the byte cap: {text}"
    );
    assert_eq!(
        target.connections.load(Ordering::SeqCst),
        0,
        "a refused forward costs the target nothing at all"
    );
}

// ---------------------------------------------------------------------------
// Item 3: the gate, A through C to B, and the three refusals
// ---------------------------------------------------------------------------

/// The plaintext the requester sends INSIDE its nested session to the target.
///
/// It exists to be looked for: it must appear in what the TARGET decrypts and
/// must not appear in the bytes that crossed the forwarder, nor in anything the
/// forwarder logged.
const MARKER: &str = "forwarder-must-never-see-this";

/// A socket that records every byte READ off it.
///
/// It goes on the TARGET's side, so the recording is exactly the stream the
/// forwarder put on the wire. Reading it at the target rather than inside the
/// forwarder is the point: a tap the forwarder held would be measuring the
/// forwarder's own copy of its own claim.
struct Tap {
    inner: TcpStream,
    seen: Arc<Mutex<Vec<u8>>>,
}

impl tokio::io::AsyncRead for Tap {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let poll = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        if let std::task::Poll::Ready(Ok(())) = &poll {
            let fresh = buf.filled()[before..].to_vec();
            self.seen
                .lock()
                .expect("the tap lock")
                .extend_from_slice(&fresh);
        }
        poll
    }
}

impl tokio::io::AsyncWrite for Tap {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// What the target learned from one forwarded session.
struct Arrived {
    /// The static key the NESTED handshake proved. The whole question of this
    /// file.
    authenticated_as: PeerId,
    /// The one frame the requester sent inside that session.
    plaintext: Vec<u8>,
    /// Every byte the forwarder put on the wire.
    on_the_wire: Vec<u8>,
}

/// A target that runs a REAL responder handshake and pins one Mac.
///
/// `pins` is the whole of its peers file, and the forwarder is deliberately
/// not in it: if the forwarder had terminated anything, this handshake would
/// be refused rather than merely authenticated as somebody else, which is a
/// stronger statement than comparing two keys afterwards.
fn responder_on(
    listener: TcpListener,
    secret: [u8; 32],
    pins: Vec<PeerRow>,
) -> tokio::task::JoinHandle<anyhow::Result<Arrived>> {
    responder_rounds_on(listener, secret, pins, 1)
}

/// [`responder_on`] for a target that is dialled more than once, answering
/// what arrived on the LAST session.
///
/// The last and not the first, because the earlier rounds are whatever the
/// dialler did on its way to the exchange this test is about, and the session
/// under assertion is the one it ended on. Same reasoning as
/// [`forwarder_rounds_on`]: the count belongs at the call site.
fn responder_rounds_on(
    listener: TcpListener,
    secret: [u8; 32],
    pins: Vec<PeerRow>,
    rounds: usize,
) -> tokio::task::JoinHandle<anyhow::Result<Arrived>> {
    tokio::spawn(async move {
        let mut last = None;
        for _ in 0..rounds {
            let (socket, _) = listener.accept().await?;
            last = Some(respond_once(socket, &secret, &pins).await?);
        }
        last.ok_or_else(|| anyhow::anyhow!("a responder asked for zero rounds accepted nothing"))
    })
}

/// One accepted session, answered. The body of [`responder_on`], lifted out so
/// that how many sessions are served is the spawner's business.
async fn respond_once(
    socket: TcpStream,
    secret: &[u8; 32],
    pins: &[PeerRow],
) -> anyhow::Result<Arrived> {
    let pins = pins.to_vec();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut tapped = Tap {
        inner: socket,
        seen: Arc::clone(&seen),
    };
    let mut session =
        noise::accept_handshake(&mut tapped, secret, Handshake::Return, &[], move |remote| {
            noise::pin_check_rows(remote, &pins)
        })
        .await?;
    let plaintext = noise::recv_encrypted(&mut tapped, &mut session.transport).await?;
    // Drain to EOF, so `on_the_wire` is EVERYTHING the forwarder wrote and
    // not merely as far as the first frame: an assertion about ciphertext
    // has to be about the whole stream, and a tap that stopped early would
    // pass by looking away.
    let mut rest = vec![0_u8; 4096];
    while tapped.read(&mut rest).await.unwrap_or(0) > 0 {}
    let on_the_wire = seen.lock().expect("the tap lock").clone();
    Ok(Arrived {
        authenticated_as: session.peer,
        plaintext,
        on_the_wire,
    })
}

/// One captured tracing event: the message, plus the fields as strings.
#[derive(Debug, Clone)]
struct Logged {
    message: String,
    fields: std::collections::HashMap<String, String>,
}

/// Collect every event on this thread while the guard lives.
#[derive(Clone, Default)]
struct Collector {
    events: Arc<Mutex<Vec<Logged>>>,
}

impl<S> tracing_subscriber::Layer<S> for Collector
where
    S: tracing::Subscriber,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        struct Visitor {
            message: String,
            fields: std::collections::HashMap<String, String>,
        }
        impl tracing::field::Visit for Visitor {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                let rendered = format!("{value:?}");
                if field.name() == "message" {
                    self.message = rendered.trim_matches('"').to_string();
                } else {
                    self.fields.insert(field.name().to_string(), rendered);
                }
            }
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                if field.name() == "message" {
                    self.message = value.to_string();
                } else {
                    self.fields
                        .insert(field.name().to_string(), value.to_string());
                }
            }
        }
        let mut visitor = Visitor {
            message: String::new(),
            fields: std::collections::HashMap::new(),
        };
        event.record(&mut visitor);
        self.events
            .lock()
            .expect("the collector lock")
            .push(Logged {
                message: visitor.message,
                fields: visitor.fields,
            });
    }
}

/// Every tracing event this test BINARY emits, collected once, process-wide.
///
/// Process-wide rather than per test, and that is a flake measured here rather
/// than a preference. With a thread-local subscriber
/// (`tracing::subscriber::set_default`), a sibling test running in parallel
/// with no subscriber of its own can be the first to reach the forwarder's
/// `info!` callsite; tracing then caches `Interest::never` for that callsite
/// GLOBALLY, and the test that wanted to read the line sees an empty log. It
/// went red once with "one forward, one line: []", an EMPTY log, not a
/// missing field, and then passed alone, passed under `--test-threads=1`, and
/// passed in both test orders, which is what that shape of failure looks like
/// from the outside. One global subscriber,
/// installed once, is asked about every callsite instead, so events from
/// every test land here and a reader filters by the peer it is asking about.
fn log() -> &'static Collector {
    static LOG: std::sync::OnceLock<Collector> = std::sync::OnceLock::new();
    LOG.get_or_init(|| {
        use tracing_subscriber::layer::SubscriberExt as _;
        let collector = Collector::default();
        let subscriber = tracing_subscriber::registry().with(collector.clone());
        tracing::subscriber::set_global_default(subscriber)
            .expect("this binary installs no other subscriber");
        collector
    })
}

/// The gate: A asks C to forward to B, and B authenticates A.
///
/// Three real Mac processes are the full end-to-end job. What runs here is two
/// in-process listeners, the forwarder's and the target's, plus a requester,
/// each with its own key material and its own peers file, and every handshake
/// and both sessions are the production ones.
///
/// Each assertion is a different way for a forwarder to be wrong: the target
/// authenticated the REQUESTER (a forwarder that terminated the session would
/// be REFUSED by the target's pin check, because the target does not pin the
/// forwarder at all, which is a stronger statement than comparing two keys
/// afterwards), the requester's plaintext reached the target intact, that
/// plaintext never appeared in the bytes the forwarder wrote, measured
/// against a floor, so an empty tap cannot pass, and the forwarder's own log
/// line carries six fields, none of which come from the stream.
#[tokio::test]
async fn a_forward_reaches_the_target_which_authenticates_the_requester_not_the_forwarder() {
    let (a, b, c) = (node(), node(), node());
    // Installed before anything runs, so this forward's line has a subscriber
    // to register against.
    let log = log();

    // B pins A and NOT C: the nested handshake has to prove A or die.
    let target_listener = loopback().await;
    let target_addr = target_listener.local_addr().expect("the target address");
    let target = responder_on(
        target_listener,
        b.secret,
        vec![row(&a.id, "requester", Vec::new(), false)],
    );

    let budget = Arc::new(Mutex::new(TunnelBudget::new()));
    let forwarder_listener = loopback().await;
    let forwarder_addr = forwarder_listener
        .local_addr()
        .expect("the forwarder address");
    let forwarding = forwarder_on(
        forwarder_listener,
        Forwarder {
            secret: c.secret,
            node: c.id,
            store: store(
                "e2e",
                1,
                vec![
                    row(&a.id, "requester", Vec::new(), true),
                    row(&b.id, "target", vec![target_addr.to_string()], false),
                ],
            ),
            target: b.id,
            cap_bytes: 1024 * 1024,
            budget: Arc::clone(&budget),
        },
    );

    // A's outer session to C, and then its own nested session to B INSIDE it.
    let mut asked = ask_to_forward(forwarder_addr, &a, &c, &forward_header(1, Vec::new())).await;
    let mut nested = noise::dial_handshake(
        &mut asked,
        &a.secret,
        Handshake::Return,
        Some(&b.id.0),
        None,
    )
    .await
    .expect("the nested handshake completes through the forwarder");
    noise::send_encrypted(&mut asked, &mut nested.transport, MARKER.as_bytes())
        .await
        .expect("the marker is written inside the nested session");

    asked.finish().await.expect("the requester's pump ends");
    let (up, down) = forwarding
        .await
        .expect("the forwarder task joins")
        .expect("the forward completes");
    let arrived = target
        .await
        .expect("the target task joins")
        .expect("the target completed a session");

    assert_eq!(
        arrived.authenticated_as, a.id,
        "the target must authenticate the REQUESTER, never the forwarder"
    );
    assert_ne!(
        arrived.authenticated_as, c.id,
        "the forwarder's own key must not be what the target saw"
    );
    assert_eq!(
        String::from_utf8_lossy(&arrived.plaintext),
        MARKER,
        "the requester's plaintext must arrive intact at the target"
    );
    let wire = String::from_utf8_lossy(&arrived.on_the_wire).to_string();
    // The tap has to have seen the stream before its absence means anything.
    // Watched the other way round too: writing the marker into the tunnel
    // OUTSIDE the nested session put it in these bytes and turned the next
    // assertion red at 174 bytes.
    assert!(
        arrived.on_the_wire.len() > 64,
        "the tap recorded only {} bytes, so it is not measuring the stream",
        arrived.on_the_wire.len()
    );
    assert!(
        !wire.contains(MARKER),
        "the marker appeared in the {} bytes the forwarder wrote, so the payload was not \
         ciphertext",
        arrived.on_the_wire.len()
    );
    assert!(
        up > 0 && down > 0,
        "a completed nested handshake moves bytes both ways: up {up}, down {down}"
    );

    let events = log.events.lock().expect("the collector lock").clone();
    for event in &events {
        assert!(
            !event.message.contains(MARKER),
            "the forwarder logged the payload: {}",
            event.message
        );
        for (name, value) in &event.fields {
            assert!(
                !value.contains(MARKER),
                "the forwarder logged the payload in field {name}: {value}"
            );
        }
    }
    // Filtered by the requester, because the log is the whole binary's: every
    // node in this file has its own fresh key, so this names exactly one
    // forward.
    let mine = a.id.display();
    let carried: Vec<&Logged> = events
        .iter()
        .filter(|event| {
            event.message == "peer forward: carried"
                && event.fields.get("peer").is_some_and(|peer| peer == &mine)
        })
        .collect();
    assert_eq!(
        carried.len(),
        1,
        "one forward, one line for {mine}: {events:?}"
    );
    let mut named: Vec<&str> = carried[0].fields.keys().map(String::as_str).collect();
    named.sort_unstable();
    assert_eq!(
        named,
        vec![
            "bytes_down",
            "bytes_up",
            "hops_left",
            "ms",
            "onward",
            // WHICH way the target was reached, a socket this Mac
            // opened or a carrier the target parked here. It is a word from a
            // closed set ([`tunnel::ReachedTarget::label`]) and this
            // assertion is what keeps that true: a field derived from the
            // stream could not be one.
            "path",
            "peer"
        ],
        "the forwarder's log line carries who, where, by which path, how deep, how much and \
         how long, and nothing that could come from the stream"
    );
}

/// A target this Mac never pinned is refused, and the target hears nothing.
///
/// The error is the weaker half of this: the assertion that matters is that a
/// Mac sitting on the LAN, pinned by the requester but not by the forwarder,
/// received no connection at all.
#[tokio::test]
async fn a_forward_to_an_unpinned_target_is_refused_with_zero_bytes_to_it() {
    let (a, b, c) = (node(), node(), node());
    let target_listener = loopback().await;
    let target_addr = target_listener.local_addr().expect("the target address");
    let target = target_on(target_listener, vec![7_u8; 16]);

    let forwarder_listener = loopback().await;
    let forwarder_addr = forwarder_listener
        .local_addr()
        .expect("the forwarder address");
    let forwarding = forwarder_on(
        forwarder_listener,
        Forwarder {
            secret: c.secret,
            node: c.id,
            // A is pinned with `relay`; B is NOT in this file at all, even
            // though its address is live and the requester named it.
            store: store(
                "unpinned",
                1,
                vec![row(&a.id, "requester", Vec::new(), true)],
            ),
            target: b.id,
            cap_bytes: 1024 * 1024,
            budget: Arc::new(Mutex::new(TunnelBudget::new())),
        },
    );

    let mut asked = ask_to_forward(forwarder_addr, &a, &c, &forward_header(1, Vec::new())).await;
    let _written = asked.write_all(&[3_u8; 32]).await;
    let refusal = forwarding
        .await
        .expect("the forwarder task joins")
        .expect_err("an unpinned target is refused");
    let text = format!("{refusal:#}");
    assert!(
        text.contains("has not pinned"),
        "the refusal must be about the missing row: {text}"
    );
    assert!(
        !text.contains(&target_addr.to_string()),
        "the refusal must not name an address nobody pinned: {text}"
    );
    assert_eq!(
        (
            target.connections.load(Ordering::SeqCst),
            target.bytes.load(Ordering::SeqCst)
        ),
        (0, 0),
        "an unpinned target must receive no connection and no byte"
    );
}

/// The hop after the first forward is refused: one forward hop is the whole
/// budget at `maxHops` 1.
///
/// This is the second-hop case as a node actually meets it. A spent its one
/// hop asking the forwarder, so the frame that reaches the NEXT Mac carries a
/// spent budget, and that Mac refuses before it dials anyone, even though it
/// pins the onward target and grants the requester `relay`.
#[tokio::test]
async fn a_second_forward_hop_is_refused() {
    let (a, b, onward) = (node(), node(), node());
    let target_listener = loopback().await;
    let target_addr = target_listener.local_addr().expect("the target address");
    let target = target_on(target_listener, vec![7_u8; 16]);

    let forwarder_listener = loopback().await;
    let forwarder_addr = forwarder_listener
        .local_addr()
        .expect("the forwarder address");
    let forwarding = forwarder_on(
        forwarder_listener,
        Forwarder {
            secret: b.secret,
            node: b.id,
            store: store(
                "second-hop",
                1,
                vec![
                    row(&a.id, "requester", Vec::new(), true),
                    row(&onward.id, "onward", vec![target_addr.to_string()], false),
                ],
            ),
            target: onward.id,
            cap_bytes: 1024 * 1024,
            budget: Arc::new(Mutex::new(TunnelBudget::new())),
        },
    );

    // Zero hops left: the budget was spent one Mac ago.
    let mut asked = ask_to_forward(forwarder_addr, &a, &b, &forward_header(0, Vec::new())).await;
    let _written = asked.write_all(&[3_u8; 32]).await;
    let refusal = forwarding
        .await
        .expect("the forwarder task joins")
        .expect_err("a frame with a spent hop budget is refused");
    let text = format!("{refusal:#}");
    assert!(
        text.contains("hop budget spent") || text.contains("no hops left"),
        "the refusal must be about the hop budget: {text}"
    );
    assert_eq!(
        target.connections.load(Ordering::SeqCst),
        0,
        "a second hop must cost the onward Mac nothing"
    );
}

// ---------------------------------------------------------------------------
// The REQUESTER's half
//
// Everything above measures a Mac that agrees to carry. Nothing above opens a
// forward, and until this file nothing in the tree did: `dial_peer` logged a
// `Via` endpoint and skipped it, so a peer reachable only through a friend was
// reachable by nobody. The cases below drive the production dial
// (`serve::dial_peer_reaching`) rather than this file's hand-rolled
// `ask_to_forward`, which is the difference between "a forward works when a
// test writes the header" and "a borrow finds its way home".
// ---------------------------------------------------------------------------

/// A [`PeerStore`] over a peers file in a directory the CALLER holds, so this
/// node's keypair can be minted beside it.
///
/// [`store`] makes its own scratch directory and keeps it; the dial under test
/// reads `NodeKey::load_or_mint(peers_path.parent())`, so a test that wants to
/// know which key the dialler will present has to own that directory.
fn store_in(dir: &std::path::Path, max_hops: u8, rows: Vec<PeerRow>) -> PeerStore {
    let path = dir.join("tcr-peers.json");
    config::save(
        &path,
        &PeerFile {
            max_hops,
            peers: rows,
            ..PeerFile::default()
        },
    )
    .expect("write the peers file");
    PeerStore::open(&path).expect("open the peers file")
}

/// A pinned row this node may ASK to carry for it: `allow.carry`, the
/// direction [`teamclaude_rs::peer::config::Allow::carry`]'s own doc separates
/// from `gateway`.
fn carry_row(peer: &PeerId, label: &str, addrs: Vec<String>) -> PeerRow {
    let mut row = row(peer, label, addrs, false);
    row.allow.carry = true;
    row
}

/// A row whose ONLY endpoint is a forwarded hop through `via`.
fn via_row(peer: &PeerId, label: &str, via: PeerId) -> PeerRow {
    let mut row = row(peer, label, Vec::new(), false);
    row.endpoints = vec![Endpoint::via(via, NOW_MS, EndpointSource::Hello)];
    row
}

/// Stand up C (the forwarder) and B (the target), and return C's address plus
/// both tasks.
///
/// One function because all three cases below want the same two listeners and
/// differ only in what A's own peers file says about how to get to B.
///
/// `carried` is how many carried streams A will open before it is done, and
/// every caller says its own number rather than inheriting one. ONE is the
/// number a dial spends on the friend it carries through, and it is the
/// assertion rather than a fixture detail: the punch step's address exchange
/// (`serve::exchange_addresses`) asks the same friend over its own connection,
/// so a dial that asked it here would open two and leave the borrow the
/// second, which is what the reverse path has no spare carrier for.
async fn forwarder_and_target(
    tag: &str,
    a: &PeerId,
    b: &Node,
    c: &Node,
    carried: usize,
) -> (
    std::net::SocketAddr,
    tokio::task::JoinHandle<anyhow::Result<(u64, u64)>>,
    tokio::task::JoinHandle<anyhow::Result<Arrived>>,
) {
    // B pins A and NOT C, so the nested handshake has to prove A or die.
    let target_listener = loopback().await;
    let target_addr = target_listener.local_addr().expect("the target address");
    let target = responder_rounds_on(
        target_listener,
        b.secret,
        vec![row(a, "requester", Vec::new(), false)],
        carried,
    );

    let forwarder_listener = loopback().await;
    let forwarder_addr = forwarder_listener
        .local_addr()
        .expect("the forwarder address");
    let forwarding = forwarder_rounds_on(
        forwarder_listener,
        Forwarder {
            secret: c.secret,
            node: c.id,
            store: store(
                tag,
                1,
                vec![
                    row(a, "requester", Vec::new(), true),
                    row(&b.id, "target", vec![target_addr.to_string()], false),
                ],
            ),
            target: b.id,
            cap_bytes: 1024 * 1024,
            budget: Arc::new(Mutex::new(TunnelBudget::new())),
        },
        carried,
    );
    (forwarder_addr, forwarding, target)
}

/// Run the nested session over `stream` and hand back what reached B.
///
/// The handshake is against B's pinned key over whatever the dial returned,
/// which is the claim being made: a forwarded stream is not a second kind of
/// stream to its caller.
async fn speak_to_target(
    stream: teamclaude_rs::peer::serve::PeerStream,
    secret: &[u8; 32],
    target: &PeerId,
) {
    let mut stream = stream;
    let mut nested = noise::dial_handshake(
        &mut stream,
        secret,
        Handshake::Return,
        Some(&target.0),
        None,
    )
    .await
    .expect("the nested handshake completes through the forwarder");
    noise::send_encrypted(&mut stream, &mut nested.transport, MARKER.as_bytes())
        .await
        .expect("the marker is written inside the nested session");
    // The pump behind `NoiseStream` is what actually writes to the socket, and
    // dropping the stream without draining it races the last frame out of the
    // process. Half-closing instead lets the forwarder see EOF after the frame.
    drop(stream);
}

/// **A row whose only endpoint is a `Via` hop is dialled through the Mac it
/// names, and the peer at the far end authenticates the DIALLER.**
///
/// This is the client case in one assertion: A holds no address for B
/// at all, only "B is reachable through C", and the production dial turns that
/// into a live session with B. Nothing in the tree could do this before: the
/// same row against `dial_peer` returns `None`, which is what the sibling case
/// below measures so this one cannot pass by accident.
#[tokio::test]
async fn a_via_endpoint_is_dialled_through_the_forwarder_it_names() {
    let (b, c) = (node(), node());
    let dir = scratch("client-via");
    let key = teamclaude_rs::peer::id::NodeKey::load_or_mint(&dir).expect("A mints its keypair");
    let (forwarder_addr, forwarding, target) =
        // One carried stream: no Mac here holds `allow.carry`, so the address
        // exchange inside the punch step has no carrier to ask and spends
        // nothing before the `Via` endpoint is followed.
        forwarder_and_target("client-via-forwarder", &key.id(), &b, &c, 1).await;

    // A's whole peers file: C, reachable and holding NO `allow.carry`; B,
    // reachable only through C.
    //
    // **C is deliberately not a carry grantee, and that is what makes this
    // test about the `Via` arm.** With the grant set, `forwarders_for` also
    // returns C and the dial reaches B down the fallback path instead: this
    // test passed with the `Via` arm mutated dead until the grant came off,
    // which is the vacuous pass mutation testing exists to find. The two
    // authorities are different acts: a `Via` endpoint is the operator naming
    // the hop for THIS peer, `allow.carry` is this node being allowed to pick
    // a carrier by itself, and the case below measures that one.
    let a_store = store_in(
        &dir,
        1,
        vec![
            row(&c.id, "forwarder", vec![forwarder_addr.to_string()], false),
            via_row(&b.id, "target", c.id),
        ],
    );
    assert!(
        teamclaude_rs::peer::serve::forwarders_for(&b.id, &a_store).is_empty(),
        "no Mac here holds `allow.carry`, so the carry-grantee fallback has nothing to \
         offer and the `Via` endpoint is the only way this dial can succeed"
    );
    let b_row = a_store.row(&b.id).expect("A pinned B");

    let stream = teamclaude_rs::peer::serve::dial_peer_reaching(&b_row, &a_store)
        .await
        .expect("the `Via` endpoint is followed through C");
    speak_to_target(stream, key.secret_bytes(), &b.id).await;

    let (up, down) = forwarding
        .await
        .expect("the forwarder task joins")
        .expect("the forward completes");
    let arrived = target
        .await
        .expect("the target task joins")
        .expect("the target completed a session");

    assert_eq!(
        arrived.authenticated_as,
        key.id(),
        "B must authenticate the Mac that DIALLED, never the one that carried"
    );
    assert_ne!(
        arrived.authenticated_as, c.id,
        "the forwarder's own key must not be what the target saw"
    );
    assert_eq!(
        String::from_utf8_lossy(&arrived.plaintext),
        MARKER,
        "the dialler's plaintext must arrive intact at the far end"
    );
    assert!(
        !String::from_utf8_lossy(&arrived.on_the_wire).contains(MARKER),
        "the payload must be ciphertext in the {} bytes the forwarder wrote",
        arrived.on_the_wire.len()
    );
    assert!(
        up > 0 && down > 0,
        "a completed nested handshake moves bytes both ways: up {up}, down {down}"
    );
}

/// The same row, on the dial that holds no peers file: `None`.
///
/// The control for the case above. Without it, a green forwarded dial says
/// only "a stream came back", not "this row was unreachable and now is not".
#[tokio::test]
async fn the_same_via_row_is_unreachable_to_a_dial_with_no_peers_file() {
    let (b, c) = (node(), node());
    let dialled = teamclaude_rs::peer::serve::dial_peer(&via_row(&b.id, "target", c.id)).await;
    assert!(
        dialled.is_none(),
        "`dial_peer` holds no peers file, so it cannot read the forwarder's row or this \
         node's key, and a `Via` endpoint is all this row has"
    );
}

/// **A peer with NO endpoint at all is still reached, through a Mac this node
/// may ask to carry.**
///
/// The case a row cannot describe. A peer that moved and whose every recorded
/// address went stale has no `Via` endpoint to follow, because nothing in this
/// build writes one; what is left is a mutual friend that IS reachable, and
/// `allow.carry` is the operator's word that this node may ask it.
#[tokio::test]
async fn a_row_with_no_endpoints_is_reached_through_a_carry_grantee() {
    let (b, c) = (node(), node());
    let dir = scratch("client-carry");
    let key = teamclaude_rs::peer::id::NodeKey::load_or_mint(&dir).expect("A mints its keypair");
    let (forwarder_addr, forwarding, target) =
        // ONE carried stream, and it is the borrow's own. This row holds
        // nothing at all, which is the state the punch step names
        // `PeerAddressUnknown` and deliberately does not ask about: asking B
        // for its address through C first would spend a second connection on C
        // and, where C holds a carrier B parked, the carrier this dial needs.
        // The fixture accepts exactly one round, so a dial that opened two
        // fails here with the carry reading as refused, which is what the
        // three-process reverse-path case reports as a borrow that never got
        // served.
        forwarder_and_target("client-carry-forwarder", &key.id(), &b, &c, 1).await;

    let a_store = store_in(
        &dir,
        1,
        vec![
            carry_row(&c.id, "forwarder", vec![forwarder_addr.to_string()]),
            row(&b.id, "target", Vec::new(), false),
        ],
    );
    let b_row = a_store.row(&b.id).expect("A pinned B");
    assert!(
        !b_row.has_endpoint(),
        "the whole point of this case is a row with nowhere to dial"
    );
    assert!(
        teamclaude_rs::peer::serve::has_a_way_back(&b_row, &a_store),
        "a row with no endpoint and a reachable carry grantee still has a way back, and a \
         borrow that filtered on `has_endpoint` would skip exactly this lender"
    );

    let stream = teamclaude_rs::peer::serve::dial_peer_reaching(&b_row, &a_store)
        .await
        .expect("the carry grantee is asked once the row itself answers nothing");
    speak_to_target(stream, key.secret_bytes(), &b.id).await;

    forwarding
        .await
        .expect("the forwarder task joins")
        .expect("the forward completes");
    let arrived = target
        .await
        .expect("the target task joins")
        .expect("the target completed a session");
    assert_eq!(
        arrived.authenticated_as,
        key.id(),
        "B must authenticate the Mac that dialled"
    );
    assert_eq!(
        String::from_utf8_lossy(&arrived.plaintext),
        MARKER,
        "the dialler's plaintext must arrive intact"
    );
}

/// Who may be asked to carry, and who may not: the three exclusions, each
/// measured against a candidate that differs in exactly one field.
#[test]
fn only_a_reachable_carry_grantee_that_is_not_the_target_is_asked() {
    let (b, c, d, e) = (node(), node(), node(), node());
    let dir = scratch("client-candidates");
    let a_store = store_in(
        &dir,
        1,
        vec![
            // Asked: the grant, an address, and not the target.
            carry_row(&c.id, "carrier", vec!["127.0.0.1:1".to_string()]),
            // Not asked: no `allow.carry`, address and all.
            row(&d.id, "no-grant", vec!["127.0.0.1:2".to_string()], true),
            // Not asked: the grant, and nowhere to dial it.
            carry_row(&e.id, "unreachable-carrier", Vec::new()),
            // Not asked: it IS the target.
            carry_row(&b.id, "target", vec!["127.0.0.1:3".to_string()]),
        ],
    );
    assert_eq!(
        teamclaude_rs::peer::serve::forwarders_for(&b.id, &a_store),
        vec![c.id],
        "a carrier needs `allow.carry` (not `relay`, which is the other direction), a \
         direct address of its own, and must not be the Mac being reached"
    );
    // And a row with no way back at all is exactly that, so the borrow filter
    // above cannot be vacuously true.
    let bare = store_in(
        &scratch("client-bare"),
        1,
        vec![row(&b.id, "target", Vec::new(), false)],
    );
    assert!(
        !teamclaude_rs::peer::serve::has_a_way_back(&bare.row(&b.id).expect("pinned"), &bare),
        "no endpoint and no carry grantee is no way back"
    );
}

/// **A lender with no address of its own still installs the borrowing seam,
/// when a Mac this node may ask can carry to it.**
///
/// `fallback::peer_lease_provider` decides once, at boot, whether this process
/// consults the peer-lease seam at all. It held a second copy of the filter
/// `PeerLeaseProvider::try_serve` asks per request, and the copies disagreed
/// after the forward path landed: `try_serve` asks `serve::has_a_way_back`, the
/// installer asked `row.has_endpoint()`. So a node that restarted while its
/// only lender had no address installed NO provider, `try_serve` was never
/// called, and the forwarder it would have found was unreachable until that
/// lender announced an address of its own. Nothing failed; the seam was simply
/// off.
///
/// The two negative legs are what make this about the CARRIER and not about
/// the lender: take the carry grant away, or take the carrier's own address
/// away, and the provider goes back to `None`, because there is then no Mac to
/// ask. Without them a bug that installed a provider unconditionally would
/// pass the first assertion.
///
/// Watched red by putting `row.has_endpoint()` back in
/// `fallback::peer_lease_provider`: the first assertion fails, and it is the
/// boot this bug hides behind.
#[test]
fn a_lender_with_no_address_still_installs_the_provider_through_a_carrier() {
    let lender = PeerId([0x21; 32]);
    let carrier = PeerId([0x22; 32]);

    let lender_row = |disclose: bool| PeerRow {
        node: lender,
        label: "attic-nuc".to_string(),
        // No address at all: the state a lender that moved leaves behind.
        endpoints: Vec::new(),
        added_at: NOW_MS,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: Allow {
            allow_disclose: disclose,
            ..Allow::default()
        },
        lend: Vec::new(),
    };
    let carrier_row = |carry: bool, addrs: Vec<String>| {
        let mut built = row(&carrier, "hallway-mini", addrs, false);
        built.allow.carry = carry;
        built
    };

    assert!(
        teamclaude_rs::fallback::peer_lease_provider(&store(
            "carrier-installs",
            2,
            vec![
                lender_row(true),
                carrier_row(true, vec!["127.0.0.1:9701".to_string()]),
            ],
        ))
        .is_some(),
        "a lender with no address of its own is still reachable through a Mac this node \
         may ask to carry, so the seam has to be installed or nothing ever asks"
    );

    assert!(
        teamclaude_rs::fallback::peer_lease_provider(&store(
            "carrier-no-grant",
            2,
            vec![
                lender_row(true),
                carrier_row(false, vec!["127.0.0.1:9702".to_string()]),
            ],
        ))
        .is_none(),
        "the carry grant is what makes that Mac askable: without it there is no way back \
         and the provider would walk an unreachable lender on every dry-fleet request"
    );

    assert!(
        teamclaude_rs::fallback::peer_lease_provider(&store(
            "carrier-no-address",
            2,
            vec![lender_row(true), carrier_row(true, Vec::new())],
        ))
        .is_none(),
        "and a carrier with no address of its own cannot be asked either, so neither row \
         offers a way back"
    );
}

// ---------------------------------------------------------------------------
// The reverse carry
// ---------------------------------------------------------------------------

/// A pinned row with no address at all, which is what an undialable Mac's row
/// looks like on its friend: nothing it could advertise would be dialable, so
/// nothing was written down.
fn undialable_row(peer: &PeerId, label: &str) -> PeerRow {
    row(peer, label, Vec::new(), false)
}

/// A socket to `addr`, boxed as the transport a desk holds.
///
/// This is the shape of the real thing rather than a stand-in: the carrier a
/// Mac parks IS a socket it opened outwards, so a client socket to the Mac
/// playing the target is the same object in the same direction.
async fn carrier_to(addr: std::net::SocketAddr) -> teamclaude_rs::peer::serve::PeerStream {
    Box::new(
        TcpStream::connect(addr)
            .await
            .expect("open a carrier to the target"),
    )
}

/// **A forward reaches a Mac whose row has no address, over the carrier that
/// Mac parked here.**
///
/// The whole of the reverse path, from the forwarder's side: B is behind a NAT
/// that maps by destination, so nothing A or C writes down can be dialled and
/// B's row on C carries no address at all. B opened a socket to C instead, C
/// parked it, and A's forward rides back out over it. Before the desk existed
/// this forward could only fail: `dial_pinned_peer` had an empty endpoint list
/// and there was no second way to try.
///
/// Watched red by putting the old body back, `dial_pinned_peer(&plan.target)`
/// in place of `reach_target(&plan.target, carry.now_ms)` in
/// `tunnel::handle_forward_on`: the target sees zero bytes and the forward
/// ends "none of its pinned addresses answered".
#[tokio::test]
async fn a_forward_rides_the_carrier_an_undialable_target_parked() {
    let (a, b, c) = (node(), node(), node());
    let target_listener = loopback().await;
    let target_addr = target_listener.local_addr().expect("the target address");
    let target = target_on(target_listener, vec![9_u8; 8]);

    // B's own socket, opened outwards to the Mac that will carry for it.
    let parked = carrier_to(target_addr).await;
    let desk_store = store("reverse-desk", 1, vec![undialable_row(&b.id, "attic")]);
    assert_eq!(
        tunnel::park_reverse_carry(&desk_store, b.id, parked, NOW_MS),
        tunnel::ParkOutcome::Parked { open: 1 },
        "a pinned Mac's carrier is held"
    );

    let budget = Arc::new(Mutex::new(TunnelBudget::new()));
    let forwarder_listener = loopback().await;
    let forwarder_addr = forwarder_listener
        .local_addr()
        .expect("the forwarder address");
    let task = forwarder_on(
        forwarder_listener,
        Forwarder {
            secret: c.secret,
            node: c.id,
            store: store(
                "reverse-forward",
                1,
                vec![
                    row(&a.id, "requester", Vec::new(), true),
                    // No address: the only way to B is the carrier above.
                    undialable_row(&b.id, "attic"),
                ],
            ),
            target: b.id,
            cap_bytes: 1024 * 1024,
            budget: Arc::clone(&budget),
        },
    );

    let mut asked = ask_to_forward(forwarder_addr, &a, &c, &forward_header(1, Vec::new())).await;
    asked
        .write_all(&[4_u8; 24])
        .await
        .expect("the requester writes the first bytes of its nested session");
    let mut back = [0_u8; 8];
    asked
        .read_exact(&mut back)
        .await
        .expect("the target's answer comes back over the carrier");
    assert_eq!(back, [9_u8; 8], "and it is the target's own bytes");
    drop(asked);

    let (up, down) = task
        .await
        .expect("the forwarder task joins")
        .expect("the forward succeeded");
    assert_eq!(
        target.bytes.load(Ordering::SeqCst),
        24,
        "every byte the requester wrote reached the Mac nothing could dial"
    );
    assert_eq!(
        target.connections.load(Ordering::SeqCst),
        1,
        "and it reached it over the ONE socket that Mac opened itself; a forward that had \
         dialled would need a second connection and an address that does not exist"
    );
    assert!(
        up >= 24 && down >= 8,
        "both directions are counted: {up}/{down}"
    );

    // Spent, and the desk has forgotten it: one connection is one stream, so
    // the next forward needs the next carrier.
    assert_eq!(
        tunnel::reverse_desk()
            .lock()
            .expect("the desk lock")
            .open_for(&b.id, NOW_MS),
        0,
        "a carrier serves one forward and is gone"
    );
}

/// **A carrier past its TTL is dropped rather than handed to a requester, and
/// a peer cannot hold more than its share of the desk.**
///
/// Both halves are what keeps a desk from becoming a place sockets accumulate:
/// a Mac that went to sleep leaves its carriers behind, and a requester
/// spliced into one of those would wait out its own timeout on a socket whose
/// far end is gone. The cap is the other direction, one friend cannot take
/// every slot.
#[test]
fn the_desk_forgets_a_stale_carrier_and_caps_what_one_peer_holds() {
    let peer = PeerId([0x31; 32]);
    let store = store("reverse-ttl", 1, vec![undialable_row(&peer, "attic")]);

    let (held, _far) = tokio::io::duplex(64);
    assert_eq!(
        tunnel::park_reverse_carry(&store, peer, Box::new(held), NOW_MS),
        tunnel::ParkOutcome::Parked { open: 1 }
    );
    assert!(
        tunnel::reverse_desk()
            .lock()
            .expect("the desk lock")
            .take(&peer, NOW_MS + tunnel::REVERSE_PARK_TTL_MS)
            .is_none(),
        "a carrier exactly at its TTL is past it, and nothing is spliced into it"
    );

    // The far ends are kept alive for the rest of the test: a duplex whose
    // other half is dropped reads EOF, and these carriers are meant to sit
    // there being counted.
    let mut far_ends = Vec::new();
    for open in 1..=tunnel::MAX_PARKED_PER_PEER {
        let (held, far) = tokio::io::duplex(64);
        far_ends.push(far);
        assert_eq!(
            tunnel::park_reverse_carry(&store, peer, Box::new(held), NOW_MS),
            tunnel::ParkOutcome::Parked { open },
        );
    }
    let (held, far) = tokio::io::duplex(64);
    far_ends.push(far);
    assert_eq!(
        tunnel::park_reverse_carry(&store, peer, Box::new(held), NOW_MS),
        tunnel::ParkOutcome::Refused(tunnel::ParkRefusal::DeskFull {
            open: tunnel::MAX_PARKED_PER_PEER
        }),
        "the cap is per peer, and the refusal says how many that peer already holds"
    );
}

/// **A Mac this node never pinned cannot leave a socket here.**
///
/// The same admission `authorize_forward` makes on the other half of the same
/// forward: a desk slot is a thing a stranger must not be able to take, and a
/// carrier held for an unpinned Mac could never be used anyway, because the
/// forward that would spend it is refused for the same reason.
#[test]
fn a_carrier_from_an_unpinned_mac_is_refused() {
    let stranger = PeerId([0x32; 32]);
    let pinned = PeerId([0x33; 32]);
    let store = store(
        "reverse-stranger",
        1,
        vec![undialable_row(&pinned, "attic")],
    );
    let (held, _far) = tokio::io::duplex(64);
    assert_eq!(
        tunnel::park_reverse_carry(&store, stranger, Box::new(held), NOW_MS),
        tunnel::ParkOutcome::Refused(tunnel::ParkRefusal::NotPinned)
    );
    assert_eq!(
        tunnel::reverse_desk()
            .lock()
            .expect("the desk lock")
            .open_for(&stranger, NOW_MS),
        0,
        "and nothing was held for it"
    );
}

/// **A node that can be dialled asks nobody to carry for it.**
///
/// The decision the keeper makes before it opens anything, over the two facts
/// `reach` measures. Either a router mapping or a global IPv6 address means a
/// peer can open a socket to this Mac, and a node that asked for a carrier
/// anyway would be spending a third Mac's bytes on a path it does not need.
#[test]
fn a_reachable_node_wants_no_carrier_and_an_unreachable_one_does() {
    use teamclaude_rs::peer::reach::{MapProtocol, Mapping};
    use teamclaude_rs::peer::tunnel::{ReverseNeed, ReverseNotWanted};

    let mapping = Mapping {
        protocol: MapProtocol::Tcp,
        internal_port: 4100,
        external_port: 41000,
        lifetime_secs: 3600,
        epoch_secs: 10,
    };
    let global_v6: std::net::Ipv6Addr = "2001:db8::1".parse().expect("a global v6 literal");

    assert_eq!(
        tunnel::reverse_carry_is_wanted(Some(&mapping), &[]),
        ReverseNeed::NotWanted(ReverseNotWanted::HoldsMapping)
    );
    assert_eq!(
        tunnel::reverse_carry_is_wanted(None, &[global_v6]),
        ReverseNeed::NotWanted(ReverseNotWanted::HasGlobalV6)
    );
    assert_eq!(
        tunnel::reverse_carry_is_wanted(None, &[]),
        ReverseNeed::Wanted,
        "no mapping and no global address is the case a friend's open socket exists for"
    );
}

/// **The Macs asked to carry for this node are the carry grantees, and the
/// list is read off the one function that already answers that question.**
///
/// "Who may carry a stream to X" with X set to this node IS "who may carry for
/// me", so a second list would be a second answer: this asserts the two agree,
/// including the exclusions that make it true (no grant, and no address of its
/// own).
#[test]
fn the_macs_asked_to_carry_for_this_node_are_its_carry_grantees() {
    let me = PeerId([0x34; 32]);
    let friend = PeerId([0x35; 32]);
    let ungranted = PeerId([0x36; 32]);
    let unreachable = PeerId([0x37; 32]);
    let store = store(
        "reverse-carriers",
        1,
        vec![
            carry_row(&friend, "hallway-mini", vec!["127.0.0.1:9801".to_string()]),
            row(
                &ungranted,
                "no-grant",
                vec!["127.0.0.1:9802".to_string()],
                true,
            ),
            carry_row(&unreachable, "no-address", Vec::new()),
        ],
    );
    assert_eq!(
        tunnel::reverse_carriers(&store, &me)
            .into_iter()
            .map(|row| row.node)
            .collect::<Vec<_>>(),
        vec![friend],
        "the carry grant plus an address of its own, and nothing else"
    );
}

/// **The keeper opens the next carrier as soon as one is spent, and a friend
/// that refuses costs one wait and not the loop.**
///
/// Nothing is ever written into a parked carrier, so "keeping it alive" is
/// this: open, wait, be spent, open again. The seam is the opener, which is
/// what lets this run with no socket at all and still test the only thing the
/// loop decides.
#[tokio::test]
async fn the_keeper_reopens_a_spent_carrier_and_waits_out_a_refusal() {
    let friend = carry_row(
        &PeerId([0x38; 32]),
        "hallway-mini",
        vec!["127.0.0.1:9803".to_string()],
    );
    let attempts = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&attempts);

    let run = tunnel::keep_reverse_carrier(
        friend,
        // A millisecond rather than the probe cadence: the wait is a real
        // `sleep` on the refusal path and this test runs it, it is the LENGTH
        // that is the caller's and has no business being waited out here.
        std::time::Duration::from_millis(1),
        Some(4),
        move |_row| {
            let seen = Arc::clone(&seen);
            async move {
                // Refused twice, then carried twice.
                let round = seen.fetch_add(1, Ordering::SeqCst);
                if round < 2 {
                    anyhow::bail!("that Mac is not holding a carrier for us");
                }
                Ok(())
            }
        },
    )
    .await;

    assert_eq!(attempts.load(Ordering::SeqCst), 4, "every round ran");
    assert_eq!(
        run,
        tunnel::ReverseKeeperRun {
            carried: 2,
            failed: 2
        },
        "both outcomes are counted, and a refusal is a wait rather than the end of the loop"
    );
}

/// **A lender this Mac can dial is asked before one that needs a third Mac to
/// carry.**
///
/// The order used to be the peers file's, so a lender reachable only through a
/// forwarder could be asked first and put a third machine's bytes and a second
/// hop's latency in front of a socket this node could open itself. The row
/// with no address is still asked, last, which is the half that keeps this
/// from being a filter: an undialable lender is exactly who the carry grant
/// and the carrier desk exist for.
#[test]
fn a_dialable_lender_is_asked_before_one_that_needs_carrying() {
    let dialable = PeerId([0x41; 32]);
    let carried = PeerId([0x42; 32]);
    let carrier = PeerId([0x43; 32]);

    let disclosing = |peer: &PeerId, label: &str, addrs: Vec<String>| {
        let mut built = row(peer, label, addrs, false);
        built.allow.allow_disclose = true;
        built
    };
    let store = store(
        "lender-order",
        1,
        vec![
            // First in the file, and reachable only through the carrier.
            disclosing(&carried, "attic", Vec::new()),
            disclosing(&dialable, "desk-mini", vec!["127.0.0.1:9901".to_string()]),
            carry_row(&carrier, "hallway-mini", vec!["127.0.0.1:9902".to_string()]),
        ],
    );

    let asked: Vec<PeerId> = teamclaude_rs::peer::lease::lenders_in_ask_order(&store)
        .into_iter()
        .map(|row| row.node)
        .collect();
    assert_eq!(
        asked.first(),
        Some(&dialable),
        "the lender with an address of its own is asked first, file order and all"
    );
    assert!(
        asked.contains(&carried),
        "and the one that needs carrying is still asked: {asked:?}"
    );
    assert!(
        !asked.contains(&carrier),
        "and the carrier itself is not a lender: it never granted disclosure: {asked:?}"
    );
}

/// **With neither a parked carrier nor an address that answers, the forwarder
/// says both halves in one sentence.**
///
/// The two are different operator problems, an address that went stale and a
/// Mac that can never be dialled at all, and before the desk existed only the
/// second could be said. The sentence is asserted here rather than in
/// `tests/peer_e2e.rs` because a refusal reaches a real peer's log through
/// `listener::RefusalLog`, a per-address rate limit another refusal in the
/// same run can spend.
#[tokio::test]
async fn a_target_with_no_carrier_and_no_address_is_refused_naming_both() {
    let (a, b, c) = (node(), node(), node());
    let budget = Arc::new(Mutex::new(TunnelBudget::new()));
    let forwarder_listener = loopback().await;
    let forwarder_addr = forwarder_listener
        .local_addr()
        .expect("the forwarder address");
    let task = forwarder_on(
        forwarder_listener,
        Forwarder {
            secret: c.secret,
            node: c.id,
            store: store(
                "reverse-neither",
                1,
                vec![
                    row(&a.id, "requester", Vec::new(), true),
                    undialable_row(&b.id, "attic"),
                ],
            ),
            target: b.id,
            cap_bytes: 1024 * 1024,
            budget: Arc::clone(&budget),
        },
    );

    let mut asked = ask_to_forward(forwarder_addr, &a, &c, &forward_header(1, Vec::new())).await;
    asked
        .write_all(&[5_u8; 16])
        .await
        .expect("the requester writes its first bytes");
    let refusal = task
        .await
        .expect("the forwarder task joins")
        .expect_err("a target with no way in at all cannot be forwarded to")
        .to_string();
    assert!(
        refusal.contains("has parked no carrier here and none of its pinned addresses answered"),
        "the refusal names the desk and the row, in that order: {refusal}"
    );
}
