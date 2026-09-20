//! Phase 2's gates: the pin check, and what a bare pin is told.
//!
//! # Why the first one is the most important test in the design
//!
//! `snow` has no pin store. An `IK` responder learns the initiator's static key
//! after reading message 1, and **if the comparison against the pin store is
//! missing, the handshake completes and nothing errors**: every existing test
//! still passes, the connection works, and any node holding this node's public
//! key can open streams. There is no failure to observe unless someone writes
//! this test and watches it go red.
//!
//! So the property is asserted on the SOCKET, not on a log line and not on a
//! return value: after a refusal, the number of bytes the responder handed its
//! writer is zero. A responder that answered first and refused afterwards
//! passes every assertion about its error type and fails this one.
//!
//! # Local, and two listeners on one box
//!
//! Every test here runs entirely on this machine. They never read the
//! operator's config file, never touch the operator's peers file and
//! never signal a running proxy: each one binds `127.0.0.1:0` and lets the
//! kernel choose the port, and the two identities are keypairs generated in
//! this process with no files at all.
//!
//! # Nothing here is `#[ignore]`d
//!
//! Two tests were ignored because `PeerStore::open`, `PeerId::display` and
//! `sanitize_label` were `todo!()` elsewhere, so the bodies
//! panicked inside phase 1 instead of measuring phase 2. Those bodies are real
//! now and both tests run. An `#[ignore]` that outlives its reason is a gate
//! that has stopped measuring, so it goes at the same time as the blocker.

use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use tcr_peer_wire::{PeerId, StreamHeader, StreamKind, TunnelTarget};
use teamclaude_rs::peer::config::{Allow, ControlGrants, PeerRow};
use teamclaude_rs::peer::listener::{
    self, hello_for_peer, peer_stream_gate_hop, peer_stream_gate_rows, NodeFacts, RequestDedup,
    StreamRefusal,
};
use teamclaude_rs::peer::noise::{self, Handshake, PinRefusal};
use teamclaude_rs::peer::pair::{self, PairingWindow};
use tokio::io::{AsyncWrite, ReadHalf, WriteHalf};
use tokio::net::{TcpListener, TcpStream};

// ---------------------------------------------------------------------------
// The instrument: a writer that counts, so "zero bytes written" is measured on
// the socket rather than inferred from an error type.
// ---------------------------------------------------------------------------

struct CountingWriter<W> {
    inner: W,
    written: Arc<AtomicUsize>,
}

impl<W: AsyncWrite + Unpin> AsyncWrite for CountingWriter<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let counted = self.written.clone();
        let polled = Pin::new(&mut self.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = &polled {
            counted.fetch_add(*n, Ordering::SeqCst);
        }
        polled
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

type Counted = tokio::io::Join<ReadHalf<TcpStream>, CountingWriter<WriteHalf<TcpStream>>>;

/// Wrap a stream so every byte it writes is counted.
fn counted(stream: TcpStream) -> (Counted, Arc<AtomicUsize>) {
    let written = Arc::new(AtomicUsize::new(0));
    let (read, write) = tokio::io::split(stream);
    let writer = CountingWriter {
        inner: write,
        written: written.clone(),
    };
    (tokio::io::join(read, writer), written)
}

/// A pair of connected sockets on a kernel-assigned loopback port. Two
/// listeners in one test binary, which is what the design's two-machine test
/// looks like on one box.
async fn loopback_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port");
    let addr = listener.local_addr().expect("the bound address");
    let dialing = tokio::spawn(async move { TcpStream::connect(addr).await });
    let (accepted, _) = listener.accept().await.expect("the connection");
    let dialed = dialing.await.expect("the dial task").expect("the dial");
    (accepted, dialed)
}

/// One pinned row for a static key, with every grant at its default.
fn pinned(key: [u8; 32]) -> PeerRow {
    PeerRow {
        node: PeerId(key),
        label: "studio-mac".to_string(),
        endpoints: Vec::new(),
        added_at: 0,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: Allow::default(),
        lend: Vec::new(),
    }
}

/// A header for one stream kind, with a hop budget and no `via`.
fn header(kind: StreamKind, target: Option<TunnelTarget>) -> StreamHeader {
    StreamHeader {
        kind,
        target,
        via: Vec::new(),
        hops_remaining: 1,
        request_id: 7,
    }
}

/// Dial a first pairing the way production does: `XX` with the versioned
/// message-1 payload (`noise::pair_message_1_payload`).
///
/// A helper rather than the bare `dial_handshake` these tests used to call,
/// because an `XX` message 1 with an EMPTY payload is 32 bytes: exactly an
/// `NN` knock, and the listener dispatches on that length. A test that dialled
/// without the payload would be testing the knock path while claiming to test
/// pairing, which is how a gate goes green about the wrong thing.
async fn dial_first_pairing<S>(
    stream: &mut S,
    secret: &[u8; 32],
) -> anyhow::Result<noise::PeerSession>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    noise::dial_handshake_with_payload(
        stream,
        secret,
        Handshake::Pair,
        None,
        None,
        &noise::pair_message_1_payload(&TEST_INSTANCE),
    )
    .await
}

/// The instance id every first-pairing dial in this file uses.
const TEST_INSTANCE: tcr_peer_wire::InstanceId =
    tcr_peer_wire::InstanceId([0x7E; tcr_peer_wire::INSTANCE_ID_BYTES]);

// ---------------------------------------------------------------------------
// The lengths the listener's "this is not a Noise message 1" check rests on
// ---------------------------------------------------------------------------

/// Every message-1 length is MEASURED against `snow`, not copied from the
/// spec, because the listener decides WHICH PATTERN ARRIVED from the length
/// alone (`Handshake::from_message_1_len`): a wrong constant is either a
/// pattern that can never connect or a length check that lets something else
/// through.
///
/// Four patterns now, and **the four lengths must stay pairwise distinct** :
/// that is asserted here rather than left implicit, because a collision is not
/// a compile error and the consequence is a knock dispatched as a first
/// pairing. It is also the reason an `XX` message 1 carries the 8-byte instance
/// id as its payload: with an empty payload it is 32 bytes, exactly an `NN`
/// message 1, and the two could not be told apart.
#[test]
fn noise_message_one_lengths_are_what_the_gate_pins() {
    let (secret, _) = noise::generate_static().expect("a static keypair");
    let (_, responder_public) = noise::generate_static().expect("a second static keypair");
    let mut scratch = [0_u8; 1024];

    let instance = tcr_peer_wire::InstanceId([0x11; tcr_peer_wire::INSTANCE_ID_BYTES]);
    let mut pairing = noise::initiator_with_secret(&secret, noise::PATTERN_PAIR, None, None)
        .expect("an XX initiator");
    let xx = pairing
        .write_message(&noise::pair_message_1_payload(&instance), &mut scratch)
        .expect("XX message 1");
    assert_eq!(
        xx,
        noise::XX_MESSAGE_1_LEN,
        "XX message 1 is one ephemeral plus the version byte and instance id it carries as \
         its payload; the listener refuses anything that is neither this nor the version-1 \
         length it answers with a refusal"
    );
    assert_eq!(
        noise::XX_MESSAGE_1_LEN_V1 + 1,
        noise::XX_MESSAGE_1_LEN,
        "the version byte is the whole difference between the two pairing lengths"
    );

    let mut returning = noise::initiator_with_secret(
        &secret,
        noise::PATTERN_RETURN,
        Some(&responder_public),
        None,
    )
    .expect("an IK initiator");
    let ik = returning
        .write_message(&[], &mut scratch)
        .expect("IK message 1");
    assert_eq!(
        ik,
        noise::IK_MESSAGE_1_LEN,
        "IK message 1 is an ephemeral, the initiator's encrypted static and two tags"
    );

    let (throwaway, _) = noise::generate_static().expect("an ephemeral-only keypair");
    let mut knocking = noise::initiator_with_secret(&throwaway, noise::PATTERN_KNOCK, None, None)
        .expect("an NN initiator");
    let nn = knocking
        .write_message(&[], &mut scratch)
        .expect("NN message 1");
    assert_eq!(
        nn,
        noise::KNOCK_MESSAGE_1_LEN,
        "NN message 1 is one ephemeral and nothing else: the knock itself is a transport \
         frame after the handshake"
    );

    let network_key = [0x44_u8; 32];
    let mut tagged = noise::initiator_with_secret(
        &throwaway,
        noise::PATTERN_KNOCK_PSK,
        None,
        Some(&network_key),
    )
    .expect("an NNpsk0 initiator");
    let nnpsk = tagged
        .write_message(&[], &mut scratch)
        .expect("NNpsk0 message 1");
    assert_eq!(
        nnpsk,
        noise::KNOCK_PSK_MESSAGE_1_LEN,
        "NNpsk0 mixes the psk before the first token, so message 1 already carries the \
         empty payload's AEAD tag"
    );

    // Pairwise distinct, and the dispatcher agrees with each. Without this, a
    // future pattern whose message 1 happened to be 40 bytes would be
    // dispatched as a first pairing and nothing would say so.
    let mut lengths = vec![nn, xx, nnpsk, ik];
    let before = lengths.len();
    lengths.sort_unstable();
    lengths.dedup();
    assert_eq!(
        lengths.len(),
        before,
        "two patterns share a message-1 length, so the listener cannot tell them apart: \
         NN={nn} XX={xx} NNpsk0={nnpsk} IK={ik}"
    );
    for (len, want) in [
        (nn, Handshake::Knock),
        (xx, Handshake::Pair),
        (nnpsk, Handshake::KnockPsk),
        (ik, Handshake::Return),
    ] {
        assert_eq!(
            Handshake::from_message_1_len(len),
            Some(want),
            "a {len}-byte first frame must dispatch to {want:?}"
        );
    }
    // And a length no pattern has dispatches to nothing, rather than to the
    // nearest match.
    assert_eq!(Handshake::from_message_1_len(33), None);
    assert_eq!(Handshake::from_message_1_len(0), None);

    // Every handshake message any pattern writes fits the scratch buffer the
    // drivers allocate. Measured against `snow` rather than reasoned about: the
    // buffer was shrunk from 65 kB to 256 bytes when the pre-authentication
    // socket slot stopped covering the handshake, and a buffer too small is a
    // handshake that fails at runtime with nothing to point at.
    let (a_secret, _a_public) = noise::generate_static().expect("a keypair");
    let (b_secret, b_public) = noise::generate_static().expect("a second keypair");
    let mut small = [0_u8; noise::HANDSHAKE_SCRATCH_BYTES];
    for (pattern, remote, psk) in [
        (noise::PATTERN_PAIR, None, None),
        (noise::PATTERN_RETURN, Some(&b_public), None),
        (noise::PATTERN_ENROL, Some(&b_public), Some(&network_key)),
        (noise::PATTERN_KNOCK, None, None),
        (noise::PATTERN_KNOCK_PSK, None, Some(&network_key)),
    ] {
        let mut initiator = noise::initiator_with_secret(&a_secret, pattern, remote, psk)
            .unwrap_or_else(|err| panic!("an initiator for {pattern}: {err:#}"));
        let mut responder = noise::responder_with_secret(
            &b_secret,
            pattern,
            match psk {
                Some(key) => std::slice::from_ref(key),
                None => &[],
            },
        )
        .unwrap_or_else(|err| panic!("a responder for {pattern}: {err:#}"));
        // Drive the whole pattern through buffers of exactly the size the
        // drivers use. `snow` errors rather than panicking on a short buffer,
        // so a too-small constant surfaces here as a refusal naming the
        // pattern.
        let mut out = [0_u8; noise::HANDSHAKE_SCRATCH_BYTES];
        let mut turn_initiator = true;
        while !initiator.is_handshake_finished() || !responder.is_handshake_finished() {
            let (writer, reader) = if turn_initiator {
                (&mut initiator, &mut responder)
            } else {
                (&mut responder, &mut initiator)
            };
            let payload: &[u8] = if turn_initiator && pattern == noise::PATTERN_PAIR {
                instance.as_bytes()
            } else {
                &[]
            };
            let len = writer
                .write_message(payload, &mut out)
                .unwrap_or_else(|err| panic!("{pattern} write into {} bytes: {err:#}", out.len()));
            reader
                .read_message(&out[..len], &mut small)
                .unwrap_or_else(|err| panic!("{pattern} read into {} bytes: {err:#}", small.len()));
            turn_initiator = !turn_initiator;
        }
    }

    // The allocation bound is the largest of the four, not a number from a
    // document: a 64-byte cap (which `abuse-resistance.md` names) would refuse
    // every returning peer and every headless enrolment.
    assert_eq!(
        noise::MAX_MESSAGE_1_BYTES,
        lengths.into_iter().max().unwrap_or(0),
        "the pre-authentication allocation bound must be the largest message 1 any pattern \
         this node speaks has, or a real peer is refused before it can authenticate"
    );
}

