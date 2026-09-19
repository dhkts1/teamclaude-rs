//! Try to beat every abuse-resistance
//! cap.** Adversarial by construction, not descriptive.
//!
//! # What this file is for, and what it is not
//!
//! `tests/peer_pairing.rs` already asserts each cap's number once, in the
//! ordinary sequential shape: the fourth knock in ten seconds, the ninth
//! outstanding knock, the third unauthenticated socket, an `XX` from an
//! unaccepted instance. Every one of those is a test that the cap is WIRED.
//! None of them is an attempt to WIN, and a token bucket in particular cannot
//! be beaten by a sequential loop: by construction the loop hands the bucket
//! the one thing it needs, which is time between calls.
//!
//! So everything here is an attack with a shape the sequential tests cannot
//! produce: many callers released at once from a barrier, a fresh random
//! instance id per connection, a fresh proposed name per connection, a socket
//! that delivers a valid message 1 and then goes silent to hold what it
//! claimed, a burst timed to land exactly on the bucket's refill instant, and
//! a frame sized to the byte at the knock bound.
//!
//! # Everything runs on this box, against temp files
//!
//! Every listener binds `127.0.0.1:0` (the kernel picks the port), every peers
//! file and state file is a fresh temp file, and no test connects to a port it
//! did not bind itself. **Nothing here touches the proxy on `127.0.0.1:3456`**
//!: no test starts, stops, signals or dials it. Accounts, names and addresses
//! are obviously fake (`alice@example.com`, `192.0.2.0/24`, the documentation
//! range), because this repository is public.
//!
//! # One address is all a loopback test has
//!
//! macOS aliases only `127.0.0.1` onto `lo0` (measured: binding `127.0.0.2`
//! fails with `EADDRNOTAVAIL`), and adding an alias needs `ifconfig` as root.
//! So every cap whose grain is "per address" is attacked from the one address a
//! test can dial from, and the NODE-WIDE arm of the socket cap
//! ([`listener::MAX_UNAUTHENTICATED_SOCKETS`], 16 across all addresses) is not
//! reachable end to end here: the per-address arm refuses at
//! [`listener::MAX_UNAUTHENTICATED_PER_ADDRESS`] long before sixteen. That arm
//! is attacked at the counter the accept loop itself decides on
//! ([`listener::Admission`]), which is the same state and not a copy of it.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tcr_peer_wire::{InstanceId, Knock, PeerId, INSTANCE_ID_BYTES, PROTO_VERSION};
use teamclaude_rs::peer::config::{self, Allow, Endpoint, EndpointSource, PeerFile, PeerRow};
use teamclaude_rs::peer::discovery::{self, Discovered, FoundList};
use teamclaude_rs::peer::id::NodeKey;
use teamclaude_rs::peer::listener::{self, Admission, SessionContext};
use teamclaude_rs::peer::noise;
use teamclaude_rs::peer::pair;
use teamclaude_rs::peer::state::{self, PeerState};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

// ---------------------------------------------------------------------------
// The plan's numbers, transcribed: not the production constants
// ---------------------------------------------------------------------------

/// Every cap as `abuse-resistance.md` states it, written out as
/// a literal here.
///
/// # Why a second copy, when the production constant is `pub` and right there
///
/// Because an attack test that asserts against the constant it is defending
/// cannot fail when that constant is loosened. `assert_eq!(taken,
/// listener::KNOCK_BURST)` passes at a burst of 3 and passes just as happily at
/// a burst of 30: the mutation the positive control depends on is exactly the
/// one such an assertion is blind to, so the test would report success while
/// testing nothing.
///
/// So the assertions in this file are against these literals: the numbers the
/// record shipped, and [`the_caps_are_still_the_numbers_the_plan_shipped`]
/// pins each production constant to its literal in one place. Loosening any cap
/// therefore reds two tests: that one, and every attack that spends the cap.
///
/// This is the one duplication in the file, it is deliberate, and it is the
/// reason the positive controls in the report mean anything.
mod plan {
    use std::time::Duration;

    /// `1 knock / 10 s, burst 3, per address`.
    pub const KNOCK_BURST: u32 = 3;
    /// `1 knock / 10 s, burst 3, per address`.
    pub const KNOCK_INTERVAL_MS: i64 = 10_000;
    /// `at most 8 PENDING per node`.
    pub const MAX_PENDING_KNOCKS: usize = 8;
    /// `16 concurrent unauthenticated sockets per node`.
    pub const MAX_UNAUTHENTICATED_SOCKETS: usize = 16;
    /// `2 per address`.
    pub const MAX_UNAUTHENTICATED_PER_ADDRESS: usize = 2;
    /// `5 s to deliver message 1`.
    pub const MESSAGE_1_TIMEOUT: Duration = Duration::from_secs(5);
    /// The handshake bound the listener's own doc-comment states.
    pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
    /// `message 1 capped at 64 bytes` is the plan's figure for message 1
    /// itself; the KNOCK FRAME the session carries is bounded separately, and
    /// `listener::MAX_KNOCK_FRAME_BYTES` is the number the code shipped for it.
    pub const MAX_KNOCK_FRAME_BYTES: usize = 512;
    /// `found list capped at 12 rows`.
    pub const MAX_FOUND_ROWS: usize = 12;
    /// `at most 2 rows per source address`.
    pub const MAX_FOUND_PER_ADDRESS: usize = 2;
    /// `a row that stops announcing leaves after 60 s`.
    pub const FOUND_TTL_MS: i64 = 60_000;
    /// `an ignored address is muted for 1 h`.
    pub const MUTE_MS: i64 = 3_600_000;
    /// `one log line per address per hour`.
    pub const REFUSAL_LOG_QUIET_MS: i64 = 3_600_000;
    /// How many addresses that log remembers: the number the code shipped,
    /// and the cost of beating the line above.
    pub const REFUSAL_LOG_ADDRESSES: usize = 1_024;
    /// `COMPARING has a 120 s window keyed to one instance id`.
    pub const PAIRING_WINDOW_SECS: i64 = 120;
}

/// **Every cap is still the number the record shipped.**
///
/// The pin that makes every other test in this file a real control: the
/// assertions there are against [`plan`]'s literals, so this is the one place
/// that says those literals are what the code actually enforces. A cap
/// loosened in `src/peer/**` reds this test by name, with both numbers in the
/// message: which is what a positive control run needs to read.
///
/// These numbers close the three open questions `abuse-resistance.md` ends on
/// ("both", "both", ship the caps as written), so these numbers are settled and
/// this test is a ratchet, not a preference.
#[test]
fn the_caps_are_still_the_numbers_the_plan_shipped() {
    assert_eq!(
        listener::KNOCK_BURST,
        plan::KNOCK_BURST,
        "the knock burst shipped as {} and the code enforces {}",
        plan::KNOCK_BURST,
        listener::KNOCK_BURST
    );
    assert_eq!(
        listener::KNOCK_INTERVAL_MS,
        plan::KNOCK_INTERVAL_MS,
        "the knock interval shipped as {}ms and the code enforces {}ms",
        plan::KNOCK_INTERVAL_MS,
        listener::KNOCK_INTERVAL_MS
    );
    assert_eq!(
        state::MAX_PENDING_KNOCKS,
        plan::MAX_PENDING_KNOCKS,
        "the pending cap shipped as {} and the code enforces {}",
        plan::MAX_PENDING_KNOCKS,
        state::MAX_PENDING_KNOCKS
    );
    assert_eq!(
        listener::MAX_UNAUTHENTICATED_SOCKETS,
        plan::MAX_UNAUTHENTICATED_SOCKETS,
        "the node-wide socket cap shipped as {} and the code enforces {}",
        plan::MAX_UNAUTHENTICATED_SOCKETS,
        listener::MAX_UNAUTHENTICATED_SOCKETS
    );
    assert_eq!(
        listener::MAX_UNAUTHENTICATED_PER_ADDRESS,
        plan::MAX_UNAUTHENTICATED_PER_ADDRESS,
        "the per-address socket cap shipped as {} and the code enforces {}",
        plan::MAX_UNAUTHENTICATED_PER_ADDRESS,
        listener::MAX_UNAUTHENTICATED_PER_ADDRESS
    );
    assert_eq!(
        listener::MESSAGE_1_TIMEOUT,
        plan::MESSAGE_1_TIMEOUT,
        "the message-1 deadline shipped as {:?} and the code enforces {:?}",
        plan::MESSAGE_1_TIMEOUT,
        listener::MESSAGE_1_TIMEOUT
    );
    assert_eq!(
        listener::HANDSHAKE_TIMEOUT,
        plan::HANDSHAKE_TIMEOUT,
        "the handshake deadline shipped as {:?} and the code enforces {:?}",
        plan::HANDSHAKE_TIMEOUT,
        listener::HANDSHAKE_TIMEOUT
    );
    assert_eq!(
        listener::MAX_KNOCK_FRAME_BYTES,
        plan::MAX_KNOCK_FRAME_BYTES,
        "the knock frame bound shipped as {} bytes and the code enforces {}",
        plan::MAX_KNOCK_FRAME_BYTES,
        listener::MAX_KNOCK_FRAME_BYTES
    );
    assert_eq!(
        discovery::MAX_FOUND_ROWS,
        plan::MAX_FOUND_ROWS,
        "the found-row display cap shipped as {} and the code enforces {}",
        plan::MAX_FOUND_ROWS,
        discovery::MAX_FOUND_ROWS
    );
    assert_eq!(
        discovery::MAX_FOUND_PER_ADDRESS,
        plan::MAX_FOUND_PER_ADDRESS,
        "the per-address found cap shipped as {} and the code enforces {}",
        plan::MAX_FOUND_PER_ADDRESS,
        discovery::MAX_FOUND_PER_ADDRESS
    );
    assert_eq!(
        discovery::FOUND_TTL_MS,
        plan::FOUND_TTL_MS,
        "the found-row TTL shipped as {}ms and the code enforces {}ms",
        plan::FOUND_TTL_MS,
        discovery::FOUND_TTL_MS
    );
    assert_eq!(
        state::MUTE_MS,
        plan::MUTE_MS,
        "Ignore shipped as a {}ms mute and the code enforces {}ms",
        plan::MUTE_MS,
        state::MUTE_MS
    );
    assert_eq!(
        listener::REFUSAL_LOG_QUIET_MS,
        plan::REFUSAL_LOG_QUIET_MS,
        "the refusal log's quiet period shipped as {}ms and the code enforces {}ms",
        plan::REFUSAL_LOG_QUIET_MS,
        listener::REFUSAL_LOG_QUIET_MS
    );
    assert_eq!(
        listener::REFUSAL_LOG_ADDRESSES,
        plan::REFUSAL_LOG_ADDRESSES,
        "the refusal log's address bound shipped as {} and the code enforces {}",
        plan::REFUSAL_LOG_ADDRESSES,
        listener::REFUSAL_LOG_ADDRESSES
    );
    assert_eq!(
        pair::PAIRING_WINDOW_SECS,
        plan::PAIRING_WINDOW_SECS,
        "the Accept window shipped as {}s and the code enforces {}s",
        plan::PAIRING_WINDOW_SECS,
        pair::PAIRING_WINDOW_SECS
    );
    // And the listener's own spelling of that window is the same deadline, not
    // a second one: two spellings of one number is how the CLI and the listener
    // come to disagree about whether a window is open.
    assert_eq!(
        listener::ACCEPTED_WINDOW_SECS,
        pair::PAIRING_WINDOW_SECS,
        "the listener and the pairing module must name one deadline"
    );
}

