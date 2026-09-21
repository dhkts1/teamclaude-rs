//! What a path to a peer costs, and the order that
//! cost puts the endpoints in.
//!
//! # What is real here and what is a stand-in
//!
//! Real: the Noise handshake and session, the production stream header, the
//! production prober (`peer::probe::probe_once` / `probe_session`), the
//! production EWMA, the production comparator, the production dial order
//! (`peer::serve::dial_order`), and, in
//! `the_real_listener_echoes_a_probe_with_both_fields_untouched`, the shipped
//! accept loop `listener::serve_on_with` answering a real probe.
//!
//! Stand-in, named at its call site: the peer at the far end in the timing and
//! old-build tests. It has to be, twice over. A fixed 40 ms delay is the whole
//! measurement and the production listener answers as fast as it can; and an
//! OLDER build is a build that does not exist in this tree, so "a peer that
//! answers Unknown" and "a peer that closes the session" can only be played by
//! a responder written to do exactly that.
//!
//! # House rules this file is built to
//!
//! Every socket binds `127.0.0.1:0` (kernel-chosen), every file is under a
//! temp dir, no account is real, and nothing touches the proxy on
//! `127.0.0.1:3456`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tcr_peer_wire::{Control, PeerId, StreamHeader, StreamKind};
use teamclaude_rs::peer::config::{
    Allow, Endpoint, EndpointSource, Locator, PeerFile, PeerRow, PeerStore,
};
use teamclaude_rs::peer::id::NodeKey;
use teamclaude_rs::peer::listener::{self, SessionContext};
use teamclaude_rs::peer::noise::{self, Handshake, PeerSession};
use teamclaude_rs::peer::probe::{
    self, PathPolicy, PathStat, PathTable, PathsConfig, ProbeOutcome, ProbeStop,
};
use teamclaude_rs::peer::serve;
use tokio::net::{TcpListener, TcpStream};

/// A clock that is not the machine's, so a timestamp in a fixture reads the
/// same on every run. 2026-01-01T00:00:00Z.
const FIXED_MS: i64 = 1_767_225_600_000;

/// The delay the timing gate is about.
const DELAY_MS: u64 = 40;

/// What the stand-in peer does with a probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FarEnd {
    /// This build: echo the probe back after a fixed delay.
    EchoAfter(Duration),
    /// A build older than the two variants, as `#[serde(other)]` leaves it: the
    /// frame parses as `Unknown` and the handler answers with its refusal. This
    /// arm plays the refusal as an explicit `Unknown` frame.
    AnswerUnknown,
    /// The same older build, playing the refusal the way its listener really
    /// ends: `bail!` out of the control loop, which drops the session.
    CloseTheSession,
}

/// One pinned row for a static key, every grant at its default.
///
/// `Allow::default()` is every grant OFF, which is enough: a CONTROL stream is
/// granted to any pinned row (`listener::peer_stream_gate_rows`), and a probe
/// is a CONTROL frame.
fn pinned(key: [u8; 32], label: &str) -> PeerRow {
    PeerRow {
        node: PeerId(key),
        label: label.to_string(),
        endpoints: Vec::new(),
        added_at: FIXED_MS,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: Allow::default(),
        lend: Vec::new(),
    }
}

/// The peer at the far end of the timing and old-build tests.
///
/// Everything up to the first control frame is production: the `IK` responder
/// handshake, the pin check against real rows, the production stream header
/// read. Only the answer to a probe is scripted, for the reason the module
/// docs give.
///
/// The returned handle answers with how many `Probe` frames it read, which is
/// what the "exactly one probe" gate is an assertion about.
fn far_end(
    listening: TcpListener,
    secret: [u8; 32],
    rows: Vec<PeerRow>,
    behaviour: FarEnd,
    probes: Arc<AtomicUsize>,
) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    tokio::spawn(async move {
        let (mut stream, _) = listening.accept().await?;
        let pin_rows = rows.clone();
        let mut session = noise::accept_handshake(
            &mut stream,
            &secret,
            Handshake::Return,
            &[],
            move |remote| noise::pin_check_rows(remote, &pin_rows),
        )
        .await?;
        let header: StreamHeader = serve::recv_control(&mut stream, &mut session).await?;
        anyhow::ensure!(header.kind == StreamKind::Control, "a CONTROL stream");

        loop {
            let frame = match noise::recv_encrypted(&mut stream, &mut session.transport).await {
                Ok(frame) => frame,
                // The dialler hung up, which is how every one of these tests
                // ends.
                Err(_) => return Ok(()),
            };
            let message: Control = serde_json::from_slice(&frame)?;
            let Control::Probe { nonce, sent_ms } = message else {
                continue;
            };
            probes.fetch_add(1, Ordering::SeqCst);
            match behaviour {
                FarEnd::EchoAfter(delay) => {
                    tokio::time::sleep(delay).await;
                    let bytes = serde_json::to_vec(&Control::ProbeAck { nonce, sent_ms })?;
                    noise::send_encrypted(&mut stream, &mut session.transport, &bytes).await?;
                }
                FarEnd::AnswerUnknown => {
                    let bytes = serde_json::to_vec(&Control::Unknown)?;
                    noise::send_encrypted(&mut stream, &mut session.transport, &bytes).await?;
                }
                FarEnd::CloseTheSession => return Ok(()),
            }
        }
    })
}

/// Open a CONTROL stream to `addr` the way `serve::say_hello` does: an `IK`
/// return against the pinned key, then the production stream header.
async fn open_control(
    addr: SocketAddr,
    secret: &[u8; 32],
    remote: &[u8; 32],
) -> anyhow::Result<(TcpStream, PeerSession)> {
    let mut stream = TcpStream::connect(addr).await?;
    let mut session =
        noise::dial_handshake(&mut stream, secret, Handshake::Return, Some(remote), None).await?;
    let header = StreamHeader {
        kind: StreamKind::Control,
        target: None,
        via: Vec::new(),
        hops_remaining: 1,
        request_id: 11,
    };
    serve::send_control(&mut stream, &mut session, &header).await?;
    Ok((stream, session))
}