// ---------------------------------------------------------------------------
// The check that carries everything
// ---------------------------------------------------------------------------

/// **The check that carries everything.**
///
/// A peer that is not in the pin store is refused while the responder is
/// reading message 1: before message 2 is written, so nothing about this node
/// is disclosed to a stranger, and `tcr peer forget` takes effect on the very
/// next handshake with no restart.
///
/// Watch it fail by deleting the comparison in
/// [`teamclaude_rs::peer::noise::pin_check`] and watching a forgotten peer
/// complete a handshake and open a stream.
#[test]
fn an_unpinned_peer_is_refused_before_message_two() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let peers = dir.path().join("tcr-peers.json");
    let store =
        teamclaude_rs::peer::config::PeerStore::open(&peers).expect("an empty peers file opens");

    // A static key nobody pinned. Obviously fake, and 32 bytes, so the refusal
    // has to be the pin decision and not a length check.
    let stranger = [9_u8; 32];

    match noise::pin_check(&stranger, &store) {
        Err(PinRefusal::NotPinned { offered }) => {
            assert!(
                offered.starts_with("tcr-"),
                "the refusal must name the key that called, in display form, so the \
                 operator can tell which machine to go look at"
            );
        }
        Err(other) => panic!("wrong refusal: {other}"),
        Ok(peer) => panic!(
            "an unpinned key was admitted as {peer:?}: this is the failure mode the \
             whole design rests on, and it errors nowhere"
        ),
    }
}

/// A refused pin costs the caller ZERO bytes of answer.
///
/// This is the socket-level form of the test above, and it is the one that can
/// run on this branch: the authorization callback is injected, so the refusal
/// is a real `PinRefusal` built here rather than one that needs
/// `PeerId::display`. Watch it fail by moving the `authorize(...)` call in
/// `noise::finish_responder` to after `write_frame`, which is exactly the
/// mistake the design says nothing would catch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_pin_gets_zero_bytes_in_answer() {
    let (responder_secret, responder_public) = noise::generate_static().expect("a keypair");
    let (initiator_secret, _) = noise::generate_static().expect("a second keypair");
    let (server, mut client) = loopback_pair().await;
    let (mut server, written) = counted(server);

    let dialing = tokio::spawn(async move {
        noise::dial_handshake(
            &mut client,
            &initiator_secret,
            Handshake::Return,
            Some(&responder_public),
            None,
        )
        .await
    });

    let message_1 = noise::read_frame(&mut server)
        .await
        .expect("message 1 arrives");
    let mut scratch = vec![0_u8; 65535];
    let state = noise::read_message_1(
        &responder_secret,
        Handshake::Return,
        &[],
        &message_1,
        &mut scratch,
    )
    .expect("message 1 authenticates: the refusal under test is the PIN, not the crypto");

    let refused = noise::finish_responder(&mut server, state, Handshake::Return, |remote| {
        assert_eq!(
            remote.len(),
            32,
            "an IK responder learns a 32-byte static here"
        );
        Err(PinRefusal::NotPinned {
            offered: "tcr-notpinned".to_string(),
        })
    })
    .await;

    let error = refused.expect_err("a refused pin must not complete a handshake");
    assert!(
        error.downcast_ref::<PinRefusal>().is_some(),
        "the refusal must survive as a PinRefusal so a caller can log which key called: {error:#}"
    );
    assert_eq!(
        written.load(Ordering::SeqCst),
        0,
        "the responder wrote bytes after refusing the pin: a stranger learned this node's \
         static key, and `tcr peer forget` stopped meaning anything"
    );

    // Close this side so the initiator's read ends instead of waiting on an
    // answer that is never coming: the refusal closes the connection, which is
    // what the responder would do in production.
    drop(server);
    assert!(
        dialing.await.expect("the dial task").is_err(),
        "the initiator must not complete a handshake nobody answered"
    );
}

/// A remote static of the wrong width is a malformed handshake, not a peer, and
/// the refusal names the width it got.
#[test]
fn pin_check_refuses_a_static_of_the_wrong_width() {
    let rows = [pinned([4_u8; 32])];
    match noise::pin_check_rows(&[4_u8; 31], &rows) {
        Err(PinRefusal::Malformed { len }) => assert_eq!(len, 31),
        other => panic!("31 bytes is not a static key: {other:?}"),
    }
}

/// A pinned key is admitted as itself, and by identity rather than by position
/// in the file.
#[test]
fn pin_check_admits_a_pinned_key() {
    let rows = [pinned([1_u8; 32]), pinned([2_u8; 32])];
    let admitted = noise::pin_check_rows(&[2_u8; 32], &rows).expect("a pinned key is admitted");
    assert_eq!(admitted, PeerId([2_u8; 32]));
}

// ---------------------------------------------------------------------------
// Enrolment: the PSK is proved inside message 1
// ---------------------------------------------------------------------------

/// A wrong join secret fails while the responder is reading message 1, and the
/// responder writes **zero bytes** in answer.
///
/// Asserted on the socket's write count, not on a log line: putting the PSK in
/// message 1 is the entire reason this property exists, and a log line would
/// pass just as happily if the responder answered first and refused after.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_join_secret_gets_no_answer_at_all() {
    let (registrar_secret, registrar_public) = noise::generate_static().expect("a keypair");
    let (joiner_secret, _) = noise::generate_static().expect("a second keypair");
    let outstanding = [[7_u8; 32]];
    let wrong = [8_u8; 32];

    let (server, mut client) = loopback_pair().await;
    let (mut server, written) = counted(server);

    let dialing = tokio::spawn(async move {
        noise::dial_handshake(
            &mut client,
            &joiner_secret,
            Handshake::Enrol,
            Some(&registrar_public),
            Some(&wrong),
        )
        .await
    });

    let refused = noise::accept_handshake(
        &mut server,
        &registrar_secret,
        Handshake::Enrol,
        &outstanding,
        |remote| match <[u8; 32]>::try_from(remote) {
            Ok(key) => Ok(PeerId(key)),
            Err(_) => Err(PinRefusal::Malformed { len: remote.len() }),
        },
    )
    .await;

    assert!(
        refused.is_err(),
        "a wrong secret must not enrol: the psk is mixed in before message 1 is read"
    );
    assert_eq!(
        written.load(Ordering::SeqCst),
        0,
        "the registrar answered a wrong join secret: psk1's whole purpose is that it \
         cannot, because a wrong secret fails before message 2 exists"
    );
    drop(server);
    assert!(dialing.await.expect("the dial task").is_err());
}

/// The right secret enrols, over a real socket, and both sides end up holding
/// the same peer id: the joiner's is the registrar's static from the token, and
/// the registrar's is the one message 1 proved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_right_join_secret_enrols() {
    let (registrar_secret, registrar_public) = noise::generate_static().expect("a keypair");
    let (joiner_secret, joiner_public) = noise::generate_static().expect("a second keypair");
    let secret = [7_u8; 32];

    let (server, mut client) = loopback_pair().await;
    let dialing = tokio::spawn(async move {
        noise::dial_handshake(
            &mut client,
            &joiner_secret,
            Handshake::Enrol,
            Some(&registrar_public),
            Some(&secret),
        )
        .await
    });

    let mut server = server;
    let accepted = noise::accept_handshake(
        &mut server,
        &registrar_secret,
        Handshake::Enrol,
        &[secret],
        |remote| match <[u8; 32]>::try_from(remote) {
            Ok(key) => Ok(PeerId(key)),
            Err(_) => Err(PinRefusal::Malformed { len: remote.len() }),
        },
    )
    .await
    .expect("the right secret enrols even though nothing is pinned yet");

    let joined = dialing
        .await
        .expect("the dial task")
        .expect("the joiner completes");
    assert_eq!(
        accepted.peer,
        PeerId(joiner_public),
        "the registrar pins the key message 1 proved, not one a beacon claimed"
    );
    assert_eq!(joined.peer, PeerId(registrar_public));
    assert_eq!(
        accepted.code, joined.code,
        "both ends derive the same handshake hash, which is what makes the six digits a \
         channel binding"
    );
}

// ---------------------------------------------------------------------------
// The listener speaks Noise and nothing else
// ---------------------------------------------------------------------------

/// First bytes that are not a Noise message 1 close the socket with nothing
/// written: no banner, no version, no error frame.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_bytes_that_are_not_a_noise_message_one_get_nothing() {
    let (secret, _) = noise::generate_static().expect("a keypair");
    let (server, mut client) = loopback_pair().await;
    let (mut server, written) = counted(server);

    let probing = tokio::spawn(async move {
        // A plausible scan: an HTTP request line, framed so the read succeeds
        // and the DECISION is about the content rather than the length prefix.
        noise::write_frame(&mut client, b"GET / HTTP/1.1\r\n\r\n").await
    });

    // `Handshake::Return` here is arbitrary: the point is that the frame's
    // LENGTH does not match any real message 1, so `accept_handshake` refuses
    // it before `snow` sees a byte, whichever pattern it is checked against.
    let refused = noise::accept_handshake(&mut server, &secret, Handshake::Return, &[], |remote| {
        noise::pin_check_rows(remote, &[])
    })
    .await;
    assert!(refused.is_err(), "an HTTP request is not a Noise message 1");
    assert_eq!(
        written.load(Ordering::SeqCst),
        0,
        "the peer socket answered a scanner; it must close with nothing written"
    );
    probing
        .await
        .expect("the probe task")
        .expect("the probe wrote its bytes");
}

/// An `IK` dial to a responder holding a different static key gets no answer at
/// all: the responder cannot decrypt message 1, so it never reaches message 2.
/// This is the wrong-key case from the initiator's side, and it costs the
/// wrong responder zero written bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ik_dial_to_the_wrong_responder_key_gets_no_answer() {
    let (responder_secret, _) = noise::generate_static().expect("a keypair");
    let (_, other_public) = noise::generate_static().expect("a second keypair");
    let (initiator_secret, initiator_public) = noise::generate_static().expect("a third keypair");

    let (server, mut client) = loopback_pair().await;
    let (mut server, written) = counted(server);

    let dialing = tokio::spawn(async move {
        noise::dial_handshake(
            &mut client,
            &initiator_secret,
            Handshake::Return,
            // The pinned key of some OTHER machine.
            Some(&other_public),
            None,
        )
        .await
    });

    let rows = [pinned(initiator_public)];
    let refused = noise::accept_handshake(
        &mut server,
        &responder_secret,
        Handshake::Return,
        &[],
        |remote| noise::pin_check_rows(remote, &rows),
    )
    .await;
    assert!(
        refused.is_err(),
        "message 1 encrypted to another node's static key cannot be read here"
    );
    assert_eq!(
        written.load(Ordering::SeqCst),
        0,
        "a wrong-key dial drew an answer out of this node"
    );
    drop(server);
    assert!(dialing.await.expect("the dial task").is_err());
}

