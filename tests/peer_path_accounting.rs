//! Which WAY the bytes and the tokens went,
//! and the two answers `PathStatus` used to refuse to give.
//!
//! # What is real here and what is a stand-in
//!
//! Real: the production forwarder (`tunnel::handle_forward_on`) over two real
//! Noise sessions to a real target socket, the production ledger
//! (`lease::Ledger::debit`), the production meter, the production state-file
//! writer (`state::save_path_traffic`) and the production derivation
//! `tcr peer status --json` prints (`status::peers_block`).
//!
//! Stand-ins, each named at its call site: the forwarder's accept loop, which
//! is `src/peer/listener.rs`'s `StreamKind::Tunnel` arm and is another file's
//! file, the same stand-in `tests/peer_forward.rs` uses, and this file's
//! harness is that one's, copied rather than shared because two test binaries
//! cannot import each other; and the verb itself, because `tcr peer status`
//! asks the RUNNING proxy and the only proxy on this box is serving real
//! traffic. `peers_block` is the one function that fills `payload.peers`
//! (`src/proxy.rs:1278`), so what is asserted here is the JSON the verb prints,
//! read one call earlier.
//!
//! # Two clocks, on purpose
//!
//! Traffic is charged at the clock the production callers use, which is the
//! real one: a forward really did just happen. Every roll-off assertion takes
//! its instants as arguments instead: nothing here sleeps, and no test is
//! allowed to be slow or flaky because an hour is long.
//!
//! # House rules
//!
//! Every socket binds `127.0.0.1:0`, every file is under a
//! process-and-thread-unique scratch directory, no address, account or key is
//! real, and nothing touches the proxy on `127.0.0.1:3456`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tcr_peer_wire::{
    Lease, LeaseUnit, LendScope, PeerId, StreamHeader, StreamKind, TunnelTarget, Window,
};
use teamclaude_rs::peer::config::{
    self, Allow, Endpoint, EndpointSource, Locator, PeerFile, PeerRow, PeerStore,
};
use teamclaude_rs::peer::lease::Ledger;
use teamclaude_rs::peer::listener;
use teamclaude_rs::peer::noise::{self, Handshake};
use teamclaude_rs::peer::tunnel::{
    self, path_meter, Carry, Forward, NoiseStream, OriginRoute, PathMeter, TunnelBudget,
    BUDGET_WINDOW_MS,
};
use teamclaude_rs::peer::{serve, state};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

/// A fixed instant for the FILE fixtures: when a row was pinned, when a lease
/// was granted. Never the instant traffic is charged at: see the module docs.
const PINNED_AT_MS: i64 = 1_767_225_600_000;

/// A scratch directory named after this process and thread, so five lanes
/// running at once never collide on one path.
fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tcr-peer-path-acct-{tag}-{}-{:?}",
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

/// A pinned row with the endpoints spelled out, which is the whole point here:
/// a row with TWO ways to reach one Mac is what makes "the other path is at
/// zero" a fact and not a tautology.
fn row_with(peer: &PeerId, label: &str, endpoints: Vec<Endpoint>, relay: bool) -> PeerRow {
    PeerRow {
        node: *peer,
        label: label.to_string(),
        endpoints,
        added_at: PINNED_AT_MS,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: Allow {
            relay,
            ..Allow::default()
        },
        lend: Vec::new(),
    }
}

/// A real peers file in a scratch directory, and the path it was written to,
/// which is what `status::peers_block` is handed.
fn peers_file(tag: &str, rows: Vec<PeerRow>) -> std::path::PathBuf {
    let path = scratch(tag).join("tcr-peers.json");
    config::save(
        &path,
        &PeerFile {
            max_hops: 1,
            peers: rows,
            ..PeerFile::default()
        },
    )
    .expect("write the peers file");
    path
}

/// The header a requester writes for a forward, with the one field the wire
/// cannot carry left out (`tests/peer_forward.rs` holds that defect's record).
fn forward_header(hops_remaining: u8) -> StreamHeader {
    StreamHeader {
        kind: StreamKind::Tunnel,
        target: None,
        via: Vec::new(),
        hops_remaining,
        request_id: 11,
    }
}

/// What the forwarder under test is, on one Mac.
struct Forwarder {
    secret: [u8; 32],
    node: PeerId,
    store: PeerStore,
    target: PeerId,
    budget: Arc<Mutex<TunnelBudget>>,
    now_ms: i64,
}

