//! The transport: `snow`, three patterns, and the four lines that are the whole
//! of authorization.
//!
//! **This is the only file in the tree that may name `snow::` for a
//! HANDSHAKE.** Everything else takes a [`HandshakeState`] or a
//! [`TransportState`] from here, so there is exactly one place to read when
//! asking "what does this node accept, and when does it decide?".
//!
//! One exemption, granted deliberately rather than discovered:
//! [`crate::peer::id`] imports `snow::params::DHChoice` and
//! `snow::resolvers::{CryptoResolver, DefaultResolver}` to generate this node's
//! static keypair. Those two `use` lines are the whole of it, the DH object
//! itself arrives as the `Box<dyn snow::types::Dh>` that `resolve_dh` returns,
//! so even that trait is never named there. That is the raw DH primitive,
//! generate, set, pubkey, with no pattern, no `HandshakeState` and no session,
//! and it is there because the root `Cargo.toml` carries no independent x25519
//! crate. The rule this file states is about who decides what a peer is allowed
//! to do, and key generation decides nothing; a second CSPRNG or a second curve
//! implementation would be the real cost.
//!
//! # Which pattern runs when, and the rule that decides it
//!
//! **A first pairing has no prior key, so it is `XX`. `IK` is for a return
//! visit to a key already pinned.** The discovery beacon carries no node id and
//! no public key (`crate::peer::discovery`), so a pairing that begins from a
//! discovered row cannot know who it is talking to until the handshake says so,
//! and the six digits are what bind that answer to the machine in front of
//! the operator.
//!
//! - [`PATTERN_PAIR`], `XX`, for a first pairing: neither side knows the
//!   other's key, both learn it inside the handshake, and the operator compares
//!   six digits on two screens. This is the default path and the one the
//!   simple surface's Trust button runs.
//! - [`PATTERN_RETURN`], `IK`, for every returning connection to a PINNED key.
//!   Not `KK`: a `KK` responder must be handed the remote public key BEFORE it
//!   can process message 1, and a multi-peer listener does not know who is
//!   calling yet, so `KK` forces either a cleartext peer hint on the wire or
//!   trial decryption against every pinned key. `IK` avoids both, the
//!   initiator's static is encrypted to the responder's static, so a passive
//!   LAN observer learns no identity, and the responder reads it with
//!   `get_remote_static()` after message 1, which is exactly where the pin
//!   check belongs. Two messages, one LAN round trip.
//! - [`PATTERN_ENROL`], `IKpsk1`, for the ONE case that does begin with a key:
//!   a pasted join token. The token itself carries the registrar's static (the
//!   `IK` half) and a 32-byte secret proved as the PSK mixed into message 1
//!   (the `psk1` half), so **a wrong secret fails while the responder is still
//!   reading message 1, before it writes message 2 and before it discloses
//!   anything.** 256 bits of pairing entropy, one paste, no second screen,
//!   which is what lets a machine with no display enrol at all. The key comes
//!   from the operator's own paste and never from a beacon, which is why this
//!   is not an exception to the rule above but the other side of it.
//!
//! SHA256 rather than BLAKE2s keeps the suite inside the `use-sha2` feature,
//! which this lock already resolved.
//!
//! # Enrolment is accepted BEFORE a pinned row exists, and that is the order
//!
//! The one ordering question in this design, decided here so nobody
//! re-derives it: **an [`Handshake::Enrol`] session may send exactly one
//! `Control::Enroll` and be pinned by it, with no pinned row required
//! beforehand**, and nothing else. Stated as the rule the listener enforces:
//!
//! 1. the pattern is `IKpsk1` and its psk matched an OUTSTANDING, unexpired
//!    invite, proved inside message 1 before this node wrote a byte;
//! 2. therefore the stream gate's "is this peer pinned" check does not apply
//!    to the first frame of such a session, because a joiner is by definition
//!    not pinned yet, that is what enrolling means;
//! 3. the only frame this exemption admits is `Control::Enroll`. It is
//!    answered by [`crate::peer::pair::accept_enrolment`], which writes the
//!    pinned row and spends the invite in one write, and every later frame on
//!    that stream meets the ordinary gate against the row it just created;
//! 4. the registrar then sends its `Control::Hello`, the ordinary message a
//!    pinned peer may always receive, and **that Hello is the acknowledgement
//!    the joiner waits for.** [`crate::peer::pair::join_as`] pins the
//!    registrar and reports success only after it arrives, because a completed
//!    handshake proves the KEY was good and nothing about what the registrar
//!    did with the frame after it. A closed stream, a different message or
//!    silence is a refusal, and it pins nothing on either side.
//!
//! Without step 2 the joiner's `Enroll` is refused for lack of the very row it
//! exists to create, `tcr peer join` prints `ok` because its own dial and
//! handshake both succeeded, and the registrar records nothing.
//! That was the hole. The alternative order (require the pin first) cannot work: a pin is
//! what enrolment produces, so requiring one makes the headless path
//! unreachable.
//!
//! This is not a hole in the pin check. The PSK is a 32-byte secret the
//! operator carried by hand, it is proved before message 2, the invite is
//! single-use by default with a short TTL, and the row it writes carries every
//! grant at its default, a bare pin that can do nothing but say hello.
//!
//! # The registrar's honest cost of `psk1`
//!
//! A PSK is mixed into the handshake state before message 1 is read, so a
//! registrar holding N outstanding invites must trial-decrypt message 1 over
//! all N, the same shape of work this design refuses `KK` for. It is bounded
//! where `KK`'s is not: N counts OUTSTANDING INVITES, not pinned peers, the
//! default is one use with a short TTL, the row is deleted on use, and
//! [`crate::peer::pair::MAX_OUTSTANDING_INVITES`] caps it. Stated rather than
//! hidden, because an unbounded invite table would make the registrar the very
//! thing this file refuses.
//!
//! # Where the bytes are, and why the two drivers are in this file
//!
//! `snow` is a frame API over byte slices: it never touches a socket. So the
//! two functions that move a handshake over a stream, [`accept_handshake`] and
//! [`dial_handshake`], live here beside the patterns rather than in
//! [`crate::peer::listener`]. That placement is the property the whole design
//! rests on: **the responder's only write happens after the authorization
//! callback has answered**, and keeping the read, the check and the write in
//! one function is what makes that readable in one screen instead of inferable
//! across two files.
//!
//! Both drivers are generic over `AsyncRead + AsyncWrite + Unpin` for one
//! reason beyond taste: a test can hand them a writer that counts bytes, which
//! is how "zero bytes written after a refusal" is asserted on the socket rather
//! than on a log line.

use anyhow::{anyhow, bail, Context, Result};
use snow::resolvers::{CryptoResolver, DefaultResolver};
use snow::{Builder, HandshakeState, TransportState};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use tcr_peer_wire::{decode_frame, encode_frame, PeerId, MAX_FRAME_BYTES};

use crate::peer::config::{PeerRow, PeerStore};
use crate::peer::id::NodeKey;

/// `XX`, a FIRST pairing, where neither side knows the other's key. The
/// default path: a discovered row carries no key, so there is nothing else to
/// run.
pub const PATTERN_PAIR: &str = "Noise_XX_25519_ChaChaPoly_SHA256";

/// `IK`, a return visit to a key this node has already pinned.
pub const PATTERN_RETURN: &str = "Noise_IK_25519_ChaChaPoly_SHA256";

/// `IKpsk1`, enrolment under a pasted join token, which carries the key. The
/// token's secret is the PSK.
pub const PATTERN_ENROL: &str = "Noise_IKpsk1_25519_ChaChaPoly_SHA256";

/// `NN`, the KNOCK, phase one of pairing.
///
/// **Ephemeral keys on both sides, no static key anywhere**, which is the whole
/// point: a knock is a stranger asking to be shown to the operator, and it must
/// disclose neither end's identity to do that. The responder learns an address
/// and a claim; the requester learns one ack byte.
///
/// `NN` gives no authentication and that is correct here. There is nothing to
/// authenticate yet, that is what "first contact" means, and the thing it
/// buys over a bare TCP write is confidentiality of the claimed name against a
/// passive LAN observer, plus one uniform framing for every inbound connection
/// so the listener has exactly one kind of first frame to parse.
pub const PATTERN_KNOCK: &str = "Noise_NN_25519_ChaChaPoly_SHA256";

/// `NNpsk0`, the knock, when a network key is set.
///
/// The network key is mixed in at position 0, **before message 1**, so a knock
/// from a Mac without the key fails while the responder is still reading its
/// first frame: zero bytes written, no queue entry, no UI row. That is the
/// sentence `abuse-resistance.md` promises, "a Mac without the key sees
/// nothing and can send nothing that reaches the UI".
pub const PATTERN_KNOCK_PSK: &str = "Noise_NNpsk0_25519_ChaChaPoly_SHA256";