/// A far end, its address, and the two keys the dialler needs.
struct Scripted {
    addr: SocketAddr,
    dialer_secret: [u8; 32],
    far_public: [u8; 32],
    probes: Arc<AtomicUsize>,
    handle: tokio::task::JoinHandle<anyhow::Result<()>>,
}

async fn scripted(behaviour: FarEnd) -> Scripted {
    let (dialer_secret, dialer_public) = noise::generate_static().expect("a dialler keypair");
    let (far_secret, far_public) = noise::generate_static().expect("a far-end keypair");
    let listening = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port");
    let addr = listening.local_addr().expect("the bound address");
    let probes = Arc::new(AtomicUsize::new(0));
    let handle = far_end(
        listening,
        far_secret,
        vec![pinned(dialer_public, "studio-mac")],
        behaviour,
        probes.clone(),
    );
    Scripted {
        addr,
        dialer_secret,
        far_public,
        probes,
        handle,
    }
}

// ---------------------------------------------------------------------------
// Item 1: the measurement
// ---------------------------------------------------------------------------

/// An upper bound wide enough to be about units, not about speed.
///
/// A round trip on loopback behind a 40 ms sleep is tens of milliseconds on an
/// idle box and can be a few hundred on a loaded one. 2000 ms is two orders of
/// magnitude above the sleep, so what it still catches is a number that is not
/// milliseconds at all: microseconds read as milliseconds, or a nanosecond
/// count cast down. It is deliberately not a precision gate, because a
/// precision gate here measures the scheduler and not the prober.
const RTT_CEILING_MS: f64 = 2000.0;

/// How much of the 40 ms sleep must survive into the difference between the
/// two runs below.
///
/// Half the sleep. The two runs pay the same costs apart from the sleep, so
/// whatever the scheduler adds falls out of the difference; what is left is
/// non-common cost, one run being parked harder than the other, and 20 ms of
/// room for it. A prober that reported a constant, or a number off the wire
/// rather than its own clock, reads the same on both runs and the difference
/// is zero.
const DELAY_VISIBLE_MS: f64 = 20.0;

/// One five-probe run against a peer that answers `delay` late, and the path
/// the asker measured for it.
///
/// A helper rather than a body, because the measurement gate below needs two
/// of these runs to compare against each other.
async fn five_probes_against(delay: Duration) -> PathStat {
    let script = scripted(FarEnd::EchoAfter(delay)).await;
    let (mut stream, mut session) =
        open_control(script.addr, &script.dialer_secret, &script.far_public)
            .await
            .expect("a CONTROL stream to the far end");

    let mut table = PathTable::default();
    let locator = Locator::Direct { addr: script.addr };
    let run = probe::probe_session(
        &mut stream,
        &mut session,
        &mut table,
        locator,
        5,
        Duration::ZERO,
    )
    .await
    .expect("five probes");

    assert_eq!(run.sent, 5, "five rounds asked for, five probes written");
    assert_eq!(run.stop, ProbeStop::Completed);
    assert_eq!(
        script.probes.load(Ordering::SeqCst),
        5,
        "and the peer read all five"
    );

    let stat = table
        .stat(&PeerId(script.far_public), &locator)
        .expect("the path this probing measured")
        .clone();

    drop(stream);
    script.handle.await.expect("the far end").expect("no error");
    stat
}

/// **Five probes against a peer that answers 40 ms late read at least 40 ms,
/// and read higher than the same five against a peer that answers at once.**
///
/// The gate for the EWMA and for the whole prober: the number recorded is a
/// ROUND TRIP measured by the asker's own clock, not something off the wire
/// and not a constant. Two facts carry that, and neither of them is a window
/// around 40:
///
/// 1. The scripted 40 ms sleep is a FLOOR the asker cannot undercut. Every
///    sample is the sleep plus two Noise frames on loopback, so every sample
///    is above 40, so an EWMA of them with any weighting is above 40. A box
///    under load pushes this number UP, which is the direction the assertion
///    already allows. The ceiling beside it is about units, see
///    [`RTT_CEILING_MS`].
/// 2. The same five probes against a peer that answers with no sleep read
///    lower, by at least half the sleep. That is the half that says the number
///    tracks THIS peer's answer and is not a constant: a prober that returned
///    40 for everything, or 0 for everything, has a difference of zero.
///
/// The window this replaced was 40 plus or minus 15 ms, and it measured the
/// scheduler: PR #349 read 68.57 ms for these same five answers on a loaded
/// CI runner, which is the 40 ms sleep plus 28 ms of scheduling on a path
/// whose only other cost is two frames on loopback. Nothing was wrong with the
/// prober in that run.
///
/// Watched red: in `src/peer/probe.rs::probe_once`, replace
/// `rtt_ms: u32::try_from(elapsed).unwrap_or(u32::MAX)` with `rtt_ms: 0` and
/// both halves go red, the floor on 0 against 40 and the difference on 0
/// against 0.
#[tokio::test]
async fn five_probes_against_a_forty_millisecond_peer_read_forty() {
    let delayed = five_probes_against(Duration::from_millis(DELAY_MS)).await;
    let rtt = delayed.rtt_ms.expect("five acks, so there is a round trip");
    assert!(
        rtt >= DELAY_MS as f64,
        "a scripted {DELAY_MS} ms answer cannot be measured faster than {DELAY_MS} ms, \
         and this read {rtt}"
    );
    assert!(
        rtt <= RTT_CEILING_MS,
        "a 40 ms answer on loopback cannot be {rtt} ms; see RTT_CEILING_MS"
    );
    assert_eq!(delayed.samples, 5);
    assert!(
        delayed.loss_pct.abs() < f64::EPSILON,
        "nothing was lost, so loss is zero and not {}",
        delayed.loss_pct
    );

    let prompt = five_probes_against(Duration::ZERO).await;
    let prompt_rtt = prompt
        .rtt_ms
        .expect("five acks from the prompt peer too, so there is a round trip");
    assert!(
        rtt - prompt_rtt >= DELAY_VISIBLE_MS,
        "the {DELAY_MS} ms peer must read at least {DELAY_VISIBLE_MS} ms above the peer \
         that answers at once, and it read {rtt} against {prompt_rtt}"
    );
}

