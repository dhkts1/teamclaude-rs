//! Two-phase pairing, the caps that make a knock cheap to
//! refuse, the network key, and the share link.
//!
//! # Everything here runs on this box, against temp files
//!
//! Every listener binds `127.0.0.1:0` (the kernel picks the port), every peers
//! file and state file is a fresh temp file at 0600, and nothing reads the
//! operator's config directory or cache directory. **Nothing here touches the proxy on
//! `127.0.0.1:3456`**: no test in this file connects to a port it did not bind
//! itself, and none starts, stops or signals anything.
//!
//! # The shape of the thing being tested
//!
//! Pairing splits in two. Phase one is a KNOCK: a `Noise_NN`
//! session with ephemeral keys on both sides, carrying `{instance_id,
//! proposed_name, wire_version}`, answered with one ack byte and closed. It
//! reveals no static key, grants nothing, and all it produces is a row an
//! operator reads. Phase two is `Noise_XX`, and it is answered **only** from an
//! instance id the operator accepted, at the address it knocked from, inside a
//! 120-second window. Everything else gets zero bytes.
//!
//! What that replaced: a node-wide two-minute window `tcr peer pair` opened on
//! the DIALLING Mac, during which any host that could reach the port got
//! message 2 and this node's static key with it.

use std::io::Write as _;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::Arc;
use std::time::Duration;

use tcr_peer_wire::{InstanceId, Knock, PeerId, INSTANCE_ID_BYTES};
use teamclaude_rs::peer::config::{self, NetworkKey, PeerFile};
use teamclaude_rs::peer::id::NodeKey;
use teamclaude_rs::peer::listener::{self, SessionContext};
use teamclaude_rs::peer::noise::{self, Handshake};
use teamclaude_rs::peer::pair;
use teamclaude_rs::peer::state::{self, BanReason, PeerState};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

// ---------------------------------------------------------------------------
// Scratch files, one set per test
// ---------------------------------------------------------------------------

/// A scratch directory named after this process, thread and a caller tag, so
/// two tests in this binary (and five lanes running at once), never collide on
/// one path.
fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tcr-peer-pairing-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the scratch dir");
    dir
}

/// One Mac's files: a peers file at 0600, a state file beside it, and a node
/// key in the same directory.
struct Node {
    dir: std::path::PathBuf,
    peers: std::path::PathBuf,
    state: std::path::PathBuf,
    key: NodeKey,
}

impl Node {
    fn new(tag: &str) -> Self {
        let dir = scratch(tag);
        let peers = dir.join("tcr-peers.json");
        let state = dir.join("peer-state.json");
        config::save(&peers, &PeerFile::default()).expect("write the peers file");
        let key = NodeKey::load_or_mint(&dir).expect("mint a node key");
        Self {
            dir,
            peers,
            state,
            key,
        }
    }

    /// Read the peers file the way every production caller does.
    fn file(&self) -> PeerFile {
        config::read_or_default(&self.peers).expect("the peers file reads")
    }

    fn write_file(&self, file: &PeerFile) {
        config::save(&self.peers, file).expect("the peers file writes");
    }

    fn state(&self) -> PeerState {
        state::load(&self.state, 0).expect("the state file reads")
    }

    fn write_state(&self, value: &PeerState) {
        state::save(&self.state, value).expect("the state file writes");
    }

    fn context(&self) -> SessionContext {
        SessionContext::new(&self.key, &self.peers, &self.state)
    }
}

/// Bind `127.0.0.1:0`, start the SHIPPED accept loop on it, and report the
/// address the kernel chose.
///
/// `serve_on_with` and not a hand-rolled accept loop: the caps, the ban check,
/// the pattern dispatch and the knock queue all live in it, so a test harness
/// that re-implemented the loop would be measuring the harness.
async fn serve(context: SessionContext) -> SocketAddr {
    let listener = listener::bind("127.0.0.1:0".parse().expect("a loopback address"))
        .await
        .expect("bind a kernel-chosen port on loopback");
    let addr = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        // The loop only returns on an accept failure, which is the harness
        // shutting down at the end of the test.
        let _ = listener::serve_on_with(listener, context).await;
    });
    addr
}

fn instance(byte: u8) -> InstanceId {
    InstanceId([byte; INSTANCE_ID_BYTES])
}

/// Send one knock over a fresh connection and report whether the far side
/// answered with the ack byte.
async fn knock_at(addr: SocketAddr, id: InstanceId, name: Option<&str>) -> anyhow::Result<()> {
    knock_at_with_key(addr, id, name, None).await
}

async fn knock_at_with_key(
    addr: SocketAddr,
    id: InstanceId,
    name: Option<&str>,
    network_key: Option<&[u8; 32]>,
) -> anyhow::Result<()> {
    let mut stream = TcpStream::connect(addr).await?;
    noise::send_knock(
        &mut stream,
        &Knock {
            instance_id: id,
            proposed_name: name.map(str::to_string),
            wire_version: tcr_peer_wire::PROTO_VERSION,
        },
        network_key,
    )
    .await
}

/// Approve one pending knock the way `tcr peer accept` does, and report the
/// window.
fn accept_pending(node: &Node, selector: &str) -> state::AcceptedInstance {
    let mut value = node.state();
    let window = value
        .accept_knock(selector, pair::now_ms(), pair::PAIRING_WINDOW_SECS)
        .expect("a pending row to accept");
    node.write_state(&value);
    window
}

/// A socket that counts what was WRITTEN to it, so "zero bytes in answer" is
/// asserted on the wire and not on an error type.
///
/// A listener that answered first and refused afterwards satisfies every
/// assertion about its `Err` and fails this one.
async fn count_written(addr: SocketAddr, message_1: &[u8]) -> usize {
    let mut stream = TcpStream::connect(addr)
        .await
        .expect("connect to the test listener");
    let mut framed = Vec::with_capacity(2 + message_1.len());
    framed.extend_from_slice(
        &u16::try_from(message_1.len())
            .expect("a short frame")
            .to_be_bytes(),
    );
    framed.extend_from_slice(message_1);
    stream.write_all(&framed).await.expect("write message 1");
    stream.flush().await.expect("flush message 1");

    let mut back = Vec::new();
    // Read until the far side closes. A refusal closes with nothing written, so
    // this returns 0; an answer is at least message 2.
    let read = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut back)).await;
    match read {
        Ok(Ok(_)) => back.len(),
        // A reset or a timeout is not an answer either, and the number that
        // matters is how many bytes came back.
        _ => back.len(),
    }
}

/// The bytes of a knock's `NN` message 1 (no payload: a knock's payload
/// rides the transport session message 2 establishes, not message 1 itself),
/// built with a throwaway ephemeral secret exactly as `noise::send_knock`
/// builds its own.
fn knock_message_1() -> Vec<u8> {
    let throwaway = noise::random_secret().expect("a throwaway secret");
    let mut state = noise::initiator_with_secret(&throwaway, noise::PATTERN_KNOCK, None, None)
        .expect("a knock initiator");
    let mut scratch = vec![0_u8; 1024];
    let len = state
        .write_message(&[], &mut scratch)
        .expect("knock message 1");
    scratch[..len].to_vec()
}

/// The bytes of an `XX` message 1 carrying `id` in the versioned pairing
/// payload, built with a throwaway static key.
fn xx_message_1(id: InstanceId) -> Vec<u8> {
    let (secret, _) = noise::generate_static().expect("a keypair");
    let mut state = noise::initiator_with_secret(&secret, noise::PATTERN_PAIR, None, None)
        .expect("an XX initiator");
    let mut scratch = vec![0_u8; 1024];
    let len = state
        .write_message(&noise::pair_message_1_payload(&id), &mut scratch)
        .expect("XX message 1");
    scratch[..len].to_vec()
}

// ---------------------------------------------------------------------------
// Item 1: `accept_enrolment` gets its production caller
// ---------------------------------------------------------------------------

/// **A headless join leaves a pinned row on BOTH sides, through the shipped
/// accept loop.**
///
/// `accept_enrolment` used to have no production caller at all
/// (`pair.rs:318`), which means the whole headless path: the one a Mac with no
/// screen has, did nothing: `tcr peer join` completed its `IKpsk1` handshake,
/// sent its `Control::Enroll`, and the registrar's stream gate refused it for
/// want of the very pinned row that frame exists to create. The joiner then
/// waited for a `Hello` that never came.
///
/// Driven through `listener::serve_on_with`, which is the production accept
/// loop, and `pair::join_as`, which is what the CLI calls. Both sides are
/// asserted, because "the joiner pinned the registrar" was already true
/// beforehand and told nobody anything: the registrar's row is the half that was
/// missing.
///
/// Watched red: comment out the `if session.handshake == Handshake::Enrol { …
/// serve_enrolment … }` block in `listener::serve_stream` and this fails with
/// the joiner's own refusal: "the registrar closed the stream without
/// answering the enrolment": and no row on the registrar.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_two_process_join_leaves_a_pinned_row_on_both_sides() {
    let registrar = Node::new("enrol-registrar");
    let joiner = Node::new("enrol-joiner");

    // The registrar needs a listen address in its file before it can mint an
    // invite: a token with no address to dial is not a token.
    let addr = serve(registrar.context()).await;
    let mut file = registrar.file();
    file.listen = Some(addr);
    registrar.write_file(&file);

    let store = config::PeerStore::open(&registrar.peers).expect("open the registrar's store");
    let (invite, token) = pair::mint_invite_as(&store, &registrar.key, "laptop-2", 600, 1)
        .expect("mint a one-use invite");
    assert_eq!(invite.uses_left, 1);

    let joiner_store = config::PeerStore::open(&joiner.peers).expect("open the joiner's store");
    let pinned = pair::join_as(&joiner_store, &joiner.key, &token, "this-mac")
        .await
        .expect("the join completes and is acknowledged");
    assert_eq!(pinned, registrar.key.id());

    // The registrar's half: the one this used to never write.
    let registrar_file = registrar.file();
    assert!(
        registrar_file
            .peers
            .iter()
            .any(|row| row.node == joiner.key.id()),
        "the registrar must hold a pinned row for the joiner; it holds {:?}",
        registrar_file
            .peers
            .iter()
            .map(|row| row.node.display())
            .collect::<Vec<_>>()
    );
    // And the invite is spent in the same write, so a one-use key is one use.
    assert!(
        registrar_file.pending_invites.is_empty(),
        "a one-use invite must be gone once it has admitted a machine, and {} remain",
        registrar_file.pending_invites.len()
    );

    // The joiner's half.
    assert!(
        joiner
            .file()
            .peers
            .iter()
            .any(|row| row.node == registrar.key.id()),
        "the joiner must hold a pinned row for the registrar"
    );

    // Every grant is at its default on the row enrolment wrote: a bare pin can
    // say hello and nothing else.
    let row = registrar_file
        .peers
        .iter()
        .find(|row| row.node == joiner.key.id())
        .expect("the row just asserted");
    assert!(!row.allow.inspect, "enrolment grants nothing");
    assert!(!row.allow.gateway, "enrolment grants nothing");
    assert!(!row.allow.relay, "enrolment grants nothing");
    assert!(row.lend.is_empty(), "enrolment lends nothing");
}

// ---------------------------------------------------------------------------
// Item 2: a one-use invite is atomic
// ---------------------------------------------------------------------------

/// **Two joiners racing one one-use invite leave exactly one `Ok`.**
///
/// Testing measured this at 40 double-accepts out of 40 trials:
/// `accept_enrolment` read the file, saw `usesLeft: 1`, and wrote: and two
/// copies of that body running together each read the same 1. `save` being
/// atomic does not help, because atomicity is about the file never being
/// half-written and this is two complete writes each built on a stale read.
///
/// Forty trials rather than one, deliberately. A single trial of a race is a
/// coin toss: it can pass on the unlocked code by luck. Forty is the same
/// number that testing used, so this test is measured against the same
/// instrument that found the bug.
///
/// Watched red: remove the `FileLock::acquire` line from `accept_enrolment` and
/// this fails inside the first few trials with "trial N admitted 2 machines on
/// a ONE-USE invite".
#[test]
fn two_concurrent_joiners_on_one_use_invite_leave_exactly_one_ok() {
    /// The same trial count, kept so this gate is as sensitive as the
    /// probe that found the defect.
    const TRIALS: usize = 40;

    for trial in 0..TRIALS {
        let node = Node::new(&format!("atomic-{trial}"));
        let mut file = node.file();
        file.listen = Some("127.0.0.1:9600".parse().expect("a loopback address"));
        node.write_file(&file);

        let store = config::PeerStore::open(&node.peers).expect("open the store");
        let (_invite, token) = pair::mint_invite_as(&store, &node.key, "laptop-2", 600, 1)
            .expect("mint a one-use invite");

        // Two joiners with DIFFERENT static keys, which is the case that
        // matters: two rows for two machines off one key.
        let first = PeerId([0x11; 32]);
        let second = PeerId([0x22; 32]);
        let enroll = tcr_peer_wire::Enroll {
            invite_id: 0,
            label: "laptop-2".to_string(),
        };

        let peers_path = node.peers.clone();
        let now = pair::now_ms();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = [first, second]
            .into_iter()
            .map(|joiner| {
                let peers_path = peers_path.clone();
                let enroll = enroll.clone();
                let secret = token.secret;
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    // Both threads arrive at the read together, which is what
                    // makes this a race and not two sequential calls.
                    barrier.wait();
                    pair::accept_enrolment(
                        &peers_path,
                        joiner,
                        &enroll,
                        &secret,
                        now,
                        std::net::SocketAddr::from(([127, 0, 0, 1], 9600)),
                    )
                })
            })
            .collect();

        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("the enrolling thread"))
            .collect();
        let admitted = results.iter().filter(|result| result.is_ok()).count();
        assert_eq!(
            admitted, 1,
            "trial {trial} admitted {admitted} machines on a ONE-USE invite. A one-use join \
             key that admits two Macs is the whole failure this lock exists to prevent"
        );

        // And the file agrees: one pinned row, no invite left.
        let after = node.file();
        assert_eq!(
            after.peers.len(),
            1,
            "trial {trial}: the peers file holds {} rows after a one-use invite",
            after.peers.len()
        );
        assert!(
            after.pending_invites.is_empty(),
            "trial {trial}: the spent invite is still on disk, which is still a live PSK"
        );

        let _ = std::fs::remove_dir_all(&node.dir);
    }
}

// ---------------------------------------------------------------------------
// Item 7: the knock, end to end
// ---------------------------------------------------------------------------

/// **A knock earns one row and one ack byte, and reveals no static key.**
///
/// The whole of phase one: the requester opens `NN`: ephemeral keys on both
/// sides, sends its claim, and the responder queues it and answers one byte.
/// Nothing is pinned, nothing is granted, and the responder's static key never
/// appears.
///
/// Watched red: drop the `noise::write_knock_ack` call from
/// `listener::serve_knock` and `send_knock` fails with "that Mac closed the
/// connection without taking the pairing request".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_knock_queues_one_row_and_earns_one_ack_byte() {
    let node = Node::new("knock-row");
    let addr = serve(node.context()).await;

    knock_at(addr, instance(0x41), Some("studio-mac"))
        .await
        .expect("a knock at a node with no network key is taken");

    let pending = node.state().pending;
    assert_eq!(
        pending.len(),
        1,
        "one knock is one pending row, and this made {}",
        pending.len()
    );
    assert_eq!(pending[0].instance_id, instance(0x41));
    assert_eq!(pending[0].proposed_name.as_deref(), Some("studio-mac"));
    assert_eq!(pending[0].wire_version, tcr_peer_wire::PROTO_VERSION);
    assert_eq!(
        pending[0].addr, "127.0.0.1",
        "the row is keyed on the IP with the ephemeral source port dropped, or the \
         eight-row cap would bound knocks rather than machines"
    );

    // Nothing was pinned and nothing was granted: a knock is a row.
    assert!(
        node.file().peers.is_empty(),
        "a knock must pin nothing at all"
    );
}