// ---------------------------------------------------------------------------
// The six digits, and the return visit
// ---------------------------------------------------------------------------

/// The interactive path's six digits come from the handshake hash, which binds
/// both statics AND both ephemerals: so a relayed handshake produces two
/// different codes and the operators see it. A hash of two pasted fingerprints
/// would not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_six_digit_code_binds_the_handshake_not_the_keys() {
    let (server_secret, server_public) = noise::generate_static().expect("a keypair");
    let (client_secret, client_public) = noise::generate_static().expect("a second keypair");

    let mut codes = Vec::new();
    for _ in 0..2 {
        let (mut server, mut client) = loopback_pair().await;
        let secret = client_secret;
        let dialing = tokio::spawn(async move { dial_first_pairing(&mut client, &secret).await });
        // `Handshake::Pair` never invokes `authorize`: see
        // `noise::accept_handshake`'s doc comment: so this closure is never
        // called; it exists only because the signature needs one.
        let accepted =
            noise::accept_handshake(&mut server, &server_secret, Handshake::Pair, &[], |_| {
                unreachable!("Handshake::Pair does not call authorize before message 2")
            })
            .await
            .expect("a first pairing needs nothing pinned");
        let dialed = dialing
            .await
            .expect("the dial task")
            .expect("the pairing completes");

        assert_eq!(
            accepted.code, dialed.code,
            "both screens must show the same six digits or the compare is theatre"
        );
        assert_eq!(
            accepted.code.len(),
            6,
            "six digits, fixed width, on both screens"
        );
        assert!(accepted.code.chars().all(|c| c.is_ascii_digit()));
        assert_eq!(accepted.peer, PeerId(client_public));
        assert_eq!(dialed.peer, PeerId(server_public));
        codes.push(accepted.code);
    }

    assert_ne!(
        codes[0], codes[1],
        "two XX handshakes between the SAME two static keys produced the same code, so the \
         code is a function of the keys alone and a man in the middle who relays both \
         halves would show the operators one matching number"
    );
}

// ---------------------------------------------------------------------------
// The six digits commit to a nonce from each side
// ---------------------------------------------------------------------------

/// Answer an `XX` dial as a responder that reveals a DIFFERENT nonce than the
/// one it committed to in message 2.
///
/// This is the man in the middle, reduced to the one move the commitment
/// forbids: pick the nonce after seeing the other side's, so the six digits
/// come out wherever you want them. `reveal` is what it sends in place of the
/// nonce it hashed.
async fn answer_pairing_revealing<S>(
    stream: &mut S,
    secret: &[u8; 32],
    committed: &[u8; noise::PAIR_NONCE_BYTES],
    reveal: &[u8; noise::PAIR_NONCE_BYTES],
) -> anyhow::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut state = noise::responder_with_secret(secret, noise::PATTERN_PAIR, &[])?;
    let mut scratch = vec![0_u8; noise::HANDSHAKE_SCRATCH_BYTES];

    let message_1 = noise::read_frame(stream).await?;
    state.read_message(&message_1, &mut scratch)?;

    let len = state.write_message(&noise::nonce_commitment(committed), &mut scratch)?;
    noise::write_frame(stream, &scratch[..len]).await?;

    let message_3 = noise::read_frame(stream).await?;
    state.read_message(&message_3, &mut scratch)?;

    let mut transport = noise::into_transport(state)?;
    noise::send_encrypted(stream, &mut transport, reveal).await
}

/// **A responder that reveals a nonce it did not commit to is refused, and
/// nothing is pinned.**
///
/// The attack the commitment closes: a machine in the middle answers the
/// dialling Mac, learns the digits it will show, and then picks its own
/// contribution so the OTHER handshake it is running comes out with the same
/// number. Choosing after the fact is exactly what this test does, and the
/// dialling side refuses before it has a code to show anybody.
///
/// Watch it fail by removing the `nonce_commitment(&revealed) != commitment`
/// check in `dial_handshake_with_payload`: the dial then completes and hands
/// back six digits derived from a nonce the responder chose last.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pairing_nonce_that_does_not_match_its_commitment_is_refused() {
    let (server_secret, _) = noise::generate_static().expect("a keypair");
    let (client_secret, _) = noise::generate_static().expect("a second keypair");
    let (mut server, mut client) = loopback_pair().await;

    let committed = [0x11_u8; noise::PAIR_NONCE_BYTES];
    let reveal = [0x22_u8; noise::PAIR_NONCE_BYTES];
    let answering = tokio::spawn(async move {
        answer_pairing_revealing(&mut server, &server_secret, &committed, &reveal).await
    });

    let refused = dial_first_pairing(&mut client, &client_secret)
        .await
        .expect_err("a reveal that misses its commitment is not a pairing");
    let text = format!("{refused:#}");
    assert!(
        text.contains("committed to"),
        "the refusal names what went wrong, so an operator is not left guessing: {text}"
    );
    let _ = answering.await.expect("the answering task");
}

/// **The digits move with the responder's nonce, on one handshake hash.**
///
/// The grind the old derivation allowed was: fix the digits by choosing keys,
/// because the code was a function of the handshake hash alone. Here the hash
/// is held CONSTANT and only the responder's nonce moves, and the code changes:
/// so an attacker who has ground its static key to some hash still cannot say
/// what the digits will be, because half the input arrives after it is
/// committed.
///
/// Watch it fail by dropping the nonces from `pairing_code`'s digest.
#[test]
fn the_pairing_code_changes_with_the_responder_nonce() {
    let hash = [0x5a_u8; 32];
    let initiator = [0x01_u8; noise::PAIR_NONCE_BYTES];

    let one = noise::pairing_code(&hash, &initiator, &[0x02_u8; noise::PAIR_NONCE_BYTES]);
    let two = noise::pairing_code(&hash, &initiator, &[0x03_u8; noise::PAIR_NONCE_BYTES]);
    assert_ne!(
        one, two,
        "one handshake hash and two responder nonces must not give one code, or the nonce \
         contributes nothing"
    );

    // And with the initiator's, for the same reason in the other direction.
    let three = noise::pairing_code(
        &hash,
        &[0x04_u8; noise::PAIR_NONCE_BYTES],
        &[0x02_u8; noise::PAIR_NONCE_BYTES],
    );
    assert_ne!(one, three, "the initiator's nonce is an input too");
    assert_eq!(one.len(), 6, "six digits, fixed width");
    assert!(one.chars().all(|c| c.is_ascii_digit()));

    // The same three inputs give the same answer on both screens, which is the
    // only reason the compare means anything.
    assert_eq!(
        one,
        noise::pairing_code(&hash, &initiator, &[0x02_u8; noise::PAIR_NONCE_BYTES])
    );
}

/// **A Mac on the older build is told which version it speaks, and nothing is
/// written back.**
///
/// Version 1 carried the bare instance id and derived the digits from the
/// handshake hash alone. It is recognized by length so the operator reads
/// "update the other Mac" rather than "those bytes are not a Noise message 1",
/// and it is refused rather than served under the weaker rule.
#[test]
fn a_version_one_pairing_payload_is_refused_by_name() {
    let instance = tcr_peer_wire::InstanceId([0x7E; tcr_peer_wire::INSTANCE_ID_BYTES]);

    assert_eq!(
        Handshake::from_message_1_len(noise::XX_MESSAGE_1_LEN_V1),
        Some(Handshake::Pair),
        "the older length still dispatches as a first pairing, or the refusal below is \
         never reached"
    );

    let (secret, _) = noise::generate_static().expect("a keypair");
    let mut state = noise::initiator_with_secret(&secret, noise::PATTERN_PAIR, None, None)
        .expect("an XX initiator");
    let mut scratch = vec![0_u8; noise::HANDSHAKE_SCRATCH_BYTES];
    let len = state
        .write_message(instance.as_bytes(), &mut scratch)
        .expect("a version-1 XX message 1");
    assert_eq!(len, noise::XX_MESSAGE_1_LEN_V1);

    let (responder_secret, _) = noise::generate_static().expect("a second keypair");
    let mut responder_scratch = vec![0_u8; noise::HANDSHAKE_SCRATCH_BYTES];
    let read = noise::read_message_1_matching(
        &responder_secret,
        Handshake::Pair,
        &[],
        &scratch[..len],
        &mut responder_scratch,
    )
    .expect("a version-1 message 1 still reads");

    let refused = read
        .instance_id()
        .expect_err("a version-1 payload is not served");
    let text = format!("{refused:#}");
    assert!(
        text.contains("version 1"),
        "the refusal names the version the other Mac speaks: {text}"
    );
    assert!(
        text.contains("nothing was written"),
        "and says the connection cost that Mac no answer: {text}"
    );

    // The control: the version this node writes is read back as the instance
    // id it carries, so the refusal above is about the version and not about
    // every payload.
    let mut state = noise::initiator_with_secret(&secret, noise::PATTERN_PAIR, None, None)
        .expect("an XX initiator");
    let len = state
        .write_message(&noise::pair_message_1_payload(&instance), &mut scratch)
        .expect("a version-2 XX message 1");
    let read = noise::read_message_1_matching(
        &responder_secret,
        Handshake::Pair,
        &[],
        &scratch[..len],
        &mut responder_scratch,
    )
    .expect("message 1 reads");
    assert_eq!(
        read.instance_id().expect("the instance id parses"),
        instance
    );
}

/// A return visit to a pinned key completes in ONE round trip: the initiator
/// writes message 1, the responder writes message 2, and the session is open.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_return_visit_completes_in_one_round_trip() {
    let (responder_secret, responder_public) = noise::generate_static().expect("a keypair");
    let (initiator_secret, initiator_public) = noise::generate_static().expect("a second keypair");

    let (server, mut client) = loopback_pair().await;
    let (mut server, written) = counted(server);
    let dialing = tokio::spawn(async move {
        noise::dial_handshake(
            &mut client,
            &initiator_secret,
            Handshake::Return,
            Some(&responder_public),
            None,
        )
        .await
    });

    let rows = [pinned(initiator_public)];
    let accepted = noise::accept_handshake(
        &mut server,
        &responder_secret,
        Handshake::Return,
        &[],
        |remote| noise::pin_check_rows(remote, &rows),
    )
    .await
    .expect("a pinned peer returns");
    let dialed = dialing
        .await
        .expect("the dial task")
        .expect("the return completes");

    assert_eq!(accepted.peer, PeerId(initiator_public));
    assert_eq!(dialed.peer, PeerId(responder_public));
    // Message 2 is 48 bytes (an ephemeral plus the empty payload's tag) and the
    // frame adds a two-byte prefix. One frame, one round trip: anything more
    // and IK's whole reason for being chosen over KK is gone.
    assert_eq!(
        written.load(Ordering::SeqCst),
        50,
        "the responder wrote something other than exactly one 48-byte message 2"
    );
}

// ---------------------------------------------------------------------------
// What a bare pin is told, and what a peer-controlled header buys
// ---------------------------------------------------------------------------