/// The PSK position `IKpsk1` mixes the join secret into: before message 1, so a
/// wrong secret fails while the responder is still reading it.
const PSK_POSITION_ONE: u8 = 1;

/// The PSK position `NNpsk0` mixes the network key into: position zero, which
/// is before the first token of message 1 rather than after it.
const PSK_POSITION_ZERO: u8 = 0;

/// A static X25519 key, a Noise message 1 ephemeral, and a PSK are all 32
/// bytes. Named once so a `32` in a bounds check is readable.
pub const KEY_BYTES: usize = 32;

/// The exact length of an `XX` message 1: one ephemeral, plus the 8-byte
/// instance id this design carries as its payload.
///
/// # Why `XX` message 1 carries a payload at all
///
/// An `XX` message 1 is answered "only from an instance id
/// that is ACCEPTED and inside its 120 s window". A responder cannot check an
/// id it was never told, and `XX` message 1 is the first thing it sees, so the
/// id rides in the payload, which in `XX` is appended in the CLEAR (no key has
/// been established when `-> e` is written). That is fine and deliberate: an
/// [`tcr_peer_wire::InstanceId`] is ephemeral, public, and already on the
/// mDNS wire in the clear.
///
/// It also disambiguates the two 32-byte patterns. `NN` message 1 with an empty
/// payload is 32 bytes and so was `XX`'s, and a listener that has to decide
/// which pattern arrived from the length alone could not tell a knock from a
/// first pairing. With the id attached the four lengths are distinct:
/// [`KNOCK_MESSAGE_1_LEN`] 32, this 40, [`KNOCK_PSK_MESSAGE_1_LEN`] 48,
/// [`IK_MESSAGE_1_LEN`] 96.
///
/// Measured against `snow` by `noise_message_one_lengths_are_what_the_gate_pins`
/// rather than taken from the spec, because
/// [`crate::peer::listener`]'s "these first bytes are not a Noise message 1"
/// check is only as good as these numbers.
pub const XX_MESSAGE_1_LEN: usize = 32 + PAIR_MESSAGE_1_PAYLOAD_LEN;

/// What version 1 of this payload was: the bare instance id, with no version
/// byte and no short-authentication-string nonce exchange behind it.
///
/// Recognized, and only so that a Mac on the older build is told what is wrong
/// instead of reading "those bytes are not a Noise message 1". See
/// [`PAIR_PAYLOAD_VERSION`] for what changed and why an old peer cannot be
/// served.
pub const XX_MESSAGE_1_LEN_V1: usize = 32 + tcr_peer_wire::INSTANCE_ID_BYTES;

/// The version this node writes in an `XX` message 1's payload, and the only
/// one it answers.
///
/// Version 1 carried the instance id alone and derived the six digits from the
/// handshake hash, which no nonce contributed to. That is why the version moved:
/// see [`six_digit_code`] for what an unbound code lets a man in the middle do,
/// and [`pairing_code`] for the exchange that replaced it. A version byte rather
/// than a length test, because the next change to this payload should be told
/// apart by what it SAYS it is.
pub const PAIR_PAYLOAD_VERSION: u8 = 2;

/// Version byte plus the instance id: an `XX` message 1's whole payload.
pub const PAIR_MESSAGE_1_PAYLOAD_LEN: usize = 1 + tcr_peer_wire::INSTANCE_ID_BYTES;

/// A nonce, a commitment to one, and the handshake hash are all 32 bytes.
pub const PAIR_NONCE_BYTES: usize = 32;

/// The message-1 payload a first pairing writes: the version this node speaks,
/// then the instance id the responder checks its accept window against.
///
/// One builder, so the dialling side and [`Message1::instance_id`] cannot
/// disagree about where the version byte sits.
pub fn pair_message_1_payload(
    instance: &tcr_peer_wire::InstanceId,
) -> [u8; PAIR_MESSAGE_1_PAYLOAD_LEN] {
    let mut out = [0_u8; PAIR_MESSAGE_1_PAYLOAD_LEN];
    out[0] = PAIR_PAYLOAD_VERSION;
    out[1..].copy_from_slice(instance.as_bytes());
    out
}

/// The exact length of an `IK` or `IKpsk1` message 1: an ephemeral, the
/// initiator's encrypted static with its tag, and the empty payload's tag.
pub const IK_MESSAGE_1_LEN: usize = 96;

/// The exact length of an `NN` message 1: one ephemeral, no payload. The knock
/// itself is a transport frame AFTER the handshake, so message 1 carries
/// nothing.
pub const KNOCK_MESSAGE_1_LEN: usize = 32;

/// The exact length of an `NNpsk0` message 1: one ephemeral plus the empty
/// payload's AEAD tag, which `psk0` makes present, the psk is mixed before the
/// first token, so message 1 is already encrypted.
pub const KNOCK_PSK_MESSAGE_1_LEN: usize = 48;

/// The longest first frame this listener will ALLOCATE for.
///
/// `abuse-resistance.md` says "message 1 capped at 64 bytes; no allocation
/// before the length check", and the cap is right while the number is wrong:
/// [`IK_MESSAGE_1_LEN`] is 96, so a 64-byte cap would refuse every returning
/// peer and every headless enrolment, the two paths that carry all the real
/// traffic. Measured, not inferred: `noise_message_one_lengths_are_what_the_\
/// gate_pins` asserts 96 against `snow` itself.
///
/// So the cap is the largest message 1 any pattern this node speaks has, which
/// is what the doc's intent reduces to: a stranger cannot make this node
/// allocate a 65 kB buffer before it has authenticated anything. Reported to
/// the lead as a corrected figure rather than silently widened.
pub const MAX_MESSAGE_1_BYTES: usize = IK_MESSAGE_1_LEN;

/// How big a scratch buffer a HANDSHAKE needs, in bytes.
///
/// # Why this is not [`MAX_FRAME_BYTES`], which it used to be
///
/// Every handshake driver here used to allocate a 65 kB scratch buffer, which
/// is the Noise TRANSPORT limit and has nothing to do with a handshake: the
/// largest handshake message any of the five patterns writes is
/// [`IK_MESSAGE_1_LEN`] at 96 bytes. That mattered once the pre-authentication
/// socket slot was narrowed to the silent window
/// ([`crate::peer::listener::MAX_UNAUTHENTICATED_SOCKETS`]), with the slot
/// released at message 1, the number of handshakes in flight is bounded by a
/// timeout rather than by a counter, and 65 kB each would be 65 MB for a
/// thousand stalled ones. At 256 bytes it is a quarter of a megabyte.
///
/// 256 and not 96: `snow` is handed this as the output buffer and a margin
/// costs nothing here. `noise_message_one_lengths_are_what_the_gate_pins`
/// (`tests/peer_noise.rs`) asserts every pattern's messages fit, against
/// `snow` itself rather than against this comment.
pub const HANDSHAKE_SCRATCH_BYTES: usize = 256;

/// Which of the three patterns is running, parsed once from a pattern string so
/// no other function in this module compares one.
///
/// The three arms are not interchangeable and the differences are the design,
/// not a detail: [`Self::Pair`] has three messages and learns the initiator's
/// static in the LAST one, so a pin check before message 2 is not merely
/// skipped there but impossible, which is why `XX` is the pattern whose
/// authorization is the six digits and not the pin store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handshake {
    /// `XX`, a first pairing. Three messages; authorization is the six digits.
    Pair,
    /// `IK`, a return visit to a pinned key. Two messages; authorization is the
    /// pin check between them.
    Return,
    /// `IKpsk1`, enrolment under a pasted token. Two messages; authorization is
    /// the PSK, proved inside message 1.
    Enrol,
    /// `NN`, a KNOCK. Two messages, no static key on either side, **no
    /// authorization at all**, the responder queues a claim for the operator
    /// and writes one ack byte. See [`PATTERN_KNOCK`].
    Knock,
    /// `NNpsk0`, a knock under the opt-in network key. Two messages, no static
    /// key; the psk is the network admission check and it is proved inside
    /// message 1. See [`PATTERN_KNOCK_PSK`].
    KnockPsk,
}