// ---------------------------------------------------------------------------
// Scratch files and one Mac, the same shape `tests/peer_pairing.rs` uses
// ---------------------------------------------------------------------------

/// A scratch directory named after this process, thread and a caller tag, so
/// two tests in this binary (and several lanes running at once), never collide
/// on one path.
fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tcr-peer-abuse-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the scratch dir");
    dir
}

/// One Mac's files: a peers file, a state file beside it, and a node key in the
/// same directory.
struct Node {
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
        Self { peers, state, key }
    }

    fn file(&self) -> PeerFile {
        config::read_or_default(&self.peers).expect("the peers file reads")
    }

    fn write_file(&self, file: &PeerFile) {
        config::save(&self.peers, file).expect("the peers file writes");
    }

    /// The state file as the LISTENER reads it: `load` runs `expire(now_ms)`
    /// on what it read, so a row's survival is a question about the clock the
    /// reader passes. Reading at zero: as `tests/peer_pairing.rs` does, where
    /// no test asks about an accepted window, expires every window an
    /// operator just opened, because `now` is then before the instant they
    /// opened it. Every read here passes the real clock for that reason.
    fn state(&self) -> PeerState {
        state::load(&self.state, pair::now_ms()).expect("the state file reads")
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
/// `serve_on_with` and not a hand-rolled loop: every cap under attack here is
/// enforced inside it, so a harness that re-implemented the loop would be
/// attacking the harness.
async fn serve(context: SessionContext) -> SocketAddr {
    let listener = listener::bind("127.0.0.1:0".parse().expect("a loopback address"))
        .await
        .expect("bind a kernel-chosen port on loopback");
    let addr = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        // The loop returns only on an accept failure, which is the harness
        // going away at the end of the test.
        let _ = listener::serve_on_with(listener, context).await;
    });
    addr
}

/// The address every connection in this file comes from, in
/// [`state::knock_address`]'s form (the IP, with the ephemeral source port
/// dropped).
const LOOPBACK: &str = "127.0.0.1";

fn instance(byte: u8) -> InstanceId {
    InstanceId([byte; INSTANCE_ID_BYTES])
}

/// A fresh random-looking instance id, so "instance-id churn" is really churn
/// and not a counter a coalescer could special-case. Derived from a hasher
/// rather than a CSPRNG: this is a label, and a test needs it unpredictable in
/// the sense of "not the same twice", nothing more.
fn churned_instance() -> InstanceId {
    use std::hash::{BuildHasher as _, Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    std::time::Instant::now().hash(&mut hasher);
    std::thread::current().id().hash(&mut hasher);
    let bits = hasher.finish().to_be_bytes();
    let mut id = [0_u8; INSTANCE_ID_BYTES];
    id.copy_from_slice(&bits[..INSTANCE_ID_BYTES]);
    // All-zero is the reservation placeholder's own id
    // (`state::Knock::is_reservation_placeholder`); a churned id that collided
    // with it would make a real row look like a placeholder.
    if id == [0_u8; INSTANCE_ID_BYTES] {
        id[0] = 1;
    }
    InstanceId(id)
}

/// Send one knock over a fresh connection and report whether the far side
/// answered with the ack byte.
async fn knock_at(addr: SocketAddr, id: InstanceId, name: Option<&str>) -> anyhow::Result<()> {
    let mut stream = TcpStream::connect(addr).await?;
    noise::send_knock(
        &mut stream,
        &Knock {
            instance_id: id,
            proposed_name: name.map(str::to_string),
            wire_version: PROTO_VERSION,
        },
        None,
    )
    .await
}

/// Send one knock built by the caller, so a test can size the knock FRAME to
/// the byte.
async fn knock_frame_at(addr: SocketAddr, knock: &Knock) -> anyhow::Result<()> {
    let mut stream = TcpStream::connect(addr).await?;
    noise::send_knock(&mut stream, knock, None).await
}

/// Approve one pending knock the way `tcr peer accept` does, and report the
/// window it opened.
fn accept_pending(node: &Node, selector: &str) -> state::AcceptedInstance {
    let mut value = node.state();
    let window = value
        .accept_knock(selector, pair::now_ms(), pair::PAIRING_WINDOW_SECS)
        .expect("a pending row to accept");
    node.write_state(&value);
    window
}

/// A socket that counts what was WRITTEN BACK to it, so "zero bytes in answer"
/// is asserted on the wire and not on an error type.
///
/// A listener that answered first and refused afterwards satisfies every
/// assertion about its `Err` and fails this one.
async fn count_written(addr: SocketAddr, message_1: &[u8]) -> usize {
    let Ok(mut stream) = TcpStream::connect(addr).await else {
        // A refused connect wrote nothing either, which is the number this
        // function reports on.
        return 0;
    };
    let mut framed = Vec::with_capacity(2 + message_1.len());
    framed.extend_from_slice(
        &u16::try_from(message_1.len())
            .expect("a short frame")
            .to_be_bytes(),
    );
    framed.extend_from_slice(message_1);
    if stream.write_all(&framed).await.is_err() {
        return 0;
    }
    let _ = stream.flush().await;

    let mut back = Vec::new();
    let read = tokio::time::timeout(
        plan::HANDSHAKE_TIMEOUT + Duration::from_secs(2),
        stream.read_to_end(&mut back),
    )
    .await;
    match read {
        Ok(Ok(_)) => back.len(),
        // A reset or a timeout is not an answer either, and the number that
        // matters is how many bytes came back before it.
        _ => back.len(),
    }
}

/// The bytes of a knock's `NN` message 1, built with a throwaway ephemeral
/// secret exactly as `noise::send_knock` builds its own. No payload: a knock's
/// payload rides the transport session message 2 establishes.
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
/// payload, built with a throwaway static key: an impostor's key, which is the
/// point: nothing about the key is what the accepted window checks.
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

/// Open a connection, deliver `message_1`, and then hold the socket open
/// saying nothing: the "hold a slot without authenticating" attack. The
/// returned stream must be kept alive by the caller; dropping it releases
/// everything under attack.
async fn stall_after(addr: SocketAddr, message_1: &[u8]) -> TcpStream {
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
    stream
}

/// Read one number off the counters the accept loop itself decides on.
fn with_admission<T>(context: &SessionContext, read: impl FnOnce(&mut Admission) -> T) -> T {
    let mut guard = match context.admission().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    read(&mut guard)
}

// ===========================================================================
// Cap: 1 knock / 10 s per address, burst 3 (`listener::KNOCK_INTERVAL_MS`,
// `listener::KNOCK_BURST`)
// ===========================================================================

/// **Thirty-two callers released together take three tokens, not thirty-two.**
///
/// The attack a sequential loop cannot run. `the_fourth_knock_in_ten_seconds_\
/// gets_nothing` (`tests/peer_pairing.rs`) spends its tokens one at a time, and
/// a token bucket refuses the fourth call whatever else is true: so it proves
/// the bucket is wired and proves nothing about a race. This releases
/// thirty-two threads from a barrier onto the SAME `Admission` at the SAME
/// `now_ms`, which is the shape that wins against a read-then-decrement that
/// is not under one lock: every caller reads three tokens left and every caller
/// spends one.
///
/// One `now_ms` for all thirty-two on purpose: with a shared clock the bucket
/// cannot refill mid-race, so the number of successes is a property of the
/// mutual exclusion alone and not of how long the threads took.
#[test]
fn a_parallel_burst_from_one_address_gets_three_tokens_not_thirty_two() {
    let admission = Arc::new(std::sync::Mutex::new(Admission::new()));
    let attackers = 32_usize;
    let barrier = Arc::new(std::sync::Barrier::new(attackers));
    let now = 1_700_000_000_000_i64;

    let taken = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..attackers {
            let admission = Arc::clone(&admission);
            let barrier = Arc::clone(&barrier);
            let taken = &taken;
            scope.spawn(move || {
                // The barrier is what makes this parallel rather than a loop
                // with extra steps: no thread proceeds until all thirty-two
                // are here.
                barrier.wait();
                let mut guard = match admission.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                if guard.take_knock_token(LOOPBACK, now) {
                    taken.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            });
        }
    });

    assert_eq!(
        taken.load(std::sync::atomic::Ordering::SeqCst),
        usize::try_from(plan::KNOCK_BURST).expect("the burst fits a usize"),
        "{attackers} callers released together took {} tokens; the burst is {} and a bucket \
         that hands out more than its burst under a race is not a rate limit",
        taken.load(std::sync::atomic::Ordering::SeqCst),
        plan::KNOCK_BURST
    );

    let left = with_admission_arc(&admission, |guard| guard.knock_tokens(LOOPBACK, now));
    assert_eq!(
        left, 0,
        "the bucket must be empty after the burst, and it reports {left} tokens"
    );
}

/// [`with_admission`] for a bare `Arc<Mutex<Admission>>` rather than one inside
/// a [`SessionContext`].
fn with_admission_arc<T>(
    admission: &Arc<std::sync::Mutex<Admission>>,
    read: impl FnOnce(&mut Admission) -> T,
) -> T {
    let mut guard = match admission.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    read(&mut guard)
}