/// One forward, as `src/peer/listener.rs`'s `StreamKind::Tunnel` arm will run
/// it: accept the session, read the header, run the PRODUCTION gate, then hand
/// the stream to the PRODUCTION forwarder.
fn forwarder_on(
    listener: TcpListener,
    forwarder: Forwarder,
) -> tokio::task::JoinHandle<anyhow::Result<(u64, u64)>> {
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
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
        // `Peer { node }` and not `Peer(_)`: the variant became a struct one
        // to make it serializable, which this file's own copy branched from
        // before that change.
        let target = TunnelTarget::Peer {
            node: forwarder.target,
        };
        let header = StreamHeader {
            target: Some(target.clone()),
            ..wire
        };
        let row = forwarder.store.row(&session.peer);
        listener::peer_stream_gate_rows(&header, row.as_ref()).map_err(anyhow::Error::new)?;
        let peer = session.peer;
        tunnel::handle_forward_on(
            stream,
            session,
            Carry {
                route: OriginRoute::Peer(forwarder.target),
                peer,
                target: &target,
                hosts: teamclaude_rs::peer::egress::PEER_EGRESS_HOSTS,
                cap_bytes: 1024 * 1024,
                budget: &forwarder.budget,
                now_ms: forwarder.now_ms,
            },
            Forward {
                store: &forwarder.store,
                node: forwarder.node,
                hops_remaining: header.hops_remaining,
                via: &header.via,
            },
        )
        .await
    })
}

/// A target that counts what reached it and answers a fixed string: a forwarder
/// splices raw bytes and has no business knowing its target's protocol.
#[derive(Clone, Default)]
struct Target {
    connections: Arc<AtomicUsize>,
}