/// **A peer whose build does not know what a probe is gets exactly one.**
///
/// Five rounds are asked for. The first answer is `Unknown`, and that is the
/// end of it: asking again would cost a frame per minute per peer for as long
/// as the session lives, and the answer cannot change inside one session.
///
/// Watched red: in `src/peer/probe.rs::probe_session`, delete the
/// `if outcome == ProbeOutcome::NotSupported { return ... }` block and the peer
/// reads five probes instead of one.
#[tokio::test]
async fn a_peer_that_answers_unknown_gets_exactly_one_probe() {
    let script = scripted(FarEnd::AnswerUnknown).await;
    let (mut stream, mut session) =
        open_control(script.addr, &script.dialer_secret, &script.far_public)
            .await
            .expect("a CONTROL stream to the far end");

    let mut table = PathTable::default();
    let run = probe::probe_session(
        &mut stream,
        &mut session,
        &mut table,
        Locator::Direct { addr: script.addr },
        5,
        Duration::ZERO,
    )
    .await
    .expect("the run ends without an error");

    assert_eq!(run.sent, 1, "one probe, and then it stops asking");
    assert_eq!(run.stop, ProbeStop::PeerDoesNotProbe);
    assert_eq!(
        script.probes.load(Ordering::SeqCst),
        1,
        "and the peer read exactly one"
    );
    assert!(
        table.stats.is_empty(),
        "an older build is not a lossy path: nothing is recorded about it, so the direct \
         path keeps the order it had"
    );

    drop(stream);
    script.handle.await.expect("the far end").expect("no error");
}

/// The same rule for how an older build really ends a control loop: its `other`
/// arm bails, which drops the session. A closed stream on a probe is the same
/// "this peer does not probe", and the same one probe.
///
/// Watched red: the same deletion as the test above, which makes this one fail
/// at the second probe with a broken-pipe error instead of stopping.
#[tokio::test]
async fn a_peer_that_closes_the_session_gets_exactly_one_probe() {
    let script = scripted(FarEnd::CloseTheSession).await;
    let (mut stream, mut session) =
        open_control(script.addr, &script.dialer_secret, &script.far_public)
            .await
            .expect("a CONTROL stream to the far end");

    let mut table = PathTable::default();
    let run = probe::probe_session(
        &mut stream,
        &mut session,
        &mut table,
        Locator::Direct { addr: script.addr },
        5,
        Duration::ZERO,
    )
    .await
    .expect("a closed session is not an error to the prober");

    assert_eq!(run.sent, 1);
    assert_eq!(run.stop, ProbeStop::PeerDoesNotProbe);
    assert_eq!(script.probes.load(Ordering::SeqCst), 1);
    assert!(table.stats.is_empty());
    script.handle.await.expect("the far end").expect("no error");
}

/// **A peer that answers after the probe timeout is recorded as loss, and the
/// session stops there: reading the stream again risks a torn frame.**
///
/// `probe_once`'s own read is wrapped in `tokio::time::timeout`, and
/// `noise::recv_encrypted` is not cancel safe (its doc explains why), so a
/// second read on the same stream after a timeout can take whatever bytes the
/// cancelled read left behind. The only safe rule is the one the two sibling
/// timeouts already follow: a timeout ends the session, here, not two rounds
/// later.
///
/// The scripted far end answers well after `PROBE_TIMEOUT`, late enough that
/// this test's own single round finishes and the test ends before that answer
/// is ever written, so nothing here depends on whether it arrives.
///
/// Watched red: before the fix, `probe_session` only stops early on
/// `ProbeOutcome::NotSupported`; a `NoAnswer` falls through to the next round
/// instead, so this test's `run.sent` reads 2, not 1, and `run.stop` reads
/// `Completed`, not `TimedOut`.
#[tokio::test]
async fn a_peer_that_answers_after_the_timeout_is_loss_and_the_session_stops() {
    let script = scripted(FarEnd::EchoAfter(
        probe::PROBE_TIMEOUT + Duration::from_secs(2),
    ))
    .await;
    let (mut stream, mut session) =
        open_control(script.addr, &script.dialer_secret, &script.far_public)
            .await
            .expect("a CONTROL stream to the far end");

    let mut table = PathTable::default();
    let locator = Locator::Direct { addr: script.addr };
    let run = probe::probe_session(
        &mut stream,
        &mut session,
        &mut table,
        locator,
        2,
        Duration::ZERO,
    )
    .await
    .expect("a timeout is not an error to the prober");

    assert_eq!(
        run.sent, 1,
        "one probe sent, and the timeout ends the session before a second"
    );
    assert_eq!(
        run.stop,
        ProbeStop::TimedOut,
        "a silent probe within the timeout is loss, not a build that cannot parse one"
    );
    assert_eq!(
        script.probes.load(Ordering::SeqCst),
        1,
        "the peer read exactly one; the second round is never sent"
    );
    let stat = table
        .stat(&PeerId(script.far_public), &locator)
        .expect("a row after the timeout");
    assert!(
        (stat.loss_pct - 100.0).abs() < f64::EPSILON,
        "one unanswered probe is total loss, not {}",
        stat.loss_pct
    );
    assert!(
        stat.rtt_ms.is_none(),
        "nothing was acked within the timeout, so there is no round trip"
    );
}