/// **A burst timed to land exactly on the refill instant earns one token, not a
/// fresh bucket**: and re-asking inside the interval earns nothing, however
/// many times it asks.
///
/// Two ways a refill computed from elapsed time goes wrong, both attacked here.
/// It can credit a whole bucket at the boundary instead of one token (then a
/// flooder gets `KNOCK_BURST` every interval instead of one). Or it can credit
/// the same interval twice by moving `last_ms` to `now` rather than forward by
/// the intervals it actually spent: which would let a caller who asks a
/// thousand times inside one interval collect a token on some of them.
#[test]
fn a_burst_on_the_exact_refill_boundary_earns_one_token_not_a_full_bucket() {
    let mut admission = Admission::new();
    let start = 1_700_000_000_000_i64;
    for spend in 0..plan::KNOCK_BURST {
        assert!(
            admission.take_knock_token(LOOPBACK, start),
            "token {spend} of the burst must be available at the same instant as the rest"
        );
    }
    assert!(
        !admission.take_knock_token(LOOPBACK, start),
        "the burst is {} and the next one at the same instant must be refused",
        plan::KNOCK_BURST
    );

    // One millisecond before the refill: still nothing.
    let just_before = start + plan::KNOCK_INTERVAL_MS - 1;
    assert!(
        !admission.take_knock_token(LOOPBACK, just_before),
        "a knock one millisecond before the refill instant must still be refused"
    );

    // Exactly on it: one token, and only one.
    let boundary = start + plan::KNOCK_INTERVAL_MS;
    assert!(
        admission.take_knock_token(LOOPBACK, boundary),
        "the refill instant must credit a token"
    );
    assert!(
        !admission.take_knock_token(LOOPBACK, boundary),
        "the refill instant credits ONE token; a full bucket every interval is the cap \
         multiplied by {}",
        plan::KNOCK_BURST
    );

    // And asking a thousand times inside the next interval collects nothing:
    // the refill is credited from `last_ms` moved FORWARD by whole intervals,
    // so repeated asks cannot each earn the same elapsed time.
    for step in 1..1_000 {
        assert!(
            !admission.take_knock_token(LOOPBACK, boundary + step % plan::KNOCK_INTERVAL_MS),
            "asking repeatedly inside one interval must not re-earn the same elapsed time"
        );
    }

    // Three whole intervals later the bucket is full and NOT overfull: a
    // saturating refill that forgot to clamp would hand out the backlog.
    let long_quiet = boundary + plan::KNOCK_INTERVAL_MS * 10;
    assert_eq!(
        admission.knock_tokens(LOOPBACK, long_quiet),
        plan::KNOCK_BURST,
        "ten quiet intervals must refill to the burst and no further"
    );
}

/// **Thirty knocks over real sockets earn at most three acks**, and leave
/// exactly one row for the operator.
///
/// The end-to-end half of the bucket attack: the pure-counter test above proves
/// the mutual exclusion, and this proves the accept loop actually consults the
/// bucket before it writes a byte.
///
/// # Why this fires in waves of two and not thirty at once, which is a defect
/// # the positive control caught
///
/// Thirty simultaneous `connect`s from one address do not measure the knock
/// bucket at all: with nothing granted, `unauthenticated_allowance` is
/// `(16, 2)`, so the twenty-eighth of them is refused by
/// `MAX_UNAUTHENTICATED_PER_ADDRESS`: a different cap, checked earlier, whose
/// refusal is deliberately indistinguishable on the wire. Measured: with
/// `take_knock_token` replaced by a function that always admits, the
/// all-at-once version of this test still saw at most three acks and stayed
/// GREEN. It was reporting on the socket cap and calling it the bucket.
///
/// So the knocks go out in waves of [`plan::MAX_UNAUTHENTICATED_PER_ADDRESS`],
/// which is the most that can be in flight from one address without the earlier
/// cap having an opinion: each wave still released together from its own
/// barrier, so the concurrency is real, and the whole run stays inside one
/// refill interval, which is asserted, so no ack can be explained by the bucket
/// refilling mid-run. With the bucket removed this now reds with thirty acks.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn thirty_knocks_in_waves_of_two_earn_at_most_three_acks() {
    let node = Node::new("bucket-wire");
    let addr = serve(node.context()).await;

    let wave = plan::MAX_UNAUTHENTICATED_PER_ADDRESS;
    let waves = 15_usize;
    let attackers = wave * waves;
    let started = std::time::Instant::now();
    let mut acked = 0_usize;
    for _ in 0..waves {
        let barrier = Arc::new(tokio::sync::Barrier::new(wave));
        let mut tasks = Vec::with_capacity(wave);
        for _ in 0..wave {
            let barrier = Arc::clone(&barrier);
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                knock_at(addr, churned_instance(), None).await.is_ok()
            }));
        }
        for task in tasks {
            if task.await.expect("the knocking task does not panic") {
                acked += 1;
            }
        }
    }
    let elapsed = started.elapsed();

    assert!(
        elapsed
            < Duration::from_millis(
                u64::try_from(plan::KNOCK_INTERVAL_MS).expect("the interval fits a u64")
            ),
        "the whole burst has to finish inside one refill interval or an ack could be a \
         refill rather than a cap failure; it took {elapsed:?}"
    );
    assert!(
        acked <= usize::try_from(plan::KNOCK_BURST).expect("the burst fits a usize"),
        "{attackers} knocks in waves of {wave} earned {acked} acks and the burst is {}; \
         none of them could have been refused by the per-address socket cap, so every \
         refusal above three was the bucket's",
        plan::KNOCK_BURST
    );
    assert!(
        acked >= 1,
        "positive control: at least one knock must get through, or this test would pass \
         against a listener that refuses everything"
    );

    // And what the operator sees is one row, not thirty.
    let visible = node.state().visible_pending();
    assert_eq!(
        visible.len(),
        1,
        "thirty knocks from one address must coalesce to one row; the operator sees {visible:?}"
    );
}

// ===========================================================================
// Cap: 8 pending knocks, coalesced by address (`state::MAX_PENDING_KNOCKS`)
// ===========================================================================

/// **Twenty knocks with a fresh random instance id each leave ONE row.**
///
/// The id changer, run in parallel and with real churn.
/// `a_rotating_instance_id_from_one_address_is_one_row`
/// (`tests/peer_pairing.rs`) rotates three FIXED ids one at a time; this fires
/// twenty at once with an unpredictable id each, which is the shape that wins
/// against coalescing keyed on the id (twenty rows), against a queue cap
/// checked outside the write lock (nine rows), and against a reservation that
/// is never released (eight placeholder rows and a queue that is full forever).
///
/// Both numbers are asserted, because they are two different failures: the
/// queue must never exceed [`state::MAX_PENDING_KNOCKS`] rows at all, and what
/// the operator is SHOWN must be one row.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn twenty_parallel_knocks_with_a_churned_instance_id_leave_one_row() {
    let node = Node::new("id-churn");
    let addr = serve(node.context()).await;

    let attackers = 20_usize;
    let barrier = Arc::new(tokio::sync::Barrier::new(attackers));
    let mut tasks = Vec::with_capacity(attackers);
    for _ in 0..attackers {
        let barrier = Arc::clone(&barrier);
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            knock_at(addr, churned_instance(), None).await.is_ok()
        }));
    }
    let mut acked = 0_usize;
    for task in tasks {
        if task.await.expect("the knocking task does not panic") {
            acked += 1;
        }
    }
    assert!(
        acked >= 1,
        "positive control: the flood must not be refused in its entirety, or the row count \
         below proves nothing"
    );

    let after = node.state();
    assert!(
        after.pending.len() <= plan::MAX_PENDING_KNOCKS,
        "the queue holds {} rows and the cap is {}; a rotating instance id must not buy a \
         row each",
        after.pending.len(),
        plan::MAX_PENDING_KNOCKS
    );
    let visible = after.visible_pending();
    assert_eq!(
        visible.len(),
        1,
        "one address is one row however many ids it churns through; the operator sees \
         {visible:?}"
    );
    assert_eq!(
        visible[0].addr, LOOPBACK,
        "the row must be keyed on the address, which is the field a completed TCP handshake \
         makes real"
    );
}

/// **A knock that renames itself every time takes no second row, and a knock
/// that borrows a pinned Mac's name changes nothing about the pin.**
///
/// Two attacks on the same surface. Name churn tries for a row per name, which
/// would be a queue cap of "eight names" rather than eight machines. Name
/// theft claims a label an operator already trusts: `abuse-resistance.md`'s
/// impersonation row: "a name is a label, never identity". So the assertion is
/// not that the name is refused (it is shown, beside the address, which is what
/// makes the row honest) but that the PEERS FILE is byte-for-byte what it was:
/// no row added, no key moved, no address learned.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_knock_that_renames_itself_never_takes_a_second_row_or_a_pinned_macs_pin() {
    let node = Node::new("name-churn");

    // One already-pinned Mac, with a label the attacker will try to wear.
    let pinned = PeerId([7_u8; 32]);
    let mut file = node.file();
    file.peers.push(PeerRow {
        node: pinned,
        label: "studio-mac".to_string(),
        endpoints: vec![Endpoint::direct(
            "192.0.2.10:9600"
                .parse()
                .expect("a fixture address is a socket address"),
            0,
            EndpointSource::Paired,
        )],
        added_at: 1_700_000_000_000,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: Allow::default(),
        lend: Vec::new(),
    });
    node.write_file(&file);
    let before = std::fs::read(&node.peers).expect("read the peers file before the attack");

    let addr = serve(node.context()).await;

    // Three knocks, which is the whole burst: a different name each time, and
    // the last one is the pinned Mac's own label.
    let names = ["laptop-2", "attic-nuc", "studio-mac"];
    let mut acked = 0_usize;
    for name in names {
        if knock_at(addr, churned_instance(), Some(name)).await.is_ok() {
            acked += 1;
        }
    }
    assert!(
        acked >= 1,
        "positive control: at least one of the three knocks must be taken, or the row \
         assertions below are vacuous"
    );

    let visible = node.state().visible_pending();
    assert_eq!(
        visible.len(),
        1,
        "a new name per knock must not buy a new row: {visible:?}"
    );

    // The row is a claim, shown beside the address. The pin is untouched.
    let after = std::fs::read(&node.peers).expect("read the peers file after the attack");
    assert_eq!(
        before, after,
        "a knock proposing a pinned Mac's name must not touch the peers file at all"
    );
    let file = node.file();
    assert_eq!(file.peers.len(), 1, "no row was added");
    assert_eq!(
        file.peers[0].node, pinned,
        "the pinned key is still the one the operator trusted; a name is a label, never \
         identity"
    );
}