/// A bare pin is told booleans and addresses, and **nothing countable**.
///
/// Without the three control grants, pinning a peer would silently subscribe it
/// to per-window lendable amounts with account counts, this build's sha, its
/// boot id, and a list of this node's other peers.
///
/// Watch it fail by attaching `lendable` unconditionally.
#[test]
fn hello_for_a_bare_pin_is_minimal() {
    let mut facts = NodeFacts::minimal(PeerId([3_u8; 32]));
    facts.label = "studio-mac".to_string();
    facts.addrs = vec!["192.0.2.10:7777".to_string()];
    facts.hops_to_egress = Some(1);
    facts.build_sha = "deadbeef".to_string();
    facts.boot_id = 42;

    let bare = hello_for_peer(&facts, &ControlGrants::default());
    assert_eq!(
        bare.lendable, None,
        "a bare pin is told no countable figure"
    );
    assert_eq!(bare.hops_to_egress, None);
    assert_eq!(
        bare.briefs, None,
        "a bare pin is told nothing about this node's other peers"
    );
    assert_eq!(bare.build_sha, None);
    assert_eq!(bare.boot_id, None);
    assert_eq!(
        bare.label, "studio-mac",
        "a label and addresses are what a pin does buy"
    );
    assert_eq!(bare.addrs, vec!["192.0.2.10:7777".to_string()]);

    // And the grants are what add each block, so the test cannot pass because
    // the fields are always None.
    let granted = hello_for_peer(
        &facts,
        &ControlGrants {
            briefs: true,
            lendable: true,
            diag: true,
            drop: true,
        },
    );
    assert_eq!(granted.hops_to_egress, Some(1));
    assert_eq!(granted.build_sha.as_deref(), Some("deadbeef"));
    assert_eq!(granted.boot_id, Some(42));
    assert!(granted.lendable.is_some());
    assert!(granted.briefs.is_some());
}

/// A kind this build does not know is REFUSED, never ignored and never degraded
/// to one it does know: a newer peer on the same LAN speaks a kind this build
/// has no handler for, and a handler that treated it as a tunnel would be an
/// open relay.
#[test]
fn an_unknown_stream_kind_is_refused_not_ignored() {
    let row = pinned([5_u8; 32]);
    let refusal = peer_stream_gate_rows(&header(StreamKind::from(9999), None), Some(&row))
        .expect_err("an unknown kind is refused");
    assert_eq!(refusal, StreamRefusal::UnknownKind(9999));
}

/// A bare pin may say hello and nothing else. Every grant defaults to false, so
/// this is what an unconfigured mesh answers to everything.
#[test]
fn a_bare_pin_is_granted_control_and_nothing_else() {
    let row = pinned([5_u8; 32]);
    assert_eq!(
        peer_stream_gate_rows(&header(StreamKind::Control, None), Some(&row)),
        Ok(())
    );
    assert_eq!(
        peer_stream_gate_rows(&header(StreamKind::Serve, None), Some(&row)),
        Err(StreamRefusal::NotGranted(StreamKind::Serve)),
        "SERVE means this node reads that peer's requests in full; it needs allow.inspect"
    );
    let origin = Some(TunnelTarget::Origin {
        host: "api.anthropic.com".to_string(),
        port: 443,
    });
    assert_eq!(
        peer_stream_gate_rows(&header(StreamKind::Tunnel, origin), Some(&row)),
        Err(StreamRefusal::NotGranted(StreamKind::Tunnel)),
        "carrying bytes out needs allow.gateway"
    );
    assert_eq!(
        peer_stream_gate_rows(
            &header(
                StreamKind::Tunnel,
                Some(TunnelTarget::Peer {
                    node: PeerId([6_u8; 32])
                })
            ),
            Some(&row)
        ),
        Err(StreamRefusal::NotGranted(StreamKind::Tunnel)),
        "forwarding to another pinned peer needs allow.relay"
    );
}

/// A forgotten peer is refused on the very next frame, with no restart: the
/// row is gone, and an absent row is the same answer as "not authorized for
/// anything".
#[test]
fn a_forgotten_peer_is_refused_on_the_next_frame() {
    assert_eq!(
        peer_stream_gate_rows(&header(StreamKind::Control, None), None),
        Err(StreamRefusal::NotGranted(StreamKind::Control)),
        "a peer whose row was deleted keeps its session open until the next frame"
    );
}

/// A spent hop budget, a loop and a duplicate are each their own refusal, and
/// the hop checks read this node's own id rather than trusting `via`.
#[test]
fn a_spent_budget_a_loop_and_a_duplicate_are_each_refused() {
    let row = pinned([5_u8; 32]);
    let mut spent = header(StreamKind::Control, None);
    spent.hops_remaining = 0;
    assert_eq!(
        peer_stream_gate_rows(&spent, Some(&row)),
        Err(StreamRefusal::HopsExhausted)
    );

    let me = PeerId([1_u8; 32]);
    let mut looping = header(StreamKind::Control, None);
    looping.via = vec![PeerId([9_u8; 32]), me];
    let mut dedup = RequestDedup::new();
    assert_eq!(
        peer_stream_gate_hop(&me, &looping, &mut dedup, 0),
        Err(StreamRefusal::LoopDetected),
        "a frame that has already been through this node is a cycle, killed outright"
    );

    let fresh = header(StreamKind::Control, None);
    assert_eq!(peer_stream_gate_hop(&me, &fresh, &mut dedup, 0), Ok(()));
    assert_eq!(
        peer_stream_gate_hop(&me, &fresh, &mut dedup, 0),
        Err(StreamRefusal::Duplicate),
        "a diamond delivery served twice is two debits for one request"
    );
    assert_eq!(
        peer_stream_gate_hop(&me, &fresh, &mut dedup, 700_000),
        Ok(()),
        "the dedup cache is TTL-bounded; a cache a peer can grow forever is a memory bug \
         with a security label"
    );
}

// ---------------------------------------------------------------------------
// The pairing window: retired here, moved to per-instance-id approval
// ---------------------------------------------------------------------------
//
// This section used to hold two tests keyed on `listener::accept_peer_session`
// and the node-wide `PairingWindow` it gated `Handshake::Pair` with: a
// stranger refused outside the window, and the SAME dial answered inside it.
// That node-wide window was replaced with a window keyed to one
// accepted instance id at one
// address (`PeerState::accepted_window`, `listener::accept_pairing_or_return`)
// specifically because the old one answered ANY stranger who reached the port
// while any pairing was in progress. `accept_peer_session` was the pre-
// decision-10 seam that still exercised the retired gate; it is deleted from
// pre-existing seam that still exercised the retired gate; it is deleted from
// `listener.rs` as of this commit, and the two tests that measured it go with it rather than being
// converted to `noise::accept_handshake`, which has no window parameter to
// convert them to. The SAME invariant, under the CURRENT gate, is
// `tests/peer_pairing.rs::xx_from_an_unaccepted_instance_gets_zero_bytes` (the
// zero-bytes refusal) and its own positive control (the same dial answered
// after Accept); `the_six_digit_code_binds_the_handshake_not_the_keys` above
// keeps the six-digit-code assertion these two also made, through
// `noise::accept_handshake` directly.

/// The window is a deadline on the wall clock, not a flag: it opens, it is open,
/// and it is closed again one second past [`pair::PAIRING_WINDOW_SECS`] with
/// nobody having to remember to close it.
#[test]
fn a_pairing_window_opens_and_then_expires_on_its_own() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let state = dir.path().join("peer-state.json");

    assert_eq!(
        pair::pairing_window(&state, 1_000).expect("a missing state file reads as closed"),
        PairingWindow::Closed,
        "a node that never paired has no window, which is the same answer as a closed one"
    );

    let until = pair::open_pairing_window(&state, 1_000).expect("the window opens");
    assert_eq!(until, 1_000 + pair::PAIRING_WINDOW_SECS * 1_000);
    assert_eq!(
        pair::pairing_window(&state, 1_000).expect("the window reads"),
        PairingWindow::Open
    );
    assert_eq!(
        pair::pairing_window(&state, until - 1).expect("the window reads"),
        PairingWindow::Open
    );
    assert_eq!(
        pair::pairing_window(&state, until).expect("the window reads"),
        PairingWindow::Closed,
        "the deadline is absolute and exclusive: a clock that moved is not a second chance"
    );

    pair::open_pairing_window(&state, until).expect("it can be opened again");
    pair::close_pairing_window(&state).expect("and closed by hand");
    assert_eq!(
        pair::pairing_window(&state, until).expect("the window reads"),
        PairingWindow::Closed
    );
}

/// Opening the window rewrites `peer-state.json`, which belongs to
/// `crate::peer::state`: so every other key in it has to survive the write.
///
/// Watch it fail by dropping the `#[serde(flatten)] rest` field from
/// `crate::peer::state::PeerState`: the key a later phase wrote vanishes, and
/// an older build's two-minute deadline silently truncates a file a newer one
/// is keeping state in. (The deadline moved into `PeerState` as a
/// typed field (see `the_pairing_window_survives_a_save_and_a_load`), so the
/// flattened remainder that keeps this test green lives there now, not in a
/// local document in `pair.rs`.)
///
/// The seed file goes in through `state::save` and not `std::fs::write`,
/// because the two writers do not produce the same file: `save` publishes at
/// mode 0600 and a bare `fs::write` lands at this box's umask, which
/// `state::load`'s mode tripwire quarantines. That is the tripwire working, so
/// the fix is the writer this program actually uses: see
/// `a_state_file_a_wider_mode_is_quarantined_and_the_saved_one_is_0600`, which
/// measures both halves of that contract.
#[test]
fn opening_the_window_keeps_every_other_key_in_the_state_file() {
    use std::os::unix::fs::PermissionsExt as _;
    use teamclaude_rs::peer::state;

    let dir = tempfile::tempdir().expect("a temp dir");
    let state = dir.path().join("peer-state.json");
    let mut seeded = state::PeerState::default();
    seeded.rest.insert(
        "somethingPhaseFourAdds".to_string(),
        serde_json::json!({"keep": "me"}),
    );
    state::save(&state, &seeded).expect("a state file written by another phase");

    pair::open_pairing_window(&state, 5_000).expect("the window opens");

    let after: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&state).expect("the file reads"))
            .expect("it is still JSON");
    assert_eq!(after["version"], 1, "the format version must survive");
    assert_eq!(
        after["somethingPhaseFourAdds"]["keep"], "me",
        "a key this writer does not know about must survive it"
    );
    assert_eq!(
        after["pairingWindowUntilMs"],
        5_000 + pair::PAIRING_WINDOW_SECS * 1_000
    );
    let mode = std::fs::metadata(&state)
        .expect("the file exists")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode, 0o600,
        "a read/modify/write of this file republishes it at 0600, not {mode:o}: \
         otherwise the next `load` quarantines what this one just wrote"
    );
}

/// The state file's own mode contract, both halves: `state::save` creates it
/// 0600, and `state::load` refuses to trust any wider mode: it renames the
/// file aside and answers with an empty state rather than reading a file this
/// program did not write.
///
/// The file carries `accepted` (the whole authorization for a first pairing)
/// and `banned` (the block list), so a mode anyone on the box could have
/// written is not an input to either decision.
///
/// Watched red: with the `if mode != 0o600` branch in `crate::peer::state::load`
/// deleted, the widened file is read back and `loaded.banned` is 1, not 0, and
/// nothing is quarantined.
#[test]
fn a_state_file_a_wider_mode_is_quarantined_and_the_saved_one_is_0600() {
    use std::os::unix::fs::PermissionsExt as _;
    use teamclaude_rs::peer::state;

    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("peer-state.json");

    let mut value = state::PeerState::default();
    value.banned.push(state::Ban {
        addr: state::knock_address(&"127.0.0.1:9600".parse().expect("an addr")),
        key: None,
        since_ms: 1_000,
        reason: state::BanReason::Blocked,
    });
    state::save(&path, &value).expect("the state file writes");

    let mode = std::fs::metadata(&path)
        .expect("the file exists")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode, 0o600,
        "the one writer creates this file 0600, not {mode:o}"
    );
    assert_eq!(
        state::load(&path, 2_000)
            .expect("a 0600 file loads")
            .banned
            .len(),
        1,
        "a file at the mode the writer produces is trusted"
    );

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
        .expect("widen it by hand");
    let loaded = state::load(&path, 2_000).expect("a widened file still boots the node");
    assert!(
        loaded.banned.is_empty(),
        "a widened state file decides nothing: the node starts cold"
    );
    assert!(
        !path.exists(),
        "and the file is renamed aside rather than left in place to be read again"
    );
    let quarantined: Vec<_> = std::fs::read_dir(dir.path())
        .expect("the dir reads")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".corrupt-"))
        .collect();
    assert_eq!(
        quarantined.len(),
        1,
        "the evidence survives the cold start: {quarantined:?}"
    );
}