/// **The shipped listener answers a probe, and echoes both fields untouched.**
///
/// `listener::serve_on_with` is the production accept loop, so what answers
/// here is the production handshake, the production stream gate, the production
/// per-frame row re-read and the production dispatch arm. The two stand-in
/// tests above measure the asker; this one is the only place the responder's
/// arm is real.
///
/// The echo is asserted field by field rather than through the prober, because
/// the prober would pass on a responder that replaced `sent_ms` with its own
/// clock, and that replacement is exactly what would turn a round trip into a
/// measurement of NTP skew.
///
/// Watched red: in `src/peer/listener.rs`'s `Control::Probe` arm, replace
/// `sent_ms` in the `ProbeAck` with `crate::now_ms()` and the echo assertion
/// fails.
#[tokio::test]
async fn the_real_listener_echoes_a_probe_with_both_fields_untouched() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let key_dir: PathBuf = dir.path().to_path_buf();
    let peers_path = dir.path().join("tcr-peers.json");
    let state_path = dir.path().join("peer-state.json");

    let (dialer_secret, dialer_public) = noise::generate_static().expect("a dialler keypair");
    let key = NodeKey::load_or_mint(&key_dir).expect("the listener's node key");
    let file = PeerFile {
        peers: vec![pinned(dialer_public, "studio-mac")],
        ..Default::default()
    };
    teamclaude_rs::peer::config::save(&peers_path, &file).expect("write the peers file");
    let store = PeerStore::open(&peers_path).expect("open the peers file");

    let listening = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port");
    let addr = listening.local_addr().expect("the bound address");
    let context = SessionContext::new(&key, store.path(), &state_path);
    tokio::spawn(async move {
        // The loop only ends when the socket is dropped, which is when this
        // test finishes.
        let _ = listener::serve_on_with(listening, context).await;
    });

    let (mut stream, mut session) = open_control(addr, &dialer_secret, &key.id().0)
        .await
        .expect("a CONTROL stream to the real listener");

    let asked = Control::Probe {
        nonce: 0x0123_4567_89ab_cdef,
        sent_ms: FIXED_MS,
    };
    serve::send_control(&mut stream, &mut session, &asked)
        .await
        .expect("the probe goes out");
    let answer: Control = serve::recv_control(&mut stream, &mut session)
        .await
        .expect("the listener answers");
    assert_eq!(
        answer,
        Control::ProbeAck {
            nonce: 0x0123_4567_89ab_cdef,
            sent_ms: FIXED_MS,
        },
        "both fields come back exactly as they were sent"
    );

    // And the prober's own path against the same listener, so the two halves
    // are known to fit: a real ack, timed by the asker.
    let outcome = probe::probe_once(&mut stream, &mut session, 99, Duration::from_secs(5))
        .await
        .expect("a probe against the real listener");
    assert!(
        matches!(outcome, ProbeOutcome::Acked { .. }),
        "the shipped listener answers a probe, not {outcome:?}"
    );
}

/// The EWMA is what `CollapseHint.observed_rtt_ms` carries, and `0` is how "no
/// measurement" reads, which is what every build before this one sent.
///
/// Watched red: in `src/peer/probe.rs::collapse_hint`, replace
/// `rtt_key(Some(rtt))` with `0` and the measured case reads 0.
#[test]
fn a_collapse_hint_carries_the_measured_round_trip() {
    let peer = PeerId([7_u8; 32]);
    let addr: SocketAddr = "127.0.0.1:9601".parse().expect("an addr");
    let empty = PathTable::default();
    assert_eq!(
        probe::collapse_hint(peer, vec![addr.to_string()], &empty).observed_rtt_ms,
        0,
        "nothing measured reads as no measurement"
    );

    let mut table = PathTable::default();
    for _ in 0..5 {
        table.record(
            peer,
            Locator::Direct { addr },
            ProbeOutcome::Acked { rtt_ms: 40 },
            FIXED_MS,
        );
    }
    let hint = probe::collapse_hint(peer, vec![addr.to_string()], &table);
    assert_eq!(hint.node, peer);
    assert_eq!(
        hint.observed_rtt_ms, 40,
        "the hint carries what this node measured"
    );
}

/// Loss is recorded from a probe that went unanswered, and an older build's
/// refusal is not loss.
///
/// Watched red: in `src/peer/probe.rs::PathTable::record`, change the
/// `ProbeOutcome::NotSupported => return false` arm to fall through to the
/// `NoAnswer` sample and the last assertion sees a loss figure.
#[test]
fn a_silent_probe_is_loss_and_an_older_build_is_not() {
    let peer = PeerId([8_u8; 32]);
    let addr: SocketAddr = "127.0.0.1:9602".parse().expect("an addr");
    let locator = Locator::Direct { addr };
    let mut table = PathTable::default();

    table.record(peer, locator, ProbeOutcome::NoAnswer, FIXED_MS);
    let stat = table.stat(&peer, &locator).expect("a row after one sample");
    assert!(
        (stat.loss_pct - 100.0).abs() < f64::EPSILON,
        "one lost probe and nothing else is total loss, not {}",
        stat.loss_pct
    );
    assert!(
        stat.rtt_ms.is_none(),
        "a path that has only ever timed out has no round trip, and a zero here would make \
         it look like the fastest path on the row"
    );

    let mut untouched = PathTable::default();
    assert!(
        !untouched.record(peer, locator, ProbeOutcome::NotSupported, FIXED_MS),
        "an older build records nothing"
    );
    assert!(untouched.stats.is_empty());
}

// ---------------------------------------------------------------------------
// Item 2: the path policy and the dial order
// ---------------------------------------------------------------------------

/// A row with two direct endpoints, the newer one at `newer`.
fn two_endpoint_row(peer: PeerId, older: SocketAddr, newer: SocketAddr) -> PeerRow {
    let mut row = pinned(peer.0, "attic-nuc");
    row.endpoints = vec![
        Endpoint::direct(newer, FIXED_MS + 60_000, EndpointSource::Hello),
        Endpoint::direct(older, FIXED_MS, EndpointSource::Paired),
    ];
    row
}