// ===========================================================================
// Cap: 2 unauthenticated sockets per address, 5 s to deliver message 1,
// 10 s handshake (`listener::MAX_UNAUTHENTICATED_PER_ADDRESS`,
// `listener::MESSAGE_1_TIMEOUT`, `listener::HANDSHAKE_TIMEOUT`)
// ===========================================================================

/// **Sockets opened and held silent take two slots and no more, and every slot
/// comes back after the message-1 deadline.**
///
/// `the_third_unauthenticated_socket_from_one_address_is_refused`
/// (`tests/peer_pairing.rs`) proves the third is refused. The attack this adds
/// is the one that makes a cap into a lockout: hold the sockets past the
/// deadline and see whether the counter is ever given back. A slot leaked on
/// the timeout path is a permanent refusal of every future connection from
/// that address, which is a denial of service built out of the defence against
/// one: and it is invisible to any test that closes its sockets politely.
///
/// Ten attackers against a cap of two, all connected before anything is read,
/// so the assertion is about the counter and not about scheduling.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn ten_silent_sockets_hold_two_slots_and_give_them_all_back() {
    let node = Node::new("silent-sockets");
    let context = node.context();
    let addr = serve(context.clone()).await;

    let mut held = Vec::new();
    for _ in 0..10 {
        // Connect and say NOTHING: the cheapest thing a stranger can do.
        if let Ok(stream) = TcpStream::connect(addr).await {
            held.push(stream);
        }
    }
    // The accept loop needs a moment to pick every connection up; the slot is
    // taken inside `serve_connection`, not by `accept`.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let (live, total) = with_admission(&context, |guard| {
        (
            guard.live_unauthenticated(LOOPBACK),
            guard.live_unauthenticated_total(),
        )
    });
    assert!(
        live <= plan::MAX_UNAUTHENTICATED_PER_ADDRESS,
        "ten silent sockets from one address hold {live} slots and the per-address allowance \
         is {}",
        plan::MAX_UNAUTHENTICATED_PER_ADDRESS
    );
    assert!(
        live >= 1,
        "positive control: a silent socket must hold a slot, or the cap above is satisfied \
         by a counter that never counts"
    );
    assert!(
        total <= plan::MAX_UNAUTHENTICATED_SOCKETS,
        "the node-wide allowance is {} and {total} sockets are counted",
        plan::MAX_UNAUTHENTICATED_SOCKETS
    );

    // Past the message-1 deadline: every slot must come back, even though the
    // attacker never closed a socket and never said a word.
    tokio::time::sleep(plan::MESSAGE_1_TIMEOUT + Duration::from_secs(2)).await;
    let after = with_admission(&context, |guard| guard.live_unauthenticated(LOOPBACK));
    assert_eq!(
        after, 0,
        "every slot must be released when the message-1 deadline closes the socket; {after} \
         are still held, which is a lockout an attacker gets for free"
    );
    // The sockets are dropped only here, so nothing above was measured against
    // a connection the kernel had already torn down.
    drop(held);

    // And the listener still works for a real knock afterwards.
    knock_at(addr, instance(9), None)
        .await
        .expect("a knock after the flood must still be taken");
    assert_eq!(
        node.state().visible_pending().len(),
        1,
        "the queue must still accept a real knock after ten stalled sockets"
    );
}

/// **A socket that delivers a VALID message 1 and then goes silent holds
/// neither a slot nor a queue row.**
///
/// The subtler version of the attack above, and the one the caps are shaped
/// around: a stalled knock holds its socket slot AND a RESERVED PENDING ROW,
/// one eighth of the operator's pairing queue, and this test is about the
/// second one (the slot is
/// `three_knocks_in_flight_from_one_address_hold_two_slots_and_the_third_gets_\
/// nothing`, below). Three stalls is the whole knock
/// burst, so if a reservation outlived its connection this node would be
/// refusing real knocks with nothing an operator could see, and
/// `state::visible_pending` hides the placeholder from them.
///
/// `a_knock_whose_message_1_does_not_validate_reserves_nothing`
/// (`tests/peer_pairing.rs`) covers the INVALID message 1. This is the valid
/// one, which is the case that gets a reservation in the first place.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_valid_message_one_then_silence_holds_no_slot_and_no_queue_row() {
    let node = Node::new("stalled-reservation");
    let context = node.context();
    let addr = serve(context.clone()).await;

    let mut held = Vec::new();
    for _ in 0..3 {
        held.push(stall_after(addr, &knock_message_1()).await);
    }
    tokio::time::sleep(Duration::from_millis(400)).await;

    // What the stall holds: the third connection is refused at the socket cap
    // (the per-address allowance is two), so this reads the reservations the
    // two admitted ones took.
    let during = node.state();
    assert!(
        during.pending.len() <= plan::MAX_PENDING_KNOCKS,
        "three stalled knocks reserved {} rows, and the cap is {}",
        during.pending.len(),
        plan::MAX_PENDING_KNOCKS
    );
    assert!(
        during.visible_pending().is_empty(),
        "a reservation is a placeholder with no instance id and no name; showing the \
         operator a pairing request from nobody is the defect `visible_pending` exists for, \
         and they are shown {:?}",
        during.visible_pending()
    );

    // Past the deadline the reservation is released, so a stalled connection
    // cannot hold a slice of the queue for as long as it keeps a TCP open.
    tokio::time::sleep(plan::MESSAGE_1_TIMEOUT + Duration::from_secs(2)).await;
    let after = node.state();
    assert!(
        after.pending.is_empty(),
        "every reservation must be released once the handshake misses its deadline; {:?} \
         remain, and each one is an eighth of the pairing queue held by a socket that never \
         said who it was",
        after.pending
    );
    assert_eq!(
        with_admission(&context, |guard| guard.live_unauthenticated(LOOPBACK)),
        0,
        "and no socket slot is still held either"
    );
    drop(held);
}

/// **An `XX` message 1 inside an open window, then silence, is closed by the
/// handshake deadline** and leaves the window usable.
///
/// The one path with the longer deadline: `HANDSHAKE_TIMEOUT` rather than
/// `MESSAGE_1_TIMEOUT`, because this connection HAS delivered message 1 and is
/// mid-handshake. An attacker who could hold that state open would hold the
/// operator's 120-second window hostage. Ten seconds is asserted as an upper
/// bound on the wire: the socket has to be closed, not merely refused
/// eventually.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_accepted_handshake_that_stalls_is_closed_by_the_handshake_deadline() {
    let node = Node::new("stalled-xx");
    let addr = serve(node.context()).await;

    let id = instance(4);
    knock_at(addr, id, Some("laptop-2"))
        .await
        .expect("the knock is taken");
    let window = accept_pending(&node, LOOPBACK);
    assert_eq!(window.instance_id, id);

    // Message 1 goes in, and then nothing. The far side answers message 2 :
    // this id IS accepted: and then waits for message 3 that never comes.
    let started = std::time::Instant::now();
    let mut stalled = stall_after(addr, &xx_message_1(id)).await;
    let mut back = Vec::new();
    let read = tokio::time::timeout(
        plan::HANDSHAKE_TIMEOUT + Duration::from_secs(5),
        stalled.read_to_end(&mut back),
    )
    .await;
    assert!(
        read.is_ok(),
        "the listener must close a stalled handshake on its own; after {:?} the socket was \
         still open, which is a slot held by saying nothing",
        started.elapsed()
    );
    assert!(
        started.elapsed() >= plan::MESSAGE_1_TIMEOUT,
        "positive control on the clock: an accepted XX must not be closed before the \
         handshake has had its time, and this one closed after {:?}",
        started.elapsed()
    );
    assert!(
        !back.is_empty(),
        "positive control: an ACCEPTED instance really does get message 2, so the closure \
         above is the deadline and not a refusal"
    );

    // The window survives its abandoned handshake: the operator's Accept is not
    // spent by a stranger stalling.
    let state = node.state();
    assert!(
        state.accepted_window(LOOPBACK, &id, pair::now_ms()),
        "a stalled handshake must not consume the operator's window"
    );
}

// ===========================================================================
// Cap: the knock frame bound (`listener::MAX_KNOCK_FRAME_BYTES`)
// ===========================================================================

/// A knock whose serialized plaintext is exactly `target` bytes, so a test can
/// size the encrypted FRAME to the byte: the frame carries the ciphertext,
/// which is the plaintext plus the 16-byte Poly1305 tag.
fn knock_of_serialized_len(id: InstanceId, target: usize) -> Knock {
    let probe = Knock {
        instance_id: id,
        proposed_name: Some(String::new()),
        wire_version: PROTO_VERSION,
    };
    let base = serde_json::to_vec(&probe)
        .expect("a knock serializes")
        .len();
    let name_len = target
        .checked_sub(base)
        .expect("the target must be above the knock's own fixed cost");
    let knock = Knock {
        instance_id: id,
        proposed_name: Some("a".repeat(name_len)),
        wire_version: PROTO_VERSION,
    };
    assert_eq!(
        serde_json::to_vec(&knock)
            .expect("a knock serializes")
            .len(),
        target,
        "the name has to absorb exactly the difference, or this test is not at the bound"
    );
    knock
}

/// The ciphertext overhead one Noise transport message adds: the Poly1305 tag.
const NOISE_TAG_BYTES: usize = 16;