/// A state file that has to be created carries the version
/// `crate::peer::state::load` expects, or that loader renames it aside as
/// untrusted and the node starts cold for no reason at all.
#[test]
fn a_state_file_this_writer_creates_carries_the_format_version() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let state = dir.path().join("peer-state.json");
    pair::open_pairing_window(&state, 5_000).expect("the window opens");

    let written: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&state).expect("the file reads")).expect("JSON");
    assert_eq!(
        written["version"],
        serde_json::json!(teamclaude_rs::peer::state::FORMAT_VERSION),
        "a version-less file is one `state::load` quarantines"
    );

    let loaded = teamclaude_rs::peer::state::load(&state, 5_000)
        .expect("the state loader reads a file the pairing window created");
    assert!(
        loaded.leases.is_empty(),
        "and it reads as an empty state rather than a quarantined one"
    );
}

// ---------------------------------------------------------------------------
// The join token
// ---------------------------------------------------------------------------

/// The token round-trips through a paste buffer: it is the one string in this
/// design an operator moves between two machines by hand, so a field that
/// survives rendering and not parsing is a wrong-key dial.
#[test]
fn a_join_token_round_trips() {
    for addr in ["127.0.0.1:9600", "[::1]:9600"] {
        let token = pair::JoinToken::new(
            vec![addr.parse().expect("a test address")],
            PeerId([3_u8; 32]),
            [7_u8; 32],
        );
        let rendered = token.to_token();
        assert!(
            rendered.starts_with(pair::KEY_PREFIX),
            "an operator has to be able to see what they are pasting: {rendered}"
        );
        assert_eq!(
            pair::JoinToken::parse(&rendered).expect("the token parses"),
            token,
            "the address {addr} did not survive the round trip"
        );
        assert_eq!(
            pair::JoinToken::parse(&format!("  {rendered}\n")).expect("a pasted token parses"),
            token,
            "a paste carries whitespace"
        );
    }
}

/// An unknown version is refused rather than guessed at, because the guess is a
/// silent dial to the wrong key: and a token that carries no secret at all is
/// refused too.
/// Watch it fail by letting `JoinToken::parse` fall through to the body when
/// the prefix does not match: every other shape below is refused on some other
/// ground, so only a token that is well-formed in every field EXCEPT its
/// version measures the version check.
#[test]
fn a_join_token_with_an_unknown_version_is_refused() {
    let good = pair::JoinToken::new(
        vec!["127.0.0.1:9600".parse().expect("a test address")],
        PeerId([3_u8; 32]),
        [7_u8; 32],
    )
    .to_token();
    // v1, v2 and v3 are all read now, so the fixture is the next version
    // along: the check is that an unknown one is refused for BEING unknown.
    // (It used to say v3 here; moved when `JoinToken::parse` grew a v3 arm,
    // the change this test's own doc comment says to watch for.)
    let unknown = good.replace("tcr-join:v2:", "tcr-join:v4:");
    assert_ne!(
        good, unknown,
        "the fixture has to differ from the good token"
    );
    let error = pair::JoinToken::parse(&unknown)
        .expect_err("a v4 token must be refused rather than read as one of the three known ones");
    assert!(
        format!("{error:#}").contains("not a join key"),
        "the refusal has to be about the VERSION: a token whose only fault is its version \
         must not be refused by accident, further in, on a field that happened to move: \
         {error:#}"
    );

    for bad in [
        "tcr-join:v2:127.0.0.1:9600:AAAA:BBBB",
        "127.0.0.1:9600",
        "tcr-join:v1:127.0.0.1:9600",
        "tcr-join:v1:not-an-address:0000000000000000000000000000000000000000000000000000:0000000000000000000000000000000000000000000000000000",
    ] {
        let refused = pair::JoinToken::parse(bad);
        assert!(
            refused.is_err(),
            "{bad:?} parsed into a token, which means `tcr peer join` would dial something \
             nobody typed"
        );
    }
}

// ---------------------------------------------------------------------------
// The log a stranger can write
// ---------------------------------------------------------------------------

/// A refused connection writes ONE log line PER ADDRESS PER HOUR, and then the
/// listener is silent to that address: with the count of what was silenced
/// folded into the next line, so the bound never turns a flood into an absence
/// of evidence.
///
/// The grain changed: this used to be one line per MINUTE for the whole
/// listener, which was wrong in both directions, one flooder silenced every
/// other address's first refusal, and a slow scan from a `/24` still wrote 254
/// lines a minute. `abuse-resistance.md` asks for one line per address per
/// hour, and the second half of this test is what makes "per address" real.
///
/// Watch it fail by returning `Some(0)` unconditionally from
/// `RefusalLog::admit`: a port scan is thousands of connections, and a line each
/// is a disk-filling primitive handed to anyone who can reach the port.
#[test]
fn an_unauthenticated_flood_writes_one_log_line_and_then_counts() {
    let mut log = listener::RefusalLog::new();
    assert_eq!(
        log.admit("192.0.2.7", 0),
        Some(0),
        "the first refusal is always logged, and nothing was suppressed before it"
    );
    for step in 1..500 {
        assert_eq!(
            log.admit("192.0.2.7", step),
            None,
            "a second line inside the quiet period is the flood this bounds"
        );
    }
    assert_eq!(
        log.admit("192.0.2.7", listener::REFUSAL_LOG_QUIET_MS),
        Some(499),
        "the line that ends the quiet period carries the count of what it stood for"
    );
    assert_eq!(
        log.admit("192.0.2.7", listener::REFUSAL_LOG_QUIET_MS + 1),
        None,
        "and the quiet period starts again from the line that was emitted"
    );
}

/// **One flooding address must not silence another address's first line.**
///
/// This is the half a global throttle cannot hold, and it is the half that
/// matters operationally: the machine you need to hear about is the one that
/// knocked once while somebody else was hammering the port.
///
/// Watched red: with `RefusalLog` reverted to a single `last_ms`/`suppressed`
/// pair for the whole listener, the second assertion here returns `None` and
/// this fails with "the quiet period for one address must not silence
/// another".
#[test]
fn one_flooding_address_does_not_silence_another() {
    let mut log = listener::RefusalLog::new();
    for step in 0..500 {
        log.admit("192.0.2.7", step);
    }
    assert_eq!(
        log.admit("192.0.2.8", 500),
        Some(0),
        "the quiet period for one address must not silence another: a Mac that knocked once          during somebody else's flood is the line an operator needs"
    );
    // And the flooder is still silenced, so the bound did not simply stop
    // working.
    assert_eq!(
        log.admit("192.0.2.7", 501),
        None,
        "the flooding address is still inside its own quiet hour"
    );
}

/// The refusal log is bounded in ADDRESSES too, not only in time.
///
/// A map keyed on something a stranger chooses is a map a stranger can grow: a
/// `/16` of source addresses is 65 536 entries at one refusal each, all inside
/// one quiet hour, so time alone does not bound it.
///
/// Watched red by deleting the `while … >= REFUSAL_LOG_ADDRESSES` eviction
/// loop: the length comes back as 2 048.
#[test]
fn the_refusal_log_is_bounded_in_addresses() {
    let mut log = listener::RefusalLog::new();
    for n in 0..2_048_u32 {
        log.admit(&format!("10.0.{}.{}", n / 256, n % 256), i64::from(n));
    }
    assert!(
        log.len() <= listener::REFUSAL_LOG_ADDRESSES,
        "the refusal log grew to {} addresses, above the {} cap: a map keyed on a value a \
         stranger picks has to be bounded",
        log.len(),
        listener::REFUSAL_LOG_ADDRESSES
    );
    assert!(
        !log.is_empty(),
        "positive control: the log must still be remembering something, or the cap above \
         is being satisfied by a log that records nothing"
    );
}

// ---------------------------------------------------------------------------
// `tcr peer find --announce-name`, through the built binary
// ---------------------------------------------------------------------------

/// `--announce-name` is a stored preference, so it is written and the command
/// exits 0: on the `off` arm too, which is where it used to be dropped.
///
/// Runs the binary this build produced (`CARGO_BIN_EXE_tcr`), never the
/// installed one, and points the whole peer surface at a temp file with
/// `--peers`, so it reads no real config and touches no running proxy.
///
/// Watch it fail by moving the `args.announce_name` flip back inside the `on`
/// arm of `run_peer_find`: the exit code stays 0, the file is still written, and
/// `announceName` is silently whatever it already was.
#[test]
fn peer_find_off_stores_announce_name_and_exits_zero() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let peers = dir.path().join("tcr-peers.json");

    for (flag, expected) in [("off", false), ("on", true)] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_tcr"))
            .args([
                "peer",
                "find",
                "off",
                "--announce-name",
                flag,
                "--peers",
                peers.to_str().expect("a utf-8 temp path"),
            ])
            .output()
            .expect("the tcr this build produced runs");
        assert!(
            output.status.success(),
            "`tcr peer find off --announce-name {flag}` exited {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );

        let written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&peers).expect("the peers file was written"))
                .expect("it is JSON");
        assert_eq!(
            written["announceName"],
            serde_json::json!(expected),
            "--announce-name {flag} was accepted and then not stored"
        );
    }
}

// ---------------------------------------------------------------------------
// The pairing window is a typed field, and a moved clock closes it
// ---------------------------------------------------------------------------

/// The window survives the round trip through `peer-state.json`'s own loader
/// and writer, because it is a FIELD of that file's type now rather than a key
/// a second writer grafted on.
///
/// `state::save` used to erase it: `PeerState` had no such field, so a save
/// from anywhere else in the process dropped the deadline an operator had just
/// opened. Open, save, load, still open.
///
/// Watched red: with `#[serde(skip)]` on `PeerState::pairing_window_until_ms`,
/// the reload reads `Closed` and this test fails on the third assertion.
#[test]
fn the_pairing_window_survives_a_save_and_a_load() {
    use teamclaude_rs::peer::state::{self, PairingDeadline};

    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("peer-state.json");

    let mut fresh = state::load(&path, 1_000).expect("a missing state file loads as empty");
    assert_eq!(fresh.pairing_window(1_000), PairingDeadline::Closed);

    let until = fresh.open_pairing_window(1_000, pair::PAIRING_WINDOW_SECS);
    state::save(&path, &fresh).expect("the state file is written");

    let reloaded = state::load(&path, 1_000).expect("and read back");
    assert_eq!(
        reloaded.pairing_window(1_000),
        PairingDeadline::Open { until_ms: until },
        "a save must not erase the window an operator just opened"
    );
    assert_eq!(reloaded.pairing_window_until_ms, Some(until));

    // Closing is a save too, and it has to survive the same round trip.
    let mut closing = reloaded;
    assert!(closing.close_pairing_window());
    state::save(&path, &closing).expect("the close is written");
    assert_eq!(
        state::load(&path, 1_000)
            .expect("read back")
            .pairing_window(1_000),
        PairingDeadline::Closed
    );
}