/// **The newer endpoint is lossy above the cap, so the older one is dialled
/// first.**
///
/// The gate for item 2, and the control beside it is what makes it a
/// measurement: with the same two endpoints and no loss recorded, the NEWER one
/// leads, which is the order the row had before this change. So the reordering is
/// the loss figure's doing and not the comparator's shape.
///
/// Watched red: in `src/peer/probe.rs::order_endpoints`, replace the `lossy`
/// key with `0u8` and the lossy newer endpoint leads again.
#[test]
fn a_lossy_newer_endpoint_is_dialled_after_a_clean_older_one() {
    let peer = PeerId([9_u8; 32]);
    let older: SocketAddr = "127.0.0.1:9701".parse().expect("an addr");
    let newer: SocketAddr = "127.0.0.1:9702".parse().expect("an addr");
    let row = two_endpoint_row(peer, older, newer);

    let clean = PathTable::default();
    assert_eq!(
        probe::order_endpoints(&row, &clean)
            .first()
            .and_then(Endpoint::direct_addr),
        Some(newer),
        "with nothing measured the newest observation leads, exactly as before this change"
    );

    let table = PathTable::new(
        PathsConfig::default(),
        vec![
            PathStat {
                peer,
                locator: Locator::Direct { addr: newer },
                rtt_ms: Some(12.0),
                // Above the five percent default, and FASTER than the older
                // path: a fast path that drops one request in five is worse
                // than a slower one that drops none.
                loss_pct: 20.0,
                samples: 20,
                updated_at_ms: FIXED_MS,
            },
            PathStat {
                peer,
                locator: Locator::Direct { addr: older },
                rtt_ms: Some(80.0),
                loss_pct: 0.0,
                samples: 20,
                updated_at_ms: FIXED_MS,
            },
        ],
    );
    let order = probe::order_endpoints(&row, &table);
    assert_eq!(
        order.first().and_then(Endpoint::direct_addr),
        Some(older),
        "the older endpoint is under the loss cap, so it is tried first"
    );
    assert_eq!(
        order.last().and_then(Endpoint::direct_addr),
        Some(newer),
        "and the lossy one is tried after it, never dropped: a lossy path that is the only \
         path is still the way home"
    );
    assert_eq!(order.len(), 2, "both endpoints are still there");
}

/// Round trip orders two paths that are both under the cap, and the policy
/// orders a hop against a socket.
///
/// Watched red: in `src/peer/probe.rs::order_endpoints`, replace the `rtt` key
/// with `0u32` and the slow-but-newer endpoint leads.
#[test]
fn round_trip_orders_two_clean_paths_and_the_policy_orders_the_hop() {
    let peer = PeerId([10_u8; 32]);
    let older: SocketAddr = "127.0.0.1:9703".parse().expect("an addr");
    let newer: SocketAddr = "127.0.0.1:9704".parse().expect("an addr");
    let carrier = PeerId([11_u8; 32]);
    let mut row = two_endpoint_row(peer, older, newer);
    row.endpoints.push(Endpoint::via(
        carrier,
        FIXED_MS + 120_000,
        EndpointSource::Hello,
    ));

    let measured = |rtt_newer: f64, rtt_older: f64| {
        vec![
            PathStat {
                peer,
                locator: Locator::Direct { addr: newer },
                rtt_ms: Some(rtt_newer),
                loss_pct: 0.0,
                samples: 20,
                updated_at_ms: FIXED_MS,
            },
            PathStat {
                peer,
                locator: Locator::Direct { addr: older },
                rtt_ms: Some(rtt_older),
                loss_pct: 0.0,
                samples: 20,
                updated_at_ms: FIXED_MS,
            },
        ]
    };

    let table = PathTable::new(PathsConfig::default(), measured(90.0, 9.0));
    let order = probe::order_endpoints(&row, &table);
    assert_eq!(
        order.first().and_then(Endpoint::direct_addr),
        Some(older),
        "both are clean, so the faster one leads even though it is the older observation"
    );
    assert!(
        order.last().is_some_and(Endpoint::is_via),
        "and the forwarded hop is last under the default policy, newest though it is"
    );

    let prefers_via = PathTable::new(
        PathsConfig {
            prefer: PathPolicy::Via,
            ..PathsConfig::default()
        },
        measured(90.0, 9.0),
    );
    assert!(
        probe::order_endpoints(&row, &prefers_via)
            .first()
            .is_some_and(Endpoint::is_via),
        "an operator who asked for `via` gets the hop first"
    );

    let refuses_that_carrier = PathTable::new(
        PathsConfig {
            prefer: PathPolicy::Via,
            via_allow: vec![PeerId([12_u8; 32])],
            ..PathsConfig::default()
        },
        measured(90.0, 9.0),
    );
    let order = probe::order_endpoints(&row, &refuses_that_carrier);
    assert!(
        order.last().is_some_and(Endpoint::is_via),
        "a hop through a forwarder outside `viaAllow` sorts behind everything, and is still \
         on the list"
    );
    assert_eq!(order.len(), 3);
}

/// **The shipped dial order delegates to the comparator, and an unmeasured
/// fleet dials exactly as it did before this change.**
///
/// `serve::dial_order` is what `dial_peer_with_endpoint` calls, so this is the
/// wiring assertion: with no measurements the answer is still via-last,
/// newest-first, which is what every build before this one did.
///
/// Watched red: in `src/peer/serve.rs`, replace the body with
/// `row.endpoints.clone()` and the hop is no longer last.
#[test]
fn the_shipped_dial_order_is_the_comparator_and_an_unmeasured_row_is_unchanged() {
    let peer = PeerId([13_u8; 32]);
    let older: SocketAddr = "127.0.0.1:9705".parse().expect("an addr");
    let newer: SocketAddr = "127.0.0.1:9706".parse().expect("an addr");
    let mut row = two_endpoint_row(peer, older, newer);
    row.endpoints.insert(
        0,
        Endpoint::via(
            PeerId([14_u8; 32]),
            FIXED_MS + 300_000,
            EndpointSource::Hello,
        ),
    );

    let shipped = serve::dial_order(&row);
    assert_eq!(
        shipped,
        probe::order_endpoints(&row, &PathTable::default()),
        "the shipped order is the comparator's order"
    );
    assert_eq!(
        shipped
            .iter()
            .filter_map(Endpoint::direct_addr)
            .collect::<Vec<_>>(),
        vec![newer, older],
        "newest first among the sockets"
    );
    assert!(
        shipped.last().is_some_and(Endpoint::is_via),
        "and the forwarded hop last, newest though it is"
    );
}