/// **A rotating instance id from one address is ONE row that updates.**
///
/// The id-changer defence, and the reason the pending queue keys on the source
/// address rather than on the id: coalescing by id would let one host hold all
/// eight pending slots by rotating its id eight times.
///
/// Watched red: change `record_knock`'s `find(|knock| knock.addr == addr)` to
/// compare `instance_id` instead, and this fails with three rows.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rotating_instance_id_from_one_address_is_one_row() {
    let node = Node::new("knock-idchanger");
    let addr = serve(node.context()).await;

    for id in [0x51_u8, 0x52, 0x53] {
        knock_at(addr, instance(id), Some("studio-mac"))
            .await
            .expect("each knock is inside the burst");
    }

    let pending = node.state().pending;
    assert_eq!(
        pending.len(),
        1,
        "three knocks from one address under three ids must coalesce into one row, and \
         they made {}",
        pending.len()
    );
    assert_eq!(
        pending[0].instance_id,
        instance(0x53),
        "the row shows the id that knocked LAST, so an operator accepting it approves the \
         one that is actually asking"
    );
}

/// A hostile claimed name never reaches the row.
///
/// The name in a knock is attacker-chosen text that lands in a panel row, a
/// `tcr peer pending` line and a log line. A name that fails the whitelist
/// drops the NAME and keeps the row, which is the same rule the discovery path
/// follows: the row is still a machine an operator may want to accept, and it
/// shows its address.
///
/// Watched red: remove the `sanitize_label` call in `listener::serve_knock` and
/// the escape sequence arrives in `proposed_name`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hostile_knock_name_is_dropped_and_the_row_survives() {
    let node = Node::new("knock-hostile-name");
    let addr = serve(node.context()).await;

    knock_at(addr, instance(0x61), Some("studio\u{1b}[31mmac"))
        .await
        .expect("the knock itself is fine; only its name is not");

    let pending = node.state().pending;
    assert_eq!(pending.len(), 1, "the row survives a name it cannot render");
    assert_eq!(
        pending[0].proposed_name, None,
        "a name this node will not render is no name at all: the row shows its address"
    );
}

// ---------------------------------------------------------------------------
// Item 9: the three gating refusals
// ---------------------------------------------------------------------------

/// **An `XX` message 1 from an instance nobody accepted gets zero bytes.**
///
/// This is the decision-10 replacement for the node-wide pairing window, and
/// the assertion is on the socket's byte count rather than on an error: a
/// listener that answered first and refused afterwards has already disclosed
/// this node's static key in message 2.
///
/// Watched red: delete the `if !peer_state.accepted_window(...)` bail from
/// `accept_pairing_or_return` and the count goes from 0 to 50 (message 2 plus
/// its frame prefix).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn xx_from_an_unaccepted_instance_gets_zero_bytes() {
    let node = Node::new("xx-unaccepted");
    let addr = serve(node.context()).await;

    let written = count_written(addr, &xx_message_1(instance(0x71))).await;
    assert_eq!(
        written, 0,
        "the listener answered a first pairing nobody approved: a stranger on this port \
         just learned this node's static key for the cost of 40 bytes"
    );

    // The POSITIVE CONTROL, which is what makes the zero above mean something:
    // after a knock and an Accept, the same dial is answered.
    knock_at(addr, instance(0x72), None)
        .await
        .expect("the knock is taken");
    accept_pending(&node, &instance(0x72).to_wire());
    let answered = count_written(addr, &xx_message_1(instance(0x72))).await;
    assert!(
        answered > 0,
        "positive control: an accepted instance must be answered, or the refusal above is \
         a listener that has stopped pairing at all"
    );
}

/// **An accepted window admits ONE instance id, not the address.**
///
/// The other half of the id-changer defence: switching ids after Accept loses
/// the window rather than inheriting it. Same address, same moment, a different
/// id.
///
/// Watched red: drop `&& &open.instance_id == instance_id` from
/// `PeerState::accepted_window` and the second dial is answered too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_accepted_window_admits_one_instance_id_only() {
    let node = Node::new("xx-one-id");
    let addr = serve(node.context()).await;

    knock_at(addr, instance(0x81), None)
        .await
        .expect("the knock is taken");
    accept_pending(&node, &instance(0x81).to_wire());

    // The accepted id is answered.
    assert!(
        count_written(addr, &xx_message_1(instance(0x81))).await > 0,
        "positive control: the accepted id must be answered"
    );
    // A different id from the same address, inside the same window, is not.
    assert_eq!(
        count_written(addr, &xx_message_1(instance(0x82))).await,
        0,
        "a second instance id inside one window must get zero bytes, or an id changer \
         inherits somebody else's approval"
    );
}

/// **A knock from a muted address gets zero bytes.**
///
/// Ignore is one of the three things an operator can do to a pending row, and
/// what it buys is an hour of quiet. The refusal is indistinguishable on the
/// wire from a machine that is not there, deliberately: telling a flooder which
/// cap it hit tells it what to change.
///
/// The assertion is on the socket's byte count, not only on `knock_at`'s
/// `Err`, for the same reason `xx_from_an_unaccepted_instance_gets_zero_bytes`
/// uses `count_written`: a listener that answered NN message 2 and refused
/// only afterwards has already spent bytes on a source it meant to ignore for
/// free, even though `send_knock` still reports it as an error (it never gets
/// its ack). Watched red: this failed with `written != 0` (message 2 plus its
/// frame prefix) before `PeerState::refusal_for_knock_source` was checked
/// ahead of `noise::finish_responder` in `listener::serve_knock`: the ban,
/// mute and queue-full refusals used to run only after message 2 was already
/// on the wire (`listener.rs`, the gap between the old `finish_responder` call
/// and the `record_knock` call that followed it).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_knock_from_a_muted_address_gets_zero_bytes() {
    let node = Node::new("knock-muted");
    let addr = serve(node.context()).await;

    knock_at(addr, instance(0x91), None)
        .await
        .expect("the first knock is taken");
    // `tcr peer ignore` on that row.
    let mut value = node.state();
    value.mute("127.0.0.1", pair::now_ms());
    node.write_state(&value);
    assert!(
        node.state().pending.is_empty(),
        "Ignore drops the row it was pressed on"
    );

    let refused = knock_at(addr, instance(0x92), None).await;
    assert!(
        refused.is_err(),
        "a knock from a muted address must not earn an ack"
    );
    assert!(
        node.state().pending.is_empty(),
        "and it must not earn a row either: {:?}",
        node.state().pending
    );

    // The byte count, on a fresh connection: zero, or the mute cost the
    // source NN message 2 before ever being refused.
    let written = count_written(addr, &knock_message_1()).await;
    assert_eq!(
        written, 0,
        "a muted address must be refused before message 2 is written, not after: {written} \
         bytes came back"
    );
}

/// **A banned static key is refused from a NEW address.**
///
/// The ban scope is both halves, and this is the half the address
/// alone cannot cover: DHCP moves addresses, so a blocked Mac that comes back
/// on a new lease with the same static key has to stay blocked.
///
/// Driven on the `IK` return path, because that is where a banned key arrives
/// from somewhere new: the peer is still pinned (blocking is not forgetting),
/// its row is still in the file, and the refusal has to come from the ban and
/// not from the pin check. The address it dials from (loopback), is
/// deliberately NOT banned, so a pass here cannot be the address half firing.
///
/// Watched red: remove the `banned.iter().any(...)` check from the pin callback
/// in `accept_pairing_or_return` and the handshake completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_banned_key_is_refused_from_a_new_address() {
    let node = Node::new("ban-key");
    let (peer_secret, peer_public) = noise::generate_static().expect("a peer keypair");
    let peer_id = PeerId(peer_public);

    // The peer is pinned, which is the point: the refusal must be the ban.
    let mut file = node.file();
    file.peers.push(config::PeerRow {
        node: peer_id,
        label: "laptop-2".to_string(),
        endpoints: Vec::new(),
        added_at: 0,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: config::Allow::default(),
        lend: Vec::new(),
    });
    node.write_file(&file);

    let addr = serve(node.context()).await;
    let responder_public = node.key.id().0;

    // The positive control FIRST, while nothing is banned: the pinned peer gets
    // in. Without this, the refusal below could be any of a dozen things.
    {
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        noise::dial_handshake(
            &mut stream,
            &peer_secret,
            Handshake::Return,
            Some(&responder_public),
            None,
        )
        .await
        .expect("positive control: a pinned peer completes a return visit");
    }

    // Now block the KEY, from an address that is not the one it dials from :
    // `203.0.113.9` is a documentation address this test never connects from,
    // so the address half of the ban cannot be what refuses the dial below.
    let mut value = node.state();
    value.ban(
        "203.0.113.9",
        Some(peer_id),
        BanReason::ForgottenAndBlocked,
        pair::now_ms(),
    );
    node.write_state(&value);
    assert!(
        !node.state().is_address_banned("127.0.0.1"),
        "the address this test dials from must NOT be banned, or a pass here proves the \
         wrong half"
    );

    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let refused = noise::dial_handshake(
        &mut stream,
        &peer_secret,
        Handshake::Return,
        Some(&responder_public),
        None,
    )
    .await;
    assert!(
        refused.is_err(),
        "a banned static key must be refused from any address, or a DHCP lease undoes a \
         block"
    );
}

/// A banned ADDRESS is refused before a frame is even read.
///
/// The cheapest refusal in the listener, and the one an operator asked for
/// explicitly. Asserted on the byte count, since the whole claim is that
/// nothing is written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_banned_address_gets_zero_bytes_before_any_frame() {
    let node = Node::new("ban-addr");
    let mut value = node.state();
    value.ban("127.0.0.1", None, BanReason::Blocked, pair::now_ms());
    node.write_state(&value);

    let addr = serve(node.context()).await;
    // A well-formed knock from a blocked address earns nothing.
    let refused = knock_at(addr, instance(0xA1), None).await;
    assert!(refused.is_err(), "a blocked address must earn no ack");
    assert!(
        node.state().pending.is_empty(),
        "and no row: {:?}",
        node.state().pending
    );

    // Unblocking restores it, so the block above is a block and not a broken
    // listener.
    let mut value = node.state();
    assert!(value.unblock("127.0.0.1"));
    node.write_state(&value);
    knock_at(addr, instance(0xA2), None)
        .await
        .expect("positive control: unblocking lets the same knock through");
    assert_eq!(node.state().pending.len(), 1);
}

// ---------------------------------------------------------------------------
// Item 10: every cap, each driving the (n+1)th
// ---------------------------------------------------------------------------

/// **The fourth knock in ten seconds gets zero bytes.**
///
/// One per ten seconds, burst three: a person pressing Trust, mistyping and
/// pressing it again is never rate-limited, and a script is.
///
/// Driven from four DIFFERENT instance ids so that what refuses the fourth is
/// the bucket and not the pending-row coalescing: the rows coalesce either
/// way, so a test that could not tell them apart would pass with the bucket
/// deleted.
///
/// Watched red: delete the `take_knock_token` block from
/// `listener::serve_knock` and the fourth knock is taken.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_fourth_knock_in_ten_seconds_gets_nothing() {
    let node = Node::new("cap-knock-bucket");
    let context = node.context();
    let admission = Arc::clone(context.admission());
    let addr = serve(context).await;

    for (n, id) in [0xB1_u8, 0xB2, 0xB3].into_iter().enumerate() {
        knock_at(addr, instance(id), None)
            .await
            .unwrap_or_else(|err| panic!("knock {n} is inside the burst of 3: {err:#}"));
    }

    let left = admission
        .lock()
        .expect("the admission lock")
        .knock_tokens("127.0.0.1", pair::now_ms());
    assert_eq!(
        left, 0,
        "three knocks spend the whole burst, and {left} tokens are left"
    );

    let refused = knock_at(addr, instance(0xB4), None).await;
    assert!(
        refused.is_err(),
        "the fourth knock inside the interval must earn nothing at all"
    );
}

/// **The ninth outstanding knock is refused.**
///
/// The many-address flood: a `/24` of hosts each knocking once costs eight rows
/// and not 254. Driven against `PeerState` directly with eight distinct
/// addresses, because the transport half of this is loopback and every
/// connection from it shares one address: a socket-level test could not
/// produce nine distinct addresses on this box.
///
/// Watched red: delete the `pending.len() >= MAX_PENDING_KNOCKS` arm from
/// `record_knock` and the ninth is queued.
#[test]
fn the_ninth_outstanding_knock_is_refused() {
    let mut value = PeerState::default();
    for n in 0..state::MAX_PENDING_KNOCKS {
        let addr = format!("192.0.2.{n}");
        assert!(
            value
                .record_knock(&addr, instance(n as u8), None, 1, 1_000)
                .expect("inside the cap"),
            "row {n} is a new address and must be a new row"
        );
    }
    assert_eq!(value.pending.len(), state::MAX_PENDING_KNOCKS);

    let refused = value.record_knock("192.0.2.99", instance(0xFF), None, 1, 1_000);
    assert!(
        matches!(refused, Err(state::KnockRefusal::QueueFull { .. })),
        "the ninth outstanding knock must be refused, and this one returned {refused:?}"
    );
    assert_eq!(
        value.pending.len(),
        state::MAX_PENDING_KNOCKS,
        "a refused knock must leave no row"
    );

    // And an address ALREADY in the queue still updates at the cap, because a
    // full queue must not make a machine the operator is already looking at
    // unreachable.
    assert!(
        !value
            .record_knock("192.0.2.0", instance(0xFE), None, 1, 1_100)
            .expect("an existing row updates even at the cap"),
        "an existing address at the cap updates in place rather than being refused"
    );
}

/// **Eight concurrent reservations at a seven-row queue
/// leave exactly eight rows, never nine: and only one of the eight racers
/// wins the one free slot.**
///
/// `listener::serve_knock`'s pre-handshake admission check used to READ the
/// state file unlocked, then run the whole Noise handshake, then only under
/// `FileLock` write the real row via `record_knock`. Eight knocks arriving
/// together could all read the same "there is room" snapshot before any of
/// them wrote anything, so all eight ran the handshake and earned message 2
///: a real cost paid by seven connections the queue was always going to
/// refuse. The fix moved the check itself under the lock and turned it into
/// a RESERVATION (`PeerState::reserve_knock_slot`), so the second racer to
/// reach the lock sees the first racer's claim and is refused before any
/// handshake bytes are ever considered.
///
/// This drives that exact mechanism: `config::FileLock::acquire` plus
/// `PeerState::reserve_knock_slot`, on the real state file, from eight real
/// OS threads: rather than going through a socket. A genuine socket-level
/// race needs eight DISTINCT source addresses, and this box cannot produce
/// them: every outbound loopback connection binds `127.0.0.1` (binding a
/// second loopback address such as `127.0.0.2` fails with `EADDRNOTAVAIL`
/// unless it is configured as an interface alias, which needs root and is
/// not available here: confirmed with a plain `socket.bind`). The
/// admission mechanism under test is the state file and its lock, not the
/// TCP or Noise layer above it, so calling it directly with eight distinct
/// address strings measures the real fix; the wire-bytes half of the
/// invariant (a refused knock writes nothing) is covered on a real socket by
/// [`the_ninth_outstanding_knock_writes_zero_bytes_on_the_wire`] below, using
/// the one real address this box has.
///
/// Watched red: with `reserve_knock_slot` reverted to a read-only check (no
/// `self.pending.push`, i.e. exactly `refusal_for_knock_source(..)` wrapped
/// in `Ok`/`Err`), none of the eight threads ever sees a sibling's claim, so
/// this fails with `admitted` anywhere from 2 to 8 depending on scheduling
/// and `pending.len()` correspondingly above 8.
#[test]
fn eight_concurrent_reservations_at_a_seven_row_queue_leave_exactly_eight_never_nine() {
    let node = Node::new("cap-race");

    let mut seeded = PeerState::default();
    for n in 0..7_u8 {
        seeded
            .record_knock(&format!("192.0.2.{n}"), instance(n), None, 1, 1_000)
            .expect("seed row");
    }
    assert_eq!(
        seeded.pending.len(),
        7,
        "seed exactly seven rows before the race"
    );
    node.write_state(&seeded);

    let state_path = node.state.clone();
    let handles: Vec<std::thread::JoinHandle<Result<bool, state::KnockRefusal>>> = (0..8_u8)
        .map(|n| {
            let state_path = state_path.clone();
            std::thread::spawn(move || {
                // Eight distinct new addresses: TEST-NET-3, never a real
                // loopback source, so nothing here collides with a real
                // socket test's `127.0.0.1`.
                let addr = format!("203.0.113.{n}");
                let _lock = config::FileLock::acquire(&state_path).expect("acquire the state lock");
                let mut value = state::load(&state_path, 1_000).expect("load state");
                let outcome = value.reserve_knock_slot(&addr, 1_000);
                if outcome.is_ok() {
                    state::save(&state_path, &value).expect("save the reservation");
                }
                outcome
            })
        })
        .collect();

    let mut admitted = 0;
    let mut refused = 0;
    for handle in handles {
        match handle.join().expect("a racing thread must not panic") {
            Ok(true) => admitted += 1,
            Ok(false) => panic!(
                "every address here is brand new to this queue, so none of them should \
                 coalesce onto an existing row"
            ),
            Err(state::KnockRefusal::QueueFull { .. }) => refused += 1,
            Err(other) => panic!("unexpected refusal from a fresh, unbanned address: {other:?}"),
        }
    }

    assert_eq!(
        admitted, 1,
        "exactly one of the eight new addresses fits in the one free slot"
    );
    assert_eq!(
        refused, 7,
        "the other seven must be refused outright, not merely queued behind the winner"
    );

    let after = node.state();
    assert_eq!(
        after.pending.len(),
        state::MAX_PENDING_KNOCKS,
        "the queue must leave exactly eight rows, never nine, after the race: {:?}",
        after.pending.iter().map(|k| &k.addr).collect::<Vec<_>>()
    );
}