/// **A clock that moved is not a second chance, in either direction.**
///
/// The window is the interval between the instant the operator opened it and
/// its deadline, so a clock set BACKWARD reads closed rather than extending a
/// stranger's chance to make this node disclose its static key: the
/// fail-closed rule the invite TTL already follows. Asserted through the same
/// `pair::pairing_window` the listener calls, on a real file.
///
/// Watched red: with the `now_ms >= opened_at_ms` half of
/// `PeerState::pairing_window` deleted (the earlier condition, a bare
/// `until > now`), the backward-clock assertion fails: the window reads Open
/// two whole minutes before it was opened.
#[test]
fn a_clock_that_moved_closes_the_pairing_window() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("peer-state.json");

    let opened_at = 1_000_000_i64;
    let until = pair::open_pairing_window(&path, opened_at).expect("the window opens");

    assert_eq!(
        pair::pairing_window(&path, opened_at).expect("the window reads"),
        PairingWindow::Open,
        "the instant it was opened is inside it"
    );
    assert_eq!(
        pair::pairing_window(&path, until - 1).expect("the window reads"),
        PairingWindow::Open
    );
    assert_eq!(
        pair::pairing_window(&path, until).expect("the window reads"),
        PairingWindow::Closed,
        "the deadline is absolute and exclusive"
    );
    assert_eq!(
        pair::pairing_window(&path, opened_at - 1).expect("the window reads"),
        PairingWindow::Closed,
        "a clock set backward must CLOSE the window, not extend it: outside the interval \
         the operator's own act defines, this node answers a first pairing with nothing"
    );
    assert_eq!(
        pair::pairing_window(&path, 0).expect("the window reads"),
        PairingWindow::Closed,
        "and a clock reset to the epoch is the same answer"
    );
}

// ---------------------------------------------------------------------------
// Enrolment writes a pinned row, and spends the invite
// ---------------------------------------------------------------------------

/// A peers file at mode 0600 with a listener address, through the one writer.
fn peers_file_with_listener(path: &std::path::Path, listen: std::net::SocketAddr) {
    let file = teamclaude_rs::peer::config::PeerFile {
        listen: Some(listen),
        ..Default::default()
    };
    teamclaude_rs::peer::config::save(path, &file).expect("the peers file is written");
}

/// `tcr peer join` used to report success while the registrar recorded
/// nothing: the joiner's `Control::Enroll` met a stream gate that wanted a
/// pinned row, and the row is the thing enrolment exists to create.
///
/// This drives BOTH halves in one process against two temp config dirs and one
/// loopback socket: the joiner runs `pair::join_as`, and the registrar's half
/// is the same five steps `src/peer/listener.rs` must run (read message 1,
/// `read_message_1_matching` for the psk that matched, `finish_responder`,
/// `accept_enrolment`, answer with `Hello`). A pinned row on both sides is the
/// gate.
///
/// Watched red twice while writing it: with `accept_enrolment` not called, the
/// registrar's file has no row and this fails on `registrar_row`; with the
/// registrar's `Hello` not sent, `join_as` returns the refusal it now produces
/// instead of `Ok` and the joiner pins nothing: which is exactly the reporting
/// bug, seen from the joiner's side.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_two_sided_enrolment_leaves_a_pinned_row_on_both_sides() {
    use tcr_peer_wire::Control;
    use teamclaude_rs::peer::config::{self as peer_config, PeerStore};
    use teamclaude_rs::peer::id::NodeKey;
    use teamclaude_rs::peer::listener::outstanding_secrets;

    let registrar_dir = tempfile::tempdir().expect("a temp dir");
    let joiner_dir = tempfile::tempdir().expect("a temp dir");
    let registrar_peers = registrar_dir.path().join("tcr-peers.json");
    let joiner_peers = joiner_dir.path().join("tcr-peers.json");

    let registrar_key = NodeKey::load_or_mint(registrar_dir.path()).expect("a registrar keypair");
    let joiner_key = NodeKey::load_or_mint(joiner_dir.path()).expect("a joiner keypair");

    // Bind first, so the token carries the port the kernel actually gave us.
    let listener = teamclaude_rs::peer::listener::bind("127.0.0.1:0".parse().expect("an addr"))
        .await
        .expect("a loopback peer socket");
    let addr = listener.local_addr().expect("the bound address");
    peers_file_with_listener(&registrar_peers, addr);
    peers_file_with_listener(&joiner_peers, "127.0.0.1:1".parse().expect("an addr"));

    let registrar_store = PeerStore::open(&registrar_peers).expect("the registrar's file opens");
    let (invite, token) = teamclaude_rs::peer::pair::mint_invite_as(
        &registrar_store,
        &registrar_key,
        "laptop-2",
        600,
        1,
        None,
    )
    .expect("an invite is minted");

    // The registrar's half: exactly what the listener has to do for an
    // enrolment, and the body the listener's own patch hands over.
    let registrar_secret = *registrar_key.secret_bytes();
    let registrar_id = registrar_key.id();
    let registrar_path = registrar_peers.clone();
    let accepting = tokio::spawn(async move {
        let (mut stream, _from) = listener.accept().await.expect("the joiner connects");
        let file = teamclaude_rs::peer::config::read_or_default(&registrar_path)
            .expect("the peers file reads");
        let psks = outstanding_secrets(&file);
        let message_1 = noise::read_frame(&mut stream).await.expect("message 1");
        let mut scratch = vec![0_u8; tcr_peer_wire::MAX_FRAME_BYTES];
        let read = noise::read_message_1_matching(
            &registrar_secret,
            Handshake::Enrol,
            &psks,
            &message_1,
            &mut scratch,
        )
        .expect("the invite's secret decrypts message 1");
        let matched = read
            .psk
            .expect("an IKpsk1 message 1 names the invite that matched it");
        let mut session =
            noise::finish_responder(&mut stream, read.state, Handshake::Enrol, |remote| {
                Ok(PeerId(
                    <[u8; 32]>::try_from(remote).expect("a 32-byte remote static"),
                ))
            })
            .await
            .expect("the enrolment handshake completes");

        // The stream header first, as on every other stream: the enrolment
        // exemption the listener needs is "no pinned row for the first frame",
        // never "no header".
        let header_frame = noise::recv_encrypted(&mut stream, &mut session.transport)
            .await
            .expect("the joiner's stream header");
        let header: StreamHeader =
            serde_json::from_slice(&header_frame).expect("the first frame is the header");
        assert_eq!(
            header.kind,
            StreamKind::Control,
            "an enrolment is a CONTROL stream and nothing else may be opened on it"
        );

        let frame = noise::recv_encrypted(&mut stream, &mut session.transport)
            .await
            .expect("the joiner's first control frame");
        let Control::Enroll(enroll) =
            serde_json::from_slice::<Control>(&frame).expect("it is a control message")
        else {
            panic!("an enrolment session may send exactly one Enroll and nothing else");
        };
        teamclaude_rs::peer::pair::accept_enrolment(
            &registrar_path,
            session.peer,
            &enroll,
            &matched,
            pair::now_ms(),
            std::net::SocketAddr::from(([127, 0, 0, 1], 9600)),
        )
        .expect("the joiner is pinned and the invite is spent");

        // The acknowledgement: the ordinary Hello a pinned peer may receive.
        let hello = hello_for_peer(
            &NodeFacts::minimal(registrar_id),
            &teamclaude_rs::peer::config::ControlGrants::default(),
        );
        let bytes = serde_json::to_vec(&Control::Hello(hello)).expect("the Hello serializes");
        noise::send_encrypted(&mut stream, &mut session.transport, &bytes)
            .await
            .expect("the Hello is sent");
    });

    let joiner_store = PeerStore::open(&joiner_peers).expect("the joiner's file opens");
    let pinned = teamclaude_rs::peer::pair::join_as(&joiner_store, &joiner_key, &token, "laptop-2")
        .await
        .expect("the joiner enrols");
    accepting.await.expect("the registrar's half");

    assert_eq!(
        pinned.peer, registrar_id,
        "the joiner pins the key the token named, as the handshake proved it"
    );

    let registrar_file =
        peer_config::read_or_default(&registrar_peers).expect("the registrar's file reads");
    let registrar_row = registrar_file
        .peers
        .iter()
        .find(|row| row.node == joiner_key.id())
        .expect("the registrar recorded the joiner: the hole this test exists for");
    assert_eq!(registrar_row.label, "laptop-2");
    assert!(
        !registrar_row.allow.inspect
            && !registrar_row.allow.gateway
            && !registrar_row.allow.relay
            && !registrar_row.allow.accept_move,
        "a bare pin can do nothing but say hello: {:?}",
        registrar_row.allow
    );

    let joiner_file = peer_config::read_or_default(&joiner_peers).expect("the joiner's file reads");
    assert!(
        joiner_file.peers.iter().any(|row| row.node == registrar_id),
        "and the joiner pins the registrar, so the row exists on both sides"
    );

    // Item 3: `--uses` is decremented, and a one-use invite is gone.
    assert!(
        registrar_file.pending_invites.is_empty(),
        "a one-use invite must be deleted when it is spent, not left on disk with \
         usesLeft: 0: a spent row is still a PSK in a file"
    );
    assert!(
        outstanding_secrets(&registrar_file).is_empty(),
        "and the registrar offers no secret for a second trial"
    );
    assert_eq!(invite.uses_left, 1, "the minted row started with one use");
}

/// A one-use invite refuses its second joiner, at the file level: the row is
/// gone, so the registrar has nothing to trial message 1 against and
/// `read_message_1` refuses before a byte is written.
///
/// Watched red: with the decrement in `accept_enrolment` removed (leaving the
/// row on disk), `outstanding_secrets` still returns one secret and the
/// `Handshake::Enrol` read below succeeds: the second joiner gets in on a key
/// the operator minted for one machine.
#[test]
fn a_one_use_invite_refuses_its_second_joiner() {
    use tcr_peer_wire::Enroll;
    use teamclaude_rs::peer::config::{self as peer_config, PeerStore};
    use teamclaude_rs::peer::id::NodeKey;
    use teamclaude_rs::peer::listener::outstanding_secrets;

    let dir = tempfile::tempdir().expect("a temp dir");
    let peers = dir.path().join("tcr-peers.json");
    peers_file_with_listener(&peers, "127.0.0.1:9600".parse().expect("an addr"));
    let node = NodeKey::load_or_mint(dir.path()).expect("a keypair");
    let store = PeerStore::open(&peers).expect("the file opens");

    let (invite, _token) =
        teamclaude_rs::peer::pair::mint_invite_as(&store, &node, "laptop-2", 600, 1, None)
            .expect("an invite is minted");
    let enroll = Enroll {
        invite_id: 0,
        label: "laptop-2".to_string(),
    };

    let first = teamclaude_rs::peer::pair::accept_enrolment(
        &peers,
        PeerId([4_u8; 32]),
        &enroll,
        &invite.secret,
        pair::now_ms(),
        std::net::SocketAddr::from(([127, 0, 0, 1], 9600)),
    )
    .expect("the first joiner proves the invite and is pinned");
    assert_eq!(first.node, PeerId([4_u8; 32]));

    let second = teamclaude_rs::peer::pair::accept_enrolment(
        &peers,
        PeerId([5_u8; 32]),
        &enroll,
        &invite.secret,
        pair::now_ms(),
        std::net::SocketAddr::from(([127, 0, 0, 1], 9600)),
    );
    let refusal = second.expect_err("a spent invite admits nobody");
    assert!(
        format!("{refusal:#}").contains("no outstanding invite"),
        "the refusal has to say the invite is gone: {refusal:#}"
    );

    let file = peer_config::read_or_default(&peers).expect("the file reads");
    assert_eq!(
        file.peers.len(),
        1,
        "the second joiner must leave no row: {:?}",
        file.peers
    );
    assert!(outstanding_secrets(&file).is_empty());
}