fn target_on(listener: TcpListener, reply: Vec<u8>) -> Target {
    let target = Target::default();
    let watch = target.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            watch.connections.fetch_add(1, Ordering::SeqCst);
            let reply = reply.clone();
            tokio::spawn(async move {
                let mut buf = vec![0_u8; 4096];
                let mut answered = false;
                loop {
                    match socket.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(_) => {
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
/// the plaintext stream the forwarder splices onward.
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

/// One request carried through the forwarder, end to end, as the requester
/// experiences it: 32 bytes up, 16 back.
async fn carry_one_request(
    requester: &Node,
    forwarder_node: &Node,
    store: PeerStore,
    target: PeerId,
    budget: &Arc<Mutex<TunnelBudget>>,
    now_ms: i64,
) -> (u64, u64) {
    let listener = loopback().await;
    let addr = listener.local_addr().expect("the forwarder address");
    let task = forwarder_on(
        listener,
        Forwarder {
            secret: forwarder_node.secret,
            node: forwarder_node.id,
            store,
            target,
            budget: Arc::clone(budget),
            now_ms,
        },
    );
    let mut asked = ask_to_forward(addr, requester, forwarder_node, &forward_header(1)).await;
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
    task.await
        .expect("the forwarder task joins")
        .expect("the forward completes")
}

/// One path out of a status row, by the endpoint string the panel reads.
fn path_of(
    peers: &[teamclaude_rs::status::PathStatus],
    endpoint: &str,
) -> teamclaude_rs::status::PathStatus {
    peers
        .iter()
        .find(|path| path.endpoint == endpoint)
        .unwrap_or_else(|| panic!("no path for {endpoint} in {peers:?}"))
        .clone()
}

// ---------------------------------------------------------------------------
// This file's named gate
// ---------------------------------------------------------------------------

/// **The gate.** Two requests carried over ONE of a peer's two paths, and
/// `tcr peer status --json` reports that path's bytes and tokens above zero
/// while the other path's are zero.
///
/// Every layer between the carry and the JSON is the production one: the
/// forwarder charges the meter keyed on the address it dialled, the ledger
/// charges the tokens it debited on the path the borrow was served over, the
/// meter is summed into the state file by its own section writer, and the
/// status derivation reads it back against the peers file.
///
/// The second path is a `Via` hop, which is a path this build records and does
/// not dial, so it is a real endpoint of the same row that genuinely carried
/// nothing, rather than a second address contrived to fail.
///
/// Watch it fail by deleting the `path_meter()` charge at the end of
/// `handle_forward_on`: both paths then report zero bytes, which is the
/// "measured, and nothing went this way" answer being given about a path that
/// carried everything.
#[tokio::test]
async fn two_carried_requests_land_on_one_path_and_the_other_stays_at_zero() {
    let (requester, target_peer, me) = (node(), node(), node());
    let elsewhere = node();
    let now_ms = teamclaude_rs::now_ms();

    let target_listener = loopback().await;
    let target_addr = target_listener.local_addr().expect("the target address");
    let target = target_on(target_listener, vec![7_u8; 16]);

    let carried = Locator::Direct { addr: target_addr };
    let peers_path = peers_file(
        "gate",
        vec![
            row_with(&requester.id, "requester", Vec::new(), true),
            row_with(
                &target_peer.id,
                "target",
                vec![
                    Endpoint::direct(target_addr, PINNED_AT_MS, EndpointSource::Paired),
                    Endpoint::via(elsewhere.id, PINNED_AT_MS, EndpointSource::Hello),
                ],
                false,
            ),
        ],
    );
    let budget = Arc::new(Mutex::new(TunnelBudget::new()));

    for _ in 0..2 {
        let (up, down) = carry_one_request(
            &requester,
            &me,
            PeerStore::open(&peers_path).expect("open the peers file"),
            target_peer.id,
            &budget,
            now_ms,
        )
        .await;
        assert_eq!((up, down), (32, 16), "the meter is the two byte counts");
    }
    assert_eq!(
        target.connections.load(Ordering::SeqCst),
        2,
        "two requests means the target was dialled twice"
    );

    // The token half, over the same path: a lease this Mac granted the target,
    // measured in tokens, served over the address that answered.
    let mut ledger = Ledger::new();
    ledger.record_scoped(
        Lease {
            lease_id: 0x51,
            window: Window::SevenDay,
            unit: LeaseUnit::Tokens(1_000_000),
            granted_at_ms: now_ms,
            expires_at_ms: now_ms + 300_000,
            spent: 0.0,
            max_inflight: 2,
            until: None,
        },
        target_peer.id,
        LendScope::All,
    );
    ledger.note_lease_path(0x51, carried);
    assert!(
        ledger.debit(0x51, 0x99, 0.01) > 0.0,
        "the relayed request is charged"
    );

    // What the serving process writes, and what the verb reads back.
    let rows = {
        let mut meter = path_meter().lock().expect("the meter lock");
        meter.rows(now_ms)
    };
    let state_path = serve::peer_state_path(&peers_path);
    state::save_path_traffic(&state_path, &rows).expect("write the traffic section");

    let peers = teamclaude_rs::status::peers_block(&peers_path, now_ms);
    // `id` is the WIRE form on a status row, which is the only spelling
    // `PeerId::parse` reads back; `display` beside it is the short form for a
    // person. This lookup matched on the display form, which stopped being
    // `id`'s spelling when the row started carrying both.
    let row = peers
        .iter()
        .find(|row| row.id == target_peer.id.to_wire())
        .expect("the target has a status row");

    let went = path_of(&row.paths, &target_addr.to_string());
    // A forwarded path's `endpoint` is the carrying Mac's WIRE id now, for the
    // reason `peers_block` states: it is the key the panel resolves names by,
    // and the display form could never be found in that map.
    let never = path_of(&row.paths, &elsewhere.id.to_wire());

    assert_eq!(
        went.bytes_per_hour,
        Some(96),
        "two 48-byte forwards over this path, and the figure is the path's, not the peer's"
    );
    assert_eq!(
        went.tokens_per_hour,
        Some(10_000),
        "1 % of a 1 000 000-token lease, on the path it was served over"
    );
    assert_eq!(
        never.bytes_per_hour,
        Some(0),
        "the other path is a MEASURED zero, which is what says the traffic went the other way"
    );
    assert_eq!(
        never.tokens_per_hour,
        Some(0),
        "and no token was drawn over it either"
    );
}

// ---------------------------------------------------------------------------
// The hour rolls, and nothing sleeps
// ---------------------------------------------------------------------------

/// A window older than an hour rolls off, at every layer that holds one.
///
/// Both instants are arguments: the meter's window is trimmed against the clock
/// it is asked at, and a written row is read against the clock the status
/// derivation is asked at. Sleeping for an hour is the alternative and it is
/// not one.
///
/// The boundary is `TunnelBudget::trim`'s: a charge exactly one window old is
/// still inside its hour, and the millisecond after it is not. Both sides are
/// asserted, so a floor that moved by one would fail here rather than quietly
/// disagreeing with the byte cap.
///
/// Watch it fail by widening `PathMeter::trim`'s floor to two windows, or by
/// making `status::rolled` return its total unconditionally.
#[test]
fn an_hour_old_window_rolls_off_at_the_meter_and_at_the_reader() {
    let peer = node();
    let path = Locator::Direct {
        addr: "127.0.0.1:9999".parse().expect("a socket address"),
    };
    // A LOCAL meter, not the process one: this test moves its clock backwards
    // relative to the tests beside it, and a shared window trimmed against a
    // past instant would drop their charges.
    let mut meter = PathMeter::new();
    let at = PINNED_AT_MS;
    meter.charge_bytes(&peer.id, path, 4_096, at);
    meter.charge_tokens(&peer.id, path, 12, at);

    assert_eq!(
        meter.bytes_last_hour(&peer.id, &path, at + BUDGET_WINDOW_MS),
        4_096,
        "exactly one window old is still inside the window, the boundary          `TunnelBudget::trim` already draws"
    );
    assert_eq!(
        meter.bytes_last_hour(&peer.id, &path, at + BUDGET_WINDOW_MS + 1),
        0,
        "a millisecond past it the hour has rolled off"
    );
    assert_eq!(
        meter.tokens_last_hour(&peer.id, &path, at + BUDGET_WINDOW_MS + 1),
        0,
        "and the tokens roll off with the bytes, on one clock"
    );

    // The key survives the roll-off, which is what keeps "carried nothing this
    // hour" different from "nothing measures this path".
    let rows = meter.rows(at + BUDGET_WINDOW_MS + 1);
    assert_eq!(rows.len(), 1, "the path is still a path: {rows:?}");
    assert_eq!(rows[0].bytes_last_hour, 0);
    assert_eq!(rows[0].tokens_last_hour, 0);

    // And the second half of the same rule, on the reader's side: a row written
    // an hour ago describes an hour that is over.
    let peers_path = peers_file(
        "rolls",
        vec![row_with(
            &peer.id,
            "peer",
            vec![Endpoint::direct(
                "127.0.0.1:9999".parse().expect("a socket address"),
                PINNED_AT_MS,
                EndpointSource::Paired,
            )],
            false,
        )],
    );
    state::save_path_traffic(
        &serve::peer_state_path(&peers_path),
        &[state::PathTraffic {
            peer: peer.id,
            locator: path,
            bytes_last_hour: 4_096,
            tokens_last_hour: 12,
            updated_at_ms: at,
        }],
    )
    .expect("write the traffic section");

    let fresh = teamclaude_rs::status::peers_block(&peers_path, at + BUDGET_WINDOW_MS);
    assert_eq!(
        fresh[0].paths[0].bytes_per_hour,
        Some(4_096),
        "inside the window the written figure stands"
    );
    let stale = teamclaude_rs::status::peers_block(&peers_path, at + BUDGET_WINDOW_MS + 1);
    assert_eq!(
        stale[0].paths[0].bytes_per_hour,
        Some(0),
        "a window later it is zero, not the stale total"
    );
    assert_eq!(
        stale[0].paths[0].tokens_per_hour,
        Some(0),
        "and the tokens with it"
    );
}

/// An unmetered peer's paths answer `None`, and that is a different answer from
/// zero.
///
/// The positive control for every `Some(0)` above: if the fill were
/// unconditional, a Mac that has never carried anything would report a
/// confident "nothing went this way" about paths nothing has ever looked at.
///
/// Watch it fail by dropping the `metered` guard in `status::peer_paths`.
#[test]
fn a_peer_nothing_has_metered_reports_absence_and_not_zero() {
    let peer = node();
    let addr: std::net::SocketAddr = "127.0.0.1:9998".parse().expect("a socket address");
    let peers_path = peers_file(
        "absent",
        vec![row_with(
            &peer.id,
            "peer",
            vec![Endpoint::direct(addr, PINNED_AT_MS, EndpointSource::Paired)],
            false,
        )],
    );

    let peers = teamclaude_rs::status::peers_block(&peers_path, PINNED_AT_MS);
    assert_eq!(
        peers[0].paths[0].bytes_per_hour, None,
        "no traffic section, no measurement, and `None` is how that is said"
    );
    assert_eq!(peers[0].paths[0].tokens_per_hour, None);

    // And it is absent from the JSON too, not a null the panel would decode as
    // a figure.
    let json = serde_json::to_string(&peers[0].paths[0]).expect("a path serializes");
    assert!(
        !json.contains("bytesPerHour") && !json.contains("tokensPerHour"),
        "an unmeasured path carries no key at all: {json}"
    );

    // The control: the same path, once something has measured it, reports the
    // zero.
    state::save_path_traffic(
        &serve::peer_state_path(&peers_path),
        &[state::PathTraffic {
            peer: peer.id,
            locator: Locator::Direct { addr },
            bytes_last_hour: 0,
            tokens_last_hour: 0,
            updated_at_ms: PINNED_AT_MS,
        }],
    )
    .expect("write the traffic section");
    let measured = teamclaude_rs::status::peers_block(&peers_path, PINNED_AT_MS);
    assert_eq!(
        measured[0].paths[0].bytes_per_hour,
        Some(0),
        "measured zero, which is a claim, and it is now true"
    );
}

/// A debit with no path noted charges no path, and a fraction lease charges no
/// tokens.
///
/// Both are the same rule from two directions: a figure this Mac cannot derive
/// is not written anywhere, because a tokens-per-path number attributed by
/// guess is worse than no number. The positive control is the third arm, where
/// both facts are known and the charge lands.
///
/// Watch it fail by defaulting the missing path to the lease's first endpoint,
/// or by treating a `Fraction` lease's charge as a token count.
#[test]
fn tokens_are_charged_only_when_the_unit_and_the_path_are_both_known() {
    let unattributed = node();
    let fractional = node();
    let known = node();
    let path = Locator::Direct {
        addr: "127.0.0.1:9997".parse().expect("a socket address"),
    };
    let now_ms = teamclaude_rs::now_ms();
    let lease = |id: u128, unit: LeaseUnit| Lease {
        lease_id: id,
        window: Window::SevenDay,
        unit,
        granted_at_ms: now_ms,
        expires_at_ms: now_ms + 300_000,
        spent: 0.0,
        max_inflight: 2,
        until: None,
    };

    let mut ledger = Ledger::new();
    ledger.record_scoped(
        lease(0x61, LeaseUnit::Tokens(1_000_000)),
        unattributed.id,
        LendScope::All,
    );
    ledger.record_scoped(
        lease(0x62, LeaseUnit::Fraction(0.5)),
        fractional.id,
        LendScope::All,
    );
    ledger.record_scoped(
        lease(0x63, LeaseUnit::Tokens(1_000_000)),
        known.id,
        LendScope::All,
    );
    ledger.note_lease_path(0x62, path);
    ledger.note_lease_path(0x63, path);

    assert!(ledger.debit(0x61, 1, 0.01) > 0.0, "the lease is charged");
    assert!(ledger.debit(0x62, 2, 0.01) > 0.0, "and so is this one");
    assert!(ledger.debit(0x63, 3, 0.01) > 0.0, "and this one");

    let mut meter = path_meter().lock().expect("the meter lock");
    assert_eq!(
        meter.tokens_last_hour(&unattributed.id, &path, now_ms),
        0,
        "a lease whose serving path nobody noted charges no path at all"
    );
    assert_eq!(
        meter.tokens_last_hour(&fractional.id, &path, now_ms),
        0,
        "a fraction of a window is not a token count and is never converted into one"
    );
    assert_eq!(
        meter.tokens_last_hour(&known.id, &path, now_ms),
        10_000,
        "both facts known, and the charge lands on the path"
    );
}

// ---------------------------------------------------------------------------
// The two wiring gaps this file's own tests cannot close
// ---------------------------------------------------------------------------

/// The shipped accept loop over a real peers file, and the id it answers under.
///
/// The harness above drives `tunnel::handle_forward_on` directly, which is the
/// right shape for measuring the forwarder itself and the wrong one for the
/// test below: the charge it is about lives in the listener's own dispatch,
/// above that call, so a test that skipped the dispatch would pass with the
/// charge deleted.
///
/// The keeper's key is MINTED here rather than passed in, because the file
/// format of a node key is `peer::id`'s business and a test that wrote one by
/// hand would be a second spelling of it. The caller pins the returned id.
async fn listener_on(
    key_dir: &std::path::Path,
    peers_path: &std::path::Path,
) -> (std::net::SocketAddr, PeerId) {
    let listening = loopback().await;
    let addr = listening.local_addr().expect("the listener address");
    let key = teamclaude_rs::peer::id::NodeKey::load_or_mint(key_dir).expect("a node key");
    let id = key.id();
    let store = PeerStore::open(peers_path).expect("open the peers file");
    let context =
        listener::SessionContext::new(&key, store.path(), &key_dir.join("peer-state.json"));
    tokio::spawn(async move {
        let _ = listener::serve_on_with(listening, context).await;
    });
    (addr, id)
}

/// **A carry is charged to the path it arrived over.**
///
/// The forwarder's own half was metered and the listener's dispatch left
/// unattributed: every TUNNEL a Mac carried was bytes nobody could place, so
/// the per-path figure this file exists to produce was blank for exactly
/// the carry an operator is most likely to ask about.
///
/// Driven through `listener::serve_on_with`, the production accept loop, so the
/// dispatch that does the charging is the one under test. The harness above
/// calls `tunnel::handle_forward_on` directly, below that dispatch, and would
/// pass with the charge deleted.
///
/// A FORWARD and not a TUNNEL-to-origin, because a gateway checks the host
/// against `PEER_EGRESS_HOSTS` and a loopback origin is not on it: the refusal
/// would happen before the line under test and the test would measure the
/// allowlist.
///
/// The requester dials from loopback and the keeper's row for it names a
/// loopback endpoint, which is what `PeerRow::locator_from` matches: the host
/// and not the port, because the recorded endpoint is the port a peer LISTENS
/// on while the source carries the ephemeral port its dial was given.
///
/// Watched red by deleting the `path_meter()` charge from `serve_stream`'s
/// TUNNEL arm: the meter holds no row for the requester at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_carry_is_charged_to_the_path_it_arrived_over() {
    let keeper_dir = scratch("gateway-charge");
    let requester = node();
    let target = node();

    let target_listener = loopback().await;
    let target_addr = target_listener.local_addr().expect("the target address");
    let _target = target_on(target_listener, vec![9_u8; 16]);

    // The keeper's row for the requester carries a LOOPBACK endpoint, which is
    // the host the dial below really comes from, and `relay` so the forward is
    // admitted by the gate rather than by this test.
    let mut requester_row = row_with(
        &requester.id,
        "requesting-mac",
        vec![Endpoint::direct(
            "127.0.0.1:9801".parse().expect("a literal address"),
            PINNED_AT_MS,
            EndpointSource::Paired,
        )],
        true,
    );
    requester_row.allow.carry = true;
    let target_row = row_with(
        &target.id,
        "target-mac",
        vec![Endpoint::direct(
            target_addr,
            PINNED_AT_MS,
            EndpointSource::Paired,
        )],
        false,
    );
    let peers_path = peers_file("gateway-charge-file", vec![requester_row, target_row]);

    let now = teamclaude_rs::now_ms();
    let rows_for = |peer: PeerId, now_ms: i64| {
        let mut meter = path_meter().lock().expect("the meter lock");
        meter
            .rows(now_ms)
            .into_iter()
            .filter(move |row| row.peer == peer)
            .collect::<Vec<_>>()
    };
    assert!(
        rows_for(requester.id, now).is_empty(),
        "the control: nothing has been charged for this freshly minted key, so a row \
         afterwards is this carry and not a leftover"
    );

    let (addr, keeper_id) = listener_on(&keeper_dir, &peers_path).await;
    let keeper = Node {
        secret: [0_u8; 32],
        id: keeper_id,
    };
    let header = StreamHeader {
        kind: StreamKind::Tunnel,
        target: Some(TunnelTarget::Peer { node: target.id }),
        via: Vec::new(),
        hops_remaining: 1,
        request_id: 0x77,
    };
    let mut asked = ask_to_forward(addr, &requester, &keeper, &header).await;
    asked.write_all(&[7_u8; 32]).await.ok();
    let mut back = [0_u8; 16];
    let _ = asked.read_exact(&mut back).await;
    asked.finish().await.ok();

    assert!(
        _target.connections.load(Ordering::SeqCst) > 0,
        "the control: the forward has to reach the target, or what follows is about the \
         carry failing and not about where it was charged"
    );
    // The control on the predicate itself, so a failure below separates "the
    // charge never ran" from "`locator_from` placed it wrong".
    let keeper_store = PeerStore::open(&peers_path).expect("reopen the keeper's file");
    assert_eq!(
        keeper_store
            .row(&requester.id)
            .and_then(|row| row.locator_from("127.0.0.1:55555".parse().expect("a literal"))),
        Some(Locator::Direct {
            addr: "127.0.0.1:9801".parse().expect("a literal address")
        }),
        "the predicate the charge uses has to place a loopback dial on the loopback row"
    );

    // WAITED FOR, not assumed. The requester's `finish` returns when its own
    // half is done; the charge is on the keeper's side, in the task
    // `serve_on_with` spawned, and the first draft of this test read the meter
    // in the gap and saw an empty one. A bounded poll, so a real regression
    // still fails within a second instead of hanging.
    let charged = {
        let mut seen = Vec::new();
        for _ in 0..100 {
            seen = rows_for(requester.id, teamclaude_rs::now_ms());
            if seen.iter().any(|row| row.bytes_last_hour > 0) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        seen
    };
    assert!(
        charged.iter().any(|row| row.bytes_last_hour > 0),
        "the carry has to land on the requester's own path, or the per-path figure is \
         blank for the one carry an operator asks about: {charged:?}"
    );
    assert!(
        charged.iter().all(|row| row.locator
            == Locator::Direct {
                addr: "127.0.0.1:9801".parse().expect("a literal address")
            }),
        "and on the locator this connection came over, not on whatever the row lists \
         first: {charged:?}"
    );
}

// ---------------------------------------------------------------------------
// The written row's shape is exactly what the privacy decision allows
// ---------------------------------------------------------------------------

/// After a real carry, the row `save_path_traffic` puts in `peer-state.json`
/// carries nothing time-shaped beyond `updatedAtMs`.
///
/// `PathTraffic`'s own doc (`src/peer/state.rs:271-290`) is the privacy
/// decision this file exists to keep true: the state file may hold the
/// SUMMED totals and the instant they were summed, and nothing about when
/// inside the hour the bytes moved. Read out of the written FILE, not out of
/// the struct: asserting against `serde_json::to_value(&PathTraffic { .. })`
/// would just restate whatever fields the struct declares, which guards
/// nothing.
///
/// Watched red by adding a throwaway `retired_at_ms: i64` field to
/// `PathTraffic` (with `#[serde(default)]` so it still compiles and
/// deserializes old files): this test then names `"retiredAtMs"` as the extra
/// key rather than passing. The field was removed again to restore, checked
/// with `git status` and by re-reading the struct.
#[tokio::test]
async fn the_written_row_carries_nothing_time_shaped_beyond_updated_at_ms() {
    let (requester, target_peer, me) = (node(), node(), node());
    let now_ms = teamclaude_rs::now_ms();

    let target_listener = loopback().await;
    let target_addr = target_listener.local_addr().expect("the target address");
    let target = target_on(target_listener, vec![5_u8; 16]);

    let peers_path = peers_file(
        "keyshape",
        vec![
            row_with(&requester.id, "requester", Vec::new(), true),
            row_with(
                &target_peer.id,
                "target",
                vec![Endpoint::direct(
                    target_addr,
                    PINNED_AT_MS,
                    EndpointSource::Paired,
                )],
                false,
            ),
        ],
    );
    let budget = Arc::new(Mutex::new(TunnelBudget::new()));

    carry_one_request(
        &requester,
        &me,
        PeerStore::open(&peers_path).expect("open the peers file"),
        target_peer.id,
        &budget,
        now_ms,
    )
    .await;
    assert_eq!(
        target.connections.load(Ordering::SeqCst),
        1,
        "the control: the carry has to actually reach the target, or a row here is not \
         a real charge"
    );

    let rows = {
        let mut meter = path_meter().lock().expect("the meter lock");
        meter.rows(now_ms)
    };
    let state_path = serve::peer_state_path(&peers_path);
    state::save_path_traffic(&state_path, &rows).expect("write the traffic section");

    let raw = std::fs::read_to_string(&state_path).expect("read the state file");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("the state file is JSON");
    let written = parsed["pathTraffic"]
        .as_array()
        .expect("a pathTraffic array")
        .iter()
        .find(|row| row["peer"] == serde_json::json!(target_peer.id.to_wire()))
        .unwrap_or_else(|| panic!("no row for the target peer in {parsed}"));

    let mut keys: Vec<&str> = written
        .as_object()
        .expect("a row is a JSON object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "addr",
            "bytesLastHour",
            "kind",
            "peer",
            "tokensLastHour",
            "updatedAtMs",
        ],
        "the only key here shaped by time is updatedAtMs, the promise PathTraffic's own \
         doc gives: {written}"
    );
}