/// **An endpoint that just failed a dial is skipped while it is cooling, and
/// the other endpoint on the row is what is left to dial.**
///
/// The row-level gate for the retry backoff: `PathTable::cool_down` records
/// the failure and `probe::drop_cooling_endpoints` is what `dial_order`
/// applies to its own sorted order, so this drives the filter directly, the
/// way the module doc says a test can.
///
/// Watched red: `PathTable::cool_down` and `probe::drop_cooling_endpoints` do
/// not exist before this change.
#[test]
fn a_cooling_endpoint_is_skipped_and_the_other_one_on_the_row_is_dialled() {
    let peer = PeerId([22_u8; 32]);
    let older: SocketAddr = "127.0.0.1:9714".parse().expect("an addr");
    let newer: SocketAddr = "127.0.0.1:9715".parse().expect("an addr");
    let row = two_endpoint_row(peer, older, newer);

    let mut table = PathTable::default();
    let order = probe::order_endpoints(&row, &table);
    assert_eq!(
        order.first().and_then(Endpoint::direct_addr),
        Some(newer),
        "unfiltered, the newest endpoint leads"
    );

    table.cool_down(peer, Locator::Direct { addr: newer }, FIXED_MS + 60_000);
    let awake = probe::drop_cooling_endpoints(order, &peer, &table, FIXED_MS);
    assert_eq!(
        awake
            .iter()
            .filter_map(Endpoint::direct_addr)
            .collect::<Vec<_>>(),
        vec![older],
        "the endpoint that just failed is dropped while it cools, and the row's other \
         endpoint is what is left to dial"
    );
}

/// **Never strand a row: when every endpoint on it is cooling, the full order
/// comes back rather than an empty list.**
///
/// The same rule `PathsConfig::max_loss_pct` already states for loss, "a
/// lossy path that is the only path is still the way home", applies here: a
/// cooling path that is the only path is still tried.
///
/// Watched red: `PathTable::cool_down` and `probe::drop_cooling_endpoints` do
/// not exist before this change.
#[test]
fn every_endpoint_cooling_still_returns_the_row_whole() {
    let peer = PeerId([23_u8; 32]);
    let older: SocketAddr = "127.0.0.1:9716".parse().expect("an addr");
    let newer: SocketAddr = "127.0.0.1:9717".parse().expect("an addr");
    let row = two_endpoint_row(peer, older, newer);

    let mut table = PathTable::default();
    let order = probe::order_endpoints(&row, &table);
    table.cool_down(peer, Locator::Direct { addr: newer }, FIXED_MS + 60_000);
    table.cool_down(peer, Locator::Direct { addr: older }, FIXED_MS + 60_000);

    let awake = probe::drop_cooling_endpoints(order.clone(), &peer, &table, FIXED_MS);
    assert_eq!(
        awake, order,
        "every endpoint on the row is cooling, so the full order comes back instead of an \
         empty list, and the row is still dialled"
    );
}

/// The carry order PROBE hands EGRESS-PIN: measured first, and an unprobed
/// fleet keeps the freshness order `resolve_via` has today.
///
/// Watched red: in `src/peer/probe.rs::carrier_key`, replace the `rtt` element
/// with `u32::MAX` and the lossless, faster carrier stops leading.
#[test]
fn the_carrier_key_is_measured_first_and_fresh_second() {
    let quick = PeerId([15_u8; 32]);
    let slow = PeerId([16_u8; 32]);
    let addr: SocketAddr = "127.0.0.1:9707".parse().expect("an addr");
    let table = PathTable::new(
        PathsConfig::default(),
        vec![
            PathStat {
                peer: quick,
                locator: Locator::Direct { addr },
                rtt_ms: Some(10.0),
                loss_pct: 0.0,
                samples: 20,
                updated_at_ms: FIXED_MS,
            },
            PathStat {
                peer: slow,
                locator: Locator::Direct { addr },
                rtt_ms: Some(200.0),
                loss_pct: 0.0,
                samples: 20,
                updated_at_ms: FIXED_MS,
            },
        ],
    );

    // The slow one was seen more recently, and still sorts second.
    let mut ordered = [(slow, Some(FIXED_MS + 60_000)), (quick, Some(FIXED_MS))];
    ordered.sort_by_key(|(peer, last_seen)| probe::carrier_key(peer, *last_seen, &table));
    assert_eq!(
        ordered.first().map(|(peer, _)| *peer),
        Some(quick),
        "a measured carrier is ordered by what it measured"
    );

    // And with nothing measured, freshness alone, which is today's rule.
    let empty = PathTable::default();
    let mut unmeasured = [
        (quick, Some(FIXED_MS)),
        (slow, Some(FIXED_MS + 60_000)),
        (PeerId([17_u8; 32]), None),
    ];
    unmeasured.sort_by_key(|(peer, last_seen)| probe::carrier_key(peer, *last_seen, &empty));
    assert_eq!(
        unmeasured.iter().map(|(peer, _)| *peer).collect::<Vec<_>>(),
        vec![slow, quick, PeerId([17_u8; 32])],
        "most recently seen first, never-seen last"
    );
}

// ---------------------------------------------------------------------------
// What survives a restart
// ---------------------------------------------------------------------------