impl Handshake {
    /// Parse one of the three patterns this node speaks, refusing anything
    /// else rather than passing it to `snow` and inheriting whatever that
    /// accepts.
    pub fn from_pattern(pattern: &str) -> Result<Self> {
        match pattern {
            PATTERN_PAIR => Ok(Self::Pair),
            PATTERN_RETURN => Ok(Self::Return),
            PATTERN_ENROL => Ok(Self::Enrol),
            PATTERN_KNOCK => Ok(Self::Knock),
            PATTERN_KNOCK_PSK => Ok(Self::KnockPsk),
            other => Err(anyhow!(
                "peer handshake: {other} is not one of the five patterns this node speaks \
                 ({PATTERN_PAIR}, {PATTERN_RETURN}, {PATTERN_ENROL}, {PATTERN_KNOCK}, \
                 {PATTERN_KNOCK_PSK})"
            )),
        }
    }

    /// The `snow` pattern string.
    pub fn pattern(self) -> &'static str {
        match self {
            Self::Pair => PATTERN_PAIR,
            Self::Return => PATTERN_RETURN,
            Self::Enrol => PATTERN_ENROL,
            Self::Knock => PATTERN_KNOCK,
            Self::KnockPsk => PATTERN_KNOCK_PSK,
        }
    }

    /// Which pattern a first frame of this length is, or `None` when no pattern
    /// this node speaks has a message 1 that long.
    ///
    /// **One place decides this**, because the alternative, a chain of `if
    /// len == …` in the listener, is where a fifth pattern gets added and a
    /// length collision goes unnoticed. `Self::Return` and `Self::Enrol` share
    /// a length by construction (`IKpsk1` differs from `IK` only in the psk
    /// mixed before message 1, never in its size), so this answers `Return` for
    /// 96 and the listener trials the enrolment secrets when `IK` fails, which
    /// is the order [`crate::peer::listener::accept_pairing_or_return`]
    /// documents.
    /// [`XX_MESSAGE_1_LEN_V1`] answers `Pair` as well, so a Mac on the older
    /// build is read far enough to be told which version it speaks rather than
    /// refused as noise. The refusal itself is [`Message1::instance_id`]'s, and
    /// it still happens before a single byte is written back.
    pub fn from_message_1_len(len: usize) -> Option<Self> {
        match len {
            KNOCK_MESSAGE_1_LEN => Some(Self::Knock),
            XX_MESSAGE_1_LEN | XX_MESSAGE_1_LEN_V1 => Some(Self::Pair),
            KNOCK_PSK_MESSAGE_1_LEN => Some(Self::KnockPsk),
            IK_MESSAGE_1_LEN => Some(Self::Return),
            _ => None,
        }
    }

    /// Whether a first frame of this length can be read as this pattern's
    /// message 1.
    ///
    /// Only [`Self::Pair`] has two answers, and only so the older payload gets
    /// a refusal that names the version. Every other pattern has exactly one
    /// length, which is what [`Self::message_1_len`] returns.
    pub fn accepts_message_1_len(self, len: usize) -> bool {
        match self {
            Self::Pair => len == XX_MESSAGE_1_LEN || len == XX_MESSAGE_1_LEN_V1,
            other => len == other.message_1_len(),
        }
    }

    /// The longest message 1 this pattern can accept, which is the most a
    /// reader may allocate for one before anything is authenticated.
    ///
    /// Per PATTERN and not the fleet-wide [`MAX_MESSAGE_1_BYTES`], because the
    /// pattern is already known where this is asked: an `XX` responder has no
    /// reason to size a buffer for the 96 bytes an `IK` message 1 has.
    pub fn max_message_1_len(self) -> usize {
        match self {
            Self::Pair => {
                if XX_MESSAGE_1_LEN > XX_MESSAGE_1_LEN_V1 {
                    XX_MESSAGE_1_LEN
                } else {
                    XX_MESSAGE_1_LEN_V1
                }
            }
            other => other.message_1_len(),
        }
    }

    /// How long this pattern's message 1 is, with the payload this design
    /// requires of it, empty everywhere except [`Self::Pair`], which carries
    /// the instance id (see [`XX_MESSAGE_1_LEN`]).
    pub fn message_1_len(self) -> usize {
        match self {
            Self::Pair => XX_MESSAGE_1_LEN,
            Self::Return | Self::Enrol => IK_MESSAGE_1_LEN,
            Self::Knock => KNOCK_MESSAGE_1_LEN,
            Self::KnockPsk => KNOCK_PSK_MESSAGE_1_LEN,
        }
    }

    /// Whether the responder learns the initiator's static key before it must
    /// write message 2, which is the same question as "can the pin check run
    /// here at all".
    ///
    /// `false` for the two knock patterns, and for a different reason than for
    /// [`Self::Pair`]: `XX` learns a static key eventually, in message 3, while
    /// `NN` never learns one at all. [`Self::learns_a_static_key`] is the
    /// question that separates them.
    pub fn pins_before_answering(self) -> bool {
        match self {
            Self::Pair | Self::Knock | Self::KnockPsk => false,
            Self::Return | Self::Enrol => true,
        }
    }

    /// Whether this pattern ever learns the initiator's static key.
    ///
    /// `false` only for the two knock patterns, which have no static key on
    /// either side. Separate from [`Self::pins_before_answering`] because
    /// `XX` answers `false` there and `true` here, and a caller that conflated
    /// the two would either look for a key `NN` will never produce or skip the
    /// key `XX` carries in message 3.
    pub fn learns_a_static_key(self) -> bool {
        !matches!(self, Self::Knock | Self::KnockPsk)
    }

    /// Whether a PSK is mixed in, and at which position.
    fn psk_position(self) -> Option<u8> {
        match self {
            Self::Enrol => Some(PSK_POSITION_ONE),
            Self::KnockPsk => Some(PSK_POSITION_ZERO),
            Self::Pair | Self::Return | Self::Knock => None,
        }
    }
}

/// Build a responder for one inbound connection.
///
/// `psks` are the secrets of every outstanding invite, in no particular order;
/// an empty slice means this is a returning connection under
/// [`PATTERN_RETURN`] and no trial decryption happens at all.
///
/// **One responder is one PSK.** `snow` fixes the PSK at BUILD time
/// (`Builder::psk`), so a trial over N outstanding invites is N responders and
/// N attempts at the same captured message 1, the loop is in
/// [`accept_handshake`], which is the only place that has the message. Handing
/// this function more than one PSK is therefore refused rather than served with
/// the first: quietly trying one of two invites is exactly the silent fallback
/// that makes an enrolment failure unexplainable.
pub fn responder(node: &NodeKey, pattern: &str, psks: &[[u8; 32]]) -> Result<HandshakeState> {
    responder_with_secret(node.secret_bytes(), pattern, psks)
}

/// [`responder`], against a raw static secret rather than the on-disk
/// [`NodeKey`].
///
/// The seam exists because [`NodeKey`] can only be built by reading this
/// machine's own key files, and a handshake test needs two identities in one
/// process with no files at all. Production callers use [`responder`].
pub fn responder_with_secret(
    secret: &[u8; KEY_BYTES],
    pattern: &str,
    psks: &[[u8; KEY_BYTES]],
) -> Result<HandshakeState> {
    let handshake = Handshake::from_pattern(pattern)?;
    let psk = one_psk(handshake, psks)?;
    build(handshake, secret, None, psk)?
        .build_responder()
        .context(
            "peer handshake: snow refused to build a responder for this pattern and static key",
        )
}

/// Build an initiator for one outbound connection.
pub fn initiator(
    node: &NodeKey,
    pattern: &str,
    remote_static: &[u8; 32],
    psk: Option<&[u8; 32]>,
) -> Result<HandshakeState> {
    initiator_with_secret(node.secret_bytes(), pattern, Some(remote_static), psk)
}

/// [`initiator`], against a raw static secret, and with the remote key
/// OPTIONAL because [`Handshake::Pair`] is the one pattern that has none.
///
/// The skeleton's [`initiator`] takes `&[u8; 32]`, which cannot express an `XX`
/// dial; it is kept unchanged and delegates here.
pub fn initiator_with_secret(
    secret: &[u8; KEY_BYTES],
    pattern: &str,
    remote_static: Option<&[u8; KEY_BYTES]>,
    psk: Option<&[u8; KEY_BYTES]>,
) -> Result<HandshakeState> {
    let handshake = Handshake::from_pattern(pattern)?;
    match (handshake, remote_static) {
        (Handshake::Pair | Handshake::Knock | Handshake::KnockPsk, Some(_)) => bail!(
            "peer handshake: {} knows no remote static key by construction; a dial that \
             already knows the key is {PATTERN_RETURN}",
            handshake.pattern()
        ),
        (Handshake::Return | Handshake::Enrol, None) => bail!(
            "peer handshake: {} needs the responder's static key, which is what makes the \
             pin check possible on this side",
            handshake.pattern()
        ),
        _ => {}
    }
    match (handshake, psk) {
        (Handshake::Enrol, None) => bail!(
            "peer handshake: {PATTERN_ENROL} is enrolment under a join token and needs that \
             token's 32-byte secret as the PSK"
        ),
        (Handshake::KnockPsk, None) => bail!(
            "peer handshake: {PATTERN_KNOCK_PSK} is a knock under the network key and needs \
             that key as psk0"
        ),
        (Handshake::Pair | Handshake::Return | Handshake::Knock, Some(_)) => bail!(
            "peer handshake: {} carries no PSK; a secret handed here would be silently ignored",
            handshake.pattern()
        ),
        _ => {}
    }
    build(handshake, secret, remote_static, psk)?
        .build_initiator()
        .context(
            "peer handshake: snow refused to build an initiator for this pattern and static key",
        )
}