/// **A knock frame at the bound is taken; one byte over gets nothing and
/// leaves no row.**
///
/// `a_first_frame_above_the_bound_is_refused_before_allocation`
/// (`tests/peer_pairing.rs`) bounds MESSAGE 1. This is the second bound, on the
/// knock frame that rides the session message 1 establishes: the frame whose
/// length prefix a stranger who has completed an `NN` handshake gets to choose.
/// Both sides of the boundary, because a bound tested only from above can be
/// satisfied by a listener that refuses everything.
///
/// The over-bound half runs on its own node: an over-bound knock is refused
/// inside `read_knock`, which is AFTER the reservation is taken, so the row it
/// must not leave behind is a row its own failure has to release.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_knock_frame_at_the_bound_is_taken_and_one_byte_over_gets_nothing() {
    // One byte over the bound, alone, so nothing else could have created the
    // row this asserts is absent.
    let over_node = Node::new("frame-over");
    let over_addr = serve(over_node.context()).await;
    let over = knock_of_serialized_len(
        instance(2),
        plan::MAX_KNOCK_FRAME_BYTES - NOISE_TAG_BYTES + 1,
    );
    let refused = knock_frame_at(over_addr, &over).await;
    assert!(
        refused.is_err(),
        "a knock frame of {} bytes is one over MAX_KNOCK_FRAME_BYTES ({}) and must earn \
         nothing",
        plan::MAX_KNOCK_FRAME_BYTES + 1,
        plan::MAX_KNOCK_FRAME_BYTES
    );
    let over_state = over_node.state();
    assert!(
        over_state.pending.is_empty(),
        "an over-bound knock must leave no row at all, not even the reservation its own \
         handshake took: {:?}",
        over_state.pending
    );

    // Exactly at the bound, on a fresh node with a full bucket.
    let at_node = Node::new("frame-at");
    let at_addr = serve(at_node.context()).await;
    let at = knock_of_serialized_len(instance(3), plan::MAX_KNOCK_FRAME_BYTES - NOISE_TAG_BYTES);
    knock_frame_at(at_addr, &at)
        .await
        .expect("a knock frame exactly at the bound must be read, not refused");
    let visible = at_node.state().visible_pending();
    assert_eq!(
        visible.len(),
        1,
        "the at-bound knock must leave exactly one row: {visible:?}"
    );
    assert!(
        visible[0].proposed_name.is_none(),
        "a 400-character name is inside the FRAME bound and outside the label whitelist, so \
         the row keeps the address and drops the name; it kept {:?}",
        visible[0].proposed_name
    );
}

// ===========================================================================
// Cap: the 120-second accepted window, one instance id
// (`listener::ACCEPTED_WINDOW_SECS`, `pair::PAIRING_WINDOW_SECS`)
// ===========================================================================

/// **An Accept admits one instance id even when ten others race it inside the
/// window.**
///
/// `an_accepted_window_admits_one_instance_id_only`
/// (`tests/peer_pairing.rs`) sends one impostor. This sends ten at once, with a
/// churned id each, and brackets them with the accepted id so the zeros cannot
/// be explained by the socket cap having refused everything: the accepted id
/// gets bytes before the flood AND after it.
///
/// Answering an `XX` message 1 hands over this node's static key, so a window
/// that widened under concurrency would be a key harvest with a 120-second
/// window and no operator involvement at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn an_accept_admits_one_instance_id_even_when_ten_others_race_it() {
    let node = Node::new("window-race");
    let addr = serve(node.context()).await;

    let accepted = instance(5);
    knock_at(addr, accepted, Some("laptop-2"))
        .await
        .expect("the knock is taken");
    accept_pending(&node, LOOPBACK);

    // Before: the accepted id gets an answer, which is what makes the zeros
    // below mean something.
    let before = count_written(addr, &xx_message_1(accepted)).await;
    assert!(
        before > 0,
        "positive control: the accepted instance must be answered, and it got {before} bytes"
    );

    let attackers = 10_usize;
    let barrier = Arc::new(tokio::sync::Barrier::new(attackers));
    let mut tasks = Vec::with_capacity(attackers);
    for _ in 0..attackers {
        let barrier = Arc::clone(&barrier);
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            count_written(addr, &xx_message_1(churned_instance())).await
        }));
    }
    for task in tasks {
        let written = task.await.expect("the impostor task does not panic");
        assert_eq!(
            written, 0,
            "an XX message 1 from an id the operator never accepted must get zero bytes \
             inside the window, and this one got {written}"
        );
    }

    // After: still answered, so the ten zeros above were the window's decision
    // and not the socket cap refusing the whole batch.
    let after = count_written(addr, &xx_message_1(accepted)).await;
    assert!(
        after > 0,
        "positive control: the accepted instance must still be answered after the flood, \
         and it got {after} bytes"
    );
}

/// **A new instance id knocking after an Accept never inherits the window.**
///
/// The id changer's best move on this surface: knock, get accepted, then come
/// back under a NEW id from the same address. A knock from an address with an
/// open window updates the PENDING queue, and if the window were keyed on the
/// address alone (or re-keyed by the later knock), that new id would be
/// answered. It is not: the window belongs to the id the operator saw.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fresh_instance_id_knocking_after_an_accept_never_inherits_the_window() {
    let node = Node::new("window-inherit");
    let addr = serve(node.context()).await;

    let accepted = instance(6);
    knock_at(addr, accepted, Some("laptop-2"))
        .await
        .expect("the first knock is taken");
    accept_pending(&node, LOOPBACK);

    // A second knock from the same address under a different id.
    let usurper = instance(7);
    knock_at(addr, usurper, Some("laptop-2"))
        .await
        .expect("the second knock is taken too; it is a new pending row, nothing more");

    let written = count_written(addr, &xx_message_1(usurper)).await;
    assert_eq!(
        written, 0,
        "the id the operator accepted is the only one the window admits; the usurper got \
         {written} bytes"
    );
    let still = count_written(addr, &xx_message_1(accepted)).await;
    assert!(
        still > 0,
        "positive control: the accepted id must still be answered, and it got {still} bytes"
    );
    assert!(
        node.state()
            .accepted_window(LOOPBACK, &accepted, pair::now_ms()),
        "and the window is still keyed to the id the operator saw"
    );
}

// ===========================================================================
// Cap: one-hour mute on Ignore (`state::MUTE_MS`)
// ===========================================================================

/// **Twenty knocks from a muted address get nothing, and the mute is not one
/// millisecond shorter for it.**
///
/// `a_knock_from_a_muted_address_gets_zero_bytes` (`tests/peer_pairing.rs`)
/// sends one knock. The attacks this adds are the two a flooder would actually
/// try: knock enough times in parallel that one of them slips between another's
/// read and write, and knock in the hope that the re-knock itself refreshes,
/// resets or consumes the mute: a mute an attacker can clear by attacking is
/// not a mute.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn twenty_parallel_knocks_from_a_muted_address_get_nothing_and_shorten_nothing() {
    let node = Node::new("muted-flood");
    let addr = serve(node.context()).await;

    // The operator pressed Ignore.
    let muted_at = pair::now_ms();
    let mut value = node.state();
    value.mute(LOOPBACK, muted_at);
    node.write_state(&value);
    let before = node.state().muted;
    assert_eq!(before.len(), 1, "the address is muted to begin with");

    let attackers = 20_usize;
    let barrier = Arc::new(tokio::sync::Barrier::new(attackers));
    let mut tasks = Vec::with_capacity(attackers);
    for _ in 0..attackers {
        let barrier = Arc::clone(&barrier);
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            knock_at(addr, churned_instance(), Some("laptop-2"))
                .await
                .is_ok()
        }));
    }
    for task in tasks {
        let acked = task.await.expect("the knocking task does not panic");
        assert!(
            !acked,
            "a knock from a muted address must earn no ack; one of twenty was acknowledged"
        );
    }

    let after = node.state();
    assert!(
        after.pending.is_empty(),
        "a muted address must reach no row at all, reservation included: {:?}",
        after.pending
    );
    assert_eq!(
        after.muted, before,
        "the mute must be exactly what the operator set: not extended, not shortened, not \
         replaced by the flood: {:?} became {:?}",
        before, after.muted
    );
    assert!(
        after.is_muted(LOOPBACK, muted_at + plan::MUTE_MS - 1),
        "and it still runs to the end of the hour"
    );
}

// ===========================================================================
// Cap: 12 found rows shown, 2 per address, 60 s row TTL
// (`discovery::MAX_FOUND_ROWS`, `discovery::MAX_FOUND_PER_ADDRESS`)
// ===========================================================================

/// One announcement, as a row that has already passed the inbound checks.
fn found_row(instance: u8, addr: &str) -> Discovered {
    Discovered {
        instance_id: InstanceId([instance; INSTANCE_ID_BYTES]),
        name: None,
        addrs: vec![addr.to_string()],
        port: 9600,
    }
}

/// **One host that rotates BOTH its instance id and its announced address takes
/// a held row each, and the twelve-row display cap still holds.**
///
/// The per-address cap is the one an id changer meets, and
/// `one_address_holds_at_most_two_found_rows` (`tests/peer_discovery.rs`)
/// proves it against a rotating id. This attacks the cap's KEY instead: the
/// address it counts by is the one the announcement carries
/// (`Discovered::addrs`), and `Discovered` holds no source address at all, so
/// an announcer that varies what it claims as its address is a new "address"
/// every time. Announcements are UDP multicast and forgeable down to their
/// source (`abuse-resistance.md`'s own first table row), so this costs an
/// attacker nothing.
///
/// What holds is the display cap, which is what the operator sees, and the
/// honest footer count. What does not hold is the per-address bound on rows
/// HELD: this test measures the real cost rather than asserting a cap the type
/// cannot enforce, and the finding is reported with it.
#[test]
fn one_host_rotating_its_announced_address_still_shows_only_twelve_rows() {
    let mut found = FoundList::new();
    let flood: Vec<Discovered> = (0..40_u8)
        .map(|n| found_row(n, &format!("192.0.2.{n}")))
        .collect();
    found.observe(flood, 1_000);

    assert_eq!(
        found.shown().len(),
        plan::MAX_FOUND_ROWS,
        "the display cap is what the operator sees and it must hold whatever arrives: {} \
         rows shown",
        found.shown().len()
    );
    assert_eq!(
        found.not_shown(),
        found.len() - plan::MAX_FOUND_ROWS,
        "the footer count must be honest about what is held back"
    );

    // The measurement behind the finding: forty rotations from one announcer
    // are forty HELD rows, because the per-address cap counts the address the
    // announcement claims and nothing in `Discovered` records where it came
    // from.
    assert_eq!(
        found.len(),
        40,
        "a rotation per row is a row per rotation; this is the cost the per-address cap does \
         not bound"
    );

    // And the cap does still bind when the claimed address repeats, which is
    // the positive control on the assertion above: it is the KEY that is weak,
    // not the counting.
    let mut repeated = FoundList::new();
    let same: Vec<Discovered> = (0..40_u8).map(|n| found_row(n, "192.0.2.7")).collect();
    repeated.observe(same, 1_000);
    assert_eq!(
        repeated.len(),
        plan::MAX_FOUND_PER_ADDRESS,
        "one claimed address holds {} rows and the cap is {}",
        repeated.len(),
        plan::MAX_FOUND_PER_ADDRESS
    );
}