/// **The over-cap refusal writes zero bytes on the wire,
/// not just a zero-length `pending` delta.**
///
/// `the_ninth_outstanding_knock_is_refused` (above) proves the ROW count at
/// the `PeerState` level with fake addresses; it asserts nothing about bytes,
/// so a fix that moved the queue-full refusal to AFTER the handshake would
/// pass it unchanged. This proves the other half, on a real socket: seed the
/// queue to the cap with eight rows at addresses no real loopback connection
/// can ever present (`192.0.2.x`, TEST-NET-1), then knock for real from
/// `127.0.0.1` (a genuinely new, ninth address), and assert both the
/// refusal and that nothing came back over the wire.
///
/// Watched red: with the pre-fix unlocked read-then-handshake-then-write
/// order restored (the state at the top of this file, before
/// `reserve_knock_slot` existed), this fails with `written != 0`: the ninth
/// knock's connection completes the Noise handshake and receives message 2
/// before the locked `record_knock` call two reads later refuses it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_ninth_outstanding_knock_writes_zero_bytes_on_the_wire() {
    let node = Node::new("cap-bytes");

    let mut seeded = PeerState::default();
    for n in 0..state::MAX_PENDING_KNOCKS {
        seeded
            .record_knock(
                &format!("192.0.2.{n}"),
                instance(n as u8),
                None,
                1,
                pair::now_ms(),
            )
            .expect("seed row");
    }
    node.write_state(&seeded);

    let addr = serve(node.context()).await;
    let written = count_written(addr, &knock_message_1()).await;
    assert_eq!(
        written, 0,
        "a knock from a genuinely new address against a full queue must write zero bytes, \
         and {written} came back"
    );

    let after = node.state();
    assert_eq!(
        after.pending.len(),
        state::MAX_PENDING_KNOCKS,
        "the refused knock must leave no row"
    );
}

/// **A knock sitting for ten minutes expires on its own.**
///
/// Nothing else leaves PENDING without the operator, which is the state
/// diagram's one exception: so the TTL is what keeps a queue that filled up
/// while nobody was at the Mac from staying full for ever.
#[test]
fn a_knock_expires_after_ten_minutes() {
    let mut value = PeerState::default();
    value
        .record_knock("192.0.2.7", instance(1), None, 1, 1_000)
        .expect("queued");
    value
        .record_knock("192.0.2.8", instance(2), None, 1, 1_000)
        .expect("queued");

    // One keeps asking; the other does not.
    value
        .record_knock(
            "192.0.2.7",
            instance(1),
            None,
            1,
            1_000 + state::KNOCK_TTL_MS,
        )
        .expect("re-knocking refreshes");
    let dropped = value.expire(1_000 + state::KNOCK_TTL_MS + 1);
    assert_eq!(dropped, 1, "exactly the silent row goes");
    assert_eq!(value.pending.len(), 1);
    assert_eq!(value.pending[0].addr, "192.0.2.7");
}

/// **A mute lifts on its own after an hour; a ban does not lift at all.**
///
/// The two are different states and the difference is the operator's intent:
/// Ignore means "not now", Block means "never". A mute that needed unblocking,
/// or a ban that expired, would each be the other verb.
#[test]
fn a_mute_expires_and_a_ban_does_not() {
    let mut value = PeerState::default();
    value.mute("192.0.2.7", 1_000);
    value.ban("192.0.2.8", None, BanReason::Blocked, 1_000);

    assert!(value.is_muted("192.0.2.7", 1_000));
    value.expire(1_000 + state::MUTE_MS + 1);
    assert!(
        !value.is_muted("192.0.2.7", 1_000 + state::MUTE_MS + 1),
        "a mute must lift on its own, or Ignore is a second Block"
    );
    assert!(
        value.is_address_banned("192.0.2.8"),
        "a ban must survive every expiry, or Block is a second Ignore"
    );
    assert!(value.unblock("192.0.2.8"), "only unblock clears a ban");
    assert!(!value.is_address_banned("192.0.2.8"));
}

/// **A first frame above the pre-authentication bound is refused before the
/// allocation.**
///
/// `read_frame` sizes a `Vec` from the peer-controlled two-byte length prefix,
/// so without a bound any host that can reach the port makes this node reserve
/// 65 kB by writing two bytes and then nothing: and with sixteen
/// unauthenticated sockets allowed that is a megabyte pinned for 32 bytes of
/// traffic.
///
/// The number is [`noise::MAX_MESSAGE_1_BYTES`], which is the largest message 1
/// any pattern here has. `abuse-resistance.md` says 64, and 64 is wrong: an
/// `IK` message 1 is 96 bytes, so that cap would refuse every returning peer
/// and every headless enrolment.
///
/// Watched red: change `read_frame_bounded`'s guard to `if false` and the frame
/// is accepted (and then fails further in, on the pattern dispatch, which is
/// after the allocation).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_frame_above_the_bound_is_refused_before_allocation() {
    let (mut reader, mut writer) = tokio::io::duplex(8192);
    let oversized = noise::MAX_MESSAGE_1_BYTES + 1;
    let mut framed = Vec::new();
    framed.extend_from_slice(&u16::try_from(oversized).expect("fits").to_be_bytes());
    framed.resize(2 + oversized, 0);
    writer.write_all(&framed).await.expect("write the frame");
    writer.flush().await.expect("flush");

    let refused = noise::read_frame_bounded(&mut reader, noise::MAX_MESSAGE_1_BYTES).await;
    let error = refused.expect_err("an oversized first frame must be refused");
    assert!(
        format!("{error:#}").contains("refused BEFORE allocating"),
        "the refusal has to name the reason, because the whole point is WHERE it happens: \
         {error:#}"
    );

    // The positive control: exactly the bound is accepted, so the refusal is
    // the bound and not a reader that refuses everything.
    let (mut reader, mut writer) = tokio::io::duplex(8192);
    let mut framed = Vec::new();
    framed.extend_from_slice(
        &u16::try_from(noise::MAX_MESSAGE_1_BYTES)
            .expect("fits")
            .to_be_bytes(),
    );
    framed.resize(2 + noise::MAX_MESSAGE_1_BYTES, 0);
    writer.write_all(&framed).await.expect("write the frame");
    writer.flush().await.expect("flush");
    let accepted = noise::read_frame_bounded(&mut reader, noise::MAX_MESSAGE_1_BYTES)
        .await
        .expect("a frame exactly at the bound must be read");
    assert_eq!(accepted.len(), noise::MAX_MESSAGE_1_BYTES);
}

/// **A socket opened and left silent is closed after five seconds.**
///
/// The cheapest thing a stranger can do, and the one that would otherwise hold
/// an unauthenticated slot for as long as the handshake timeout: ten seconds
/// for a connection that has not said anything at all.
///
/// Watched red: raise `MESSAGE_1_TIMEOUT` to 60 seconds and this fails on the
/// elapsed assertion.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_silent_socket_is_closed_after_the_message_one_timeout() {
    let node = Node::new("cap-silence");
    let addr = serve(node.context()).await;

    let started = std::time::Instant::now();
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let mut back = Vec::new();
    // The far side closes when the timeout fires, which ends this read.
    let read = tokio::time::timeout(
        listener::MESSAGE_1_TIMEOUT * 3,
        stream.read_to_end(&mut back),
    )
    .await;
    let elapsed = started.elapsed();

    assert!(
        read.is_ok(),
        "the listener must close a silent socket rather than hold it, and it was still \
         open after {:?}",
        elapsed
    );
    assert_eq!(
        back.len(),
        0,
        "and it must close with nothing written: a silent connection learns nothing"
    );
    assert!(
        elapsed < listener::MESSAGE_1_TIMEOUT * 2,
        "the close took {elapsed:?}, which is well past the {:?} bound",
        listener::MESSAGE_1_TIMEOUT
    );
}

/// **The third unauthenticated socket from one address is refused.**
///
/// Two per address, sixteen in total. Driven by holding two silent connections
/// open: silent, because a connection that has not delivered message 1 is
/// exactly what the counter counts, and then opening a third.
///
/// Watched red: delete the `per_address >= MAX_UNAUTHENTICATED_PER_ADDRESS`
/// arm from `SocketSlot::acquire` and the third connection is held for the full
/// message-1 timeout instead of being closed at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_third_unauthenticated_socket_from_one_address_is_refused() {
    let node = Node::new("cap-sockets");
    let context = node.context();
    let admission = Arc::clone(context.admission());
    let addr = serve(context).await;

    // Two silent connections, held open.
    let mut held = Vec::new();
    for _ in 0..listener::MAX_UNAUTHENTICATED_PER_ADDRESS {
        held.push(TcpStream::connect(addr).await.expect("connect"));
    }
    // Wait until the listener has really counted both, rather than assuming the
    // accept loop has run: the counter is the thing under test, so reading it
    // is the honest way to know the setup is in place.
    for _ in 0..200 {
        if admission
            .lock()
            .expect("the admission lock")
            .live_unauthenticated("127.0.0.1")
            >= listener::MAX_UNAUTHENTICATED_PER_ADDRESS
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        admission
            .lock()
            .expect("the admission lock")
            .live_unauthenticated("127.0.0.1"),
        listener::MAX_UNAUTHENTICATED_PER_ADDRESS,
        "setup: both silent connections must be counted before the third is opened"
    );

    // The third is closed at once: well inside the message-1 timeout, which is
    // what distinguishes "refused by the cap" from "timed out like the others".
    let mut third = TcpStream::connect(addr).await.expect("connect");
    let mut back = Vec::new();
    let closed = tokio::time::timeout(
        listener::MESSAGE_1_TIMEOUT / 2,
        third.read_to_end(&mut back),
    )
    .await;
    assert!(
        closed.is_ok(),
        "the third unauthenticated socket from one address must be closed at once, not \
         held until the message-1 timeout"
    );
    assert_eq!(back.len(), 0, "and closed with nothing written");

    // The slot is released when a connection ends, so the cap is not a
    // permanent lockout after two refusals: the failure mode an RAII-free
    // counter has.
    drop(held);
    for _ in 0..200 {
        if admission
            .lock()
            .expect("the admission lock")
            .live_unauthenticated("127.0.0.1")
            == 0
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        admission
            .lock()
            .expect("the admission lock")
            .live_unauthenticated("127.0.0.1"),
        0,
        "every slot must be released when its connection ends, or sixteen refusals lock \
         this node out of its own mesh"
    );
}

/// **Three pinned-peer handshakes at once from ONE address all complete.**
///
/// The regression for a shipped defect, later measured: the
/// pre-authentication socket slot originally covered the whole handshake, so
/// `MAX_UNAUTHENTICATED_PER_ADDRESS` (two), silently became a cap on how many
/// streams one pinned peer could have open at once. One TCP is one Noise
/// session is one stream in this design, so a lender serving a borrower's
/// `max_inflight: 5` refused the third with zero bytes and the borrower read it
/// as "the handshake with the lender failed".
///
/// Caught by `borrowed_requests_are_paced_by_the_lenders_own_bucket`
/// (`tests/peer_lease.rs`) in the full workspace run, not
/// by anything this file wrote: which is why it is pinned HERE, in the file
/// that owns the cap, rather than left to a neighbour's test to notice again.
///
/// The peer's grant is `max_inflight: 5`, the same figure the lease test uses,
/// so the allowance this exercises is the operator's own number and not one
/// this test invented.
///
/// **Every socket is connected BEFORE any of them writes**, deliberately: that
/// is the state the cap actually sees. The accept loop spawns a task per
/// connection and the task reads message 1, so three simultaneous `connect`s
/// are three accepted-and-silent sockets whatever the handshake costs: and a
/// version of this test that connected-and-handshook in one step passed by luck
/// on the interleaving while the lease test kept failing.
///
/// Watched red twice. With the caps as fixed constants
/// (`MAX_UNAUTHENTICATED_PER_ADDRESS` alone, no
/// [`listener::unauthenticated_allowance`]), this fails with the third dial
/// reporting "message 2 did not authenticate"; that is the shape the lease test
/// reported as "the handshake with the lender failed". With the `drop(slot)`
/// moved back below `accept_pairing_or_return` it fails the same way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_handshakes_up_to_the_granted_inflight_all_complete() {
    /// The lease test's own figure.
    const GRANTED_INFLIGHT: u8 = 5;

    let node = Node::new("concurrent-handshakes");
    let (peer_secret, peer_public) = noise::generate_static().expect("a peer keypair");
    let mut file = node.file();
    file.peers.push(config::PeerRow {
        node: PeerId(peer_public),
        label: "laptop-2".to_string(),
        endpoints: Vec::new(),
        added_at: 0,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: config::Allow {
            inspect: true,
            ..config::Allow::default()
        },
        lend: vec![config::LendGrant::new(
            tcr_peer_wire::Window::SevenDay,
            0.5,
            300,
            GRANTED_INFLIGHT,
        )],
    });
    node.write_file(&file);

    // The allowance really is derived from that grant, and it really is above
    // the bare constant: otherwise the assertion below would pass for the
    // wrong reason.
    let (total, per_address) = listener::unauthenticated_allowance(&node.file());
    assert_eq!(
        per_address,
        listener::MAX_UNAUTHENTICATED_PER_ADDRESS + usize::from(GRANTED_INFLIGHT),
        "the per-address allowance must be the stranger's floor plus what was granted"
    );
    assert!(total >= per_address);

    let addr = serve(node.context()).await;
    let responder_public = node.key.id().0;

    let concurrent = usize::from(GRANTED_INFLIGHT);

    // Every socket open and silent before any handshake starts. This is the
    // state the cap sees.
    let mut sockets = Vec::new();
    for _ in 0..concurrent {
        sockets.push(TcpStream::connect(addr).await.expect("connect"));
    }

    let dials: Vec<_> = sockets
        .into_iter()
        .enumerate()
        .map(|(n, mut stream)| {
            tokio::spawn(async move {
                noise::dial_handshake(
                    &mut stream,
                    &peer_secret,
                    Handshake::Return,
                    Some(&responder_public),
                    None,
                )
                .await
                .map(|_| n)
                .map_err(|err| (n, format!("{err:#}")))
            })
        })
        .collect();

    let mut failed = Vec::new();
    for dial in dials {
        match dial.await.expect("the dialling task") {
            Ok(_) => {}
            Err((n, err)) => failed.push((n, err)),
        }
    }
    assert!(
        failed.is_empty(),
        "{} of {concurrent} concurrent handshakes from one pinned peer were refused: \
         {failed:?}. A pre-authentication cap below the concurrency the operator granted \
         is a cap on how many streams that peer may have open",
        failed.len()
    );
}