/// The one `snow::Builder` assembly in the tree.
fn build<'a>(
    handshake: Handshake,
    secret: &'a [u8; KEY_BYTES],
    remote_static: Option<&'a [u8; KEY_BYTES]>,
    psk: Option<&'a [u8; KEY_BYTES]>,
) -> Result<Builder<'a>> {
    let params = handshake
        .pattern()
        .parse()
        .with_context(|| format!("peer handshake: snow rejected {}", handshake.pattern()))?;
    let mut builder = Builder::new(params)
        .local_private_key(secret)
        .context("peer handshake: snow rejected this node's static secret")?;
    if let Some(remote) = remote_static {
        builder = builder
            .remote_public_key(remote)
            .context("peer handshake: snow rejected the remote static key")?;
    }
    if let Some(secret) = psk {
        let Some(position) = handshake.psk_position() else {
            bail!(
                "peer handshake: {} carries no PSK; a secret handed here would be silently \
                 ignored",
                handshake.pattern()
            );
        };
        builder = builder.psk(position, secret).with_context(|| {
            format!(
                "peer handshake: snow rejected the secret as psk{position} for {}",
                handshake.pattern()
            )
        })?;
    }
    Ok(builder)
}

/// Exactly the PSK this pattern needs, or a refusal that says which.
fn one_psk(handshake: Handshake, psks: &[[u8; KEY_BYTES]]) -> Result<Option<&[u8; KEY_BYTES]>> {
    let needs_psk = handshake.psk_position().is_some();
    match (needs_psk, psks) {
        (true, []) => bail!(
            "peer handshake: {} needs one 32-byte secret as its psk and none was offered",
            handshake.pattern()
        ),
        (true, [only]) => Ok(Some(only)),
        (true, many) => bail!(
            "peer handshake: snow fixes the psk at build time, so {} candidate secrets are \
             {} responders; the trial belongs to read_message_1, which holds message 1",
            many.len(),
            many.len()
        ),
        (false, []) => Ok(None),
        (false, _) => bail!(
            "peer handshake: {} carries no PSK; a secret handed here would be silently ignored",
            handshake.pattern()
        ),
    }
}

/// **The check that carries everything.**
///
/// `snow` has no pin store. `HandshakeState` exposes `get_remote_static()` and
/// its builder takes an OPTIONAL remote public key, and neither consults a list
/// of allowed peers. So for an `IK` responder the pin comparison is application
/// code sitting between `read_message` and `write_message`, and **if it is
/// missing the handshake completes and nothing errors**: any node holding this
/// node's public key opens streams, a forgotten peer is still admitted, and
/// every test in the suite still passes.
///
/// Call it in exactly one place, [`crate::peer::listener::peer_stream_gate`]
/// check 1, and prove it by deleting the comparison and watching a revoked
/// peer get in.
pub fn pin_check(remote_static: &[u8], store: &PeerStore) -> Result<PeerId, PinRefusal> {
    pin_check_rows(remote_static, &store.peers())
}

/// [`pin_check`] against the pinned rows themselves.
///
/// The decision is one comparison and it is the whole of authorization, so it
/// is reachable without a [`PeerStore`], which can only be built by reading a
/// real file. [`pin_check`] is this function plus one read, and there is one
/// implementation of the comparison.
pub fn pin_check_rows(remote_static: &[u8], rows: &[PeerRow]) -> Result<PeerId, PinRefusal> {
    let Ok(key) = <[u8; KEY_BYTES]>::try_from(remote_static) else {
        return Err(PinRefusal::Malformed {
            len: remote_static.len(),
        });
    };
    let offered = PeerId(key);
    if rows.iter().any(|row| row.node == offered) {
        Ok(offered)
    } else {
        Err(PinRefusal::NotPinned {
            offered: offered.display(),
        })
    }
}

/// Why a handshake was refused at the pin check. Never a prompt, never a
/// "continue anyway": a changed key on a pinned node names both fingerprints
/// and stops, which is the SSH host-key rule and the right one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinRefusal {
    /// The initiator's static is not in the pin store. This is also what
    /// `tcr peer forget` looks like on the next connection.
    NotPinned {
        /// The display form of the key that called, for the log line.
        offered: String,
    },
    /// A remote static of the wrong width. A malformed handshake, not a peer.
    Malformed {
        /// How many bytes arrived.
        len: usize,
    },
}

impl std::fmt::Display for PinRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotPinned { offered } => write!(
                f,
                "peer handshake refused: {offered} is not pinned on this node \
                 (`tcr peer ls` to see what is)"
            ),
            Self::Malformed { len } => write!(
                f,
                "peer handshake refused: remote static is {len} bytes, not 32"
            ),
        }
    }
}

impl std::error::Error for PinRefusal {}

/// The six digits for a handshake that carries no nonce exchange.
///
/// Derived from the handshake hash alone, which binds both statics and both
/// ephemerals. **On its own that is not enough for a first pairing**, and the
/// doc here used to claim it was: it said a man in the middle "produces two
/// different codes", which holds only for one that RELAYS both halves. A man in
/// the middle that runs two handshakes of its own answers A as responder,
/// learns A's code, and then dials B grinding its own static key until B's code
/// matches: 20 bits is about a million X25519 key generations, seconds of work,
/// inside the 120-second window B's Accept opens. The grind works because in
/// `XX` the initiator writes its static LAST, so the attacker picks the value
/// the hash still depends on after it has seen the code it must match.
///
/// So this is what [`Handshake::Return`], [`Handshake::Enrol`] and the two
/// knocks get, where the code is a by-product nobody compares, and a first
/// pairing gets [`pairing_code`] instead, over a nonce from each side that
/// neither could choose after seeing the other's.
pub fn six_digit_code(handshake: &HandshakeState) -> String {
    let hash = handshake.get_handshake_hash();
    fold_to_six_digits(hash)
}

/// The six digits both operators compare during interactive pairing.
///
/// `hash` is the completed handshake hash, and the two nonces go in by ROLE and
/// never by who is calling, so the two screens cannot disagree over an argument
/// swap: the initiator's first, the responder's second, on both ends.
///
/// # Why a nonce from each side, committed before either is seen
///
/// The responder commits to its nonce in message 2 (it sends `SHA-256(nB)`),
/// the initiator reveals `nA` in message 3, and only then does the responder
/// reveal `nB`, which the initiator checks against the commitment it already
/// holds. Neither side can choose its contribution after learning the other's:
/// the initiator picks `nA` knowing only a hash of `nB`, and the responder is
/// bound to the `nB` it committed to before `nA` existed. That is what makes
/// the digits a short authentication string rather than a fingerprint of values
/// one end controls, and it is the property [`six_digit_code`] does not have.
pub fn pairing_code(
    hash: &[u8],
    initiator_nonce: &[u8; PAIR_NONCE_BYTES],
    responder_nonce: &[u8; PAIR_NONCE_BYTES],
) -> String {
    use sha2::Digest as _;
    let mut digest = sha2::Sha256::new();
    digest.update(hash);
    digest.update(initiator_nonce);
    digest.update(responder_nonce);
    fold_to_six_digits(&digest.finalize())
}

/// A commitment to one nonce: `SHA-256` of it, and nothing else, so both ends
/// compute it the same way.
pub fn nonce_commitment(nonce: &[u8; PAIR_NONCE_BYTES]) -> [u8; PAIR_NONCE_BYTES] {
    use sha2::Digest as _;
    let mut digest = sha2::Sha256::new();
    digest.update(nonce);
    digest.finalize().into()
}