/// **A row cannot be kept alive by a stale re-announcement, and the TTL is not
/// restarted by the scan that drops it.**
///
/// `a_row_that_stops_announcing_leaves_after_sixty_seconds`
/// (`tests/peer_discovery.rs`) covers the ordinary expiry. The attack here is
/// the boundary: a row observed at the exact TTL instant must be gone (the
/// bound is strict), and a flood arriving in the same scan must not refresh
/// rows it is not for.
#[test]
fn a_found_row_dies_exactly_at_its_ttl_and_a_flood_does_not_revive_it() {
    let mut found = FoundList::new();
    found.observe(vec![found_row(1, "192.0.2.7")], 1_000);
    assert_eq!(found.len(), 1);

    // One millisecond inside the TTL: still there.
    found.observe(Vec::new(), 1_000 + plan::FOUND_TTL_MS - 1);
    assert_eq!(
        found.len(),
        1,
        "a row one millisecond inside its TTL must survive"
    );

    // Exactly at the TTL, and the scan that arrives is somebody else's flood:
    // the old row goes, and the flood does not inherit its freshness.
    let flood: Vec<Discovered> = (0..5_u8)
        .map(|n| found_row(100 + n, &format!("198.51.100.{n}")))
        .collect();
    found.observe(flood, 1_000 + plan::FOUND_TTL_MS);
    assert!(
        !found
            .shown()
            .iter()
            .any(|row| row.instance_id == InstanceId([1; INSTANCE_ID_BYTES])),
        "the row is at exactly its TTL and the bound is strict, so it must be gone: {:?}",
        found.shown()
    );
    assert_eq!(
        found.len(),
        5,
        "and the flood's own five rows are all that is left"
    );
}

// ===========================================================================
// Cap: one refusal log line per address per hour
// (`listener::REFUSAL_LOG_QUIET_MS`, `listener::REFUSAL_LOG_ADDRESSES`)
// ===========================================================================

/// **One address's quiet hour can be beaten, and what it costs is a full
/// thousand fresh addresses.**
///
/// `an_unauthenticated_flood_writes_one_log_line_and_then_counts` and
/// `the_refusal_log_is_bounded_in_addresses` (`tests/peer_noise.rs`) cover the
/// time bound and the map bound separately. This attacks the two TOGETHER,
/// which is where they interact: the map is capped at
/// [`listener::REFUSAL_LOG_ADDRESSES`] and evicts the address whose last line
/// is OLDEST, so a flooder that keeps refusing from address A while filling the
/// map with fresh addresses gets A's entry evicted: and A's next refusal is a
/// brand-new address as far as the log is concerned, so it writes a second line
/// inside the same hour.
///
/// The eviction is deliberate and documented (`listener::RefusalLog::admit`):
/// the alternative is a map a stranger can grow without bound, which is the
/// worse failure. So this test asserts the honest bound rather than the cap's
/// literal words: the amplification an attacker gets is one extra line per
/// [`listener::REFUSAL_LOG_ADDRESSES`] distinct source addresses, and every one
/// of those addresses wrote its own line anyway. Both halves are asserted: that
/// half a mapful is NOT enough (so the cost really is the map size), and that a
/// full mapful is.
#[test]
fn beating_one_addresss_quiet_hour_costs_a_thousand_fresh_addresses() {
    let flooder = "192.0.2.7";

    // Half a mapful changes nothing: the quiet hour holds.
    let mut log = listener::RefusalLog::new();
    assert_eq!(
        log.admit(flooder, 0),
        Some(0),
        "the first refusal is always logged"
    );
    for n in 0..(plan::REFUSAL_LOG_ADDRESSES / 2) {
        let churn = format!("198.51.100.{}.{}", n / 256, n % 256);
        log.admit(&churn, 1);
    }
    assert_eq!(
        log.admit(flooder, 2),
        None,
        "half a mapful of fresh addresses must not evict the flooder's entry, or the cost \
         asserted below is not the real one"
    );

    // A full mapful does evict it, and the flooder writes a second line two
    // milliseconds into its own quiet hour.
    let mut log = listener::RefusalLog::new();
    assert_eq!(log.admit(flooder, 0), Some(0));
    for n in 0..plan::REFUSAL_LOG_ADDRESSES {
        let churn = format!("198.51.100.{}.{}", n / 256, n % 256);
        log.admit(&churn, 1);
    }
    assert!(
        log.admit(flooder, 2).is_some(),
        "measured, and reported as a finding: filling the address map evicts the flooder's \
         own entry and its next refusal reads as a first one. The cost is \
         {} distinct source addresses per extra line, each of which wrote a line of its own",
        plan::REFUSAL_LOG_ADDRESSES
    );
    assert!(
        log.len() <= plan::REFUSAL_LOG_ADDRESSES,
        "and the map is still bounded, which is what the eviction is for: {} entries",
        log.len()
    );
}

// ===========================================================================
// Cap: 8 pending knocks across DISTINCT addresses, and the reservation that
// holds one (`state::MAX_PENDING_KNOCKS`, `state::reserve_knock_slot`,
// `state::release_knock_reservation`)
// ===========================================================================

/// **A flood from a whole `/24` costs eight rows, not two hundred and
/// fifty-four.**
///
/// The sentence `state::MAX_PENDING_KNOCKS`'s own doc-comment makes, asserted
/// for the first time. Every other pending-queue test in this file: and in
/// `tests/peer_pairing.rs`, knocks from `127.0.0.1`, which is the only address
/// a loopback test can dial from, and `state::knock_address` coalesces all of
/// them onto ONE row. So they measure the coalescing and they never once reach
/// the cap: an assertion that the queue holds `<= 8` rows is satisfied by a
/// queue that holds one, and it would be satisfied just as happily by a cap
/// that had stopped working.
///
/// This spends the cap for real, at the seam the accept loop itself decides on
/// ([`state::PeerState::reserve_knock_slot`], which is what `serve_knock` calls
/// before it writes a byte), with 254 distinct addresses from the `192.0.2.0/24`
/// documentation range.
///
/// # Which earlier cap could be answering instead, and the control
///
/// `refusal_for_knock_source` tries BANNED first, then MUTED, then coalescing,
/// and only then the queue: and all four refusals are deliberately
/// indistinguishable on the wire. A test that counted "how many were refused"
/// would be answered identically by a state file that banned the whole range.
/// So this does not count refusals: it matches the refusal ARM, and every
/// outcome that is not `QueueFull` or a plain admission is collected into
/// `unexpected` and named in the failure. A ban or a mute answering instead
/// reds this test with the arm that fired.
#[test]
fn a_flood_from_a_whole_slash_twenty_four_costs_eight_rows_not_two_hundred_and_fifty_four() {
    let hosts = 254_usize;
    let mut value = PeerState::default();
    let now = pair::now_ms();

    let mut admitted = 0_usize;
    let mut queue_full = 0_usize;
    let mut unexpected = Vec::new();
    for host in 1..=hosts {
        let addr = format!("192.0.2.{host}");
        match value.reserve_knock_slot(&addr, now) {
            Ok(true) => admitted += 1,
            Ok(false) => unexpected.push(format!("{addr} coalesced onto a row it never had")),
            Err(state::KnockRefusal::QueueFull { .. }) => queue_full += 1,
            Err(refusal) => unexpected.push(format!("{addr} was refused {refusal:?}")),
        }
    }

    assert!(
        unexpected.is_empty(),
        "every address in the range is fresh, unbanned and unmuted, so the only two \
         outcomes are an admission and QueueFull; these were neither: {unexpected:?}"
    );
    assert_eq!(
        admitted,
        plan::MAX_PENDING_KNOCKS,
        "a /24 flood must buy {} rows and it bought {admitted}",
        plan::MAX_PENDING_KNOCKS
    );
    assert_eq!(
        queue_full,
        hosts - plan::MAX_PENDING_KNOCKS,
        "and the other {} addresses must each be refused QueueFull; {queue_full} were",
        hosts - plan::MAX_PENDING_KNOCKS
    );
    assert_eq!(
        value.pending.len(),
        plan::MAX_PENDING_KNOCKS,
        "the queue itself holds {} rows and the cap is {}",
        value.pending.len(),
        plan::MAX_PENDING_KNOCKS
    );
}

/// **The ninth address is refused `QueueFull` and says eight are outstanding :
/// and a full queue still lets the eight machines already in it re-knock.**
///
/// The boundary, to the row, plus the lockout the flood above would otherwise
/// buy for free. A queue cap that refused EVERY knock once it was full would
/// pass the flood test and would hand an attacker the real prize: fill the
/// eight slots from a `/24`, and the operator's own Mac, which already holds a
/// row: can never refresh it. `refusal_for_knock_source` returns `None` for an
/// address already pending before it ever looks at the length, and that
/// ordering is what this pins.
///
/// # Which earlier cap could be answering instead, and the control
///
/// Same three arms as above, and the same control: the refusal is matched as
/// `QueueFull { pending: 8 }` (both the arm and its payload), so a ban or a
/// mute cannot satisfy it, and a cap that refused at some other depth reds it
/// with the number it actually reported.
#[test]
fn the_ninth_address_is_refused_queue_full_and_the_eight_already_queued_can_still_reknock() {
    let mut value = PeerState::default();
    let now = pair::now_ms();

    for host in 1..=plan::MAX_PENDING_KNOCKS {
        let addr = format!("192.0.2.{host}");
        assert_eq!(
            value.reserve_knock_slot(&addr, now),
            Ok(true),
            "{addr} is within the first {} and must take a row",
            plan::MAX_PENDING_KNOCKS
        );
    }

    let ninth = format!("192.0.2.{}", plan::MAX_PENDING_KNOCKS + 1);
    assert_eq!(
        value.reserve_knock_slot(&ninth, now),
        Err(state::KnockRefusal::QueueFull {
            pending: plan::MAX_PENDING_KNOCKS
        }),
        "the ninth address must be refused QueueFull with {} outstanding",
        plan::MAX_PENDING_KNOCKS
    );

    // The lockout that would make the flood worth running: an address that
    // already holds a row must not be refused by the cap its own row counts
    // towards.
    assert_eq!(
        value.reserve_knock_slot("192.0.2.1", now + 1),
        Ok(false),
        "a machine already in the queue must coalesce onto its own row, not be locked out \
         by a queue its row is part of"
    );
    assert_eq!(
        value.pending.len(),
        plan::MAX_PENDING_KNOCKS,
        "and the re-knock added nothing: {} rows",
        value.pending.len()
    );
}