/// The caps are exactly `abuse-resistance.md`'s numbers when the operator has
/// granted nothing, which is the default and the home-LAN case.
///
/// Without this, the derived allowance above could have quietly raised the
/// stranger's ceiling for everybody: the thing it must not do.
#[test]
fn with_nothing_granted_the_allowance_is_the_documented_pair() {
    let bare = PeerFile::default();
    assert_eq!(
        listener::unauthenticated_allowance(&bare),
        (
            listener::MAX_UNAUTHENTICATED_SOCKETS,
            listener::MAX_UNAUTHENTICATED_PER_ADDRESS
        ),
        "a fresh config must be judged against 16 and 2 and nothing else"
    );

    // A pinned peer with no LEND grant raises nothing either: what raises the
    // ceiling is granted concurrency, never the mere existence of a peer.
    let pinned = PeerFile {
        peers: vec![config::PeerRow {
            node: PeerId([1; 32]),
            label: "laptop-2".to_string(),
            endpoints: Vec::new(),
            added_at: 0,
            rendezvous_secret: None,
            sees_us_at: None,
            allow: config::Allow::default(),
            lend: Vec::new(),
        }],
        ..PeerFile::default()
    };
    assert_eq!(
        listener::unauthenticated_allowance(&pinned),
        (
            listener::MAX_UNAUTHENTICATED_SOCKETS,
            listener::MAX_UNAUTHENTICATED_PER_ADDRESS
        ),
        "pinning a peer is not granting it concurrency"
    );
}

// ---------------------------------------------------------------------------
// Item 11: the network key
// ---------------------------------------------------------------------------

/// **A knock without the network key gets zero bytes at a node that has one.**
///
/// The sentence `abuse-resistance.md` promises: "a Mac without the key sees
/// nothing and can send nothing that reaches the UI". The refusal happens
/// inside message 1 (`NNpsk0` mixes the psk before the first token), so there
/// is no queue entry and no row.
///
/// Watched red: delete the `(Some(_), Handshake::Knock)` arm from
/// `listener::serve_knock`'s network-key match and the plain knock is taken.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_knock_without_the_network_key_gets_zero_bytes() {
    let node = Node::new("netkey-knock");
    let key = NetworkKey::from_bytes([0x5A; 32]);
    let mut file = node.file();
    file.network_key = Some(key);
    node.write_file(&file);
    let addr = serve(node.context()).await;

    let refused = knock_at(addr, instance(0xC1), None).await;
    assert!(
        refused.is_err(),
        "a knock with no network key must earn nothing at a node that requires one"
    );
    assert!(
        node.state().pending.is_empty(),
        "and it must reach no row: {:?}",
        node.state().pending
    );

    // A knock under the WRONG key fails the same way, which is the case that
    // matters on an office network with two meshes on it.
    let refused = knock_at_with_key(addr, instance(0xC2), None, Some(&[0x11; 32])).await;
    assert!(
        refused.is_err(),
        "a knock under another network key must earn nothing either"
    );
    assert!(node.state().pending.is_empty());

    // The positive control: with the right key, the same knock is taken.
    knock_at_with_key(addr, instance(0xC3), None, Some(key.as_bytes()))
        .await
        .expect("positive control: the right network key is admitted");
    assert_eq!(
        node.state().pending.len(),
        1,
        "the refusals above are the key check and not a listener that takes no knocks"
    );
}

/// **A knock whose message 1 does not validate reserves nothing**: the ordering
/// found here: the reservation was taken BEFORE
/// message 1 was decrypted, so a stranger sending the right NUMBER of wrong
/// bytes claimed one of the eight pairing slots and made this node write its
/// state file twice before the cryptography had said a word about the sender.
///
/// The instrument is the state FILE, not the queue. Both orders end with an
/// empty `pending` (the old one reserved and then released), so a test that
/// only read `pending` could not tell them apart, and none did. What the two
/// orders cannot both do is leave the file untouched: a reservation is a
/// `state::save`, and `state::save` creates the file.
///
/// Watched red: move the `reserve_knock_slot` block in `listener::serve_knock`
/// back above the `read_message_1_matching` call and this test fails on the
/// first assertion, with `peer-state.json` present and its `pending` empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_knock_whose_message_1_does_not_validate_reserves_nothing() {
    let node = Node::new("knock-order");
    let key = NetworkKey::from_bytes([0x7C; 32]);
    let mut file = node.file();
    file.network_key = Some(key);
    node.write_file(&file);
    let addr = serve(node.context()).await;

    // The file must not exist yet, or the assertion below proves nothing about
    // who created it.
    assert!(
        !node.state.exists(),
        "the premise: this node has written no state file yet"
    );

    // A knock under the WRONG network key. Its message 1 is exactly the right
    // length: `Handshake::from_message_1_len` accepts it and dispatches it as
    // a keyed knock: and it fails inside `NNpsk0`, which mixes the psk before
    // the first token.
    let refused = knock_at_with_key(addr, instance(0xE1), None, Some(&[0x22; 32])).await;
    assert!(
        refused.is_err(),
        "a knock under another network key must earn nothing"
    );
    assert!(
        !node.state.exists(),
        "and it must not have reserved a slot: a reservation writes the state \
         file, and nothing here was ever entitled to one"
    );
    assert!(
        node.state().pending.is_empty(),
        "no row either, which was true before this fix as well"
    );

    // The positive control, on the same node: a knock that DOES validate
    // reserves, records and leaves the file behind. Without it, the two
    // assertions above would pass against a listener that had stopped writing
    // state at all.
    knock_at_with_key(addr, instance(0xE2), None, Some(key.as_bytes()))
        .await
        .expect("positive control: the right network key is admitted");
    assert!(
        node.state.exists(),
        "a knock that validates does write the state file"
    );
    assert_eq!(
        node.state().pending.len(),
        1,
        "and it leaves exactly one row for the operator"
    );
}

/// **A reservation placeholder is never shown to the operator.**
///
///
/// A reservation is a row in `PeerState::pending` with an all-zero instance id
/// and no name, pushed before the handshake so the eight-row cap is taken
/// atomically. `tcr peer pending` and the panel render `pending` verbatim, so a
/// connection that stalled showed the operator a pairing request from nobody,
/// with a blank name, that no Accept could complete.
///
/// Two readers of one row, which is why this is a filter and not a narrower
/// field: the listener must keep COUNTING it (it is the cap) and the operator
/// must not be SHOWN it.
///
/// Watched red: make `PeerState::visible_pending` return `self.pending.clone()`
/// and the first assertion fails.
#[test]
fn a_reservation_placeholder_is_counted_but_never_rendered() {
    let mut value = PeerState::default();
    let now = pair::now_ms();

    assert_eq!(
        value.reserve_knock_slot("10.0.0.9", now),
        Ok(true),
        "the reservation is created"
    );
    assert_eq!(
        value.pending.len(),
        1,
        "and the listener counts it, which is the whole reason it exists"
    );
    assert!(
        value.visible_pending().is_empty(),
        "but the operator is shown nothing: {:?}",
        value.visible_pending()
    );

    // Once the handshake fills it in, the SAME row is visible: the filter is
    // about an unfilled placeholder and not about the address.
    value
        .record_knock(
            "10.0.0.9",
            instance(0x5B),
            Some("studio-mac".to_string()),
            tcr_peer_wire::PROTO_VERSION,
            now,
        )
        .expect("the knock records onto the reserved row");
    assert_eq!(
        value.pending.len(),
        1,
        "it coalesced onto the reservation rather than adding a second row"
    );
    assert_eq!(
        value.visible_pending().len(),
        1,
        "and now the operator sees it"
    );
    assert_eq!(value.visible_pending()[0].instance_id, instance(0x5B));
}

/// A knock that CARRIES the key at a node that has none is refused too.
///
/// The symmetric case, and it is not cosmetic: a half-configured office would
/// otherwise have Macs that could knock at each other in one direction only,
/// which is the sort of asymmetry that gets diagnosed as a network fault.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_keyed_knock_at_a_keyless_node_is_refused() {
    let node = Node::new("netkey-asymmetric");
    let addr = serve(node.context()).await;

    let refused = knock_at_with_key(addr, instance(0xD1), None, Some(&[0x33; 32])).await;
    assert!(
        refused.is_err(),
        "a node with no network key has nothing to verify a keyed knock against"
    );
    assert!(node.state().pending.is_empty());

    knock_at(addr, instance(0xD2), None)
        .await
        .expect("positive control: a plain knock is taken at a keyless node");
}

/// HMAC-SHA256 is verified against **RFC 4231 test case 2**, not against its
/// own output.
///
/// A hand-rolled MAC that is only ever compared to itself is a MAC that agrees
/// with nobody: it would be self-consistent, pass every round-trip test in this
/// file, and interoperate with no other implementation, including the next
/// build of this program, if the construction were ever replaced by the `hmac`
/// crate.
///
/// Case 2 is `key = "Jefe"`, `data = "what do ya want for nothing?"`, and its
/// published HMAC-SHA-256 digest is
/// `5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843`.
///
/// The construction here takes a `[u8; 32]` key, and `"Jefe"` is four bytes :
/// so this drives the private function through a shim in the module that owns
/// it, where the key width is the one the RFC vector needs.
#[test]
fn hmac_sha256_matches_rfc_4231_case_2() {
    let digest = config::hmac_sha256(b"Jefe", b"what do ya want for nothing?");
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    assert_eq!(
        hex, "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
        "the HMAC construction must agree with RFC 4231 case 2, or it agrees with nobody"
    );
}

/// The network key round-trips through its paste string, and a short or
/// mistyped paste is refused rather than silently truncated.
#[test]
fn the_network_key_round_trips_through_its_paste_string() {
    let key = NetworkKey::from_bytes([0x7C; 32]);
    let pasted = key.to_paste_string();
    assert_eq!(
        pasted.chars().count(),
        52,
        "32 bytes in Crockford base32 is 52 characters, and this is {pasted}"
    );
    assert_eq!(
        NetworkKey::try_from(pasted.clone()).expect("the paste string reads back"),
        key
    );
    // Whitespace around a paste is what a paste buffer adds, and refusing it
    // would send an operator hunting a problem they cannot see.
    assert_eq!(
        NetworkKey::try_from(format!("  {pasted}\n")).expect("a trimmed paste reads back"),
        key
    );
    // A short paste is refused, not padded.
    assert!(NetworkKey::try_from(pasted[..40].to_string()).is_err());
    // And the debug form never prints the bytes: a `{:?}` in a log line is how
    // a shared secret ends up in a file.
    assert_eq!(format!("{key:?}"), "NetworkKey(set)");
}

// ---------------------------------------------------------------------------
// Item 12: the share link
// ---------------------------------------------------------------------------

/// **A share link round-trips**, with and without a join key riding along.
///
/// Watched red: change `ShareLink::to_link` to emit `v=2` and the parse refuses
/// with "is not v1".
#[test]
fn a_share_link_round_trips_with_and_without_a_join_key() {
    let network_key = NetworkKey::from_bytes([0x2B; 32]);
    let token = pair::JoinToken {
        addr: "127.0.0.1:9600".parse().expect("a loopback address"),
        registrar: PeerId([0x3C; 32]),
        secret: [0x4D; 32],
    };

    let bare = pair::ShareLink {
        network_key,
        join: None,
    };
    let rendered = bare.to_link();
    assert!(
        rendered.starts_with(pair::LINK_PREFIX),
        "a link must be openable by the URL handler, and this one is {rendered}"
    );
    assert_eq!(
        pair::ShareLink::parse(&rendered).expect("a bare link parses"),
        bare
    );

    let invited = pair::ShareLink {
        network_key,
        join: Some(token.clone()),
    };
    let rendered = invited.to_link();
    let parsed = pair::ShareLink::parse(&rendered).expect("an invite link parses");
    assert_eq!(parsed, invited);
    assert_eq!(
        parsed.join.as_ref().map(|join| join.addr),
        Some(token.addr),
        "the join key inside a link must carry the address to dial"
    );

    // Every refusal names its cause, because the operator's next move depends
    // on which of these it was.
    for (bad, needle) in [
        ("https://example.com/", "not a share link"),
        ("tcr://peer/join?nk=AAAA", "no `v=` version"),
        (
            &format!("tcr://peer/join?v=2&nk={}", network_key.to_paste_string()),
            "not v1",
        ),
        ("tcr://peer/join?v=1", "no `nk=` network key"),
        (
            &format!(
                "tcr://peer/join?v=1&nk={}&surprise=1",
                network_key.to_paste_string()
            ),
            "field this build does not know",
        ),
    ] {
        let error = pair::ShareLink::parse(bad)
            .expect_err(&format!("{bad:?} must be refused, not guessed at"));
        assert!(
            format!("{error:#}").contains(needle),
            "the refusal for {bad:?} has to name {needle:?}: {error:#}"
        );
    }
}

/// **A link whose join key is spent still sets the network key.**
///
/// The two halves are independent by design: a link posted in a channel is
/// opened by whoever gets there first, and everyone after them still has to
/// end up on the office mesh: otherwise the second person to click reads
/// "expired" and has no idea their real problem is now that they cannot see
/// anybody.
///
/// Driven through the parse and the two accessors `tcr peer join` uses, so what
/// is asserted is what the CLI acts on.
#[test]
fn a_link_with_a_spent_join_key_still_sets_the_network_key() {
    let network_key = NetworkKey::from_bytes([0x6E; 32]);
    let link = pair::ShareLink {
        network_key,
        join: Some(pair::JoinToken {
            addr: "127.0.0.1:9600".parse().expect("a loopback address"),
            registrar: PeerId([0x7F; 32]),
            // A secret for an invite that is no longer on the registrar's
            // disk: the enrolment will fail, and the network key must not.
            secret: [0x80; 32],
        }),
    };

    let input = pair::JoinInput::parse(&link.to_link()).expect("the link parses");
    assert_eq!(
        input.network_key(),
        Some(network_key),
        "the network key half of a link is what a `tcr peer join` sets FIRST, before it \
         tries to enrol: so a spent join key cannot cost it"
    );
    assert!(
        input.join_token().is_some(),
        "and the join key is still offered, so the enrolment is attempted and its failure \
         is reported"
    );
}

/// `tcr peer join` accepts a link OR a bare join key, and refuses anything
/// else by naming both shapes.
#[test]
fn join_input_accepts_a_link_or_a_bare_key() {
    let token = pair::JoinToken {
        addr: "127.0.0.1:9600".parse().expect("a loopback address"),
        registrar: PeerId([0x11; 32]),
        secret: [0x22; 32],
    };
    let bare = pair::JoinInput::parse(&token.to_token()).expect("a bare key parses");
    assert_eq!(bare.join_token(), Some(&token));
    assert_eq!(
        bare.network_key(),
        None,
        "a bare join key carries no network key, and inventing one would put a Mac on a \
         mesh nobody offered it"
    );

    let link = pair::ShareLink {
        network_key: NetworkKey::from_bytes([0x33; 32]),
        join: Some(token.clone()),
    };
    let from_link = pair::JoinInput::parse(&link.to_link()).expect("a link parses");
    assert_eq!(from_link.join_token(), Some(&token));
    assert!(from_link.network_key().is_some());

    let error = pair::JoinInput::parse("hello").expect_err("neither shape must be refused");
    let text = format!("{error:#}");
    assert!(
        text.contains("tcr://peer/join?") && text.contains("tcr-join:v1:"),
        "the refusal has to name BOTH shapes, because the operator does not know which one \
         they were given: {text}"
    );
}