/// The first 20 bits of a digest, as six decimal digits.
fn fold_to_six_digits(hash: &[u8]) -> String {
    // The first 20 bits of the hash, then folded into six decimal digits. Six
    // digits is 10^6 and 20 bits is 2^20, so the fold drops a little over one
    // bit; the alternative (20 bits printed as `%06d`) is SEVEN digits for
    // every value above 999999, and the operator is comparing a fixed-width
    // string on two screens.
    let mut head = [0_u8; 4];
    for (slot, byte) in head.iter_mut().zip(hash.iter()) {
        *slot = *byte;
    }
    let first_20_bits = u32::from_be_bytes(head) >> 12;
    format!("{:06}", first_20_bits % 1_000_000)
}

/// Finish the handshake and take the transport session.
///
/// Separate from [`pin_check`] on purpose: this is the step that must not be
/// reachable until the pin check has answered.
pub fn into_transport(handshake: HandshakeState) -> Result<TransportState> {
    handshake
        .into_transport_mode()
        .context("peer handshake: snow refused to enter transport mode before it completed")
}

/// One authenticated peer stream, as both drivers leave it.
pub struct PeerSession {
    /// Which pattern ran. Kept because it is what tells a caller whether
    /// [`Self::code`] is a number a human must compare (a first pairing) or a
    /// by-product (a return visit), and a caller that has to re-derive that
    /// from context gets it wrong once.
    pub handshake: Handshake,
    /// Who is on the other end, as the handshake proved it.
    pub peer: PeerId,
    /// The six digits for this handshake. Meaningful to a human only on
    /// [`Handshake::Pair`]; computed either way because it costs one hash read
    /// and a missing code on the pairing path is the failure that matters.
    pub code: String,
    /// The handshake hash this session ended on.
    ///
    /// Kept because it is the one input `crate::peer::reach::port_secret`
    /// takes, and `snow` exposes `get_handshake_hash` on `HandshakeState`
    /// only: once `into_transport` has run, the value is gone and a dialler
    /// has no way to compute the pair's rendezvous ports.
    ///
    /// Not printed by the `Debug` below, for the same reason the transport
    /// half is not.
    pub handshake_hash: Vec<u8>,
    /// The transport half, ready for [`send_encrypted`] / [`recv_encrypted`].
    pub transport: TransportState,
}

impl std::fmt::Debug for PeerSession {
    /// The transport half is deliberately not printed: it holds the session
    /// keys, and a `{:?}` in a log line is how a key ends up in a file.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerSession")
            .field("handshake", &self.handshake)
            .field("peer", &self.peer)
            .field("code", &self.code)
            .finish_non_exhaustive()
    }
}

/// Generate 32 cryptographically random bytes through `snow`'s own resolver.
///
/// Here rather than in [`crate::peer::pair`] so the join secret and a Noise
/// ephemeral come from the same CSPRNG, and so this file stays the only one
/// that names `snow::`.
pub fn random_secret() -> Result<[u8; KEY_BYTES]> {
    let mut rng = DefaultResolver
        .resolve_rng()
        .ok_or_else(|| anyhow!("peer handshake: snow's default resolver offered no CSPRNG"))?;
    let mut out = [0_u8; KEY_BYTES];
    rng.try_fill_bytes(&mut out)
        .map_err(|err| anyhow!("peer handshake: the CSPRNG failed: {err}"))?;
    Ok(out)
}

/// Generate a static keypair, for a first boot and for a test that needs two
/// identities in one process.
pub fn generate_static() -> Result<([u8; KEY_BYTES], [u8; KEY_BYTES])> {
    let params = PATTERN_PAIR
        .parse()
        .context("peer handshake: snow rejected the pairing pattern")?;
    let pair = Builder::new(params)
        .generate_keypair()
        .context("peer handshake: snow could not generate a static keypair")?;
    let secret = <[u8; KEY_BYTES]>::try_from(pair.private.as_slice()).map_err(|_| {
        anyhow!(
            "peer handshake: snow returned a {}-byte private key",
            pair.private.len()
        )
    })?;
    let public = <[u8; KEY_BYTES]>::try_from(pair.public.as_slice()).map_err(|_| {
        anyhow!(
            "peer handshake: snow returned a {}-byte public key",
            pair.public.len()
        )
    })?;
    Ok((secret, public))
}

/// Read one `u16`-length-prefixed frame.
///
/// The length prefix is written by [`encode_frame`] and the bounds are checked
/// by [`decode_frame`], which is deliberately the only place that decides how
/// long a peer-controlled frame may be.
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Vec<u8>> {
    read_frame_bounded(reader, MAX_FRAME_BYTES).await
}

/// [`read_frame`] with a ceiling on what it will ALLOCATE.
///
/// # The allocation is the point, not the read
///
/// `abuse-resistance.md`'s "resource exhaustion before auth" row asks for "no
/// allocation before the length check", and the plain [`read_frame`] above
/// cannot give it: it reads the peer-controlled two-byte prefix and then sizes
/// a `Vec` from it, so any host that can reach the port makes this node
/// allocate 65 kB per connection by writing two bytes and then nothing. With
/// 16 concurrent unauthenticated sockets allowed that is a megabyte a stranger
/// can pin with 32 bytes of traffic, small, but it is the shape of the bug and
/// it costs one comparison to close.
///
/// The FIRST frame of every connection is read through this with
/// [`MAX_MESSAGE_1_BYTES`], which is the largest message 1 any pattern this
/// node speaks has. Frames after the handshake are authenticated and read
/// through [`read_frame`] at the full Noise limit.
pub async fn read_frame_bounded<R: AsyncRead + Unpin>(
    reader: &mut R,
    max_payload: usize,
) -> Result<Vec<u8>> {
    let mut prefix = [0_u8; 2];
    reader
        .read_exact(&mut prefix)
        .await
        .context("peer frame: no length prefix")?;
    let len = usize::from(u16::from_be_bytes(prefix));
    if len > max_payload {
        bail!(
            "peer frame: the length prefix promises {len} bytes and the bound here is \
             {max_payload}; refused BEFORE allocating, so two bytes from a stranger cannot \
             reserve a buffer"
        );
    }
    let mut frame = vec![0_u8; 2 + len];
    frame[..2].copy_from_slice(&prefix);
    reader
        .read_exact(&mut frame[2..])
        .await
        .with_context(|| format!("peer frame: short read, {len} bytes promised"))?;
    let (payload, _) = decode_frame(&frame).map_err(|err| anyhow!("peer frame: {err}"))?;
    Ok(payload.to_vec())
}

/// Write one `u16`-length-prefixed frame.
pub async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, payload: &[u8]) -> Result<()> {
    let mut out = Vec::with_capacity(2 + payload.len());
    encode_frame(payload, &mut out).map_err(|err| anyhow!("peer frame: {err}"))?;
    writer
        .write_all(&out)
        .await
        .context("peer frame: the write failed")?;
    writer.flush().await.context("peer frame: the flush failed")
}

/// Encrypt one post-handshake message and write it as a frame.
pub async fn send_encrypted<W: AsyncWrite + Unpin>(
    writer: &mut W,
    transport: &mut TransportState,
    plaintext: &[u8],
) -> Result<()> {
    let mut ciphertext = vec![0_u8; MAX_FRAME_BYTES];
    let len = transport
        .write_message(plaintext, &mut ciphertext)
        .context("peer stream: snow refused to encrypt this message")?;
    ciphertext.truncate(len);
    write_frame(writer, &ciphertext).await
}

/// Read one frame and decrypt it.
pub async fn recv_encrypted<R: AsyncRead + Unpin>(
    reader: &mut R,
    transport: &mut TransportState,
) -> Result<Vec<u8>> {
    let frame = read_frame(reader).await?;
    let mut plaintext = vec![0_u8; MAX_FRAME_BYTES];
    let len = transport
        .read_message(&frame, &mut plaintext)
        .context("peer stream: this frame did not decrypt under the session key")?;
    plaintext.truncate(len);
    Ok(plaintext)
}