/// **Eight stalled reservations, released, give the whole queue back.**
///
/// The denial of service the reservation itself creates. `reserve_knock_slot`
/// pushes a placeholder row BEFORE the handshake, which is what makes the cap
/// atomic: and it means eight connections that open and then say nothing hold
/// the entire pairing queue. If `release_knock_reservation` did not give the
/// row back, eight TCP connections would shut the node's pairing out
/// permanently for every other machine on the LAN, at a cost of eight sockets.
///
/// `a_valid_message_one_then_silence_holds_no_slot_and_no_queue_row` covers the
/// wire path for ONE address; because loopback coalesces, it can only ever hold
/// one row, so it cannot show the queue being emptied. This holds all eight and
/// shows the ninth address getting in afterwards: which is the observable that
/// says the queue is really back, not merely shorter.
///
/// # Which earlier cap could be answering instead, and the control
///
/// The ninth address's admission at the end is the control: if the releases had
/// done nothing, that call returns `Err(QueueFull)` and the test reds. The
/// intermediate `pending.is_empty()` assertion alone would also be satisfied by
/// a `release` that cleared the queue indiscriminately, so the test that
/// follows this one pins the other direction.
#[test]
fn eight_stalled_reservations_released_give_the_whole_queue_back() {
    let mut value = PeerState::default();
    let now = pair::now_ms();

    let mut held = Vec::new();
    for host in 1..=plan::MAX_PENDING_KNOCKS {
        let addr = format!("192.0.2.{host}");
        assert_eq!(
            value.reserve_knock_slot(&addr, now),
            Ok(true),
            "{addr} takes a placeholder row"
        );
        held.push(addr);
    }
    assert!(
        value.visible_pending().is_empty(),
        "eight placeholders are eight rows the operator must not be shown: {:?}",
        value.visible_pending()
    );
    assert_eq!(
        value.reserve_knock_slot("192.0.2.200", now),
        Err(state::KnockRefusal::QueueFull {
            pending: plan::MAX_PENDING_KNOCKS
        }),
        "positive control: with all eight held, a fresh address really is shut out"
    );

    for addr in &held {
        value.release_knock_reservation(addr, now);
    }
    assert!(
        value.pending.is_empty(),
        "every stalled reservation must be released; {:?} remain",
        value.pending
    );
    assert_eq!(
        value.reserve_knock_slot("192.0.2.200", now),
        Ok(true),
        "and the address that was shut out a moment ago now gets in, which is what says the \
         queue is back rather than merely shorter"
    );
}

/// **A stalled connection's release never deletes the row that coalesced onto
/// its placeholder.**
///
/// The attack the release opens, and the nastier half of the pair. An attacker
/// connects from an address, takes the placeholder, and waits. A real machine
/// behind the same NAT (or the attacker's own earlier honest knock), completes
/// its handshake and `record_knock` fills that same row in, because coalescing
/// is keyed on the address. The attacker then drops its socket. If release were
/// keyed on the address alone, the drop would delete the operator's real
/// pairing request, and the attacker could erase every incoming knock from its
/// network for as long as it cared to keep reconnecting.
///
/// `release_knock_reservation` guards on the placeholder being untouched: an
/// all-zero instance id AND `first_seen_ms` still equal to the reserved instant
///: and this is the test that spends both halves of that guard.
///
/// # Which earlier cap could be answering instead, and the control
///
/// Nothing earlier can: no ban, mute or queue-full path removes a row, and the
/// row is confirmed present and VISIBLE (a filled row, not a placeholder)
/// immediately before the release is called, so a green result cannot be
/// explained by the row never having existed.
#[test]
fn a_stalled_connections_release_never_deletes_the_row_that_coalesced_onto_it() {
    let mut value = PeerState::default();
    let reserved_at = pair::now_ms();
    let victim = "192.0.2.42";

    assert_eq!(
        value.reserve_knock_slot(victim, reserved_at),
        Ok(true),
        "the attacker's connection takes the placeholder first"
    );

    // A real handshake from the same address completes and fills the row in.
    let real = instance(3);
    assert_eq!(
        value.record_knock(
            victim,
            real,
            Some("laptop-2".to_string()),
            PROTO_VERSION,
            reserved_at + 5
        ),
        Ok(false),
        "a completed handshake coalesces onto the placeholder rather than taking a ninth row"
    );
    assert_eq!(
        value.visible_pending().len(),
        1,
        "control: the operator really is being shown a pairing request at this point"
    );

    // The attacker drops its socket, and the listener releases what it
    // reserved.
    value.release_knock_reservation(victim, reserved_at);

    let visible = value.visible_pending();
    assert_eq!(
        visible.len(),
        1,
        "the real row must survive the stalled connection's release; the operator is now \
         shown {visible:?}"
    );
    assert_eq!(
        visible[0].instance_id, real,
        "and it is still the row the real handshake wrote"
    );
}

// ===========================================================================
// Cap: the accepted window runs out (`pair::PAIRING_WINDOW_SECS`)
// ===========================================================================

/// **An accepted window is shut at the instant it expires, and shut before it
/// opens.**
///
/// The window's OTHER dimension. `an_accept_admits_one_instance_id_even_when_\
/// ten_others_race_it` and `a_fresh_instance_id_knocking_after_an_accept_never_\
/// inherits_the_window` both attack which id the window admits, and both ask
/// only about the present instant: so a window with no deadline at all passes
/// each of them. This asks how long, to the millisecond, and it asks the
/// backward-clock question the constructor's own doc-comment claims: an
/// attacker who can move this node's clock must not be able to reopen a window
/// by moving it back before the operator opened it.
///
/// Answering an `XX` message 1 hands over this node's static key, so "the
/// window never actually closes" is a permanent key-harvest offer to one
/// address, bought with a single Accept the operator may have forgotten.
///
/// # Which earlier cap could be answering instead, and the control
///
/// The id check in the same predicate: a window that was shut for the wrong
/// reason (wrong id) would read identically to one that had expired. It is
/// ruled out by asserting the window OPEN for the same address and the same id
/// one millisecond before the deadline: only the clock differs between the
/// `true` and the `false`.
#[test]
fn an_accepted_window_is_shut_at_the_instant_it_expires_and_before_it_opens() {
    let mut value = PeerState::default();
    let opened = pair::now_ms();
    let accepted = instance(8);
    let addr = "192.0.2.77";

    value
        .record_knock(addr, accepted, None, PROTO_VERSION, opened)
        .expect("the knock is queued");
    let window = value
        .accept_knock(addr, opened, plan::PAIRING_WINDOW_SECS)
        .expect("a pending row to accept");
    assert_eq!(
        window.until_ms,
        opened + plan::PAIRING_WINDOW_SECS * 1_000,
        "the window the operator opened must run exactly {} seconds",
        plan::PAIRING_WINDOW_SECS
    );

    assert!(
        value.accepted_window(addr, &accepted, window.until_ms - 1),
        "control: one millisecond before the deadline the window is open, so every `false` \
         below differs from this call only by its clock"
    );
    assert!(
        !value.accepted_window(addr, &accepted, window.until_ms),
        "the deadline is exclusive: at {} the window must be shut",
        window.until_ms
    );
    assert!(
        !value.accepted_window(addr, &accepted, window.until_ms + 60_000),
        "and a minute past it, shut"
    );
    assert!(
        !value.accepted_window(addr, &accepted, opened - 1),
        "a clock set back to before the operator accepted must CLOSE the window, not reopen \
         it: an attacker who can move this node's clock must not be able to buy a second \
         window with it"
    );
}

/// **An `XX` inside a window that has run out gets zero bytes back.**
///
/// The end-to-end half: the predicate above is only a defence if the accept
/// loop consults it with a real clock on every connection. The expired window
/// is written straight into the state file rather than waited for, so the test
/// costs no time and still exercises the shipped path.
///
/// # Which earlier cap could be answering instead, and the control
///
/// Every one of them: the knock bucket, the per-address socket cap, the queue
///: refuses with the same zero bytes, and this test's whole assertion is zero
/// bytes. So the control is the second half: the SAME address, the SAME
/// instance id and the SAME `XX` bytes against a window that is still open must
/// come back with bytes, over a connection opened immediately after the first
/// one closed. The two calls differ in nothing but the window's deadline, so a
/// cap that had refused everything reds the control.
///
/// # What this test does NOT do on its own, measured
///
/// **Two independent mechanisms refuse an expired window, so this test reds
/// only when BOTH are broken, and it must not be read as a gate on either
/// one.** [`state::PeerState::expire`] runs inside every [`state::load`] and
/// drops the row before the connection is even looked at; if it did not,
/// [`state::PeerState::accepted_window`]'s `now_ms < until_ms` clause refuses
/// the same `XX` a moment later. Measured, by deleting each in turn and
/// re-running: with the `accepted_window` clause deleted this test stayed
/// GREEN, and with `expire`'s deadline deleted it stayed GREEN. Only the pair
/// reds it.
///
/// That is good layering and a weak assertion, so each mechanism is given its
/// own red elsewhere:
/// [`an_accepted_window_is_shut_at_the_instant_it_expires_and_before_it_opens`]
/// reds on the predicate, and
/// [`reading_the_state_file_drops_a_window_that_has_run_out`] reds on the load.
/// What this test adds over both is the only thing neither can show: that the
/// SHIPPED accept loop consults one of them with a real clock before it writes
/// a byte.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_xx_inside_a_window_that_has_run_out_gets_zero_bytes() {
    let node = Node::new("window-expiry-wire");
    let addr = serve(node.context()).await;
    let accepted = instance(9);
    let now = pair::now_ms();

    let expired = state::AcceptedInstance {
        instance_id: accepted,
        addr: LOOPBACK.to_string(),
        until_ms: now - 1_000,
        opened_at_ms: now - plan::PAIRING_WINDOW_SECS * 1_000 - 1_000,
        learned_key: None,
    };
    let mut value = PeerState::default();
    value.accepted.push(expired);
    node.write_state(&value);

    let written = count_written(addr, &xx_message_1(accepted)).await;
    assert_eq!(
        written, 0,
        "a window that ran out a second ago must answer an XX with nothing; it wrote \
         {written} bytes, and the first of them is this node's static key on its way to a \
         stranger"
    );

    // Control: same address, same id, same bytes, a window that is still open.
    let live = state::AcceptedInstance {
        instance_id: accepted,
        addr: LOOPBACK.to_string(),
        until_ms: now + plan::PAIRING_WINDOW_SECS * 1_000,
        opened_at_ms: now,
        learned_key: None,
    };
    let mut open = PeerState::default();
    open.accepted.push(live);
    node.write_state(&open);

    let still = count_written(addr, &xx_message_1(accepted)).await;
    assert!(
        still > 0,
        "positive control: with the window open the same XX must be answered, or the zero \
         above says nothing about the deadline; it got {still} bytes"
    );
}