// ---------------------------------------------------------------------------
// Item 4: `--stdin`, and no log line carries the secret
// ---------------------------------------------------------------------------

/// **`tcr peer join --stdin` reads the key off standard input**, and neither
/// its output nor its error output ever carries the secret.
///
/// Runs the binary this build produced (`CARGO_BIN_EXE_tcr`), never the
/// installed one, and points the whole peer surface at a temp file with
/// `--peers`, so it reads no real config and touches no running proxy.
///
/// The join itself FAILS here: there is no registrar at the address in the
/// token, and that is the case worth greping: a failure path prints more than
/// a success path, so if any line is going to echo the token back it is one of
/// these.
///
/// Watched red: add `eprintln!("{}", token.to_token())` to the `Join` arm of
/// `run_peer` and this fails with "the join key must never appear in output".
#[test]
fn join_stdin_keeps_the_key_out_of_argv_and_every_log_line() {
    let dir = scratch("join-stdin");
    let peers = dir.join("tcr-peers.json");
    config::save(&peers, &PeerFile::default()).expect("write the peers file");

    // A token pointing at a port nothing is listening on: the dial fails, which
    // is the noisy path.
    let token = pair::JoinToken {
        addr: "127.0.0.1:1".parse().expect("a loopback address"),
        registrar: PeerId([0x99; 32]),
        secret: [0xAA; 32],
    };
    let rendered = token.to_token();
    let secret_field = tcr_peer_wire::encode_key32(&token.secret);

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_tcr"))
        .args([
            "peer",
            "join",
            "--peers",
            peers.to_str().expect("a utf-8 path"),
            "--stdin",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn the tcr built by this build");
    child
        .stdin
        .as_mut()
        .expect("the child's stdin")
        .write_all(format!("{rendered}\n").as_bytes())
        .expect("write the token to stdin");
    let output = child.wait_with_output().expect("the child exits");

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Positive control: the command really ran and really read the token: it
    // names the address the token carries. Without this, a binary that failed
    // on its arguments would satisfy every absence below.
    assert!(
        combined.contains("127.0.0.1:1"),
        "positive control: the command did not get as far as dialling the token's address, \
         so its silence about the secret proves nothing. Output: {combined}"
    );

    assert!(
        !combined.contains(&secret_field),
        "the join key's secret field must never appear in output: a terminal keeps \
         scrollback and a failed paste is still a live bearer secret. Output: {combined}"
    );
    assert!(
        !combined.contains(&rendered),
        "the whole join key must never appear in output either. Output: {combined}"
    );
}

/// The `--stdin` reader takes ONE line, refuses an empty one, and never prints
/// what it read.
///
/// One line rather than read-to-end: reading to end of file would make a
/// trailing newline plus anything after it part of the secret field, and the
/// operator would see a refusal about base32 for a key that was fine.
#[test]
fn the_stdin_reader_takes_one_line_and_refuses_an_empty_one() {
    let token = pair::JoinToken {
        addr: "127.0.0.1:9600".parse().expect("a loopback address"),
        registrar: PeerId([0x44; 32]),
        secret: [0x55; 32],
    };
    let rendered = token.to_token();

    let input = pair::join_input_from_reader(std::io::Cursor::new(format!(
        "{rendered}\nand a second line nobody asked for\n"
    )))
    .expect("one line is read");
    assert_eq!(input.join_token(), Some(&token));

    let error = pair::join_input_from_reader(std::io::Cursor::new("\n"))
        .expect_err("an empty line must be refused");
    assert!(
        format!("{error:#}").contains("--stdin"),
        "the refusal has to name the flag, because that is what the operator has to fix: \
         {error:#}"
    );
}

// ---------------------------------------------------------------------------
// Item 8: the operator verbs, through the built binary
// ---------------------------------------------------------------------------

/// Run `tcr peer …` against a temp peers file and return `(stdout, stderr,
/// success)`.
fn run_tcr(peers: &std::path::Path, args: &[&str]) -> (String, String, bool) {
    let mut full = vec!["peer"];
    full.extend_from_slice(args);
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_tcr"))
        .arg("peer")
        .args(args)
        .args(["--peers", peers.to_str().expect("a utf-8 path")])
        .output()
        .unwrap_or_else(|err| panic!("spawn tcr {full:?}: {err}"));
    (
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.success(),
    )
}

/// **The five approval verbs work on a real pending row, and `ls --json`
/// reports the counts.**
///
/// Driven through the shipped binary rather than the library, because the claim
/// is about the surface an operator and the panel both read: `tcr peer pending
/// --json` and `tcr peer ls --json` are what the Peers tab is fed from, and a
/// library-level test would not catch a verb that never reached the dispatcher.
///
/// Watched red: delete the `pending`/`pendingCount` fields from `run_peer`'s
/// `PeerLsJson` and this fails on the `pendingCount` assertion.
#[test]
fn the_approval_verbs_and_ls_json_agree_about_one_pending_row() {
    let dir = scratch("verbs");
    let peers = dir.join("tcr-peers.json");
    let state_file = dir.join("peer-state.json");
    config::save(&peers, &PeerFile::default()).expect("write the peers file");

    // One pending row, written the way the listener writes it.
    let mut value = PeerState::default();
    value
        .record_knock(
            "192.0.2.24",
            instance(0xE1),
            Some("studio-mac".to_string()),
            1,
            pair::now_ms(),
        )
        .expect("queued");
    state::save(&state_file, &value).expect("write the state file");

    // `pending`, as text: the row `abuse-resistance.md` specifies.
    let (out, err, ok) = run_tcr(&peers, &["pending"]);
    assert!(ok, "`tcr peer pending` must exit 0: {err}");
    assert!(
        out.contains("studio-mac") && out.contains("192.0.2.24") && out.contains("wants to pair"),
        "the pending row must name the Mac AND its address, because a name is a label and \
         never identity: {out}"
    );

    // `pending --json`, which is what the panel reads.
    let (out, err, ok) = run_tcr(&peers, &["pending", "--json"]);
    assert!(ok, "`tcr peer pending --json` must exit 0: {err}");
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).expect("valid JSON");
    assert_eq!(parsed["pending"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        parsed["pending"][0]["instanceId"].as_str(),
        Some(instance(0xE1).to_wire().as_str())
    );

    // `ls --json` gains the counts and the rows.
    let (out, err, ok) = run_tcr(&peers, &["ls", "--json"]);
    assert!(ok, "`tcr peer ls --json` must exit 0: {err}");
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).expect("valid JSON");
    assert_eq!(parsed["pendingCount"].as_u64(), Some(1));
    assert_eq!(parsed["blockedCount"].as_u64(), Some(0));
    assert_eq!(parsed["mutedCount"].as_u64(), Some(0));
    assert_eq!(parsed["limited"].as_u64(), Some(0));
    // The caps ride along, so the panel's Advanced pane and this binary cannot
    // disagree about what they are.
    assert_eq!(
        parsed["caps"]["pending"].as_u64(),
        Some(state::MAX_PENDING_KNOCKS as u64)
    );
    assert_eq!(
        parsed["caps"]["foundRows"].as_u64(),
        Some(teamclaude_rs::peer::discovery::MAX_FOUND_ROWS as u64)
    );

    // `accept` by instance id opens a window for that one Mac.
    let (out, err, ok) = run_tcr(&peers, &["accept", &instance(0xE1).to_wire()]);
    assert!(ok, "`tcr peer accept` must exit 0: {err}");
    assert!(out.contains("peer accept: ok"), "{out}");
    let after = state::load(&state_file, pair::now_ms()).expect("the state reads");
    assert!(
        after.pending.is_empty(),
        "accepting a row consumes it: the operator has decided"
    );
    assert!(
        after.accepted_window("192.0.2.24", &instance(0xE1), pair::now_ms()),
        "and it opens a window for that one instance at that one address"
    );

    // `block` by address bans it, `unblock` lifts it, and the counts follow.
    let (out, err, ok) = run_tcr(&peers, &["block", "192.0.2.24"]);
    assert!(ok, "`tcr peer block` must exit 0: {err}");
    assert!(out.contains("peer block: ok"), "{out}");
    let (out, _err, _ok) = run_tcr(&peers, &["ls", "--json"]);
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).expect("valid JSON");
    assert_eq!(parsed["blockedCount"].as_u64(), Some(1));

    let (out, err, ok) = run_tcr(&peers, &["unblock", "192.0.2.24"]);
    assert!(ok, "`tcr peer unblock` must exit 0: {err}");
    assert!(out.contains("peer unblock: ok"), "{out}");
    let (out, _err, _ok) = run_tcr(&peers, &["ls", "--json"]);
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).expect("valid JSON");
    assert_eq!(parsed["blockedCount"].as_u64(), Some(0));

    // `ignore` mutes, and the muted count follows.
    let mut value = PeerState::default();
    value
        .record_knock("192.0.2.25", instance(0xE2), None, 1, pair::now_ms())
        .expect("queued");
    state::save(&state_file, &value).expect("write the state file");
    let (out, err, ok) = run_tcr(&peers, &["ignore", "192.0.2.25"]);
    assert!(ok, "`tcr peer ignore` must exit 0: {err}");
    assert!(out.contains("peer ignore: ok"), "{out}");
    let (out, _err, _ok) = run_tcr(&peers, &["ls", "--json"]);
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).expect("valid JSON");
    assert_eq!(parsed["mutedCount"].as_u64(), Some(1));
    assert_eq!(parsed["pendingCount"].as_u64(), Some(0));

    // And a selector nothing matches is a refusal with a next step, never a
    // silent success.
    let (_out, err, ok) = run_tcr(&peers, &["accept", "192.0.2.99"]);
    assert!(!ok, "accepting a row that does not exist must fail");
    assert!(
        err.contains("tcr peer pending"),
        "the refusal has to say where to look: {err}"
    );
}

/// **Through the real CLI: a link whose join key is spent still sets the
/// network key.**
///
/// `a_link_with_a_spent_join_key_still_sets_the_network_key` asserts the two
/// accessors; this asserts the ORDER the binary acts in, which is the thing
/// that matters: a `tcr peer join` that enrolled first and set the key second
/// would leave the second person to click a link with "expired" on screen and
/// no idea their real problem is that they now cannot see anybody.
///
/// The join key points at a port nothing is listening on, so the enrolment
/// fails as loudly as it can. The network key must be on disk anyway.
///
/// Watched red: move the `if let Some(network_key) = input.network_key()` block
/// in `run_peer`'s `Join` arm below the `pair::join` call and this fails with
/// the peers file carrying no network key.
#[test]
fn a_link_with_a_dead_join_key_still_sets_the_network_key_through_the_cli() {
    let dir = scratch("link-cli");
    let peers = dir.join("tcr-peers.json");
    config::save(&peers, &PeerFile::default()).expect("write the peers file");

    let network_key = NetworkKey::from_bytes([0x9C; 32]);
    let link = pair::ShareLink {
        network_key,
        join: Some(pair::JoinToken {
            addr: "127.0.0.1:1".parse().expect("a loopback address"),
            registrar: PeerId([0xAB; 32]),
            secret: [0xCD; 32],
        }),
    }
    .to_link();

    let (out, err, ok) = run_tcr(&peers, &["join", &link]);
    assert!(
        !ok,
        "the enrolment half must fail (nothing is listening on that port), so this test \
         is about what survived a failure: {out}{err}"
    );
    assert!(
        out.contains("network key set"),
        "the command must say it set the network key before it tried to enrol: {out}"
    );

    let file = config::read_or_default(&peers).expect("the peers file reads");
    assert_eq!(
        file.network_key,
        Some(network_key),
        "a dead join key must not cost the network key: the two halves of a link are \
         independent, and the key is the half everyone who clicks late still needs"
    );

    // And the failure was reported rather than swallowed: a silent success
    // here would be worse than the refusal.
    assert!(
        err.contains("could not reach") || err.contains("peer join"),
        "the enrolment failure has to surface: {err}"
    );
}

/// **`tcr peer network-key set` prints the key ONCE, and nothing prints it
/// again.**
///
/// A key an operator can re-read from a terminal is a key in every scrollback
/// buffer on the Mac, so `show` reports presence and never the value.
///
/// Watched red: change the `Show` arm to print `key.to_paste_string()` and this
/// fails on the `show` assertion.
#[test]
fn the_network_key_is_printed_once_and_never_again() {
    let dir = scratch("netkey-cli");
    let peers = dir.join("tcr-peers.json");
    config::save(&peers, &PeerFile::default()).expect("write the peers file");

    let (out, err, ok) = run_tcr(&peers, &["network-key", "set"]);
    assert!(ok, "`tcr peer network-key set` must exit 0: {err}");
    let printed = out
        .lines()
        .next()
        .expect("the key is the first line")
        .trim()
        .to_string();
    assert_eq!(
        printed.chars().count(),
        52,
        "the minted key must be printed as its 52-character paste string: {out}"
    );

    // It is on disk, at 0600.
    let file = config::read_or_default(&peers).expect("the peers file reads");
    assert_eq!(
        file.network_key.map(|key| key.to_paste_string()).as_deref(),
        Some(printed.as_str()),
        "what was printed must be what was stored"
    );
    let mode = std::fs::metadata(&peers)
        .expect("stat the peers file")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "a shared secret lives at 0600, not {mode:o}");

    // And `show` says only that it is set.
    let (out, _err, ok) = run_tcr(&peers, &["network-key", "show"]);
    assert!(ok);
    assert!(out.contains("set"), "{out}");
    assert!(
        !out.contains(&printed),
        "`show` must never print the key again: {out}"
    );

    // `link` carries it, and the link round-trips to the same key.
    let (out, err, ok) = run_tcr(&peers, &["link"]);
    assert!(ok, "`tcr peer link` must exit 0: {err}");
    let link = out
        .lines()
        .next()
        .expect("the link is the first line")
        .trim();
    let parsed = pair::ShareLink::parse(link).expect("the printed link parses");
    assert_eq!(parsed.network_key.to_paste_string(), printed);
    assert!(
        parsed.join.is_none(),
        "without --invite a link carries the network key alone"
    );

    // `clear` removes it, and `link` then refuses rather than printing a link
    // with nothing in it.
    let (out, _err, ok) = run_tcr(&peers, &["network-key", "clear"]);
    assert!(ok);
    assert!(out.contains("cleared"), "{out}");
    let (_out, err, ok) = run_tcr(&peers, &["link"]);
    assert!(!ok, "a link with no network key must be refused");
    assert!(
        err.contains("network-key set"),
        "the refusal has to say what to run: {err}"
    );
}