/// A frame reader that keeps the bytes it has already taken off the socket.
///
/// # Why this exists: a timeout around [`recv_encrypted`] tears frames
///
/// The control loop wants two things from one stream: the next frame, and a
/// wake-up once a second so a lease bearer can be renewed while the borrower
/// says nothing. Written as `timeout(one_second, recv_encrypted(..))` those two
/// wants fight, because [`read_frame_bounded`] is built on `read_exact` and
/// `read_exact` is NOT cancel safe: it can consume bytes into a buffer it owns
/// and then be dropped at the tick, taking them with it. On a LAN that is
/// invisible, since a frame arrives inside one poll interval. Over a WAN it is
/// a torn frame whenever an RTT spike, a retransmit or a laptop going to sleep
/// puts a second between the length prefix and the body: the next read takes
/// the REST of the old frame as a length prefix, the Noise nonce desyncs
/// against a peer that did nothing wrong, and the session dies reporting a
/// decrypt failure, which sends the next reader hunting for a key problem that
/// is not there.
///
/// The fix is to move the partial bytes out of the future and into a value the
/// caller owns. [`tokio::io::AsyncReadExt::read_buf`] is cancel safe, and
/// everything it has read is already in this struct's buffer when the timeout
/// fires, so the next call resumes the same frame. The buffer cannot grow past
/// one frame's worth of pending bytes plus whatever a single read appended,
/// because a whole frame is parsed and drained as soon as it is present, and
/// `MAX_FRAME_BYTES` is `u16::MAX`, the largest a two-byte prefix can promise.
///
/// One reader per stream, held for the life of the session: a second one would
/// start with an empty buffer and read from the middle of a frame.
#[derive(Debug, Default)]
pub struct FrameReader {
    pending: bytes::BytesMut,
}

impl FrameReader {
    /// A reader with nothing buffered, for a stream positioned at a frame
    /// boundary (which is where a finished handshake leaves it).
    pub fn new() -> Self {
        Self {
            pending: bytes::BytesMut::with_capacity(2 + MAX_FRAME_BYTES),
        }
    }

    /// How many bytes of a part-read frame are held right now.
    ///
    /// For tests, which is why it exists: "the frame survived" and "the reader
    /// kept the first half across the gap" are different claims and the second
    /// one needs a way to be seen.
    pub fn buffered(&self) -> usize {
        self.pending.len()
    }

    /// The next whole frame, resuming whatever a cancelled call left behind.
    ///
    /// Cancel safe: dropping this future loses no bytes, because every byte it
    /// read is in `self.pending` before the future yields again.
    pub async fn next_frame<R: AsyncRead + Unpin>(&mut self, reader: &mut R) -> Result<Vec<u8>> {
        loop {
            match decode_frame(&self.pending) {
                Ok((payload, consumed)) => {
                    let frame = payload.to_vec();
                    let _drained = self.pending.split_to(consumed);
                    return Ok(frame);
                }
                Err(tcr_peer_wire::FrameError::Incomplete) => {}
                Err(err) => bail!("peer frame: {err}"),
            }
            let read = reader
                .read_buf(&mut self.pending)
                .await
                .context("peer frame: the read failed")?;
            if read == 0 {
                bail!(
                    "peer frame: the other end closed the stream with {} bytes of a frame \
                     buffered here",
                    self.pending.len()
                );
            }
        }
    }

    /// [`next_frame`](Self::next_frame), then decrypt, so a caller can swap
    /// this in for [`recv_encrypted`] one call site at a time.
    pub async fn recv_encrypted<R: AsyncRead + Unpin>(
        &mut self,
        reader: &mut R,
        transport: &mut TransportState,
    ) -> Result<Vec<u8>> {
        let frame = self.next_frame(reader).await?;
        let mut plaintext = vec![0_u8; MAX_FRAME_BYTES];
        let len = transport
            .read_message(&frame, &mut plaintext)
            .context("peer stream: this frame did not decrypt under the session key")?;
        plaintext.truncate(len);
        Ok(plaintext)
    }
}

/// Run the responder's half of a handshake over `stream`.
///
/// `authorize` is [`pin_check`] on the serving path, injected rather than
/// called directly for one reason: **this function's contract is that nothing
/// is written until that callback has answered**, and a test proves it by
/// counting the bytes the writer received after a refusal. A callback that
/// cannot be replaced is a contract that cannot be measured.
///
/// It is not called on [`Handshake::Pair`], and that is not an omission: `XX`
/// carries the initiator's static in message 3, so there is no key to compare
/// before message 2 exists. `XX` is the FIRST pairing, whose authorization is
/// the six digits the two operators compare.
pub async fn accept_handshake<S, A>(
    stream: &mut S,
    secret: &[u8; KEY_BYTES],
    handshake: Handshake,
    psks: &[[u8; KEY_BYTES]],
    authorize: A,
) -> Result<PeerSession>
where
    S: AsyncRead + AsyncWrite + Unpin,
    A: Fn(&[u8]) -> Result<PeerId, PinRefusal>,
{
    // BOUNDED by the pattern's own message 1, not by the Noise transport
    // limit. This is the first frame of a connection a stranger can open, and
    // the plain reader sizes its buffer from the peer's own two-byte prefix: a
    // `0xFFFF` prefix and then silence made this node allocate 65 535 bytes per
    // connection before the length check below could refuse it. The listener's
    // own path already read through the bounded reader, which is why this was
    // latent rather than reachable, and a second caller of this function is one
    // nobody would think to check.
    let message_1 = read_frame_bounded(stream, handshake.max_message_1_len()).await?;
    if !handshake.accepts_message_1_len(message_1.len()) {
        bail!(
            "peer handshake: {} bytes are not a {} message 1 ({} expected)",
            message_1.len(),
            handshake.pattern(),
            handshake.message_1_len()
        );
    }

    let mut scratch = vec![0_u8; HANDSHAKE_SCRATCH_BYTES];
    let state = read_message_1(secret, handshake, psks, &message_1, &mut scratch)?;
    finish_responder(stream, state, handshake, authorize).await
}

/// The responder's half from message 1 onwards: the pin check, then message 2,
/// then whatever the pattern has left.
///
/// Split from [`accept_handshake`] because the listener must decide WHICH
/// pattern is arriving from the message it already holds
/// ([`crate::peer::listener::accept_pairing_or_return`]), and the decision
/// needs the read to have happened. **The only write in this function is after
/// `authorize` has answered**, which is the property the design rests on.
pub async fn finish_responder<S, A>(
    stream: &mut S,
    mut state: HandshakeState,
    handshake: Handshake,
    authorize: A,
) -> Result<PeerSession>
where
    S: AsyncRead + AsyncWrite + Unpin,
    A: Fn(&[u8]) -> Result<PeerId, PinRefusal>,
{
    let mut scratch = vec![0_u8; HANDSHAKE_SCRATCH_BYTES];

    let pinned = if handshake.pins_before_answering() {
        // The pin check, between message 1 and message 2, and the only thing
        // between a stranger and this node. Nothing has been written yet and
        // nothing is written on this path.
        let remote = state
            .get_remote_static()
            .ok_or_else(|| anyhow!("peer handshake: message 1 carried no initiator static"))?;
        Some(authorize(remote).map_err(anyhow::Error::new)?)
    } else {
        None
    };

    // A first pairing's message 2 carries this node's COMMITMENT to the nonce
    // its half of the six digits is built from, and message 3 carries the
    // initiator's nonce in the clear. The order is the whole point: this node
    // is bound to `ours` before it learns `theirs`, and the initiator chose
    // `theirs` knowing only a hash of `ours`. See [`pairing_code`].
    let ours = if handshake == Handshake::Pair {
        Some(random_secret()?)
    } else {
        None
    };
    let message_2_payload: &[u8] = match &ours {
        Some(nonce) => &nonce_commitment(nonce),
        None => &[],
    };
    let len = state
        .write_message(message_2_payload, &mut scratch)
        .context("peer handshake: snow refused to write message 2")?;
    write_frame(stream, &scratch[..len]).await?;

    let mut theirs: Option<[u8; PAIR_NONCE_BYTES]> = None;
    let peer = match pinned {
        Some(peer) => peer,
        // A knock has no third message and no static key on either side, so
        // there is nothing to read and nothing to name. `PeerId([0; 32])` here
        // is not a peer and never reaches a pin store: the knock path in
        // `crate::peer::listener` writes a row keyed on the SOURCE ADDRESS and
        // discards the session.
        None if !handshake.learns_a_static_key() => PeerId([0_u8; KEY_BYTES]),
        None => {
            let message_3 = read_frame(stream).await?;
            let payload_len = state
                .read_message(&message_3, &mut scratch)
                .context("peer handshake: message 3 did not authenticate")?;
            if ours.is_some() {
                let revealed: [u8; PAIR_NONCE_BYTES] =
                    scratch[..payload_len].try_into().map_err(|_| {
                        anyhow!(
                            "peer handshake: a first pairing's message 3 carried {payload_len} \
                             payload bytes, not the {PAIR_NONCE_BYTES} of the nonce the six \
                             digits are built from. A Mac on an older build reaches here; \
                             update it and pair again"
                        )
                    })?;
                theirs = Some(revealed);
            }
            let remote = state
                .get_remote_static()
                .ok_or_else(|| anyhow!("peer handshake: message 3 carried no initiator static"))?;
            let key = <[u8; KEY_BYTES]>::try_from(remote).map_err(|_| {
                anyhow!(
                    "peer handshake: remote static is {} bytes, not 32",
                    remote.len()
                )
            })?;
            PeerId(key)
        }
    };

    // Read before `into_transport` consumes the handshake state, which is the
    // only thing that can answer for this hash.
    let handshake_hash = state.get_handshake_hash().to_vec();
    let code = match (&ours, &theirs) {
        (Some(ours), Some(theirs)) => pairing_code(&handshake_hash, theirs, ours),
        _ => six_digit_code(&state),
    };
    crate::peer::reach::remember_pair_hash(peer, &handshake_hash);
    let mut transport = into_transport(state)?;

    // The reveal, and the reason it is a transport frame rather than a fourth
    // handshake message: `XX` has three, and this value must not leave until
    // the initiator's nonce has arrived. Encrypted, like everything after the
    // handshake, and read by `dial_handshake_with_payload` as its next frame,
    // so both halves stay inside this module and no caller has to know.
    if let Some(nonce) = &ours {
        send_encrypted(stream, &mut transport, nonce)
            .await
            .context(
                "peer handshake: the pairing nonce this node committed to could not be sent",
            )?;
    }

    Ok(PeerSession {
        handshake,
        peer,
        code,
        handshake_hash,
        transport,
    })
}