/// The `paths` section round-trips through the state file, and writing it keeps
/// every other key.
///
/// Watched red: in `src/peer/state.rs::save_paths`, write `state.paths =
/// Vec::new()` instead of the rows and the first assertion sees nothing.
#[test]
fn the_measured_paths_survive_a_restart_and_keep_the_rest_of_the_file() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("peer-state.json");
    let peer = PeerId([18_u8; 32]);
    let addr: SocketAddr = "127.0.0.1:9708".parse().expect("an addr");

    let mut state = teamclaude_rs::peer::state::PeerState::default();
    state.muted.push(teamclaude_rs::peer::state::Mute {
        // Dated far ahead on purpose: `save_paths` reads the file through
        // `load` at the REAL clock, which prunes a mute whose deadline has
        // passed, so a mute dated from this file's fixed clock would be pruned
        // by the writer and read as "the section writer lost it".
        addr: "127.0.0.1".to_string(),
        until_ms: 4_102_444_800_000,
    });
    teamclaude_rs::peer::state::save(&path, &state).expect("the first write");

    let mut table = PathTable::default();
    for _ in 0..5 {
        table.record(
            peer,
            Locator::Direct { addr },
            ProbeOutcome::Acked { rtt_ms: 40 },
            FIXED_MS,
        );
    }
    teamclaude_rs::peer::state::save_paths(&path, &table.stats).expect("the paths write");

    let restored = teamclaude_rs::peer::state::load(&path, FIXED_MS).expect("the state reads");
    assert_eq!(restored.paths.len(), 1, "the measured path survived");
    let stat = restored.paths.first().expect("the row");
    assert_eq!(stat.peer, peer);
    assert_eq!(stat.locator, Locator::Direct { addr });
    assert_eq!(stat.rtt_ms, Some(40.0));
    assert_eq!(
        restored.muted.len(),
        1,
        "and a section writer keeps every other key: the mute the operator set is still there"
    );
}

/// **An endpoint a neighbour's brief supplied is dialled after the one the
/// pairing proved, newer and faster though it is.**
///
/// A failing ordering input. `Hello.briefs` is a peer's word about where a
/// THIRD machine answers, and a brief endpoint carries this node's own clock,
/// so it is always the newest thing on the row: with recency as the only
/// tiebreak, an injected address led the dial order for a peer whose real
/// address a completed handshake had proved.
///
/// Two controls make this the source key and nothing else. The same row with
/// that endpoint's source changed to `Hello` puts it first, which is the order
/// before this change; and a measured, fast, lossless brief endpoint still
/// sorts behind an unmeasured paired one, which is the claim that matters: the
/// measurements are taken on the paths this list chose, so a stranger who can
/// make one fast path must not be able to buy the front of the queue with it.
///
/// Watched red: in `src/peer/probe.rs::order_endpoints`, replace the
/// `source_rank(endpoint)` key with `0u8` and both assertions on the brief
/// endpoint fail, in the unmeasured case and in the measured one.
#[test]
fn a_brief_endpoint_is_dialled_after_a_paired_one() {
    let peer = PeerId([21_u8; 32]);
    let paired: SocketAddr = "127.0.0.1:9711".parse().expect("an addr");
    let briefed: SocketAddr = "127.0.0.1:9712".parse().expect("an addr");

    let mut row = pinned(peer.0, "attic-nuc");
    row.endpoints = vec![
        Endpoint::direct(briefed, FIXED_MS + 60_000, EndpointSource::Brief),
        Endpoint::direct(paired, FIXED_MS, EndpointSource::Paired),
    ];

    let order = probe::order_endpoints(&row, &PathTable::default());
    assert_eq!(
        order.first().and_then(Endpoint::direct_addr),
        Some(paired),
        "the proven endpoint leads: {order:?}"
    );
    assert_eq!(
        order.last().and_then(Endpoint::direct_addr),
        Some(briefed),
        "and the briefed one is still tried, last: {order:?}"
    );

    // Control one: the same row, the same timestamps, that endpoint learned
    // from a session instead. Now the newest leads, which is the order every
    // build before this key had.
    let mut from_session = row.clone();
    from_session.endpoints[0] = Endpoint::direct(briefed, FIXED_MS + 60_000, EndpointSource::Hello);
    assert_eq!(
        probe::order_endpoints(&from_session, &PathTable::default())
            .first()
            .and_then(Endpoint::direct_addr),
        Some(briefed),
        "control: with the source the only difference, the newest endpoint leads again"
    );

    // Control two: measured, fast and lossless, and still behind.
    let table = PathTable::new(
        PathsConfig::default(),
        vec![PathStat {
            peer,
            locator: Locator::Direct { addr: briefed },
            rtt_ms: Some(3.0),
            loss_pct: 0.0,
            samples: 20,
            updated_at_ms: FIXED_MS,
        }],
    );
    let measured = probe::order_endpoints(&row, &table);
    assert_eq!(
        measured.first().and_then(Endpoint::direct_addr),
        Some(paired),
        "a three millisecond brief endpoint does not outrank an unmeasured paired one: \
         {measured:?}"
    );
}