/// **`tcr peer network-key set` over an existing key requires `--replace`,
/// and the refusal names what a replace cuts off.**
///
/// The verb lives in `main.rs`, applied elsewhere, so this test is the ask,
/// not a claim the production body is fixed: it is red until the patch
/// `run_peer_network_key`'s `Set` arm needs (a `--replace` flag on
/// `PeerNetworkKeyArgs`, checked when `file.network_key` is already
/// `Some`).
///
/// Watched red: as of this commit, a second `set` with no `--replace`
/// silently mints and stores a new key: `ok` is `true` and the printed key
/// changes: so the first two assertions below fail against the unpatched
/// binary.
#[test]
fn network_key_set_over_an_existing_key_requires_replace() {
    let dir = scratch("netkey-replace");
    let peers = dir.join("tcr-peers.json");
    config::save(&peers, &PeerFile::default()).expect("write the peers file");

    let (first, _err, ok) = run_tcr(&peers, &["network-key", "set"]);
    assert!(ok, "the first `set` must exit 0: {_err}");
    let first_key = first
        .lines()
        .next()
        .expect("the key is the first line")
        .trim()
        .to_string();

    // A second `set`, no `--replace`: refused, and the stored key is
    // untouched: every Mac still holding it must still be able to knock.
    let (_out, err, ok) = run_tcr(&peers, &["network-key", "set"]);
    assert!(
        !ok,
        "a second `set` with no `--replace` must be refused, not silently mint a new key"
    );
    assert!(
        err.contains("--replace"),
        "the refusal must name the flag that unblocks it: {err}"
    );
    assert!(
        err.contains("OLD key") || err.contains("old key") || err.contains("cuts off"),
        "the refusal must say what a replace cuts off: every Mac still on the old key: {err}"
    );
    let file = config::read_or_default(&peers).expect("the peers file reads");
    assert_eq!(
        file.network_key.map(|key| key.to_paste_string()).as_deref(),
        Some(first_key.as_str()),
        "a refused `set` must not have touched the stored key"
    );

    // With `--replace`: it mints a new one and says so.
    let (second, err, ok) = run_tcr(&peers, &["network-key", "set", "--replace"]);
    assert!(ok, "`set --replace` must exit 0: {err}");
    let second_key = second
        .lines()
        .next()
        .expect("the key is the first line")
        .trim()
        .to_string();
    assert_ne!(
        second_key, first_key,
        "`--replace` must mint a genuinely new key, not repeat the old one"
    );
    let file = config::read_or_default(&peers).expect("the peers file reads");
    assert_eq!(
        file.network_key.map(|key| key.to_paste_string()).as_deref(),
        Some(second_key.as_str()),
        "the replaced key must be what is stored"
    );
}

// ---------------------------------------------------------------------------
// Both tests below are asks to LEASE-WIRE
// (`listener.rs` and `state.rs` are their files), and these two
// tests are the red-first proof handed over with the patch. Neither test
// edits `listener.rs` or `state.rs`: both drive the shipped binary and the
// shipped `PeerState` API exactly as an operator or the panel would.
// ---------------------------------------------------------------------------

/// **Item 2: a bare reservation placeholder must never be operator-visible.**
///
/// [`state::PeerState::reserve_knock_slot`] writes a placeholder row: an
/// all-zero instance id, no name, `wire_version` zero, to hold an address's
/// slot before the handshake that would earn it real details has even
/// started (`state.rs:441-473`). That row is meant to be invisible: it is
/// not a Mac that asked to pair, it is a slot held against one that might.
///
/// Today every read surface (`tcr peer pending`, `tcr peer pending --json`,
/// `tcr peer ls --json`) reads `state.pending` directly with no filter, so a
/// reservation caught between "taken" and "released or filled in": which a
/// stalled connection can hold open indefinitely today, see the next test :
/// shows up as a pending row with nothing to show: no name, address only,
/// the all-zero instance id an operator cannot use with `tcr peer accept`.
///
/// Watched red against the shipped binary; green once `main.rs`'s three read
/// paths call a filter (`PeerState::visible_pending`, in the ask below)
/// instead of touching `state.pending` directly.
#[test]
fn a_reservation_placeholder_is_filtered_from_every_read_surface() {
    let dir = scratch("placeholder-filter");
    let peers = dir.join("tcr-peers.json");
    let state_file = dir.join("peer-state.json");
    config::save(&peers, &PeerFile::default()).expect("write the peers file");

    // A reservation placeholder, written the exact way
    // `PeerState::reserve_knock_slot` writes one: no production caller in
    // `listener.rs` needs to run for this row to exist.
    let mut value = PeerState::default();
    let created = value
        .reserve_knock_slot("192.0.2.77", pair::now_ms())
        .expect("a fresh reservation");
    assert!(
        created,
        "the address must be new for this to be a genuine reservation"
    );
    state::save(&state_file, &value).expect("write the state file");

    let (out, err, ok) = run_tcr(&peers, &["pending"]);
    assert!(ok, "`tcr peer pending` must exit 0: {err}");
    assert!(
        !out.contains("192.0.2.77"),
        "a bare reservation placeholder (no name learned, no instance id learned, the \
         handshake has not even started) must never be operator-visible as a pending row: \
         {out}"
    );

    let (out, err, ok) = run_tcr(&peers, &["pending", "--json"]);
    assert!(ok, "`tcr peer pending --json` must exit 0: {err}");
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).expect("valid JSON");
    assert_eq!(
        parsed["pending"].as_array().map(Vec::len),
        Some(0),
        "the panel reads this exact field, and a placeholder row here is a Mac the operator \
         never actually saw try to pair: {out}"
    );

    let (out, err, ok) = run_tcr(&peers, &["ls", "--json"]);
    assert!(ok, "`tcr peer ls --json` must exit 0: {err}");
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).expect("valid JSON");
    assert_eq!(
        parsed["pendingCount"].as_u64(),
        Some(0),
        "the count the panel's badge is built from must not include a reservation \
         placeholder: {out}"
    );
}

/// **Item 3: a knock that stalls after message 1 must be bounded and must
/// release its reservation, not hold a real pending row open forever.**
///
/// This is the regression testing found: `serve_knock` reserves the
/// address's slot (`listener.rs:974-985`) BEFORE `run_knock_handshake`
/// (`:1061-1086`) cryptographically validates message 1, writes message 2
/// and reads the knock frame: and nothing bounds that second half. A
/// connection that delivers a well-formed message 1 and then sends nothing
/// else holds its reservation exactly as long as it likes: `read_knock`
/// blocks on a read with no timeout, `MESSAGE_1_TIMEOUT` only ever bounded
/// the read of message 1's own bytes, which already happened.
///
/// The ask to LEASE-WIRE is to move the reservation to AFTER message 1 is
/// validated (so a wrong-key or malformed message 1 never creates a
/// placeholder at all) and to wrap the rest of the handshake: message 2
/// plus the knock frame read, in a `tokio::time::timeout(MESSAGE_1_TIMEOUT,
/// ..)`, releasing the reservation on either an error or that timeout firing.
///
/// Bounded by `MESSAGE_1_TIMEOUT * 3` so this test cannot hang forever on a
/// red run: it will fail the `elapsed` assertion instead of hanging.
///
/// Watched red against the shipped binary today.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_knock_after_message_1_is_bounded_and_releases_its_reservation() {
    let node = Node::new("cap-stall");
    let addr = serve(node.context()).await;

    let started = std::time::Instant::now();
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let message_1 = knock_message_1();
    let mut framed = Vec::with_capacity(2 + message_1.len());
    framed.extend_from_slice(
        &u16::try_from(message_1.len())
            .expect("a short frame")
            .to_be_bytes(),
    );
    framed.extend_from_slice(&message_1);
    stream.write_all(&framed).await.expect("write message 1");
    stream.flush().await.expect("flush message 1");

    // Message 1 has landed. The far side is now mid-handshake: it has
    // written or is about to write message 2, and is waiting to read the
    // knock frame this connection deliberately never sends. Every byte
    // needed to validate the caller has already arrived; the only thing
    // withheld is the frame that would complete the knock.
    let mut back = Vec::new();
    let closed = tokio::time::timeout(
        listener::MESSAGE_1_TIMEOUT * 3,
        stream.read_to_end(&mut back),
    )
    .await;
    let elapsed = started.elapsed();

    assert!(
        closed.is_ok(),
        "a knock stalled after message 1 must be closed within a bound the same size as \
         MESSAGE_1_TIMEOUT ({:?}), not held open indefinitely: still open after {elapsed:?}",
        listener::MESSAGE_1_TIMEOUT
    );
    assert!(
        elapsed < listener::MESSAGE_1_TIMEOUT * 2,
        "the close took {elapsed:?}, well past twice the {:?} bound this asks for",
        listener::MESSAGE_1_TIMEOUT
    );

    let state = node.state();
    assert!(
        state.pending.is_empty(),
        "a stalled knock handshake must release the reservation it took, not leave a \
         placeholder pending row behind: {:?}",
        state.pending
    );
}

/// **The help an operator reads agrees with the default the code applies** :
/// the review's L4, on `--announce-name`.
///
/// The doc said "On by default: a row that says 'studio-mac' is the difference
/// between a feature and a manual." That default says the opposite and so does
/// the code: `PeerFile::announce_name` is `#[serde(default)]` on a `bool`, and
/// the beacon carries a name only when the stored flag is set. So the behaviour
/// was right and the one sentence an operator reads before choosing was wrong.
///
/// Asserted against the SHIPPED `--help`, not against the source string,
/// because the source string is what was wrong and a test reading it would have
/// agreed with it.
///
/// Watched red: put "On by default" back in `PeerFindArgs::announce_name`'s doc
/// comment.
#[test]
fn the_announce_name_help_says_it_is_off_by_default() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_tcr"))
        .args(["peer", "find", "--help"])
        .output()
        .expect("spawn tcr peer find --help");
    assert!(
        output.status.success(),
        "`tcr peer find --help` must exit 0"
    );
    let help = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        help.contains("--announce-name"),
        "the flag this is about must be in the help: {help}"
    );
    assert!(
        help.to_lowercase().contains("off by default"),
        "the help must state the default the code applies: {help}"
    );
    assert!(
        !help.to_lowercase().contains("on by default"),
        "and must not still claim the opposite: {help}"
    );

    // The code's own answer, so this test is about a DISAGREEMENT and not
    // merely about a string: a fresh peers file announces no name.
    let fresh = PeerFile::default();
    assert!(
        !fresh.announce_name,
        "a file nobody has tuned announces no name, which is what the help now says"
    );
}

// ---------------------------------------------------------------------------
// The identifier and the locator are different types
// ---------------------------------------------------------------------------

/// **An older peers file keeps its routing advice.**
///
/// The old key was `addrs: ["host:port"]` and the new one is `endpoints`, a
/// list that also says WHEN each was observed and WHAT observed it. An
/// operator who upgrades has one file, not two, so the reader has to accept
/// the old key, and the next save has to write the new one, or the migration
/// runs again on every read forever.
///
/// Three facts, because three separate things can break: the endpoint arrives,
/// it arrives with the row's own `addedAt` as its observation instant (the only
/// honest answer, the old key carried no time of its own), and the file that
/// comes back out names `endpoints` and no longer names `addrs`.
///
/// Watched red: with the `for legacy in &disk.addrs` loop deleted from
/// `PeerRowOnDisk`'s `From` impl (`src/peer/config.rs`), this fails on the
/// first assertion, `left: 0, right: 1`, "an upgraded file keeps the one
/// address it was paired over", which is precisely the operator losing the
/// only way back to a Mac they had already trusted.
#[test]
fn a_peers_file_with_the_old_addrs_key_loads_and_re_saves_as_endpoints() {
    let dir = scratch("legacy-addrs");
    let path = dir.join("tcr-peers.json");

    // Written by hand rather than by `save`, because the shape under test is
    // the one no build in this tree can produce any more.
    let legacy = r#"{
      "peers": [
        {
          "node": "248H248H248H248H248H248H248H248H248H248H248H248H248G",
          "label": "studio-mac",
          "addrs": ["192.0.2.7:9600"],
          "addedAt": 1767225600000,
          "allow": {},
          "lend": []
        }
      ]
    }"#;
    std::fs::write(&path, legacy).expect("write the legacy peers file");
    // 0600, because  refuses a peers file this program did
    // not write: an invite secret sits in this file.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .expect("tighten the fixture to the mode the reader requires");

    let file = config::read_or_default(&path).expect("a legacy peers file still reads");
    let row = &file.peers[0];
    assert_eq!(
        row.endpoints.len(),
        1,
        "an upgraded file keeps the one address it was paired over: {:?}",
        row.endpoints
    );
    assert_eq!(
        row.endpoints[0],
        config::Endpoint::direct(
            "192.0.2.7:9600".parse().expect("the literal parses"),
            1_767_225_600_000,
            config::EndpointSource::Paired,
        ),
        "the old key was written at the pin, so it reads back as Paired, observed when the \
         row was added"
    );

    config::save(&path, &file).expect("the migrated file writes");
    let written: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("re-read the saved file"))
            .expect("the saved file is json");
    let saved = &written["peers"][0];
    assert!(
        saved["endpoints"].is_array(),
        "the save writes the new key: {saved}"
    );
    assert!(
        saved.get("addrs").is_none(),
        "and stops writing the old one, or the migration runs on every read forever: {saved}"
    );
    assert_eq!(
        saved["endpoints"][0]["kind"], "direct",
        "a socket endpoint is tagged `direct`, so a `via` hop is a different shape and not a \
         string that happens to look different: {saved}"
    );
}

// ---------------------------------------------------------------------------
// A pin that keeps no way back has trusted a Mac it cannot
// reach
// ---------------------------------------------------------------------------

/// **Both writers of a pinned row record the address the pin was taken over.**
///
/// There are exactly two: the six-digit compare (`pair::confirm`, run once on
/// each Mac, each recording the socket ITS handshake ran over) and the
/// registrar's half of an enrolment (`pair::accept_enrolment`, recording the
/// socket the joiner arrived on). Before this fix the first passed `None` and
/// the second wrote an empty list, so a Mac paired with six digits was pinned,
/// trusted, and filtered out of every candidate set that asks whether a row
/// can be reached at all.
///
/// Both halves are in one test because the invariant is about the CLASS: a row
/// a completed session created has at least one endpoint. A test per function
/// would let a third writer arrive and satisfy neither.
///
/// Watched red twice.
/// - `confirm` half: with `pin_row(store, *peer, &label, addr)` changed back to
///   `None`, this fails on "the six-digit pin records the socket the compare
///   ran over" with `left: 0, right: 1`.
/// - `accept_enrolment` half: with the `row.observe_endpoint(...)` line
///   deleted, it fails on "the registrar records the socket the joiner arrived
///   on", same numbers.
#[test]
fn every_pin_writer_records_the_address_it_was_reached_over() {
    let node = Node::new("pin-records-address");
    let store = config::PeerStore::open(&node.peers).expect("open the store");

    // ---- the six-digit compare, as `tcr peer pair <host:port> <code>` runs it
    let paired_over: SocketAddr = "192.0.2.7:9600".parse().expect("the literal parses");
    let peer = PeerId([0x21_u8; 32]);
    pair::confirm(&store, &peer, "123456", Some(paired_over)).expect("the pin writes");

    let row = node
        .file()
        .peers
        .into_iter()
        .find(|row| row.node == peer)
        .expect("the compare pinned a row");
    assert_eq!(
        row.endpoints.len(),
        1,
        "the six-digit pin records the socket the compare ran over: {:?}",
        row.endpoints
    );
    assert_eq!(
        row.endpoints[0].direct_addr(),
        Some(paired_over),
        "and it is that socket and not a rewritten one: {:?}",
        row.endpoints
    );
    assert_eq!(
        row.endpoints[0].source,
        config::EndpointSource::Paired,
        "recorded as Paired, because the pin itself is what observed it"
    );

    // ---- the registrar's half of an enrolment
    // An invite needs a listen address to hand out, so the file names one; the
    // port is never bound here because this half drives `accept_enrolment`
    // directly rather than over a socket.
    let mut file = node.file();
    file.listen = Some("127.0.0.1:9600".parse().expect("the literal parses"));
    node.write_file(&file);
    let store = config::PeerStore::open(&node.peers).expect("re-open after the listen edit");
    let (invite, _token) = pair::mint_invite_as(&store, &node.key, "attic-nuc", 600, 1)
        .expect("mint a one-use invite");
    let joiner = PeerId([0x22_u8; 32]);
    let arrived_from: SocketAddr = "192.0.2.8:51234".parse().expect("the literal parses");
    let enrolled = pair::accept_enrolment(
        &node.peers,
        joiner,
        &tcr_peer_wire::Enroll {
            invite_id: 0,
            label: "attic-nuc".to_string(),
        },
        &invite.secret,
        1_767_225_600_000,
        arrived_from,
    )
    .expect("the joiner proves the invite and is pinned");

    assert_eq!(
        enrolled.endpoints.len(),
        1,
        "the registrar records the socket the joiner arrived on: {:?}",
        enrolled.endpoints
    );
    assert_eq!(
        enrolled.endpoints[0].direct_addr(),
        Some(arrived_from),
        "and it is the observed socket, not anything the joiner claimed: {:?}",
        enrolled.endpoints
    );

    // The row on disk carries it too, which is the half that survives a
    // restart, a returned value nobody saved would pass the two assertions
    // above and still leave the operator with an unreachable peer.
    let saved = node
        .file()
        .peers
        .into_iter()
        .find(|row| row.node == joiner)
        .expect("the enrolment pinned a row");
    assert_eq!(
        saved.endpoints, enrolled.endpoints,
        "the endpoint is on disk and not only in the returned row"
    );
}