/// **Reading the state file drops a window that has run out.**
///
/// The second of the two independent mechanisms that refuse an expired window,
/// given its own red. [`state::PeerState::expire`] runs on every
/// [`state::load`], so a window past its deadline is gone before
/// [`state::PeerState::accepted_window`] is ever consulted: which is why
/// [`an_xx_inside_a_window_that_has_run_out_gets_zero_bytes`] cannot tell the
/// two apart, and why this test exists beside it.
///
/// # Which earlier cap could be answering instead, and the control
///
/// A `load` that returned an empty state for any reason: a parse failure
/// swallowed into a default, a path that never existed, would satisfy "no
/// accepted rows" without expiring anything. The live window written in the
/// second half is the control: the same file, the same writer, the same reader,
/// and it survives. Only the deadline differs.
#[test]
fn reading_the_state_file_drops_a_window_that_has_run_out() {
    let node = Node::new("window-expire-load");
    let accepted = instance(10);
    let now = pair::now_ms();

    let mut value = PeerState::default();
    value.accepted.push(state::AcceptedInstance {
        instance_id: accepted,
        addr: LOOPBACK.to_string(),
        until_ms: now - 1_000,
        opened_at_ms: now - plan::PAIRING_WINDOW_SECS * 1_000 - 1_000,
        learned_key: None,
    });
    node.write_state(&value);
    assert!(
        node.state().accepted.is_empty(),
        "a window a second past its deadline must not survive the read that every connection \
         makes; it came back as {:?}",
        node.state().accepted
    );

    // Control: the same file and the same reader keep a window that is open.
    let mut open = PeerState::default();
    open.accepted.push(state::AcceptedInstance {
        instance_id: accepted,
        addr: LOOPBACK.to_string(),
        until_ms: now + plan::PAIRING_WINDOW_SECS * 1_000,
        opened_at_ms: now,
        learned_key: None,
    });
    node.write_state(&open);
    assert_eq!(
        node.state().accepted.len(),
        1,
        "positive control: an open window must survive the same read, or the assertion above \
         is about the reader and not about the deadline"
    );
}

// ---------------------------------------------------------------------------
// A tailnet is not this LAN
// ---------------------------------------------------------------------------

/// **A node on a tailnet reaches the knock path on no setting, and neither
/// does a link-local address.**
///
/// `listener::is_lan_scope` used to count `100.64.0.0/10` (RFC 6598, which is
/// what Tailscale numbers a tailnet out of) and `169.254.0.0/16` (RFC 3927
/// link-local) as LAN, and `internet_admission` tests the SOURCE before it
/// looks at anything else. So a listener bound `0.0.0.0` with
/// `peer.internet` off answered a knock and a first pairing from any tailnet
/// node on earth, and the switch the operator set could not refuse it: the
/// pairing queue on the operator's screen was reachable from a network they
/// never meant to expose it to.
///
/// Watch it fail: put `a == 100 && (64..128).contains(&b)` back into
/// `is_lan_scope_v4` and the first two rows below return `Answer`.
#[test]
fn a_tailnet_or_link_local_source_never_reaches_the_knock_path() {
    let wide_open: std::net::IpAddr = "0.0.0.0".parse().expect("a literal address parses");

    for source in ["100.64.0.1", "100.127.255.254", "169.254.1.1"] {
        let from: std::net::IpAddr = source.parse().expect("a literal address parses");
        for pattern in [
            noise::Handshake::Knock,
            noise::Handshake::KnockPsk,
            noise::Handshake::Pair,
        ] {
            for internet in [false, true] {
                assert_eq!(
                    listener::internet_admission(wide_open, from, pattern, internet, &[]),
                    listener::InternetAdmission::Refuse,
                    "{source} is not on this LAN, so a {} must get nothing back from a \
                     listener bound to a globally routable address (internet={internet})",
                    pattern.pattern()
                );
            }
        }

        // And the path such a peer really uses is untouched: a return visit
        // against a pinned key is still answered, because the pin is what
        // decides it.
        assert_eq!(
            listener::internet_admission(wide_open, from, noise::Handshake::Return, false, &[]),
            listener::InternetAdmission::Answer,
            "{source} must still reach the pin check with an IK return visit"
        );
    }

    // Positive control: a real private-LAN source is answered exactly as
    // before, or the assertions above are about a gate that refuses everybody.
    let neighbour: std::net::IpAddr = "192.168.1.4".parse().expect("a literal address parses");
    assert_eq!(
        listener::internet_admission(wide_open, neighbour, noise::Handshake::Knock, true, &[]),
        listener::InternetAdmission::Answer,
        "a neighbour on the private LAN must still be able to knock"
    );
}

// ---------------------------------------------------------------------------
// One failed accept is one connection, not the end of the mesh
// ---------------------------------------------------------------------------

/// **The errors a busy machine really produces do not end the accept loop.**
///
/// The loop used to propagate every `accept` error. `ECONNABORTED` (a peer
/// that hung up between the SYN and the accept) and `EMFILE` (this process
/// momentarily out of descriptors, which a burst of leases or anything else in
/// the proxy can cause) are both ordinary events on a busy node, and either
/// one ended the peer listener for the life of the process: `server::supervise`
/// supervises a task that has already returned, so nothing starts it again.
///
/// Watch it fail: make `accept_error_is_fatal` answer `true` for everything
/// and the first three rows below go red.
#[test]
fn a_transient_accept_error_does_not_end_the_listener() {
    use std::io::{Error, ErrorKind};

    // ECONNABORTED (53 on macOS, 103 on Linux) and EMFILE/ENFILE: one
    // connection's problem, or a shortage that clears.
    for transient in [
        Error::from(ErrorKind::ConnectionAborted),
        Error::from_raw_os_error(24), // EMFILE
        Error::from_raw_os_error(23), // ENFILE
        Error::from(ErrorKind::Interrupted),
        Error::from(ErrorKind::WouldBlock),
    ] {
        assert!(
            !listener::accept_error_is_fatal(&transient),
            "{transient:?} is one connection's problem; ending the accept loop over it takes \
             the whole mesh down until the next restart"
        );
    }

    // EBADF and a listener that is not a socket: retrying answers the same way
    // forever, so the loop has to end and say so.
    for fatal in [
        Error::from_raw_os_error(9), // EBADF
        Error::from(ErrorKind::InvalidInput),
        Error::from(ErrorKind::NotConnected),
    ] {
        assert!(
            listener::accept_error_is_fatal(&fatal),
            "{fatal:?} says the listening socket itself is gone; retrying it is a spin"
        );
    }
}

/// **A third knock in flight from one address gets nothing back.**
///
/// The socket slot used to be released the instant message 1 was in hand, for
/// every pattern. That is right for a connection going on to authenticate, and
/// wrong for a knock: a knock never authenticates, and everything after its
/// message 1 is a state-file write under a lock that waits with
/// `std::thread::sleep`. So the knock path had no in-flight bound at all, and
/// one LAN host could hold as many of them open as it could open sockets.
///
/// Watch it fail: move the `drop(slot)` in `serve_connection` back above the
/// knock arm and the third connection is admitted, the counter reads zero
/// while all three are in flight, and this goes red on both assertions.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_knocks_in_flight_from_one_address_hold_two_slots_and_the_third_gets_nothing() {
    let node = Node::new("knock-in-flight");
    let context = node.context();
    let addr = serve(context.clone()).await;

    // Two knocks that deliver a valid message 1 and then say nothing: both
    // are mid-knock, so both hold a slot.
    let mut held = Vec::new();
    for _ in 0..plan::MAX_UNAUTHENTICATED_PER_ADDRESS {
        held.push(stall_after(addr, &knock_message_1()).await);
    }
    tokio::time::sleep(Duration::from_millis(400)).await;

    assert_eq!(
        with_admission(&context, |guard| guard.live_unauthenticated(LOOPBACK)),
        plan::MAX_UNAUTHENTICATED_PER_ADDRESS,
        "a knock in flight has authenticated nothing, so it has to hold its slot for as long \
         as it is in flight"
    );

    // The third one is refused with zero bytes written, which is what a bound
    // means on the wire.
    assert_eq!(
        count_written(addr, &knock_message_1()).await,
        0,
        "the third knock in flight from one address is over the per-address allowance and \
         must get nothing back"
    );

    // And the slots come back: the stalls hit `MESSAGE_1_TIMEOUT` inside the
    // knock handshake, the connections close, and the counter empties.
    drop(held);
    tokio::time::sleep(plan::MESSAGE_1_TIMEOUT + Duration::from_secs(2)).await;
    assert_eq!(
        with_admission(&context, |guard| guard.live_unauthenticated(LOOPBACK)),
        0,
        "every slot is released on every exit path, or the bound becomes a lockout"
    );
}