/// An enrolment carrying a hostile label is refused, and refusing it does NOT
/// spend the invite: a stranger who cannot pass the sanitizer must not be able
/// to burn an operator's one-use key.
#[test]
fn a_hostile_enrolment_label_is_refused_and_spends_nothing() {
    use tcr_peer_wire::Enroll;
    use teamclaude_rs::peer::config::{self as peer_config, PeerStore};
    use teamclaude_rs::peer::id::NodeKey;

    let dir = tempfile::tempdir().expect("a temp dir");
    let peers = dir.path().join("tcr-peers.json");
    peers_file_with_listener(&peers, "127.0.0.1:9600".parse().expect("an addr"));
    let node = NodeKey::load_or_mint(dir.path()).expect("a keypair");
    let store = PeerStore::open(&peers).expect("the file opens");
    let (invite, _token) =
        teamclaude_rs::peer::pair::mint_invite_as(&store, &node, "laptop-2", 600, 1, None)
            .expect("an invite is minted");

    for hostile in ["alice@example.com", "laptop\u{1b}[31m2", "laptop\n2"] {
        let refusal = teamclaude_rs::peer::pair::accept_enrolment(
            &peers,
            PeerId([6_u8; 32]),
            &Enroll {
                invite_id: 0,
                label: hostile.to_string(),
            },
            &invite.secret,
            pair::now_ms(),
            std::net::SocketAddr::from(([127, 0, 0, 1], 9600)),
        )
        .expect_err("a label this node will not render is refused");
        assert!(
            format!("{refusal:#}").contains("label"),
            "the refusal names the field: {refusal:#}"
        );
    }

    let file = peer_config::read_or_default(&peers).expect("the file reads");
    assert!(file.peers.is_empty(), "nothing is pinned on a refusal");
    assert_eq!(
        file.pending_invites.len(),
        1,
        "and the invite is untouched: a refused joiner must not be able to spend an \
         operator's one-use key"
    );
    assert_eq!(file.pending_invites[0].uses_left, 1);
}

// ---------------------------------------------------------------------------
// One writer for each file, and it is a safe one
// ---------------------------------------------------------------------------

/// The peers file is created at 0600 by the one writer, and a file with any
/// other mode is refused rather than trusted.
///
/// Watched red: with the `.permissions(0o600)` line removed from
/// `crate::config::write_atomic`, the first assertion reads 0644 (this box's
/// umask) and fails.
#[test]
fn the_peers_file_is_written_at_0600_and_a_wider_mode_is_refused() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().expect("a temp dir");
    let peers = dir.path().join("tcr-peers.json");
    peers_file_with_listener(&peers, "127.0.0.1:9600".parse().expect("an addr"));

    let mode = std::fs::metadata(&peers)
        .expect("the file exists")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode, 0o600,
        "the peers file holds join secrets; it is created 0600 or not at all"
    );

    std::fs::set_permissions(&peers, std::fs::Permissions::from_mode(0o644))
        .expect("widen it by hand");
    let refusal = teamclaude_rs::peer::config::read_or_default(&peers)
        .expect_err("a widened peers file is refused");
    assert!(
        format!("{refusal:#}").contains("0600"),
        "the refusal names the mode it wanted: {refusal:#}"
    );
}

/// A symlink planted where this node's private key goes does not capture it.
///
/// `load_or_mint` reaches its writer only when the path does not exist, and a
/// DANGLING symlink is exactly that: `exists()` follows the link and answers
/// false. An earlier writer then opened it with `create(true)`, which follows
/// the link and writes this node's static secret wherever the link points.
///
/// Watched red: with `create_new(true)` in `write_key_file` changed back to
/// `create(true).truncate(true)`, `load_or_mint` returns `Ok` and the 32-byte
/// secret appears at the link's target.
#[test]
fn a_planted_symlink_does_not_capture_the_node_key() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let captured = dir.path().join("captured.key");
    let key_path = teamclaude_rs::peer::id::private_key_path(dir.path());
    std::os::unix::fs::symlink(&captured, &key_path).expect("plant the link");

    // `NodeKey` has no `Debug` on purpose (it holds the secret), so the
    // refusal is read off the `Err` arm rather than through `expect_err`.
    let refusal = match teamclaude_rs::peer::id::NodeKey::load_or_mint(dir.path()) {
        Ok(_) => String::from("load_or_mint returned a key"),
        Err(err) => format!("{err:#}"),
    };
    assert!(
        !captured.exists(),
        "this node's private static key was written through a planted symlink: {refusal}"
    );
    assert!(
        refusal.contains("tcr-node.key"),
        "a key path that is already something else is a refusal that names it: {refusal}"
    );
}

// ---------------------------------------------------------------------------
// The join key can arrive on stdin, and never in a log line
// ---------------------------------------------------------------------------

/// The token reaches `tcr peer join` through stdin, so the secret never enters
/// this process's argv: where every other process on this Mac can read it out
/// of `ps`, and the shell records it in its history.
///
/// Watched red: with `token_from_reader` reading nothing and returning the
/// empty-input refusal, the first assertion fails.
#[test]
fn a_join_token_arrives_on_stdin_and_never_in_a_message() {
    let token = pair::JoinToken::new(
        vec!["127.0.0.1:9600".parse().expect("a test address")],
        PeerId([3_u8; 32]),
        [7_u8; 32],
    );
    let rendered = token.to_token();

    assert_eq!(
        pair::token_from_reader(std::io::Cursor::new(format!("{rendered}\n")))
            .expect("a token on stdin parses"),
        token,
        "the panel writes the key to stdin and this is what reads it"
    );

    let empty = pair::token_from_reader(std::io::Cursor::new(String::new()))
        .expect_err("empty stdin is a refusal, not a default");
    assert!(
        format!("{empty:#}").contains("--stdin"),
        "the refusal tells the operator what --stdin expects: {empty:#}"
    );

    // **No refusal on this path may echo the secret.** A pasted key that
    // failed to parse is still a live bearer secret, and the place operators
    // paste one keeps scrollback.
    let sentinel = "SENTINELSECRETVALUE0123456789ABCDEFGHJKMNPQRSTVWXYZ0";
    let malformed = format!(
        "{}{}:{}:{sentinel}",
        pair::TOKEN_PREFIX,
        "127.0.0.1:9600",
        PeerId([3_u8; 32]).to_wire()
    );
    let refusal = pair::JoinToken::parse(&malformed)
        .expect_err("a 51-character secret field is not 32 bytes");
    let text = format!("{refusal:#}");
    assert!(
        !text.contains(sentinel),
        "a refusal must not print the secret it was handed: {text}"
    );
    assert!(
        text.contains("secret field"),
        "it names the field instead: {text}"
    );
}

// ---------------------------------------------------------------------------
// The last two unlocked writes of the peers file
// ---------------------------------------------------------------------------

/// **`pair::forget` takes the file lock, so a revocation cannot be lost.**
///
/// It was a bare read-modify-write of the peers file: the one write in this
/// module that still was. Unlocked, a revocation is the write most easily lost:
/// a concurrent enrolment or `tcr peer allow` reads the file, and if it saves
/// after the forget, it saves a snapshot taken while the forgotten row was
/// still in it. The operator is told `peer forget: ok`, the row comes back, and
/// the peer's next handshake passes the pin check.
///
/// **Measured by the WAIT, not by a refusal, and that is a fact about
/// `FileLock` worth having in a test**: a held lockfile does not refuse an
/// acquirer, it delays one. `FileLock::acquire` spins, and breaks a lockfile
/// older than `LOCK_STALE_MS` because a `kill -9`'d holder must not wedge the
/// listener: so a live holder that keeps the lock for longer than that is
/// broken through, with a log line. What a caller that takes the lock therefore
/// looks like from outside is a call that cannot return before that bound while
/// somebody else holds the file, against a control call that returns in
/// milliseconds when nobody does.
///
/// Watched red: delete the `FileLock::acquire` line from `pair::forget` and the
/// locked half returns in 16.77 ms instead of waiting out the 2 s hold.
#[test]
fn forget_takes_the_file_lock() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let peers = dir.path().join("tcr-peers.json");
    let doomed = PeerId([64_u8; 32]);
    let other = PeerId([65_u8; 32]);
    let pinned = |node: PeerId, label: &str| PeerRow {
        node,
        label: label.to_string(),
        endpoints: Vec::new(),
        added_at: 1_767_225_600_000,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: Allow::default(),
        lend: Vec::new(),
    };
    let file = teamclaude_rs::peer::config::PeerFile {
        peers: vec![pinned(doomed, "studio-mac"), pinned(other, "attic-nuc")],
        ..Default::default()
    };
    teamclaude_rs::peer::config::save(&peers, &file).expect("write the peers file");
    let store = teamclaude_rs::peer::config::PeerStore::open(&peers).expect("open the store");

    // The control first, so the comparison is against a measurement rather
    // than against an assumption about how fast a file write is.
    let started = std::time::Instant::now();
    assert!(
        teamclaude_rs::peer::pair::forget(&store, &other).expect("the forget writes"),
        "the row was there, so this reports a removal"
    );
    let unlocked = started.elapsed();
    assert!(
        unlocked < std::time::Duration::from_millis(200),
        "with nobody holding the file a forget is immediate: {unlocked:?}"
    );

    // And now with somebody else's read-modify-write in flight.
    let held =
        teamclaude_rs::peer::config::FileLock::acquire(&peers).expect("take the file lock first");
    let started = std::time::Instant::now();
    let removed = teamclaude_rs::peer::pair::forget(&store, &doomed).expect("the forget writes");
    let waited = started.elapsed();
    drop(held);

    assert!(removed, "it still removes the row once it has the lock");
    assert!(
        waited >= std::time::Duration::from_millis(teamclaude_rs::peer::config::LOCK_STALE_MS),
        "a forget must wait for the holder rather than write over its read-modify-write: it \
         took {waited:?}, and the hold is only broken after {}ms",
        teamclaude_rs::peer::config::LOCK_STALE_MS
    );
    assert!(
        teamclaude_rs::peer::config::read_or_default(&peers)
            .expect("the file reads")
            .peers
            .is_empty(),
        "and both rows are gone"
    );
    assert!(
        !teamclaude_rs::peer::pair::forget(&store, &doomed)
            .expect("a second forget is not an error"),
        "a peer that is not pinned reports no removal and writes nothing"
    );
}

/// **A pin takes the same lock**, for the same reason and measured the same
/// way: `pair::confirm` and the joiner's half of an enrolment both wrote the
/// row through an unlocked read-modify-write, so a pin racing any other write
/// to this file could be the snapshot that loses it.
///
/// Safe to lock there because neither caller holds the lock already: the
/// registrar's side writes its row inside `accept_enrolment`'s own lock and
/// never through `pin_row`. The last third of this test is that fact: an
/// enrolment still completes, so the lock added below it is not a deadlock.
///
/// Watched red: delete the `FileLock::acquire` line from `pair::pin_row` and
/// the middle section returns in 16.71 ms instead of waiting out the 2 s hold.
#[test]
fn a_pin_takes_the_file_lock_and_an_enrolment_still_completes() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let peers = dir.path().join("tcr-peers.json");
    // With a listener address, because the enrolment at the end mints an
    // invite and an invite with no address to dial is refused.
    peers_file_with_listener(&peers, "127.0.0.1:9600".parse().expect("an addr"));
    let store = teamclaude_rs::peer::config::PeerStore::open(&peers).expect("open the store");
    let peer = PeerId([66_u8; 32]);

    let started = std::time::Instant::now();
    teamclaude_rs::peer::pair::confirm(&store, &peer, "123456", None).expect("the pin writes");
    let unlocked = started.elapsed();
    assert!(
        unlocked < std::time::Duration::from_millis(200),
        "with nobody holding the file a pin is immediate: {unlocked:?}"
    );
    let written = teamclaude_rs::peer::config::read_or_default(&peers).expect("the file reads");
    assert_eq!(
        written.peers.len(),
        1,
        "one row, for the peer just confirmed"
    );
    assert_eq!(written.peers[0].node, peer);

    let held =
        teamclaude_rs::peer::config::FileLock::acquire(&peers).expect("take the file lock first");
    let started = std::time::Instant::now();
    teamclaude_rs::peer::pair::confirm(&store, &peer, "654321", None).expect("the re-pin writes");
    let waited = started.elapsed();
    drop(held);
    assert!(
        waited >= std::time::Duration::from_millis(teamclaude_rs::peer::config::LOCK_STALE_MS),
        "a pin must wait for the holder rather than write over its read-modify-write: it took \
         {waited:?}"
    );

    // The registrar's own write is inside `accept_enrolment`'s lock and does
    // not go through `pin_row`, so it still completes: the deadlock this
    // could have introduced.
    let joiner = PeerId([67_u8; 32]);
    let node = teamclaude_rs::peer::id::NodeKey::load_or_mint(dir.path())
        .expect("a node key for the registrar");
    let (invite, _token) =
        teamclaude_rs::peer::pair::mint_invite_as(&store, &node, "attic-nuc", 600, 1, None)
            .expect("an invite is minted");
    let row = teamclaude_rs::peer::pair::accept_enrolment(
        &peers,
        joiner,
        &tcr_peer_wire::Enroll {
            invite_id: 0,
            label: "attic-nuc".to_string(),
        },
        &invite.secret,
        pair::now_ms(),
        std::net::SocketAddr::from(([127, 0, 0, 1], 9600)),
    )
    .expect("the registrar pins the joiner under its own lock");
    assert_eq!(row.node, joiner);
}