// ---------------------------------------------------------------------------
// An endpoint is a side effect of an authenticated session
// ---------------------------------------------------------------------------

/// **B changes its listen port, tells A in the next Hello, and A reaches it
/// without re-pairing.**
///
/// This is the whole of what "identity is the key, an address is advice" buys.
/// A pinned row is not re-pinned, no operator presses anything, no six digits
/// are compared again: B dials A inside an ordinary `IK` session against the
/// static key A already trusts, its `Hello` carries the socket it now listens
/// on, and A writes that against B's row. WireGuard's roaming rule, in this
/// tree's own types.
///
/// Driven over real loopback sockets and the shipped accept loop, because the
/// claim is about two facts meeting: the producer filling `Hello.addrs` and
/// the consumer merging them. A unit test on either half alone would pass with
/// the other half missing, which is exactly the state this tree was in, the
/// field has been on the wire since the skeleton with nothing filling it.
///
/// The stale endpoint is a POSITIVE CONTROL and not scenery: the first
/// assertion proves A really cannot reach B before the Hello, so the third
/// assertion is about the Hello and not about a port that was always open.
///
/// Watched red: with the `Control::Hello(incoming)` arm's
/// `config::observe_endpoints(...)` call deleted from
/// `listener::serve_control`, the last assertion fails, "A dials the port B
/// announced", because A's row still holds only the dead port, which is a
/// peer that moved one desk and is now unreachable forever.
#[tokio::test]
async fn a_peer_that_moved_announces_its_new_port_and_is_reached_there() {
    let a = Node::new("hello-refresh-a");
    let b = Node::new("hello-refresh-b");

    // A accepts; B's real listener is a second accept loop on its own port.
    let a_addr = serve(a.context()).await;
    let b_addr = serve(b.context()).await;

    // A dead port B is NOT listening on: bound, read for its number, dropped.
    let dead = {
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a port to learn its number");
        probe.local_addr().expect("the bound address")
    };

    // Both sides pin the other, which is what an `IK` session needs. A's row
    // for B names the dead port and nothing else, the state a Mac that moved
    // leaves behind.
    let a_store = config::PeerStore::open(&a.peers).expect("open A's store");
    let b_store = config::PeerStore::open(&b.peers).expect("open B's store");
    pair::confirm(&a_store, &b.key.id(), "111111", Some(dead)).expect("A pins B");
    pair::confirm(&b_store, &a.key.id(), "222222", Some(a_addr)).expect("B pins A");

    // B announces the port it really listens on.
    let mut b_file = b.file();
    b_file.listen = Some(b_addr);
    b.write_file(&b_file);

    // ---- control: before the Hello, A cannot reach B at all.
    let stale_row = a
        .file()
        .peers
        .into_iter()
        .find(|row| row.node == b.key.id())
        .expect("A has a row for B");
    assert!(
        teamclaude_rs::peer::serve::dial_peer(&stale_row)
            .await
            .is_none(),
        "the control: A's only endpoint for B is a port nobody listens on, so the dial \
         must fail before the Hello, otherwise the last assertion proves nothing"
    );

    // ---- B says hello to A, over the endpoint B has for A.
    let answered = teamclaude_rs::peer::serve::say_hello(&b_store, &a.key.id())
        .await
        .expect("the hello round trip completes")
        .expect("A is pinned and answered");
    assert_eq!(
        answered.node,
        a.key.id(),
        "the answer comes from A's own static key"
    );

    // ---- A's row for B now names the port B announced, and A reaches it.
    let refreshed = config::read_or_default(&a.peers)
        .expect("A's file reads")
        .peers
        .into_iter()
        .find(|row| row.node == b.key.id())
        .expect("A still has one row for B, not two");
    assert!(
        refreshed
            .endpoints
            .iter()
            .any(|endpoint| endpoint.direct_addr() == Some(b_addr)),
        "A learned the port B announced: {:?}",
        refreshed.endpoints
    );
    assert_eq!(
        refreshed.added_at, stale_row.added_at,
        "and learned it without re-pinning: the row is the same row, so every grant on it \
         survived"
    );
    assert!(
        teamclaude_rs::peer::serve::dial_peer(&refreshed)
            .await
            .is_some(),
        "A dials the port B announced and B answers: {:?}",
        refreshed.endpoints
    );
}

/// **A completed session registers the pair's rendezvous secret, with nothing
/// sent to say so.**
///
/// The dialler and the responder both end on the same handshake hash, and each
/// one derives the pair's port secret from its own copy: that is what lets a
/// moved Mac be met on a port neither end ever named. Until this fix the hash
/// was dropped by `into_transport` the instant the session went to transport
/// mode, so `reach::remember_pair_hash` had no production caller and the
/// register stayed empty on a real session, no matter how many completed.
///
/// The `is_none` before the handshake is the positive control: the key pair is
/// generated in this test, so nothing else in this binary can have registered
/// it, and an assertion that only ever saw a populated register would pass
/// against a register that is never emptied.
///
/// Watched red: drop the `remember_pair_hash` call from `noise.rs`'s dialling
/// constructor and the second assertion reads `None`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_completed_return_visit_registers_the_pairs_rendezvous_secret() {
    use teamclaude_rs::peer::reach;

    let node = Node::new("rendezvous-secret");
    let (peer_secret, peer_public) = noise::generate_static().expect("a peer keypair");
    let peer_id = PeerId(peer_public);

    let mut file = node.file();
    file.peers.push(config::PeerRow {
        node: peer_id,
        label: "laptop-9".to_string(),
        endpoints: Vec::new(),
        added_at: 0,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: config::Allow::default(),
        lend: Vec::new(),
    });
    node.write_file(&file);

    let addr = serve(node.context()).await;
    let responder_public = node.key.id().0;
    let responder_id = PeerId(responder_public);

    assert!(
        reach::port_secret_for(&responder_id).is_none(),
        "the responder's key was generated for this test, so nothing may hold a secret \
         for it before the handshake runs"
    );

    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let session = noise::dial_handshake(
        &mut stream,
        &peer_secret,
        Handshake::Return,
        Some(&responder_public),
        None,
    )
    .await
    .expect("a pinned peer completes a return visit");

    assert!(
        !session.handshake_hash.is_empty(),
        "the session carries the hash it ended on, or nothing downstream can derive a port"
    );
    assert_eq!(
        reach::port_secret_for(&responder_id),
        Some(reach::port_secret(&session.handshake_hash)),
        "the register holds the DERIVED secret for this pair, and it is the one \
         `reach::port_secret` computes from the session's own hash"
    );
}

/// **A `Hello` carries a brief about a mutual friend, over the shipped accept
/// loop, and the far side records it.**
///
/// The unit gate for the brief wiring. `discovery::neighbor_briefs` and
/// `observe_neighbor_briefs` have tests of their own, and those tests call
/// both functions directly: they stay green with every call site deleted,
/// which was the state of this tree before this fix, the field on the wire
/// and nothing filling it.
///
/// Driven the way `a_peer_that_moved_announces_its_new_port_...` above is, on
/// two real listeners, because the claim is again about two halves meeting:
/// A's answering `Hello` PRODUCING a brief for C, and B's dialling half
/// RECORDING it.
///
/// C never runs. It does not have to: a brief is one trusted Mac's word about
/// another, and what is under test is whether that word travels and lands, not
/// whether C is awake.
///
/// Watched red: with `facts.briefs` set to an empty `Vec` in
/// `listener::serve_control`'s `Control::Hello` arm, the answered Hello
/// carries no brief and the first assertion fails.
#[tokio::test]
async fn a_hello_carries_a_brief_for_a_mutual_friend_and_the_far_side_records_it() {
    let a = Node::new("brief-wire-a");
    let b = Node::new("brief-wire-b");
    let c = PeerId([0x5c; 32]);
    let c_addr: SocketAddr = "192.0.2.44:9700".parse().expect("a literal address");

    let a_addr = serve(a.context()).await;

    let a_store = config::PeerStore::open(&a.peers).expect("open A's store");
    let b_store = config::PeerStore::open(&b.peers).expect("open B's store");
    pair::confirm(&a_store, &b.key.id(), "111111", None).expect("A pins B");
    pair::confirm(&b_store, &a.key.id(), "222222", Some(a_addr)).expect("B pins A");

    // A knows where C answers and grants B the one level of briefs. B trusts C
    // too, with no address for it at all: the state a Mac is in when a friend
    // it has not spoken to since it moved.
    let mut a_file = a.file();
    a_file.peers.push(config::PeerRow {
        node: c,
        label: "studio-mac".to_string(),
        endpoints: vec![config::Endpoint::direct(
            c_addr,
            1_000,
            config::EndpointSource::Hello,
        )],
        added_at: 0,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: config::Allow::default(),
        lend: Vec::new(),
    });
    for row in a_file.peers.iter_mut() {
        if row.node == b.key.id() {
            row.allow.control.briefs = true;
        }
    }
    a.write_file(&a_file);

    let mut b_file = b.file();
    b_file.peers.push(config::PeerRow {
        node: c,
        label: "the-studio".to_string(),
        endpoints: Vec::new(),
        added_at: 0,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: config::Allow::default(),
        lend: Vec::new(),
    });
    // B grants A the same flag, because `allow.control.briefs` is read on both
    // sides now: it decides what this node TELLS a peer and what it ACCEPTS
    // from one. Before that, any pinned peer could write endpoints onto rows
    // here, whatever the operator had granted it. So the two nodes swapping
    // neighbour lists is two grants, and this test sets both rather than
    // relying on the applying side taking whatever arrives.
    for row in b_file.peers.iter_mut() {
        if row.node == a.key.id() {
            row.allow.control.briefs = true;
        }
    }
    b.write_file(&b_file);

    // ---- control: B holds no way to reach C before the Hello.
    let before = b
        .file()
        .peers
        .into_iter()
        .find(|row| row.node == c)
        .expect("B has a row for C");
    assert!(
        before.endpoints.is_empty(),
        "the control: B must start with no endpoint for C, or the assertion below is \
         about an address B always had"
    );

    let answered = teamclaude_rs::peer::serve::say_hello(&b_store, &a.key.id())
        .await
        .expect("the hello round trip completes")
        .expect("A is pinned and answered");

    // ---- the producing half: A's answer names C.
    let briefs = answered
        .briefs
        .as_deref()
        .expect("A granted B briefs, so the answer carries the list rather than omitting it");
    assert!(
        briefs
            .iter()
            .any(|brief| brief.node == c
                && brief.addrs.iter().any(|addr| addr == &c_addr.to_string())),
        "A's answering Hello has to carry C's locator, or nothing downstream can record \
         it: {briefs:?}"
    );

    // ---- the recording half: B's row for C gains that locator, as a brief.
    let after = config::read_or_default(&b.peers)
        .expect("B's file reads")
        .peers
        .into_iter()
        .find(|row| row.node == c)
        .expect("B still has one row for C, not two");
    let learned = after
        .endpoints
        .iter()
        .find(|endpoint| endpoint.direct_addr() == Some(c_addr))
        .unwrap_or_else(|| panic!("C's locator, learned through A, is on B's row: {after:?}"));
    assert_eq!(
        learned.source,
        config::EndpointSource::Brief,
        "recorded as what taught it, a trusted peer's word, and not as a session B ran \
         with C itself"
    );
}

// ---------------------------------------------------------------------------
// The CLI verbs that used to overwrite, or to say ok and store nothing
// ---------------------------------------------------------------------------

/// Run the `tcr` this build produced, against one Mac's files.
fn tcr(args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_tcr"))
        .args(args)
        .output()
        .expect("the tcr this build produced runs")
}

/// **A join link carrying a network key does not replace one this Mac already
/// has, unless the operator says so.**
///
/// `tcr peer network-key join` has refused this since it shipped, and
/// `docs/peers.md` promises the refusal, but `tcr peer join` took the same key
/// by a different door and overwrote in silence: one clicked `tcr://peer/join`
/// link was enough to cut a Mac off from every Mac still holding the old key.
///
/// Watch it fail by removing the `replaced && !a.replace` refusal in the Join
/// arm: the first run below exits 0 and the second assertion, that the old key
/// is still on disk, goes red.
#[test]
fn a_join_link_does_not_replace_a_network_key_without_replace() {
    let theirs = Node::new("join-link-source");
    let mine = Node::new("join-link-target");

    // The other Mac's link: its own network key, no invite.
    let minted = tcr(&[
        "peer",
        "network-key",
        "set",
        "--peers",
        theirs.peers.to_str().expect("a utf-8 path"),
    ]);
    assert!(minted.status.success(), "the other Mac mints a key");
    let linked = tcr(&[
        "peer",
        "link",
        "--peers",
        theirs.peers.to_str().expect("a utf-8 path"),
    ]);
    assert!(linked.status.success(), "the other Mac prints a link");
    let link = String::from_utf8_lossy(&linked.stdout)
        .lines()
        .next()
        .expect("the link is the first line")
        .to_string();

    // This Mac already has one.
    assert!(tcr(&[
        "peer",
        "network-key",
        "set",
        "--peers",
        mine.peers.to_str().expect("a utf-8 path"),
    ])
    .status
    .success());
    let before = mine.file().network_key.expect("this Mac has a key");

    let refused = tcr(&[
        "peer",
        "join",
        &link,
        "--peers",
        mine.peers.to_str().expect("a utf-8 path"),
    ]);
    assert!(
        !refused.status.success(),
        "a link that would replace the network key is refused: {}",
        String::from_utf8_lossy(&refused.stdout)
    );
    let text = String::from_utf8_lossy(&refused.stderr).to_string();
    assert!(
        text.contains("--replace"),
        "the refusal names the flag that means it: {text}"
    );
    assert_eq!(
        mine.file().network_key.expect("the key is still there"),
        before,
        "a refusal changes nothing on disk"
    );

    // And with the flag, it is taken: the refusal is a gate, not a wall.
    let taken = tcr(&[
        "peer",
        "join",
        &link,
        "--replace",
        "--peers",
        mine.peers.to_str().expect("a utf-8 path"),
    ]);
    assert!(
        taken.status.success(),
        "--replace means it: {}",
        String::from_utf8_lossy(&taken.stderr)
    );
    assert_ne!(
        mine.file().network_key.expect("a key is set"),
        before,
        "the link's key replaced this Mac's own"
    );
}