/// Read message 1, trialling the outstanding invite secrets on
/// [`Handshake::Enrol`].
///
/// The trial is here rather than in [`responder`] because `snow` fixes the PSK
/// at build time: N outstanding invites are N responders against the one
/// captured message 1. Every attempt is a read and nothing else, so a failure
/// costs the caller a closed socket and zero written bytes.
pub fn read_message_1(
    secret: &[u8; KEY_BYTES],
    handshake: Handshake,
    psks: &[[u8; KEY_BYTES]],
    message_1: &[u8],
    scratch: &mut [u8],
) -> Result<HandshakeState> {
    read_message_1_matching(secret, handshake, psks, message_1, scratch).map(|read| read.state)
}

/// What reading message 1 produced: the half-finished handshake, which secret
/// unlocked it, and what it carried.
///
/// A struct rather than a tuple because the third field arrived last and a
/// three-tuple at a call site is where `psk` and `payload` get swapped: two
/// fields whose types are close enough (`Option<[u8; 32]>` and `Vec<u8>`) that
/// only a name separates them.
pub struct Message1 {
    /// The responder's state, positioned to write message 2.
    pub state: HandshakeState,
    /// Which candidate secret decrypted the message. `None` for a pattern that
    /// carries no psk. On [`Handshake::Enrol`] this is the only thing that
    /// identifies WHICH invite is being spent, because a join token carries no
    /// invite id. See [`crate::peer::pair::accept_enrolment`].
    pub psk: Option<[u8; KEY_BYTES]>,
    /// Message 1's payload. Empty for every pattern except
    /// [`Handshake::Pair`], which carries the initiator's
    /// [`tcr_peer_wire::InstanceId`]. See [`XX_MESSAGE_1_LEN`].
    pub payload: Vec<u8>,
}

impl Message1 {
    /// The instance id in an `XX` message 1's payload.
    ///
    /// **This is untrusted input** and it is the value the accepted-window
    /// check keys on ([`crate::peer::state::PeerState::accepted_window`]), so
    /// it is parsed rather than assumed: a payload of the wrong width is a
    /// malformed handshake and gets the same zero bytes everything else
    /// malformed gets.
    pub fn instance_id(&self) -> Result<tcr_peer_wire::InstanceId> {
        // The older payload, recognized only to say so. A Mac on that build
        // derives its six digits from the handshake hash alone, which is the
        // weakness this version exists to close, so it is refused rather than
        // served under the weaker rule.
        if self.payload.len() == tcr_peer_wire::INSTANCE_ID_BYTES {
            bail!(
                "peer handshake: this first pairing speaks version 1 of the pairing payload \
                 and this Mac speaks version {PAIR_PAYLOAD_VERSION}, whose six digits commit \
                 to a nonce from each side. Update the other Mac (`tcr update`) and pair \
                 again; nothing was written back"
            );
        }
        if self.payload.len() != PAIR_MESSAGE_1_PAYLOAD_LEN {
            bail!(
                "peer handshake: a first pairing's message 1 carried {} payload bytes, not \
                 the {PAIR_MESSAGE_1_PAYLOAD_LEN} of a version byte and an instance id; \
                 closing with nothing written",
                self.payload.len()
            );
        }
        let version = self.payload[0];
        if version != PAIR_PAYLOAD_VERSION {
            bail!(
                "peer handshake: this first pairing claims pairing payload version {version} \
                 and this Mac speaks version {PAIR_PAYLOAD_VERSION}; closing with nothing \
                 written"
            );
        }
        let bytes: [u8; tcr_peer_wire::INSTANCE_ID_BYTES] = self.payload[1..]
            .try_into()
            .map_err(|_| anyhow!("peer handshake: message 1 carried no instance id"))?;
        Ok(tcr_peer_wire::InstanceId(bytes))
    }
}

/// [`read_message_1`], reporting WHICH outstanding invite's secret decrypted
/// the message.
///
/// The registrar has to know that to retire the right row: the token carries no
/// invite id, so the PSK that matched message 1 is the only thing that
/// identifies the invite being spent (see
/// [`crate::peer::pair::accept_enrolment`]). `None` on
/// [`Handshake::Pair`]/[`Handshake::Return`], which carry no PSK at all.
///
/// A second function rather than a wider signature on [`read_message_1`]:
/// `src/peer/listener.rs` calls that one in three places, so re-signing it
/// would change all three call sites for one caller's sake.
pub fn read_message_1_matching(
    secret: &[u8; KEY_BYTES],
    handshake: Handshake,
    psks: &[[u8; KEY_BYTES]],
    message_1: &[u8],
    scratch: &mut [u8],
) -> Result<Message1> {
    let attempts: Vec<Option<[u8; KEY_BYTES]>> = if handshake.psk_position().is_some() {
        if psks.is_empty() {
            bail!(
                "peer handshake: a {} arrived with no candidate secret to prove it against",
                handshake.pattern()
            );
        }
        psks.iter().copied().map(Some).collect()
    } else {
        vec![None]
    };

    let mut last: Option<anyhow::Error> = None;
    for psk in attempts {
        let psks: &[[u8; KEY_BYTES]] = match &psk {
            Some(one) => std::slice::from_ref(one),
            None => &[],
        };
        let mut state = responder_with_secret(secret, handshake.pattern(), psks)?;
        match state.read_message(message_1, scratch) {
            Ok(len) => {
                return Ok(Message1 {
                    state,
                    psk,
                    payload: scratch[..len].to_vec(),
                })
            }
            Err(err) => {
                last = Some(anyhow!(
                    "peer handshake: message 1 did not authenticate under {}: {err}",
                    handshake.pattern()
                ));
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("peer handshake: message 1 could not be read")))
}

/// Run the initiator's half of a handshake over `stream`.
///
/// On [`Handshake::Return`] a responder holding a static key other than
/// `remote_static` cannot decrypt message 1 at all, so it answers nothing and
/// this call fails on the read, which is the wrong-key case seen from this
/// side, and it costs the wrong responder zero written bytes.
pub async fn dial_handshake<S>(
    stream: &mut S,
    secret: &[u8; KEY_BYTES],
    handshake: Handshake,
    remote_static: Option<&[u8; KEY_BYTES]>,
    psk: Option<&[u8; KEY_BYTES]>,
) -> Result<PeerSession>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    dial_handshake_with_payload(stream, secret, handshake, remote_static, psk, &[]).await
}