// ---------------------------------------------------------------------------
// The control loop polls once a second, so a frame may straddle a tick
// ---------------------------------------------------------------------------

/// A frame whose halves arrive 1.5 seconds apart still arrives, and the session
/// still decrypts the frame after it.
///
/// # The bug this is the control for
///
/// `serve_control` wraps its read in `timeout(HANDOFF_POLL_INTERVAL, ..)` so an
/// idle borrower still gets its bearer renewed. Built on `recv_encrypted`, that
/// read is `read_exact`, which is not cancel safe: the bytes it had already
/// taken off the socket die with the cancelled future at the tick. The next
/// read then starts in the MIDDLE of the frame and reads ciphertext as a length
/// prefix, so the Noise nonce desyncs and the session dies blaming the session
/// key. On a LAN no frame is ever slow enough to see it; one WAN retransmit, or
/// a laptop that slept between the prefix and the body, is enough.
///
/// The loop below is the production loop's shape on purpose, ticks and all.
/// The tick count is asserted rather than ignored, because a test where the
/// timeout never fired would pass against the torn reader too and measure
/// nothing.
///
/// Watch it fail: swap `frames.recv_encrypted(..)` for
/// `noise::recv_encrypted(&mut server, &mut accepted.transport)`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frame_split_across_the_poll_tick_survives() {
    use tokio::io::AsyncWriteExt;

    let (responder_secret, responder_public) = noise::generate_static().expect("a keypair");
    let (initiator_secret, initiator_public) = noise::generate_static().expect("a second keypair");

    let (mut server, mut client) = loopback_pair().await;
    let dialing = tokio::spawn(async move {
        let mut dialed = noise::dial_handshake(
            &mut client,
            &initiator_secret,
            Handshake::Return,
            Some(&responder_public),
            None,
        )
        .await
        .expect("the return completes");

        // Both frames encrypted up front, into memory, so the split below is a
        // decision about WHEN bytes reach the socket and nothing else.
        let mut wire: Vec<u8> = Vec::new();
        noise::send_encrypted(&mut wire, &mut dialed.transport, b"first")
            .await
            .expect("the first frame encrypts");
        let first_frame_len = wire.len();
        noise::send_encrypted(&mut wire, &mut dialed.transport, b"second")
            .await
            .expect("the second frame encrypts");

        // Three bytes: the whole length prefix and one byte of ciphertext, so
        // the reader is provably inside the frame when the tick lands.
        client.write_all(&wire[..3]).await.expect("the prefix goes");
        client.flush().await.expect("the prefix flushes");
        tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;
        client
            .write_all(&wire[3..first_frame_len])
            .await
            .expect("the rest of the first frame goes");
        client
            .write_all(&wire[first_frame_len..])
            .await
            .expect("the second frame goes");
        client.flush().await.expect("the body flushes");
        // Hold the socket open: a close would be a second way for the reader to
        // finish and this test is about the first one.
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    });

    let rows = [pinned(initiator_public)];
    let mut accepted = noise::accept_handshake(
        &mut server,
        &responder_secret,
        Handshake::Return,
        &[],
        |remote| noise::pin_check_rows(remote, &rows),
    )
    .await
    .expect("a pinned peer returns");

    let mut frames = noise::FrameReader::new();
    let mut read: Vec<Vec<u8>> = Vec::new();
    let mut ticks = 0_u32;
    while read.len() < 2 {
        match tokio::time::timeout(
            std::time::Duration::from_secs(1),
            frames.recv_encrypted(&mut server, &mut accepted.transport),
        )
        .await
        {
            Ok(frame) => read.push(frame.expect(
                "the frame straddled a poll tick and the session must survive it: a torn read \
                 shows up here as a decrypt failure",
            )),
            Err(_elapsed) => {
                ticks += 1;
                assert!(
                    ticks <= 6,
                    "six poll ticks and still no whole frame: the reader lost the bytes it had \
                     already taken and is now reading ciphertext as a length prefix"
                );
            }
        }
    }

    dialing.await.expect("the writing task");

    assert!(
        ticks >= 1,
        "the poll timeout never fired, so this run never exercised a cancelled read"
    );
    assert_eq!(
        read,
        vec![b"first".to_vec(), b"second".to_vec()],
        "both frames, in order, decrypted under a transport whose nonce never skipped"
    );
}

/// **This node's key file never exists half-written.**
///
/// It was created and then written, two steps, so a crash, a full disk or a
/// SIGKILL between them left a 0-byte `tcr-node.key`. That file is not
/// repairable by this program: `load_or_mint` refuses it ("holds 0 bytes,
/// expected exactly 32") on every boot afterwards, and minting refuses a path
/// that already exists, by design, so the Mac has no identity and no way to
/// mint one until somebody deletes a file nothing told them about.
///
/// The instrument is a reader spinning on the path while the key is minted,
/// which is the same thing a boot on the next start is: it records the length
/// of the FIRST version of the file it manages to see. Run over many mints so
/// the window, which is microseconds wide, is actually sampled.
///
/// Watched red by restoring the create-then-write writer: the reader sees a
/// 0-byte key file.
#[test]
fn a_node_key_file_is_never_visible_before_its_bytes() {
    const MINTS: usize = 200;
    let mut seen_empty = 0_usize;
    let mut sampled = 0_usize;

    for _ in 0..MINTS {
        let dir = tempfile::tempdir().expect("a temp dir");
        let key_path = teamclaude_rs::peer::id::private_key_path(dir.path());
        let start = std::sync::Arc::new(std::sync::Barrier::new(2));
        let watcher_start = start.clone();
        let watched = key_path.clone();
        // The first length this reader manages to see, and whether it saw the
        // file at all: a run that never sampled proves nothing, so it is
        // counted separately.
        let watcher = std::thread::spawn(move || {
            watcher_start.wait();
            for _ in 0..200_000 {
                if let Ok(meta) = std::fs::metadata(&watched) {
                    return Some(meta.len());
                }
            }
            None
        });

        start.wait();
        teamclaude_rs::peer::id::NodeKey::load_or_mint(dir.path()).expect("a keypair is minted");
        if let Some(first) = watcher.join().expect("the watching thread finishes") {
            sampled += 1;
            if first != 32 {
                seen_empty += 1;
            }
        }
        assert_eq!(
            std::fs::read(&key_path).expect("the key file reads").len(),
            32,
            "the key that was minted is 32 bytes whatever the reader saw"
        );
    }

    assert!(
        sampled > 0,
        "the reader never caught the file at all, so this run measured nothing"
    );
    assert_eq!(
        seen_empty, 0,
        "a reader caught the key file at {seen_empty} of {sampled} sampled mints holding \
         something other than the 32 bytes: that file is refused on every later boot and \
         nothing overwrites it"
    );
}

/// Nothing is left beside the key files when minting succeeds.
///
/// Staging the bytes elsewhere and publishing them is how the write above
/// became atomic, and the failure that fix brings with it is an orphan: a file
/// holding this node's private static key, under a name nobody looks at, with
/// no owner to clean it up.
#[test]
fn minting_leaves_only_the_two_key_files() {
    let dir = tempfile::tempdir().expect("a temp dir");
    teamclaude_rs::peer::id::NodeKey::load_or_mint(dir.path()).expect("a keypair is minted");

    let mut names: Vec<String> = std::fs::read_dir(dir.path())
        .expect("the config dir reads")
        .filter_map(|entry| Some(entry.ok()?.file_name().to_string_lossy().to_string()))
        .collect();
    names.sort();
    assert_eq!(
        names,
        ["tcr-node.key", "tcr-node.pub"],
        "a staged copy of the private key is still a copy of the private key"
    );
}

/// **A responder sizes its first read by the pattern it is running, not by the
/// Noise transport limit.**
///
/// `accept_handshake` read message 1 through the unbounded reader, which sizes
/// its buffer from the peer's own two-byte prefix: a stranger who writes
/// `0xFFFF` and then nothing made this node allocate 65 535 bytes for a frame
/// it was about to refuse. It was latent because the production listener reads
/// its own first frame through the bounded reader; this function is the one a
/// second caller would reach for.
///
/// The instrument is the REFUSAL's own wording, which says where it happened:
/// before the allocation, on the length prefix, rather than after a frame was
/// read and measured.
///
/// Watched red by putting `read_frame(stream)` back: the oversized prefix is
/// accepted, 65 535 bytes are allocated, and the refusal that follows is the
/// pattern check, which says "are not a Noise_XX… message 1".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_responders_first_read_is_bounded_by_its_own_pattern() {
    use tokio::io::AsyncWriteExt as _;
    let refusal_for = |promised: usize, handshake: Handshake| async move {
        let (mut responder, mut caller) = tokio::io::duplex(8192);
        let mut framed = u16::try_from(promised)
            .expect("the prefix fits")
            .to_be_bytes()
            .to_vec();
        // The prefix PROMISES more than the body carries, which is the whole
        // attack: two bytes and then nothing.
        framed.resize(2 + 8, 0);
        caller.write_all(&framed).await.expect("write the prefix");
        caller.flush().await.expect("flush");
        drop(caller);

        let err =
            noise::accept_handshake(&mut responder, &[9_u8; 32], handshake, &[], |_offered| {
                Err(PinRefusal::NotPinned {
                    offered: "no pinned key in this test".to_string(),
                })
            })
            .await
            .expect_err("a first frame that promises more than the pattern has is refused");
        format!("{err:#}")
    };

    let refusal = refusal_for(65_535, Handshake::Pair).await;
    assert!(
        refusal.contains("refused BEFORE allocating"),
        "the refusal has to happen on the length prefix, which is the only place it costs \
         nothing: {refusal}"
    );

    // An `IK` message 1 is 96 bytes and an `XX` one is not: a responder running
    // `XX` will not size a buffer for the longest pattern this node speaks
    // either.
    let refusal = refusal_for(noise::IK_MESSAGE_1_LEN, Handshake::Pair).await;
    assert!(
        refusal.contains("refused BEFORE allocating"),
        "the bound is this pattern's own message 1, not the largest any pattern has: \
         {refusal}"
    );

    // The positive control: the length this pattern DOES accept gets past the
    // bound and is refused for what it is, not for how long it is.
    let refusal = refusal_for(noise::IK_MESSAGE_1_LEN, Handshake::Return).await;
    assert!(
        !refusal.contains("refused BEFORE allocating"),
        "a message 1 of exactly this pattern's length is read, so the refusal above is the \
         bound and not a reader that refuses everything: {refusal}"
    );
}