/// **An address off a dead drop is dialled last of all, below a brief.**
///
/// A brief came from a Mac this node pinned, over a session that proved a
/// static key, about a Mac this node also pinned. A dead-drop record came off a
/// surface that can withhold and is sealed under a symmetric key a forgotten
/// peer still holds, so it is the weakest evidence on the list and it sorts
/// after everything, the brief included.
///
/// Asserted as the WHOLE expected order over the six sources this row is built
/// from rather than as one comparison between two: a single "drop is after
/// brief" assertion passes just as happily if the new band swallowed one of the
/// four above it. The seventh source, a pasted link, shares the drop's band and
/// is asserted where it is written, in `tests/peer_moved.rs`.
///
/// The recency key is deliberately set against the source key here: the drop
/// endpoint is the newest on the row and the paired one the oldest, so an order
/// that read recency first would be the exact reverse of the one asserted.
///
/// Watched red: give [`teamclaude_rs::peer::config::EndpointSource::Drop`] the
/// brief's rank (`2`) in `probe::source_rank` and the drop endpoint ties with
/// the brief, landing at index 4 instead of 5.
#[test]
fn a_drop_endpoint_sorts_below_a_brief() {
    let peer = PeerId([22_u8; 32]);
    let port_of = |source: EndpointSource| -> u16 {
        match source {
            EndpointSource::Paired => 9_721,
            EndpointSource::Hello => 9_722,
            EndpointSource::Mapping => 9_723,
            EndpointSource::Beacon => 9_724,
            EndpointSource::Brief => 9_725,
            EndpointSource::Drop => 9_726,
            EndpointSource::Moved => 9_727,
        }
    };
    let addr_of = |source: EndpointSource| -> SocketAddr {
        format!("127.0.0.1:{}", port_of(source))
            .parse()
            .expect("an addr")
    };

    // Newest first in the list and in the clock, which is the order the source
    // key has to overturn.
    let weakest_first = [
        EndpointSource::Drop,
        EndpointSource::Brief,
        EndpointSource::Beacon,
        EndpointSource::Mapping,
        EndpointSource::Hello,
        EndpointSource::Paired,
    ];
    let mut row = pinned(peer.0, "attic-nuc");
    row.endpoints = weakest_first
        .iter()
        .enumerate()
        .map(|(nth, source)| {
            let age_ms = i64::try_from(nth).expect("six endpoints fit") * 60_000;
            Endpoint::direct(addr_of(*source), FIXED_MS - age_ms, *source)
        })
        .collect();

    let ordered: Vec<SocketAddr> = probe::order_endpoints(&row, &PathTable::default())
        .into_iter()
        .filter_map(|endpoint| endpoint.direct_addr())
        .collect();

    // Paired and Hello share a band, as do Mapping and Beacon, so within each
    // the newer one leads: that is the recency tiebreak, not a fourth band.
    let expected = vec![
        addr_of(EndpointSource::Hello),
        addr_of(EndpointSource::Paired),
        addr_of(EndpointSource::Beacon),
        addr_of(EndpointSource::Mapping),
        addr_of(EndpointSource::Brief),
        addr_of(EndpointSource::Drop),
    ];
    assert_eq!(
        ordered, expected,
        "the whole dial order, weakest evidence last: what a handshake proved, then this \
         Mac's own hints, then a friend's word, then a record off a surface nobody here owns"
    );
}

// ---------------------------------------------------------------------------
// The loop: a completed session measures, records, and survives a restart
// ---------------------------------------------------------------------------

/// **A completed `peer hello` measures the path it ran over, and the number
/// survives into the state file.**
///
/// Real on both ends: the shipped listener (`listener::serve_on_with`)
/// answering a real `Control::Probe`, and `serve::say_hello`, the same
/// function `tcr peer hello` calls. Nothing here stands in for the prober.
///
/// Watched red: before the loop is wired, `say_hello` does not call
/// `probe::probe_session` at all, so `probe::with_table` never gains a row for
/// this peer and `state::save_paths` is never called on this session; the
/// first assertion below fails with "no row for the peer this session dialled".
#[tokio::test]
async fn a_completed_hello_measures_the_path_and_it_survives_a_restart() {
    let listener_dir = tempfile::tempdir().expect("a temp dir for the listener");
    let listener_key_dir: PathBuf = listener_dir.path().to_path_buf();
    let listener_peers_path = listener_dir.path().join("tcr-peers.json");
    let listener_state_path = listener_dir.path().join("peer-state.json");

    let dialer_dir = tempfile::tempdir().expect("a temp dir for the dialer");
    let dialer_key_dir: PathBuf = dialer_dir.path().to_path_buf();
    let dialer_peers_path = dialer_dir.path().join("tcr-peers.json");

    let dialer_key = NodeKey::load_or_mint(&dialer_key_dir).expect("the dialer's node key");
    let listener_key = NodeKey::load_or_mint(&listener_key_dir).expect("the listener's node key");

    // The listener pins the dialer, the way a completed pairing would have
    // left it.
    let listener_file = PeerFile {
        peers: vec![pinned(dialer_key.id().0, "attic-nuc")],
        ..Default::default()
    };
    teamclaude_rs::peer::config::save(&listener_peers_path, &listener_file)
        .expect("write the listener's peers file");

    let listening = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port");
    let listener_addr = listening.local_addr().expect("the bound address");
    let listener_store =
        PeerStore::open(&listener_peers_path).expect("open the listener's peers file");
    let context = SessionContext::new(&listener_key, listener_store.path(), &listener_state_path);
    tokio::spawn(async move {
        // Ends when the socket drops, at the end of this test.
        let _ = listener::serve_on_with(listening, context).await;
    });

    // The dialer pins the listener at its bound loopback address, the row
    // `dial_peer_with_endpoint` reads.
    let mut dialer_row = pinned(listener_key.id().0, "studio-mac");
    dialer_row.endpoints = vec![Endpoint::direct(
        listener_addr,
        FIXED_MS,
        EndpointSource::Paired,
    )];
    let dialer_file = PeerFile {
        peers: vec![dialer_row],
        ..Default::default()
    };
    teamclaude_rs::peer::config::save(&dialer_peers_path, &dialer_file)
        .expect("write the dialer's peers file");
    let dialer_store = PeerStore::open(&dialer_peers_path).expect("open the dialer's peers file");

    let _hello = serve::say_hello(&dialer_store, &PeerId(listener_key.id().0))
        .await
        .expect("say_hello did not error")
        .expect("the listener answered the hello");

    let locator = Locator::Direct {
        addr: listener_addr,
    };
    let measured =
        probe::with_table(|table| table.stat(&PeerId(listener_key.id().0), &locator).cloned())
            .expect("the path table's lock is not poisoned")
            .expect("no row for the peer this session dialled");
    assert!(
        measured.rtt_ms.is_some(),
        "a completed session against a listener that answers probes must leave a round trip, \
         not {:?}",
        measured.rtt_ms
    );

    let dialer_state_path = serve::peer_state_path(&dialer_peers_path);
    let restored = teamclaude_rs::peer::state::load(&dialer_state_path, FIXED_MS)
        .expect("the dialer's state file reads");
    let saved = restored
        .paths
        .iter()
        .find(|stat| stat.peer == PeerId(listener_key.id().0) && stat.locator == locator)
        .expect("the measured path survived into the state file");
    assert!(
        saved.rtt_ms.is_some(),
        "the row written to disk must carry the round trip, not {:?}",
        saved.rtt_ms
    );
}