/// [`dial_handshake`] with a message-1 payload.
///
/// The one caller that needs it is a first pairing, which carries the dialling
/// Mac's [`tcr_peer_wire::InstanceId`] so the responder can check it against
/// the window an Accept opened. See [`XX_MESSAGE_1_LEN`] for why the payload
/// and not a later frame. [`dial_handshake`] keeps its signature and delegates
/// with an empty payload, because `src/peer/serve.rs`, `src/peer/lease.rs` and
/// `tests/peer_lease.rs` all call it.
pub async fn dial_handshake_with_payload<S>(
    stream: &mut S,
    secret: &[u8; KEY_BYTES],
    handshake: Handshake,
    remote_static: Option<&[u8; KEY_BYTES]>,
    psk: Option<&[u8; KEY_BYTES]>,
    message_1_payload: &[u8],
) -> Result<PeerSession>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut state = initiator_with_secret(secret, handshake.pattern(), remote_static, psk)?;
    let mut scratch = vec![0_u8; HANDSHAKE_SCRATCH_BYTES];

    let len = state
        .write_message(message_1_payload, &mut scratch)
        .context("peer handshake: snow refused to write message 1")?;
    write_frame(stream, &scratch[..len]).await?;

    let message_2 = read_frame(stream).await?;
    let message_2_payload_len = state
        .read_message(&message_2, &mut scratch)
        .context("peer handshake: message 2 did not authenticate, the responder's key is not the one this dial expected")?;

    // A first pairing's message 2 carries the responder's COMMITMENT to its
    // nonce. This node picks its own knowing nothing but that hash, which is
    // what stops either end choosing its contribution after seeing the other's.
    // See [`pairing_code`].
    let commitment: Option<[u8; PAIR_NONCE_BYTES]> = if handshake == Handshake::Pair {
        Some(scratch[..message_2_payload_len].try_into().map_err(|_| {
            anyhow!(
                "peer handshake: a first pairing's message 2 carried \
                         {message_2_payload_len} payload bytes, not the {PAIR_NONCE_BYTES} of \
                         a commitment to the responder's pairing nonce. A Mac on an older \
                         build answers like this; update it and pair again"
            )
        })?)
    } else {
        None
    };
    let ours: Option<[u8; PAIR_NONCE_BYTES]> = match commitment {
        Some(_) => Some(random_secret()?),
        None => None,
    };

    // `XX` has a third message; `IK`/`IKpsk1` complete in two, and so do the
    // two knock patterns, for different reasons, which is why the condition
    // is both questions and not the negation of one. See
    // [`Handshake::learns_a_static_key`].
    if !handshake.pins_before_answering() && handshake.learns_a_static_key() {
        let message_3_payload: &[u8] = match &ours {
            Some(nonce) => nonce,
            None => &[],
        };
        let len = state
            .write_message(message_3_payload, &mut scratch)
            .context("peer handshake: snow refused to write message 3")?;
        write_frame(stream, &scratch[..len]).await?;
    }

    let peer = match remote_static {
        Some(key) => PeerId(*key),
        // A knock learns nobody's static key, in either direction. See the
        // matching arm in `finish_responder`.
        None if !handshake.learns_a_static_key() => PeerId([0_u8; KEY_BYTES]),
        None => {
            let remote = state
                .get_remote_static()
                .ok_or_else(|| anyhow!("peer handshake: message 2 carried no responder static"))?;
            let key = <[u8; KEY_BYTES]>::try_from(remote).map_err(|_| {
                anyhow!(
                    "peer handshake: remote static is {} bytes, not 32",
                    remote.len()
                )
            })?;
            PeerId(key)
        }
    };

    let unbound_code = six_digit_code(&state);
    // Read before `into_transport` consumes the handshake state, which is the
    // only thing that can answer for this hash.
    let handshake_hash = state.get_handshake_hash().to_vec();
    crate::peer::reach::remember_pair_hash(peer, &handshake_hash);
    let mut transport = into_transport(state)?;

    // The responder's reveal. Checked against the commitment that arrived in
    // message 2 before it is allowed anywhere near the digits: a responder that
    // answers with any other nonce is a responder that chose it after seeing
    // this node's, which is the whole attack.
    let code = match (commitment, &ours) {
        (Some(commitment), Some(ours)) => {
            let revealed = recv_encrypted(stream, &mut transport).await.context(
                "peer handshake: the responder never revealed the pairing nonce it committed to",
            )?;
            let revealed: [u8; PAIR_NONCE_BYTES] =
                revealed.as_slice().try_into().map_err(|_| {
                    anyhow!(
                        "peer handshake: the responder's pairing nonce is {} bytes, not \
                     {PAIR_NONCE_BYTES}",
                        revealed.len()
                    )
                })?;
            if nonce_commitment(&revealed) != commitment {
                bail!(
                    "peer handshake refused: the Mac that answered revealed a pairing nonce \
                     that is not the one it committed to in message 2. That is what a machine \
                     in the middle of this pairing looks like; nothing was pinned"
                );
            }
            pairing_code(&handshake_hash, ours, &revealed)
        }
        _ => unbound_code,
    };

    Ok(PeerSession {
        handshake,
        peer,
        code,
        handshake_hash,
        transport,
    })
}

// ---------------------------------------------------------------------------
// The knock: phase one of pairing
// ---------------------------------------------------------------------------

/// How long the knocking Mac waits for the one ack byte before calling the
/// other end unreachable.
///
/// `abuse-resistance.md`'s state diagram: "no ack 10 s → UNREACHABLE". A
/// forged announcement is exactly this case, pressing Trust opens a TCP
/// connection to an address nobody is listening on, and it has to fail rather
/// than hang.
pub const KNOCK_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Send a knock and wait for the ack. The initiator's whole half of phase one.
///
/// Returns `Ok(())` on the ack byte and an error on anything else, silence, a
/// closed socket, a different byte. **Every refusal on the responder's side
/// looks the same from here**, which is deliberate: a muted address, a banned
/// address and a full queue all get zero bytes, so a flooder cannot learn which
/// cap it hit and what to change.
///
/// `network_key` is `Some` when this Mac has one set, and it selects
/// [`PATTERN_KNOCK_PSK`] over [`PATTERN_KNOCK`]. A knock to a Mac that expects
/// the key without it fails inside message 1 on the far side; a knock WITH the
/// key to a Mac that expects none fails the same way. That is the honest
/// consequence of an opt-in shared secret and the operator sees "did not
/// answer".
pub async fn send_knock<S>(
    stream: &mut S,
    knock: &tcr_peer_wire::Knock,
    network_key: Option<&[u8; KEY_BYTES]>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let handshake = match network_key {
        Some(_) => Handshake::KnockPsk,
        None => Handshake::Knock,
    };
    // A knock has no static key on either side, so the "secret" the builder
    // takes is an ephemeral throwaway rather than this node's identity. That is
    // not a convenience: handing `NN` this node's real static secret would put
    // it in a builder for a pattern that never uses it, and the next reader
    // would reasonably conclude a knock discloses identity.
    let throwaway = random_secret()?;
    let mut session = dial_handshake(stream, &throwaway, handshake, None, network_key).await?;

    let bytes = serde_json::to_vec(knock).context("peer knock: the knock did not serialize")?;
    send_encrypted(stream, &mut session.transport, &bytes).await?;

    let ack = tokio::time::timeout(KNOCK_ACK_TIMEOUT, read_frame(stream))
        .await
        .map_err(|_| {
            anyhow!(
                "peer knock: no answer in {} seconds, so that Mac did not take the pairing \
                 request (a forged announcement looks exactly like this)",
                KNOCK_ACK_TIMEOUT.as_secs()
            )
        })?
        .context(
            "peer knock: that Mac closed the connection without taking the pairing request, \
             which is what its refusal looks like, blocked here, ignored here, or already \
             holding as many requests as it will hold",
        )?;

    let mut plaintext = vec![0_u8; MAX_FRAME_BYTES];
    let len = session
        .transport
        .read_message(&ack, &mut plaintext)
        .context("peer knock: the answer did not decrypt under the knock session key")?;
    if plaintext[..len] != [tcr_peer_wire::KNOCK_ACK] {
        bail!(
            "peer knock: that Mac answered {} bytes instead of the one-byte receipt a knock \
             earns; treating it as a refusal",
            len
        );
    }
    Ok(())
}

/// Read a knock off an already-completed knock session, and answer the one ack
/// byte.
///
/// Split from the queueing decision on purpose: this function moves bytes and
/// makes no policy, so the cap, the mute and the ban all live in
/// [`crate::peer::listener`] beside every other refusal. The ack is written
/// only AFTER the queueing decision said yes. See that call site: a refused
/// knock gets zero bytes.
pub async fn read_knock<S>(
    stream: &mut S,
    session: &mut PeerSession,
    max_bytes: usize,
) -> Result<tcr_peer_wire::Knock>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let frame = read_frame_bounded(stream, max_bytes).await?;
    let mut plaintext = vec![0_u8; MAX_FRAME_BYTES];
    let len = session
        .transport
        .read_message(&frame, &mut plaintext)
        .context("peer knock: this frame did not decrypt under the knock session key")?;
    serde_json::from_slice(&plaintext[..len])
        .context("peer knock: the first frame of a knock session is not a knock")
}

/// Write the one-byte receipt a knock earns, and nothing else.
pub async fn write_knock_ack<S>(stream: &mut S, session: &mut PeerSession) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    send_encrypted(stream, &mut session.transport, &[tcr_peer_wire::KNOCK_ACK]).await
}