/// **A join link carrying BOTH a network key and a join key still pairs when
/// the network key alone is refused.**
///
/// Before this fix, the network-key refusal `anyhow::bail!`ed the whole
/// `Join` arm, so a link with both halves paired NOTHING when this Mac
/// already held a different network key: the join token was never even
/// read.
///
/// The join token here points at a port nothing is listening on, so the join
/// half fails as loudly as it can; this test is about what still RAN before
/// that failure, not about a successful pairing.
///
/// Watched red: restore `anyhow::bail!` where `network_key_refusal =
/// Some(...)` now sits, and the CLI's stderr goes back to the refusal alone,
/// never reaching `pair::join`'s own "could not reach". The assertion below
/// on that string is what catches it.
#[test]
fn a_join_link_with_both_keys_still_reads_the_join_token_when_the_network_key_is_refused() {
    let dir = scratch("join-link-both-keys");
    let peers = dir.join("tcr-peers.json");
    config::save(&peers, &PeerFile::default()).expect("write the peers file");

    // This Mac already has a network key.
    let (_out, _err, ok) = run_tcr(&peers, &["network-key", "set"]);
    assert!(ok, "this Mac mints its own key first");
    let before = config::read_or_default(&peers)
        .expect("the peers file reads")
        .network_key
        .expect("this Mac has a key");

    // A link from elsewhere, carrying a DIFFERENT network key and a join
    // token that points nowhere.
    let other_key = NetworkKey::from_bytes([0x77; 32]);
    let link = pair::ShareLink {
        network_key: other_key,
        join: Some(pair::JoinToken {
            addr: "127.0.0.1:1".parse().expect("a loopback address"),
            registrar: PeerId([0xAB; 32]),
            secret: [0xCD; 32],
        }),
    }
    .to_link();

    let (out, err, ok) = run_tcr(&peers, &["join", &link]);
    assert!(
        !ok,
        "the join half must fail (nothing is listening on that port): {out}{err}"
    );
    assert!(
        out.contains("a network key is already set on this Mac"),
        "the network-key refusal is still reported, on stdout not a bail: {out}"
    );
    assert!(
        err.contains("could not reach"),
        "the join token must have been READ and tried, so the failure is \
         pair::join's own connect error, not the earlier network-key bail: {err}"
    );

    let file = config::read_or_default(&peers).expect("the peers file reads");
    assert_eq!(
        file.network_key,
        Some(before),
        "a refused network key must not land on disk"
    );
}

/// **`tcr peer share on` keeps the mode and the schedule of the grant it is
/// replacing, and `--fraction 0` removes rather than grants.**
///
/// It used to build a fresh `LendGrant::new` per row: mode serve, no end, no
/// daily window. So an operator who had lent one Mac a BEARER (`--mode hand`,
/// ending at 18:00, weekdays only) and then adjusted the shared fraction got a
/// serve grant with no end and no schedule, and nothing said so. `peer lend`
/// already carries that rule and explains it; this verb now does too.
///
/// Watch it fail by dropping the `replacing` block in the Share arm: `mode`
/// reads `serve` and `until` reads null.
#[test]
fn share_on_keeps_the_mode_and_the_window_of_the_grant_it_replaces() {
    use teamclaude_rs::peer::config::{LendGrant, LendMode};

    let mac = Node::new("share-preserves-mode");
    let mut file = mac.file();
    let mut granted = LendGrant::new(tcr_peer_wire::Window::SevenDay, 0.1, 600, 1);
    granted.mode = LendMode::Hand;
    granted.until = Some(4_102_444_800);
    granted.between = Some("09:00-18:00".parse().expect("a daily window"));
    granted.ensure_id().expect("an id");
    file.peers
        .push(pinned_row(PeerId([0x31; 32]), "studio-mac", granted));
    mac.write_file(&file);

    let shared = tcr(&[
        "peer",
        "share",
        "on",
        "--window",
        "7d",
        "--fraction",
        "0.25",
        "--peers",
        mac.peers.to_str().expect("a utf-8 path"),
    ]);
    assert!(
        shared.status.success(),
        "share on: {}",
        String::from_utf8_lossy(&shared.stderr)
    );

    let after = mac.file();
    let grant = after.peers[0]
        .lend
        .first()
        .expect("the row still has its grant")
        .clone();
    assert_eq!(
        grant.mode,
        LendMode::Hand,
        "a hand grant must not become a serve grant because the fraction moved: {grant:?}"
    );
    assert_eq!(
        grant.until,
        Some(4_102_444_800),
        "the end the operator set survives: {grant:?}"
    );
    assert!(
        grant.between.is_some(),
        "and so does the daily window: {grant:?}"
    );
    assert!(
        (grant.fraction - 0.25).abs() < f64::EPSILON,
        "the fraction is the thing that DID change: {grant:?}"
    );

    // Zero is a removal, the rule `peer lend` already follows.
    let removed = tcr(&[
        "peer",
        "share",
        "on",
        "--window",
        "7d",
        "--fraction",
        "0",
        "--peers",
        mac.peers.to_str().expect("a utf-8 path"),
    ]);
    assert!(
        removed.status.success(),
        "share on --fraction 0: {}",
        String::from_utf8_lossy(&removed.stderr)
    );
    assert!(
        mac.file().peers[0].lend.is_empty(),
        "--fraction 0 removes the grant rather than lending nothing: {:?}",
        mac.file().peers[0].lend
    );
}

/// One pinned row with one grant, for the CLI tests above.
fn pinned_row(
    node: PeerId,
    label: &str,
    grant: teamclaude_rs::peer::config::LendGrant,
) -> teamclaude_rs::peer::config::PeerRow {
    teamclaude_rs::peer::config::PeerRow {
        node,
        label: label.to_string(),
        endpoints: Vec::new(),
        added_at: 1,
        allow: Default::default(),
        lend: vec![grant],
        rendezvous_secret: None,
        sees_us_at: None,
    }
}

/// **A block or an ignore whose target is neither a pending request nor an
/// address is an error, not an ok.**
///
/// Both verbs used to fall back to the raw argv. The listener compares against
/// the bare IP a connection arrived on, so `tcr peer block laptop` recorded a
/// ban nothing could ever equal and printed ok: the operator believed a Mac was
/// blocked and it was not.
///
/// Watch it fail by restoring the `None => a.target.clone()` arm: the command
/// exits 0 and the ban lands under the name.
#[test]
fn block_and_ignore_refuse_a_target_that_is_neither_a_request_nor_an_address() {
    let mac = Node::new("block-selector");
    let peers = mac.peers.to_str().expect("a utf-8 path").to_string();

    for verb in ["block", "ignore"] {
        let refused = tcr(&["peer", verb, "studio-mac", "--peers", &peers]);
        assert!(
            !refused.status.success(),
            "`tcr peer {verb} studio-mac` must not report success: {}",
            String::from_utf8_lossy(&refused.stdout)
        );
        let text = String::from_utf8_lossy(&refused.stderr).to_string();
        assert!(
            text.contains("is not an address"),
            "the refusal says what the target had to be: {text}"
        );
    }
    let state = mac.state();
    assert!(
        state.banned.is_empty() && state.muted.is_empty(),
        "a refusal writes nothing: {state:?}"
    );

    // The control: a bare IP with no pending row is still allowed, because
    // "make this stop" is a reasonable thing to type at a row that expired.
    let accepted = tcr(&["peer", "block", "192.0.2.51", "--peers", &peers]);
    assert!(
        accepted.status.success(),
        "an address is still accepted: {}",
        String::from_utf8_lossy(&accepted.stderr)
    );
    assert!(
        mac.state()
            .banned
            .iter()
            .any(|ban| ban.addr == "192.0.2.51"),
        "and it lands under the address the listener compares"
    );
}

/// **A reservation placeholder is not something `accept` can open a window
/// for.**
///
/// The placeholder holds the pending cap for a connection that has said
/// nothing yet; it carries an all-zero instance id and every screen already
/// hides it (`PeerState::visible_pending`). `find_pending` matched it anyway,
/// so `tcr peer accept <address>` against a stalled connection opened a
/// 120-second window keyed to an id no real pairing can present, and told the
/// operator that Mac was admitted.
///
/// Watch it fail by dropping the `is_reservation_placeholder` filter in
/// `find_pending`.
#[test]
fn a_reservation_placeholder_is_never_selected() {
    let mut state = PeerState::default();
    let reserved_at = 1_000;
    state
        .reserve_knock_slot("192.0.2.9", reserved_at)
        .expect("the slot is reserved");
    assert!(
        state.find_pending("192.0.2.9").is_none(),
        "a placeholder is not a pairing request: {:?}",
        state.pending
    );
    assert!(
        state.accept_knock("192.0.2.9", reserved_at, 120).is_none(),
        "and no window opens for one: {:?}",
        state.accepted
    );
    assert!(
        state.accepted.is_empty(),
        "nothing was admitted: {:?}",
        state.accepted
    );
    assert_eq!(
        state.pending.len(),
        1,
        "the reservation still holds its slot, which is what it is for"
    );
}

/// **A static key a pairing revealed outlives the two-minute window, so a
/// block afterwards bans the key as well as the address.**
///
/// The key is written onto the accepted window and nowhere else, and the window
/// was dropped the moment it expired. An operator blocking a Mac after the
/// digits failed to match, which is the ordinary reason to block, recorded an
/// address-only ban and read "no handshake ever revealed a static key". A new
/// DHCP lease walked past it.
///
/// Watch it fail by restoring the plain deadline test in `PeerState::expire`.
#[test]
fn a_learned_key_survives_the_window_it_was_learned_in() {
    let mut state = PeerState::default();
    let learned = PeerId([0x44; 32]);
    state
        .accepted
        .push(teamclaude_rs::peer::state::AcceptedInstance {
            instance_id: InstanceId([0x01; INSTANCE_ID_BYTES]),
            addr: "192.0.2.30".to_string(),
            until_ms: 2_000,
            opened_at_ms: 1_000,
            learned_key: Some(learned),
        });
    // A window with no key at all: it goes, which is what keeps the kept rows
    // from being "expire stopped working".
    state
        .accepted
        .push(teamclaude_rs::peer::state::AcceptedInstance {
            instance_id: InstanceId([0x02; INSTANCE_ID_BYTES]),
            addr: "192.0.2.31".to_string(),
            until_ms: 2_000,
            opened_at_ms: 1_000,
            learned_key: None,
        });

    let after_the_window = 900_000;
    state.expire(after_the_window);

    assert_eq!(
        state.key_learned_at("192.0.2.30"),
        Some(learned),
        "the key that address revealed is still readable: {:?}",
        state.accepted
    );
    assert_eq!(
        state.key_learned_at("192.0.2.31"),
        None,
        "and a window that learned nothing left nothing behind"
    );
    assert!(
        !state.accepted_window(
            "192.0.2.30",
            &InstanceId([0x01; INSTANCE_ID_BYTES]),
            after_the_window
        ),
        "the kept row authorizes nothing: it is a record, not a window"
    );

    state.ban(
        "192.0.2.30",
        state.key_learned_at("192.0.2.30"),
        BanReason::Blocked,
        after_the_window,
    );
    assert!(
        state.is_key_banned(&learned),
        "so the block bans the key as well as the address: {:?}",
        state.banned
    );
}

/// **An open pairing window does not cost a learned key.**
///
/// A closed accepted window is kept for the static key its handshake revealed,
/// which is the half of a ban that survives a new DHCP lease, and the list is
/// bounded at [`state::MAX_LEARNED_KEYS`]. The bound was counted in CLOSED rows
/// and the eviction in the length of the WHOLE list, open windows included, so
/// every window still open evicted one extra key: the oldest went early, and an
/// operator who blocked that Mac afterwards got an address-only ban against a
/// key this node had really learned.
///
/// Measured at the bound, with exactly one row over it and one window open, so
/// the arithmetic is the only thing that can decide the count.
///
/// Watch it fail by putting `self.accepted.len()` back in `PeerState::expire`'s
/// `take(..)`: two closed rows go instead of one and the oldest key is gone.
#[test]
fn an_open_window_does_not_evict_an_extra_learned_key() {
    let now = 10_000_000_i64;
    let mut value = PeerState::default();

    // One over the bound, every one closed and every one carrying a key. The
    // oldest is first, so "the oldest went" is visible by name.
    for nth in 0..=state::MAX_LEARNED_KEYS {
        let byte = u8::try_from(nth).expect("the bound is well under 255");
        value.accepted.push(state::AcceptedInstance {
            instance_id: InstanceId([byte; INSTANCE_ID_BYTES]),
            addr: format!("10.0.0.{byte}"),
            opened_at_ms: now - 600_000 + i64::from(byte),
            until_ms: now - 300_000,
            learned_key: Some(PeerId([byte; 32])),
        });
    }
    // And one window still OPEN, which is what used to cost a key.
    value.accepted.push(state::AcceptedInstance {
        instance_id: InstanceId([200_u8; INSTANCE_ID_BYTES]),
        addr: "10.0.0.200".to_string(),
        opened_at_ms: now - 1_000,
        until_ms: now + 60_000,
        learned_key: None,
    });

    value.expire(now);

    let kept_keys = value
        .accepted
        .iter()
        .filter(|window| window.learned_key.is_some())
        .count();
    assert_eq!(
        kept_keys,
        state::MAX_LEARNED_KEYS,
        "one row over the bound drops exactly one row, whatever else is open: {:?}",
        value
            .accepted
            .iter()
            .map(|window| (window.addr.clone(), window.learned_key.is_some()))
            .collect::<Vec<_>>()
    );
    assert!(
        value.key_learned_at("10.0.0.1").is_some(),
        "and the second-oldest key is still there: only the oldest was over the bound"
    );
    assert!(
        value.key_learned_at("10.0.0.0").is_none(),
        "while the oldest is the one that went, which is the rule the bound states"
    );
    assert!(
        value
            .accepted
            .iter()
            .any(|window| window.addr == "10.0.0.200"),
        "the open window is untouched: it is not a learned key and never was"
    );
}

// ---------------------------------------------------------------------------
// revoke and enrolment race on the same invite
// ---------------------------------------------------------------------------

/// **A concurrent revoke and enrolment never resurrect the invite or drop the
/// pinned row.**
///
/// `revoke_invite` and `mint_invite_as` read-modify-write the peers file with
/// no lock, while `accept_enrolment` holds `FileLock` across its own
/// read-modify-write. Unlocked, a revoke that reads before and saves after
/// `accept_enrolment`'s own save clobbers it with a stale copy: either the
/// revoked invite comes back (the stale copy still has it) or the row
/// `accept_enrolment` just pinned disappears (the stale copy predates it).
///
/// Watched red: remove the `FileLock::acquire` line this test's production
/// change adds to `revoke_invite`, and this fails inside the first few trials
/// with either "the invite came back" or "accept_enrolment said Ok but the row
/// is gone".
#[test]
fn a_concurrent_revoke_and_enrolment_never_resurrect_or_drop_a_row() {
    const TRIALS: usize = 20;

    for trial in 0..TRIALS {
        let node = Node::new(&format!("revoke-race-{trial}"));
        let mut file = node.file();
        file.listen = Some("127.0.0.1:9600".parse().expect("a loopback address"));
        node.write_file(&file);

        let store = config::PeerStore::open(&node.peers).expect("open the store");
        let (invite, token) = pair::mint_invite_as(&store, &node.key, "laptop-2", 600, 1)
            .expect("mint a one-use invite");

        let joiner = PeerId([0x33; 32]);
        let enroll = tcr_peer_wire::Enroll {
            invite_id: 0,
            label: "laptop-2".to_string(),
        };

        let peers_path = node.peers.clone();
        let now = pair::now_ms();
        let barrier = Arc::new(std::sync::Barrier::new(2));

        let revoke_handle = {
            let store = config::PeerStore::open(&node.peers).expect("open the store");
            let barrier = Arc::clone(&barrier);
            let id = invite.id;
            std::thread::spawn(move || {
                barrier.wait();
                pair::revoke_invite(&store, id)
            })
        };
        let enrol_handle = {
            let peers_path = peers_path.clone();
            let secret = token.secret;
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                pair::accept_enrolment(
                    &peers_path,
                    joiner,
                    &enroll,
                    &secret,
                    now,
                    std::net::SocketAddr::from(([127, 0, 0, 1], 9600)),
                )
            })
        };

        let revoked = revoke_handle.join().expect("the revoking thread");
        let enrolled = enrol_handle.join().expect("the enrolling thread");

        let after = node.file();
        assert!(
            after.pending_invites.iter().all(|row| row.id != invite.id),
            "trial {trial}: the invite came back after a concurrent revoke and enrolment \
             raced (revoked={revoked:?}), pending_invites={:?}",
            after.pending_invites
        );

        if let Ok(row) = &enrolled {
            assert!(
                after.peers.iter().any(|existing| existing.node == row.node),
                "trial {trial}: accept_enrolment said Ok but the pinned row for {:?} is gone \
                 from disk after the race with revoke_invite (revoked={revoked:?})",
                row.node
            );
        }

        let _ = std::fs::remove_dir_all(&node.dir);
    }
}
