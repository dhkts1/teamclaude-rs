//! The peer-mesh wire contract: every type two `tcr` processes exchange over a
//! Noise-authenticated TCP stream, and nothing else.
//!
//! # The invariant this crate exists to hold
//!
//! **One type here carries a credential, `Control::Handoff`, and nothing else
//! may gain one.** A peer stream carries opaque bytes it cannot read (TUNNEL),
//! an HTTP request the receiving node re-signs with its OWN account (SERVE), or
//! control messages about identity, capability and leases (CONTROL). None of
//! those needs a token on the wire, and a token on the wire makes every
//! forwarding hop a disclosure boundary.
//!
//! The exemption is `hand` mode and it is the whole of that mode: the owner
//! lends its quota by handing the borrower a bearer so the borrowed request
//! leaves the BORROWER's machine, which is the one thing a lender cannot
//! achieve without putting a credential on the wire. What crosses is the
//! SHORT-LIVED access token and never the refresh token, which is the
//! difference between lending an account for a while and giving it away, and it
//! is why revocation is "stop renewing" ([`HandoffToken`]). It crosses a
//! CONTROL stream, which nests end to end inside a TUNNEL, so a forwarding hop
//! holds ciphertext for a session it has no key to.
//!
//! The gate is `wire_has_no_credential_field` in `tests/peer_wire.rs`: a source
//! grep with a positive control, so it fails if the grep itself stops working,
//! and it exempts that one variant's declaration by name and nothing else.
//!
//! # Forward compatibility is not optional here
//!
//! A LAN runs mixed builds by default, one machine updates, the other does
//! not, so every enum carries an unknown arm and the PARSE never fails on a
//! value a newer build invented. The unknown arm keeps the parse alive; the
//! HANDLER refuses it (`src/peer/listener.rs`). Those are different decisions
//! and this crate only makes the first. Degrading an unknown STREAM KIND to
//! anything other than a refusal would be an open relay, which is the one place
//! this differs from `ProxyHost::Unknown` (`src/singleton.rs:105-125`), where
//! degrading to "a proxy is there, do not signal it" is strictly safer.
//!
//! # Nothing here identifies an account, an organization or a person
//!
//! No account uuid, no org uuid, no email, no workspace name, in any message.
//! Lendable capacity is advertised per WINDOW as an aggregate plus a count of
//! backing accounts; a receipt carries an opaque lease id and a spend figure and
//! no account identity at all, because a borrower that does not need to know
//! which account served it should not be told. This repository is public and
//! every fixture would otherwise carry one.
//!
//! # This crate was built against an internal design blueprint
//!
//! That document is not tracked in this repository, so every contract it
//! imposed is restated in full beside the item it constrains: a reader here
//! never needs it.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

/// The wire version both ends state in [`Hello::proto`]. Bumped only for a
/// change no unknown arm can absorb; a field added with a serde default is not
/// such a change.
pub const PROTO_VERSION: u16 = 1;

/// The largest payload one frame may carry, in bytes.
///
/// This is not a tuning knob: a Noise transport message is capped at 65535
/// bytes by the protocol itself, so a `u16` length prefix is exactly the right
/// width and no framing crate is needed. Plaintext is chunked well below it
/// (see `src/peer/tunnel.rs`) so one large body cannot monopolise a splice.
pub const MAX_FRAME_BYTES: usize = 65_535;

/// How many one-level neighbour briefs a [`Hello`] may carry, when the peer is
/// granted `control.briefs` at all. One level only: the table is "my peers plus
/// their peers", and a node three hops out is invisible. That is accepted,
/// it needs no sequence reconciliation and no anti-entropy, and a brief that is
/// wrong costs one failed dial.
pub const MAX_NEIGHBOR_BRIEFS: usize = 8;

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// A peer's identity: the raw 32-byte Noise static X25519 public key.
///
/// **Two words, stated once: a node is this machine, a peer is another machine
/// as seen from here.** So `tcr-node.key` and `PeerId` are both right and
/// neither renames the other.
///
/// The public key itself rather than a hash of it, because the transport needs
/// the raw key to complete an `IK` handshake and a hash would cost a second
/// lookup on every connection.
///
/// # Two forms, and only one of them round-trips
///
/// - The **wire form** is the full key in Crockford base32 (52 characters), and
///   it is what [`Serialize`]/[`Deserialize`] use. Lossless by construction.
/// - The **display form** ([`PeerId::display`]) is `tcr-` plus the first ten
///   base32 characters, about 50 bits, enough for a human to read one row
///   aloud and nowhere near enough to reconstruct the key. It is for panels,
///   log lines and CLI output. **Nothing parses it back**, and
///   [`PeerId::parse`] deliberately refuses it, because a truncated id that
///   silently resolved to a pinned peer would be a prefix-collision attack with
///   a friendly face.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub struct PeerId(pub [u8; 32]);

impl PeerId {
    /// The human-readable short form, `tcr-7f3k9m2q4x`. See the type docs for
    /// why this is one-way.
    pub fn display(&self) -> String {
        let wire = self.to_wire();
        format!("tcr-{}", &wire[..10])
    }

    /// The full, lossless wire form: 52 Crockford base32 characters, no prefix.
    pub fn to_wire(&self) -> String {
        crockford_encode(&self.0)
    }

    /// Parse the full wire form. Refuses the display form. See the type docs.
    pub fn parse(wire: &str) -> Result<Self, PeerIdError> {
        decode_key32(wire).map(PeerId)
    }
}

/// Encode 32 bytes that are **not** an identity (a join secret) in the same
/// Crockford base32 this wire uses for a [`PeerId`].
///
/// Exists so `JoinToken` (`src/peer/pair.rs`) has a codec to call that does not
/// make a secret pass through the identity type on its way to a string. The
/// alternative, borrowing [`PeerId::to_wire`], reads as though a join secret
/// were a node's public key; the alternative to THAT is a second Crockford
/// implementation in the root crate, which is the drift this function exists to
/// prevent.
pub fn encode_key32(bytes: &[u8; 32]) -> String {
    crockford_encode(bytes)
}

/// The inverse of [`encode_key32`]: exactly 32 bytes, or a refusal that names
/// what it found.
pub fn decode_key32(field: &str) -> Result<[u8; 32], Key32Refusal> {
    let bytes = crockford_decode(field)?;
    let got = bytes.len();
    bytes.try_into().map_err(|_| PeerIdError::Length { got })
}

/// Why 32 base32 characters could not be read.
///
/// An alias rather than a second enum: an identity and a join secret differ in
/// MEANING, not in shape, and one refusal type is what keeps two spellings of
/// "that is not 32 Crockford bytes" from drifting apart. The name is here so a
/// caller decoding a SECRET does not have to read `PeerIdError` and wonder
/// which of the two it is holding.
pub type Key32Refusal = PeerIdError;

/// How many bytes an [`InstanceId`] is: eight, minted fresh on every boot.
///
/// Eight and not 32, because this is deliberately NOT identity. See the type
/// docs. Eight bytes is enough that two Macs on one LAN do not collide within a
/// boot and short enough to read off a screen.
pub const INSTANCE_ID_BYTES: usize = 8;

/// The ephemeral, per-boot id a node announces and knocks under.
///
/// **This is not identity and must never be treated as one.** The rule is:
/// announcements carry ephemeral data only: a fresh random 8 bytes per boot,
/// the port, the wire version, never the node id, never the static public key.
/// Identity is the static key, learned inside the `XX` handshake after the
/// operator presses Accept and bound by the six digits both operators compare.
///
/// What it IS for: coalescing. A discovery row, a knock and the 120-second
/// pairing window a knock's Accept opens all have to name the same *thing*
/// somebody is trying to pair with, without that name being a key. So it rides
/// the announcement's TXT record, the knock's payload and the `XX` message-1
/// payload, and the responder compares the three.
///
/// A rotating id buys nothing: the pending queue coalesces by SOURCE ADDRESS
/// (`crate::peer::state`), which is the field a TCP handshake makes real, and
/// the window an Accept opens is keyed to the id that was accepted, so
/// switching id after Accept loses the window rather than gaining a second one.
///
/// Wire form is lower-case hex, 16 characters, because it lands in an mDNS TXT
/// record a person reads with `dns-sd` and Crockford base32 would make the
/// shorter string the harder one to compare by eye.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub struct InstanceId(pub [u8; INSTANCE_ID_BYTES]);

impl InstanceId {
    /// The 16-character lower-case hex form, which is the only form.
    ///
    /// One form on purpose, unlike [`PeerId`]: a truncated instance id would be
    /// a second spelling of a value whose whole job is to be compared for
    /// equality.
    pub fn to_wire(&self) -> String {
        let mut out = String::with_capacity(INSTANCE_ID_BYTES * 2);
        for byte in self.0 {
            out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
            out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
        }
        out
    }

    /// Parse the hex form. Case-insensitive on input, because an operator
    /// retyping one off a screen should not be refused over shift.
    pub fn parse(wire: &str) -> Result<Self, InstanceIdError> {
        let wire = wire.trim();
        if wire.chars().count() != INSTANCE_ID_BYTES * 2 {
            return Err(InstanceIdError::Length {
                got: wire.chars().count(),
            });
        }
        let mut out = [0_u8; INSTANCE_ID_BYTES];
        let bytes = wire.as_bytes();
        // Indexed rather than chunked: the length is already exactly
        // `INSTANCE_ID_BYTES * 2` (checked above), so the pair positions are
        // arithmetic and not an iterator's business.
        for (index, slot) in out.iter_mut().enumerate() {
            let hi = hex_nibble(bytes[index * 2])?;
            let lo = hex_nibble(bytes[index * 2 + 1])?;
            *slot = (hi << 4) | lo;
        }
        Ok(Self(out))
    }

    /// The raw bytes, for the `XX` message-1 payload and the knock.
    pub fn as_bytes(&self) -> &[u8; INSTANCE_ID_BYTES] {
        &self.0
    }
}

fn hex_nibble(byte: u8) -> Result<u8, InstanceIdError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        other => Err(InstanceIdError::Alphabet(char::from(other))),
    }
}

impl std::fmt::Display for InstanceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_wire())
    }
}

impl From<InstanceId> for String {
    fn from(id: InstanceId) -> Self {
        id.to_wire()
    }
}

impl TryFrom<String> for InstanceId {
    type Error = InstanceIdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        InstanceId::parse(&value)
    }
}

/// Why an instance id could not be read. Names what it found, like every other
/// refusal on this wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstanceIdError {
    /// A character outside `[0-9a-fA-F]`.
    Alphabet(char),
    /// Not exactly 16 characters.
    Length {
        /// How many characters arrived.
        got: usize,
    },
}

impl std::fmt::Display for InstanceIdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Alphabet(c) => write!(
                f,
                "instance id: {c:?} is not a hex character (an instance id is 16 of them)"
            ),
            Self::Length { got } => write!(
                f,
                "instance id: {got} characters, need exactly {} (8 bytes as hex)",
                INSTANCE_ID_BYTES * 2
            ),
        }
    }
}

impl std::error::Error for InstanceIdError {}

// ---------------------------------------------------------------------------
// Crockford base32, via `data-encoding`
// ---------------------------------------------------------------------------
//
// `data-encoding` is a runtime dependency of this crate
// (already in `Cargo.lock` as a transitive dependency, so this is zero new
// packages). `JoinToken::to_token`/`parse` (`src/peer/pair.rs`) call
// [`encode_key32`] / [`decode_key32`] below, so there is one Crockford
// implementation on this wire, one alphabet, one translation table, one
// length refusal, and not a hand-rolled one here and a second one there to
// drift apart. That became true later: until then the token's two
// 32-byte fields went through `PeerId::to_wire`, which is the same codec but
// names a secret an identity on the way past.
//
// Crockford's alphabet excludes I, L, O, U; lower-case input is folded to
// upper before lookup, matching the hand-rolled decoder this replaces.

use std::sync::OnceLock;

use data_encoding::{Encoding, Specification};

fn crockford_encoding() -> &'static Encoding {
    static ENCODING: OnceLock<Encoding> = OnceLock::new();
    ENCODING.get_or_init(|| {
        let mut spec = Specification::new();
        spec.symbols
            .push_str(std::str::from_utf8(CROCKFORD_ALPHABET).expect("ascii alphabet"));
        spec.translate.from.push_str("abcdefghjkmnpqrstvwxyz");
        spec.translate.to.push_str("ABCDEFGHJKMNPQRSTVWXYZ");
        spec.encoding()
            .expect("the Crockford alphabet is a valid data-encoding specification")
    })
}

const CROCKFORD_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

fn crockford_encode(bytes: &[u8]) -> String {
    crockford_encoding().encode(bytes)
}

fn crockford_decode(s: &str) -> Result<Vec<u8>, PeerIdError> {
    crockford_encoding()
        .decode(s.as_bytes())
        .map_err(|err| match err.kind {
            // The one refusal that is about a CHARACTER. `err.position` is a
            // BYTE offset into the input and is read as one: a character index
            // happens to agree here, because every symbol this alphabet accepts
            // is one byte and so the first byte the decoder refuses is the first
            // byte of the character it refused, but the two indices are
            // different quantities and only one of them is what was reported.
            data_encoding::DecodeKind::Symbol => PeerIdError::Alphabet(
                s.get(err.position..)
                    .and_then(|rest| rest.chars().next())
                    .unwrap_or('\u{fffd}'),
            ),
            // Everything else is about the input's SIZE or its last symbol's
            // spare bits, and every one of them used to be reported as a bad
            // character: a truncated id named whatever happened to sit at the
            // failure's position, which sent the reader looking for a typo in
            // a string whose characters were all fine.
            _ => PeerIdError::Malformed {
                length: s.chars().count(),
            },
        })
}

impl From<PeerId> for String {
    fn from(id: PeerId) -> Self {
        id.to_wire()
    }
}

impl TryFrom<String> for PeerId {
    type Error = PeerIdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        PeerId::parse(&value)
    }
}

/// Why a peer id on the wire could not be read. Every arm names the offending
/// input, because a refusal that does not is one a reader cannot act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerIdError {
    /// A character outside the Crockford base32 alphabet.
    Alphabet(char),
    /// A decode that produced the wrong number of bytes, most often the
    /// ten-character display form, which is not an identity.
    Length {
        /// How many bytes the input decoded to.
        got: usize,
    },
    /// The input is not a whole peer id: its length is not a whole number of
    /// base32 symbols, or its last symbol carries bits an encoder would never
    /// have set. Distinct from [`Self::Alphabet`] because every character in it
    /// may be perfectly legal, and a refusal that names one of them sends the
    /// reader hunting a typo that is not there.
    Malformed {
        /// How many characters the input held.
        length: usize,
    },
}

impl std::fmt::Display for PeerIdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Alphabet(c) => {
                write!(f, "peer id: {c:?} is not a Crockford base32 character")
            }
            Self::Length { got } => write!(
                f,
                "peer id: decoded {got} bytes, need exactly 32 (the short `tcr-…` \
                 display form is not an identity and cannot be parsed back)"
            ),
            Self::Malformed { length } => write!(
                f,
                "peer id: {length} characters is not a whole peer id; paste all 52 of \
                 them, as `tcr peer ls` prints them"
            ),
        }
    }
}

impl std::error::Error for PeerIdError {}

// ---------------------------------------------------------------------------
// Stream kinds and the header that declares one
// ---------------------------------------------------------------------------

/// What a stream is for, declared in the FIRST TRANSPORT MESSAGE after the
/// handshake completes, never in a handshake payload. See [`StreamHeader`] for
/// why that placement is a security rule rather than a style choice.
///
/// One TCP connection is one Noise session is one stream. There is no
/// multiplexer: role isolation becomes a property of the OS, so a wedged blind
/// tunnel cannot stall a lease request, and the single largest hidden component
/// in the design (a mux plus flow control plus a head-of-line policy, with no
/// analogue anywhere in this tree) does not exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "u16", into = "u16")]
pub enum StreamKind {
    /// Opaque bytes. The hop sees a peer id, a target, byte counts and timing,
    /// and there is nothing else to see: the payload is either the initiator's
    /// own TLS to Anthropic or a nested Noise session it holds no key for.
    Tunnel,
    /// An HTTP request the receiving node serves on its OWN account, seeing the
    /// full request and response plaintext. Needs an explicit grant on BOTH
    /// machines, so an unconfigured mesh is blind-only.
    Serve,
    /// Adverts, enrolment, lease lifecycle, collapse hints. Carries no
    /// credential, ever.
    Control,
    /// "Hold this socket open for me": a Mac nothing can dial, offering the
    /// one thing it can, a socket it opened outwards itself.
    ///
    /// The frame that says it is the LAST frame either side writes on this
    /// stream. What the receiver does with it is hold the raw socket on its
    /// carrier desk until a forward for that Mac needs a way back out, and a
    /// byte written here to prove liveness would land in the middle of
    /// somebody's carried session. So the Noise session this kind is announced
    /// over authenticates WHO is parking and nothing else, and both ends drop
    /// it immediately after.
    Park,
    /// A kind this build does not know: a newer peer on the same LAN. Kept so
    /// the PARSE survives; **the handler refuses it, logs it and closes the
    /// connection.** A kind that degraded to anything else would be an open
    /// relay.
    Unknown(u16),
}

impl From<u16> for StreamKind {
    fn from(raw: u16) -> Self {
        match raw {
            1 => Self::Tunnel,
            2 => Self::Serve,
            3 => Self::Control,
            4 => Self::Park,
            other => Self::Unknown(other),
        }
    }
}

impl From<StreamKind> for u16 {
    fn from(kind: StreamKind) -> Self {
        match kind {
            StreamKind::Tunnel => 1,
            StreamKind::Serve => 2,
            StreamKind::Control => 3,
            StreamKind::Park => 4,
            StreamKind::Unknown(raw) => raw,
        }
    }
}

/// Where a TUNNEL goes. Host-and-port granularity is all a blind hop can
/// enforce, because everything else is inside somebody else's TLS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case")]
pub enum TunnelTarget {
    /// Out to the internet, for a host on the peer egress allow-list. The
    /// gateway also asserts that the first ClientHello's SNI equals this host,
    /// which is the ceiling of what blind validation can be.
    Origin {
        /// The host the initiator asked for. Checked against an explicit
        /// allow-list, DENY BY DEFAULT, the inverse of the local proxy's
        /// blind-tunnel default, which is correct there because that surface is
        /// loopback and wrong on a LAN socket, where it is an open proxy.
        host: String,
        /// The port. Part of the allow-list decision, not a free field.
        port: u16,
    },
    /// On to another peer, and only to one **this** node has itself pinned, so
    /// no node is an open relay to an arbitrary address.
    ///
    /// # A struct variant, and the one line of prose that says why
    ///
    /// This was `Peer(PeerId)` and could not be written to the wire AT ALL:
    /// the enum is internally tagged (`tag = "target"`), `PeerId` serializes
    /// as a string, and serde refuses an internally-tagged newtype variant
    /// whose content is not a map. So every forwarded stream was unreachable
    /// from a client, which is what `tests/peer_egress.rs`'s
    /// `gateway_on_target` had to work around and what its
    /// `a_relay_target_cannot_be_serialized_yet` recorded as a wire-crate
    /// defect awaiting this patch. One named field is the smallest shape that
    /// serializes, and the name is `node` because that is what
    /// [`crate::NeighborBrief`] and the peers file already call a peer's
    /// pinned key.
    Peer {
        /// The pinned key of the Mac to forward to.
        node: PeerId,
    },
}

/// The first transport message on every stream.
///
/// # Why this is not in the handshake payload
///
/// An `IK` message-1 payload is replayable by construction: no responder
/// ephemeral has contributed to it yet. A LAN attacker who captured one could
/// otherwise replay it and make the responder open a TUNNEL that spends a
/// gateway's byte budget, or a SERVE that spends a lender's quota, repeatedly,
/// while learning nothing and authenticating as nobody. `IK` completes in two
/// messages, so carrying this one message later costs nothing measurable.
/// [`Self::request_id`]'s dedup cache is an ACCOUNTING backstop, never this
/// defence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamHeader {
    pub kind: StreamKind,
    /// The target, on a TUNNEL. `None` for SERVE and CONTROL, which terminate
    /// at the node that reads this header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<TunnelTarget>,
    /// Every node this frame has already passed through. A node refuses to
    /// forward a frame whose `via` already contains its own id, which kills a
    /// cycle outright, and never forwards back out the ingress link.
    #[serde(default)]
    pub via: Vec<PeerId>,
    /// Decremented at each forward, refused at zero. **This bounds a FRAME,
    /// not a request**: because a terminal solves its own onward problem
    /// recursively, a node three hops out may open three more on its own
    /// behalf. What bounds cost end to end is each node's grant set plus each
    /// gateway's byte cap.
    pub hops_remaining: u8,
    /// One request, one id, for the whole path. Exists so a diamond delivery,
    /// two disjoint paths converging on one terminal, is debited ONCE. A
    /// `via` stamp cannot catch a diamond, which is why both mechanisms are
    /// here.
    pub request_id: u128,
}

// ---------------------------------------------------------------------------
// Quota vocabulary
// ---------------------------------------------------------------------------

/// Which rate-limit window a lease is against.
///
/// **New on the wire, not a type this tree already has.** What exists in
/// `src/quota.rs` is three separately named `Option<QuotaWindow>` fields
/// (`five_hour` at `:75`, `seven_day` at `:77`, `seven_day_oi` at `:82`); this
/// enum is the wire's name for those three plus a forward-compat arm.
///
/// Only [`Self::SevenDayOi`] is model-scoped upstream
/// (`Quota::model_weekly_exhausted`, `src/quota.rs:271`, whose single
/// production caller sits behind `is_fable &&`), so a [`Self::FiveHour`] or
/// [`Self::SevenDay`] lease is UNTIERED and every panel string that describes
/// one has to say so. A design that advertised per-tier lending for opus would
/// be inventing a bucket upstream does not report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Window {
    /// The rolling five-hour window. Untiered.
    #[serde(rename = "5h")]
    FiveHour,
    /// The weekly window. Untiered, and the one 798 of 911 measured refusals
    /// were bound by.
    #[serde(rename = "7d")]
    SevenDay,
    /// The weekly Fable window, the only model-scoped bucket upstream reports.
    #[serde(rename = "7d_oi")]
    SevenDayOi,
    /// A window this build does not know. Parse survives, handler refuses.
    #[serde(other)]
    Unknown,
}

/// How much of a window is lent, in the unit that window is measured in.
///
/// On the wire this is `{"unit":"fraction","amount":0.2}`, so the unit is a
/// VALUE a reader can see rather than a shape they have to infer. In memory it
/// is one typed value, parsed once, because a unit carried alongside a bare
/// number is a unit some later call site will read with a different
/// assumption.
///
/// # Why the conversion is hand-written and not `#[serde(tag, content)]`
///
/// Measured, not assumed: adjacent tagging plus `#[serde(other)]` refuses
/// `{"unit":"goats","amount":3}` with *"invalid type: integer 3, expected unit
/// variant"*, because the unknown tag routes to a UNIT variant that then
/// cannot hold the content. A newer build's unknown unit always arrives WITH
/// an amount, so the forward-compat arm would have failed on exactly the
/// message it exists for, and a whole `LeaseGrant` would have failed to parse
/// with it. The pair of conversions below is what makes the unknown arm real.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(from = "LeaseUnitWire", into = "LeaseUnitWire")]
pub enum LeaseUnit {
    /// For OAuth accounts, whose only upstream measure is a utilization
    /// fraction (`QuotaWindow::utilization`, `src/quota.rs:47`).
    Fraction(f64),
    /// For api-key accounts, which really do report exact counts
    /// (`tokens_limit` / `tokens_remaining`, `src/quota.rs:86-87`). **Defined
    /// here and refused at runtime** until a fleet with such an account wants
    /// to lend one, so the refusal is typed rather than a surprise.
    Tokens(u64),
    /// A unit this build does not know, or an amount it cannot read as the unit
    /// claims. Parse survives, handler refuses.
    Unknown,
}

/// The wire shape of [`LeaseUnit`]. Private: nothing outside this crate should
/// hold an un-parsed unit.
///
/// `amount` is a `Value` rather than a number so that an unknown unit with an
/// amount of any shape (or with none) still parses into
/// [`LeaseUnit::Unknown`] instead of failing the message it rode in on.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LeaseUnitWire {
    unit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    amount: Option<serde_json::Value>,
}

impl From<LeaseUnitWire> for LeaseUnit {
    fn from(wire: LeaseUnitWire) -> Self {
        let amount = wire.amount.as_ref();
        match wire.unit.as_str() {
            "fraction" => amount
                .and_then(serde_json::Value::as_f64)
                .map_or(Self::Unknown, Self::Fraction),
            "tokens" => amount
                .and_then(serde_json::Value::as_u64)
                .map_or(Self::Unknown, Self::Tokens),
            _ => Self::Unknown,
        }
    }
}

impl From<LeaseUnit> for LeaseUnitWire {
    fn from(unit: LeaseUnit) -> Self {
        match unit {
            LeaseUnit::Fraction(fraction) => Self {
                unit: "fraction".to_string(),
                amount: Some(serde_json::Value::from(fraction)),
            },
            LeaseUnit::Tokens(tokens) => Self {
                unit: "tokens".to_string(),
                amount: Some(serde_json::Value::from(tokens)),
            },
            LeaseUnit::Unknown => Self {
                unit: "unknown".to_string(),
                amount: None,
            },
        }
    }
}

/// What a node advertises it could lend, per window.
///
/// **Aggregate per window plus a count, never per account.** Three reasons
/// compound: an advert reaches a node before any lease exists and an account
/// uuid is upstream-issued identity; this repository is public, so every
/// fixture would carry one; and the borrower never picks the lender's account
/// anyway, because the lender's own picker does.
///
/// "Lend me two whole accounts" is NOT expressible here and that is deliberate:
/// a lender cannot promise which of its accounts will serve, because its own
/// picker chooses per request. [`Self::accounts`] is a confidence hint, "this
/// fraction is backed by four accounts, not one", and never a unit anyone can
/// spend.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Lendable {
    /// The window. The blueprint's prose calls this a "tier" in §6 and a
    /// "window" in §7; one name wins here, and it is the one `src/quota.rs`
    /// already uses for the same three buckets.
    pub window: Window,
    pub unit: LeaseUnit,
    /// How many of the lender's accounts back this figure. A hint, not a unit.
    pub accounts: u8,
}

/// A `u128` id as lower-case hex, because **a `u128` cannot survive this
/// crate's own message envelope as a number.**
///
/// Measured, not theorised. [`Control`] is an INTERNALLY TAGGED enum
/// (`#[serde(tag = "type")]`), and serde deserializes one by buffering the frame
/// into its private `Content` tree before dispatching on the tag. That tree has
/// `U64` and `I64` and no 128-bit arm, so `ContentDeserializer::deserialize_u128`
/// answers `Err("u128 is not supported")`. A `Control::LeaseGrant` therefore
/// SERIALIZED perfectly and could never be read back:
///
/// ```text
/// WIRE: {"type":"lease_grant","lease":{"leaseId":22685491128062564230891640495451214097,...}}
/// BACK: Err(Error("u128 is not supported", line: 0, column: 0))
/// ```
///
/// which is why a borrower asking for a lease failed with "a frame did not
/// parse" against a lender that had just granted one. The same limitation bites
/// a second time on disk: `peer-state.json` carries a `#[serde(flatten)]`
/// unknown-key map, and a flattened struct serializes through `serde_json`'s
/// `Value`, which has no `u128` variant either.
///
/// So the id goes on the wire as a STRING. That is also the shape this tree
/// already chose for the same number in a file, `LendGrant::id` in
/// `src/peer/config.rs`, whose doc gives the second reason: "a `u128` in JSON is
/// a number no double can hold, so every reader that parses JSON into doubles,
/// the panel among them, would silently round it and then revoke a lease
/// nobody minted." One spelling, two readers, and the id an operator pastes
/// into `--revoke` is the id on the wire.
mod id_hex {
    use serde::de::Error as _;
    use serde::{Deserialize as _, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(id: &u128, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("{id:032x}"))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u128, D::Error> {
        let raw = String::deserialize(deserializer)?;
        let trimmed = raw.trim();
        let trimmed = trimmed.strip_prefix("0x").unwrap_or(trimmed);
        u128::from_str_radix(trimmed, 16)
            .map_err(|err| D::Error::custom(format!("{raw:?} is not a 32-character hex id: {err}")))
    }
}

/// A granted lease: a budget with a deadline.
///
/// It is not a rate limiter, the in-process GCRA (`src/manager/throttle.rs`)
/// is untouched by all of this, and it is not a fixed-window limiter either.
/// The lender's copy is authoritative; the borrower's is a cached hint and
/// every surface that renders it says so.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Lease {
    /// Opaque. Carries no account or org identity, by construction.
    ///
    /// **Hex on the wire**, and [`id_hex`] is where the measurement is: as a
    /// JSON number this field made every `Control::LeaseGrant` unparseable.
    #[serde(with = "id_hex")]
    pub lease_id: u128,
    pub window: Window,
    pub unit: LeaseUnit,
    pub granted_at_ms: i64,
    /// An ABSOLUTE deadline. A lease expires on a wall clock and never on a
    /// missed heartbeat: sleep is the normal state for a laptop, and a
    /// heartbeat-based lease safety reduces to a timeout with extra steps.
    pub expires_at_ms: i64,
    /// What the lender has debited so far, in [`Self::unit`]'s unit.
    pub spent: f64,
    /// How many relayed requests may be in flight at once. Small on purpose:
    /// the debit reads headers this tree documents as lagging, so unbounded
    /// concurrency can overdraw a small lease before the first debit lands.
    /// This caps the overdraft at a known number of requests.
    pub max_inflight: u8,
    /// When the LENDING itself ends, in absolute unix SECONDS, **the one time
    /// field a borrower may know**, and the only field added to
    /// this message.
    ///
    /// Separate from [`Self::expires_at_ms`], and the difference is the whole
    /// reason it is a second field rather than a smaller `expires_at_ms`:
    /// `expires_at_ms` is the RENEWAL ttl, a few minutes, and a borrower that
    /// reaches it asks again; `until` is the operator's "lend this until 18:00",
    /// and a borrower that reaches it is done. Collapsing the two would make
    /// "ask again in a moment" and "this is over" the same fact, which is
    /// exactly the distinction [`LeaseRefusal::InFlightFull`]'s doc says a
    /// borrower must be able to draw.
    ///
    /// `None` is "no end", which is the default an operator gets by not asking
    /// for one. Seconds and not milliseconds because it is rendered as a clock
    /// time ("ends in 1 h") and is set from one, so a millisecond field would
    /// carry three digits no surface reads.
    ///
    /// It names no account and no organization: it is a wall-clock instant, so
    /// it is the one piece of the lender's own lending policy that can cross
    /// the wire without carrying the scope with it. See [`LendScope`], which
    /// never does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<u64>,
}

/// What one lease draws from, **on the lender and only on the lender**.
///
/// # Why it is in this crate and never in a message
///
/// It lives here because the lender's own files, its CLI and its ledger all
/// need one name for it, and this crate is where the peer surface's shared
/// types live. It is **not** a field of [`Lease`], [`LeaseRequest`],
/// [`LeaseGrant`] or [`Lendable`], and the reason is explicit: a
/// scope names the lender's ACCOUNTS or its GROUPS, both of which are the
/// lender's private vocabulary, and the borrower never picks which account
/// serves anyway because the lender's own picker does. So the wire carries the
/// window, the amount, the ttl and the lease id, and a borrower cannot learn
/// from a lease which account paid for it.
///
/// The gate that holds this is not this comment: it is
/// `every_wire_type_serializes_only_allowlisted_keys` in `tests/peer_wire.rs`,
/// which asserts the exact key set of every message. Putting a scope on a
/// message fails it.
///
/// [`Self::Accounts`] holds **sanitized labels** ([`sanitize_label`]), never an
/// email and never a uuid, this repository is public and a lease scope is
/// written to a file and printed by a CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LendScope {
    /// Every account this node could lend on, which is the shape every lease
    /// had before scopes existed.
    All,
    /// One `tcr group`, resolved by NAME at serve time and never frozen into a
    /// list of members: groups hot-reload (`Manager::reload_groups_if_changed`),
    /// so a lease scoped to `work` follows the group when an account joins or
    /// leaves it. Freezing the membership at lend time would silently keep
    /// lending an account the operator had just moved out.
    Group(String),
    /// One or more named accounts. A single account is this with one element
    /// rather than a fourth variant: one code path, one parser, and no caller
    /// that has to handle "the singular one" separately.
    Accounts(Vec<String>),
}

impl LendScope {
    /// Parse the CLI's `--scope` spelling: `all`, `group:<name>`, or
    /// `account:<label>[,<label>…]`.
    ///
    /// One parser, used by the CLI and by anything that reads a scope back out
    /// of a file as text, because two spellings of "what does `group:work`
    /// mean" is how a lease comes to draw from somewhere the operator did not
    /// name.
    ///
    /// Every label goes through [`sanitize_label`] here rather than at the call
    /// site: the refusal an operator needs ("that looks like a uuid") is the
    /// one this crate already writes, and a scope that skipped it could put an
    /// email in a peers file.
    pub fn parse(spec: &str) -> Result<Self, LendScopeRefusal> {
        let spec = spec.trim();
        if spec.eq_ignore_ascii_case("all") {
            return Ok(Self::All);
        }
        if let Some(name) = spec.strip_prefix("group:") {
            let name = sanitize_label(name.trim()).map_err(LendScopeRefusal::Label)?;
            return Ok(Self::Group(name));
        }
        // `account:` and `accounts:` both, because an operator naming two of
        // them writes the plural and a refusal over an `s` is a refusal that
        // teaches nothing.
        let list = spec
            .strip_prefix("account:")
            .or_else(|| spec.strip_prefix("accounts:"));
        if let Some(list) = list {
            let mut labels = Vec::new();
            for raw in list.split(',') {
                let raw = raw.trim();
                if raw.is_empty() {
                    continue;
                }
                labels.push(sanitize_label(raw).map_err(LendScopeRefusal::Label)?);
            }
            if labels.is_empty() {
                return Err(LendScopeRefusal::NoAccounts);
            }
            return Ok(Self::Accounts(labels));
        }
        Err(LendScopeRefusal::Shape {
            spec: spec.to_string(),
        })
    }

    /// The spelling [`Self::parse`] accepts, so a round trip through the CLI's
    /// own vocabulary is lossless and a printed scope can be pasted back.
    pub fn to_spec(&self) -> String {
        match self {
            Self::All => "all".to_string(),
            Self::Group(name) => format!("group:{name}"),
            Self::Accounts(labels) => format!("account:{}", labels.join(",")),
        }
    }
}

impl Default for LendScope {
    /// Every account, which is what a lease meant before scopes existed, so a
    /// grant written by an older build, or by an operator who named no scope,
    /// keeps behaving exactly as it did.
    fn default() -> Self {
        Self::All
    }
}

impl std::fmt::Display for LendScope {
    /// The operator-facing form, which is the same string [`Self::to_spec`]
    /// produces: one spelling for the CLI's input, its output and a panel row,
    /// because three spellings of one scope is three chances to render a lease
    /// as drawing from somewhere it does not.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_spec())
    }
}

/// Why a `--scope` spelling was refused.
///
/// No `Serialize` derive, like [`LabelRefusal`] and [`Key32Refusal`]: a refusal
/// is something an operator reads, never something that crosses this wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LendScopeRefusal {
    /// Not `all`, `group:…` or `account:…` at all.
    Shape {
        /// What was passed, so the message names it.
        spec: String,
    },
    /// `account:` with nothing after it, or only commas.
    NoAccounts,
    /// One of the labels is not a label. See [`sanitize_label`].
    Label(LabelRefusal),
}

impl std::fmt::Display for LendScopeRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Shape { spec } => write!(
                f,
                "scope: {spec:?} is not a scope; write `all`, `group:<name>` or \
                 `account:<label>[,<label>]`"
            ),
            Self::NoAccounts => write!(
                f,
                "scope: `account:` names no account; write `account:<label>[,<label>]`"
            ),
            Self::Label(refusal) => write!(f, "scope: {refusal}"),
        }
    }
}

impl std::error::Error for LendScopeRefusal {}

// ---------------------------------------------------------------------------
// CONTROL messages
// ---------------------------------------------------------------------------

/// What a node tells one pinned peer about itself, on connect, on change, and
/// on a slow interval.
///
/// **Assembled from that peer's grant set, never filtered on the way out.** A
/// bare pin receives booleans and addresses and nothing countable; the three
/// optional blocks below each need their own grant. The difference matters: a
/// filter is something a later edit forgets to apply, an assembly is something
/// a later edit cannot reach.
///
/// Quota headroom is NEVER advertised here. It travels only in a
/// [`LeaseGrant`], at the moment of need, where the answer is the offer, a
/// fraction changes on every request, so an advertised one is stale on arrival.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hello {
    pub proto: u16,
    pub node: PeerId,
    /// The operator's own label for this machine. **The one free-text field
    /// that reaches another host**, so it is sanitized at every entry point by
    /// [`sanitize_label`], not only at the discovery beacon, because a label
    /// reaches every pinned peer through this message whether discovery ever
    /// ships or not.
    pub label: String,
    /// Monotonic per sender, so a stale advert is ignorable without a clock
    /// comparison.
    pub seq: u64,
    pub caps: Caps,
    /// Addresses this node can be reached on. Routing advice, not identity:
    /// identity is re-proven by the handshake against a pinned static key.
    pub addrs: Vec<String>,
    /// How long this advert may be believed.
    pub ttl_s: u32,
    /// Needs `allow.control.lendable` on the receiving side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lendable: Option<Vec<Lendable>>,
    /// Needs `allow.control.lendable`. `None` means "not told", never "no
    /// egress". [`Caps::egress`] answers that question.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hops_to_egress: Option<u8>,
    /// One level of neighbour briefs, re-advertised with split horizon (never
    /// back out the link they were learned from). Needs
    /// `allow.control.briefs`, and is capped at [`MAX_NEIGHBOR_BRIEFS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub briefs: Option<Vec<NeighborBrief>>,
    /// Needs `allow.control.diag`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_sha: Option<String>,
    /// Needs `allow.control.diag`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_id: Option<u64>,
    /// The source address this Hello's SENDER saw the RECEIVER arrive from.
    ///
    /// The one field here that is about the reader rather than the writer, and
    /// the only way a Mac behind a NAT learns the address the rest of the
    /// world reaches it at without a STUN server: the other end of an
    /// authenticated session already knows, so it says so.
    ///
    /// Behind no grant, because it discloses nothing the receiver does not
    /// already own: it is that machine's own public address, told back to it.
    /// Optional and skipped when absent, so a peer on an older build that
    /// never sets it reads the same message it read before, and one that does
    /// not know the field ignores it.
    ///
    /// A string for the same reason [`Self::addrs`] is one: a socket address
    /// that does not parse is a routing hint dropped, never a frame refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_you_at: Option<String>,
}

/// The three booleans a bare pin is told, and the whole of what it is told.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Caps {
    /// This node can reach Anthropic.
    pub egress: bool,
    /// This node will forward to peers it has pinned.
    pub forward: bool,
    /// This node lends quota to someone, on some window. Which windows and how
    /// much is [`Hello::lendable`], and that needs a grant.
    pub lends: bool,
}

/// One hop out, as learned from a neighbour. Deliberately the smallest thing
/// that lets a node try a second-hop dial: a brief that is wrong costs one
/// failed dial, so it needs no reconciliation protocol behind it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NeighborBrief {
    pub node: PeerId,
    pub caps: Caps,
    pub addrs: Vec<String>,
}

/// A joiner presenting itself under an outstanding invite. The invite's secret
/// is proved as the Noise PSK during the handshake, so it does NOT appear here:
/// by the time this message is read, the joiner has already proved it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Enroll {
    /// Which invite this is against, so a registrar holding several can retire
    /// the right row.
    pub invite_id: u64,
    /// The joiner's own label for itself. Sanitized on arrival by
    /// [`sanitize_label`], same as every other entry point.
    pub label: String,
}

/// Phase one of pairing: "someone at this address would like to pair".
///
/// Sent on a `Noise_NN` session, **ephemeral keys on both sides, no static
/// key anywhere in the handshake**, so a knock discloses neither end's
/// identity. The rule is: "the requester opens a Noise `NN` session (ephemeral
/// keys only, no static on either side) and sends `{instance_id,
/// proposed_name, wire_version}`; the responder queues it and shows it in the
/// Peers tab as '<name or address> wants to pair' with Accept / Ignore;
/// nothing else happens, no static key is revealed."
///
/// The responder answers one byte ([`KNOCK_ACK`]) and closes. That one byte is
/// the whole of what a stranger learns: something is listening and it took the
/// knock. It is not an approval and it is not a capability.
///
/// # Every field here is a CLAIM, and two of the three prove nothing
///
/// [`Self::instance_id`] is ephemeral by construction and rotatable at will;
/// [`Self::proposed_name`] is attacker-chosen text. Only the source address,
/// which is not in this message, because a TCP handshake already made it real,
/// is worth coalescing on. So the queue keys on the address and this message
/// is what the operator READS beside it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Knock {
    /// Who is asking, as an ephemeral per-boot id. See [`InstanceId`]: not
    /// identity, and the responder treats it as a label to coalesce a retry on.
    pub instance_id: InstanceId,
    /// The name it would like to be shown as. **Untrusted text**, run through
    /// [`sanitize_label`] on arrival like every other free-text field on this
    /// wire; `None` when it offers none, and then the row shows its address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_name: Option<String>,
    /// The wire version it speaks, so a responder on a different one can say so
    /// in the pending row instead of the operator discovering it at `XX`.
    pub wire_version: u16,
}

/// The single byte a responder writes back on a knock, and the only byte a
/// knock ever earns. See [`Knock`]: it is a receipt, never an approval.
pub const KNOCK_ACK: u8 = 0x01;

/// "May I spend some of your window?" One attempt per request, never per
/// account, so the borrower's retry budget is undisturbed.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseRequest {
    pub window: Window,
    /// How much is wanted. The lender answers with what it will actually give.
    pub unit: LeaseUnit,
    pub ttl_s: u32,
    pub max_inflight: u8,
}

/// The lender's answer. **The answer is the offer**: there is no negotiation
/// round, because a fraction changes on every request and a second round trip
/// would be spent on a stale number.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseGrant {
    /// `None` is a refusal, and [`Self::refusal`] says which one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<Lease>,
    /// Why nothing was granted. Ordered vocabulary, so a borrower can tell "ask
    /// again later" from "never ask again": see `src/peer/lease.rs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<LeaseRefusal>,
}

/// Why a lease was refused, in the order the lender checks them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LeaseRefusal {
    /// The lease's absolute deadline has passed.
    LeaseExpired,
    /// The lease's budget is used up.
    LeaseSpent,
    /// **Fires on its own, even with budget left on the lease.** The lender's
    /// own guard band is the owner's, and a lease can never spend it.
    OwnerGuard,
    /// **This peer already holds the most leases one peer may hold here.**
    /// The refusal is about how many leases this peer holds, not about the
    /// ask or the owner's headroom, a borrower that reads this one knows to
    /// wait for one of its own leases to expire or spend, rather than
    /// retrying with a smaller ask or suspecting the lender's own guard.
    ///
    /// Before this variant existed, `src/peer/lease.rs`'s
    /// `MAX_LEASES_PER_PEER` cap enforced the same rule and answered
    /// `OwnerGuard` for lack of a truer word.
    TooManyLeases,
    /// This peer has not been granted `inspect` here, so SERVE is not on offer
    /// at all.
    InspectNotGranted,
    /// A window this build does not know, or a unit it refuses.
    Unsupported,
    /// **The lease is fine; this REQUEST is the (n+1)th at once.** The lease is
    /// live, funded and inside the lender's guard, and `max_inflight` is full.
    ///
    /// Its own variant rather than a reuse of [`Self::LeaseSpent`] because the
    /// two tell a borrower opposite things: spent means stop asking, a full pipe
    /// means retry in a moment. A borrower that read one for the other either
    /// hammers a dead lease or abandons a live one.
    ///
    /// `src/peer/lease.rs`'s `RelayRefusal::TooManyInflight` could enforce the
    /// cap locally but had no wire word for it until this variant existed.
    InFlightFull,
    /// The grant that would have minted this lease has a schedule (decision
    /// row 14), and `now` falls outside it. Distinct from
    /// [`Self::InspectNotGranted`]: the peer *is* granted `inspect` on this
    /// window, just not at this hour, so a borrower that reads this one knows
    /// to retry later in the window rather than to stop asking altogether.
    ///
    /// The check itself (`src/peer/lease.rs`'s `schedule_refusal`) is not yet
    /// wired into `Ledger::grant`: it reads `LendGrant::schedule`, a field that
    /// has not landed.
    OutsideSchedule,
}

/// What the lender debited for one relayed request. Opaque lease id, a spend
/// figure, and **no account or org identifier at all**.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseReceipt {
    /// Hex on the wire. See [`id_hex`]. A `LeaseReceipt` travels inside
    /// [`Control`] like a grant does, so it had the identical defect.
    #[serde(with = "id_hex")]
    pub lease_id: u128,
    /// The request this debit is for, so a retried or diamond-delivered relay
    /// debits once. Hex on the wire. See [`id_hex`].
    #[serde(with = "id_hex")]
    pub request_id: u128,
    pub spent: f64,
}

/// "You reached me through a middle hop; here is how to reach me directly."
///
/// **Unauthenticated routing advice that needs no trust**: identity is
/// re-proven by the handshake against the pinned static key, so a lying hint
/// fails, and a hint naming an unpinned node is refused before a socket opens.
/// That one sentence is what makes chain collapse free.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CollapseHint {
    pub node: PeerId,
    pub addrs: Vec<String>,
    pub observed_rtt_ms: u32,
}

/// The last phase, and the only one that moves a credential between hosts.
///
/// Empty on purpose: the destination **pulls** the minted token rather than
/// being handed one, so what this message carries is an offer and nothing else.
/// Its fields are the phase that builds it to define, against a revoke verb
/// that does not exist yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveOffer {}

/// The one credential this wire carries, in a type that will not print itself.
///
/// # Why a type and not a `String`
///
/// A review found the listener refusing an unexpected frame with
/// `bail!("{other:?}")`, and that error is logged. A derived `Debug` on
/// [`Control::Handoff`] put the owner's plain bearer in the log line of any
/// node that received one out of turn. Redacting at the ONE call site would
/// have left the next formatter to find, so the redaction is on the value: a
/// `{:?}` of this token is `<redacted>` wherever it is reached from, and
/// reading the bytes takes [`Self::reveal`], which is greppable.
///
/// It is transparent on the wire: the frame's JSON is a plain string, exactly
/// as it was before this type existed, so a peer on an older build reads it
/// unchanged.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HandoffToken(String);

impl HandoffToken {
    /// Wrap the owner's short-lived bearer for the one frame that carries it.
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// The bytes, for the one caller that has to send them upstream.
    ///
    /// Named `reveal` rather than `as_str` so that every place the plaintext
    /// is taken out is one grep away, which is what makes "it is never logged"
    /// a checkable claim rather than a hope.
    pub fn reveal(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for HandoffToken {
    /// No length, no prefix, no first four characters: a fingerprint of a
    /// short-lived bearer is still a fact about a credential, and the reader of
    /// a log line does not need one to know which frame this was.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Everything a CONTROL stream can say.
///
/// **CONTROL is a stream kind, not an HTTP route.** The peer listener speaks no
/// HTTP at all, so there is no `Host` header, no Bearer, no api-key header and
/// no route to accidentally expose.
///
/// Through a forwarder, a CONTROL stream is a FRESH end-to-end Noise session
/// nested inside a TUNNEL, exactly as SERVE is. Stated negatively because an
/// enumeration is a list with an item missing, and the item missed last time
/// was this one: a forwarder that could read CONTROL could forge a
/// [`LeaseGrant`], which is a free lease at the lender's expense.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Control {
    Hello(Hello),
    Enroll(Enroll),
    LeaseRequest(LeaseRequest),
    LeaseGrant(LeaseGrant),
    LeaseReceipt(LeaseReceipt),
    CollapseHint(CollapseHint),
    MoveOffer(MoveOffer),
    /// **The one named exception to "no credential on this wire"** (decision
    /// 15, 2026-09-18): a `hand`-mode grant lets the borrower send on its own
    /// IP, so the owner hands it the account's short-lived access token and
    /// keeps the refresh token.
    ///
    /// The exception is this variant and nothing else. It is sent ONLY inside
    /// an `IK`-authenticated CONTROL session, ONLY to the grantee of
    /// `lease_id`, and only while that lease lives: the owner pushes a fresh
    /// token before `expires_at_ms` and stops at the lease's `until` or on
    /// revoke. Revocation IS "stop renewing": there is no recall message,
    /// because a token already on another machine cannot be taken back.
    ///
    /// A forwarder never sees it: a CONTROL stream through a hop is a fresh
    /// end-to-end Noise session, as the enum's own doc says.
    ///
    /// `wire_has_no_credential_field` (`tests/peer_wire.rs`) exempts exactly
    /// this declaration's span and nothing else, so `refresh_token` here, or
    /// `access_token` on any other type, is still a red test.
    #[serde(rename_all = "camelCase")]
    Handoff {
        /// Which lease this token is for. Hex on the wire, see [`id_hex`].
        #[serde(with = "id_hex")]
        lease_id: u128,
        /// The owner's short-lived bearer, to be held in memory by the
        /// borrower and never written to a config or a log. See
        /// [`HandoffToken`], whose whole job is that the line above stays true
        /// when something formats this frame.
        access_token: HandoffToken,
        /// When the bearer above stops working, absolute, so the borrower can
        /// stop using it without waiting for a 401.
        expires_at_ms: i64,
        /// The owner's own utilization on the lease's window at the instant
        /// this frame went out, raw, with no guard band applied.
        ///
        /// **It is the baseline a [`Self::UsageHint`] is a rise above.** The
        /// borrower never sees the owner's window except through the rate-limit
        /// headers on its own answers, so without a reading from before the
        /// first of them there is no pair to subtract and the first answer on
        /// every lease is free.
        ///
        /// `None` is not a figure of zero and is never sent by this build: it
        /// is what a peer OLDER than this one leaves out, and the borrower
        /// refuses such a handoff by the lender's name rather than storing a
        /// bearer whose spend it could never report. An old BORROWER ignores
        /// the field and keeps its own build's behaviour, which is why the
        /// field is added rather than the frame replaced.
        #[serde(default)]
        utilization: Option<f64>,
    },
    /// What a hand-mode borrow just cost, reported by the BORROWER.
    ///
    /// In serve mode the owner reads the rate-limit headers off its own
    /// response and debits the lease itself. In hand mode the response never
    /// touches the owner's Mac, so the only party that sees those headers is
    /// the borrower, and a lease with no debit at all never ends. This frame is
    /// the borrower saying what the headers moved by.
    ///
    /// **It is a HINT, and the name is the contract.** It is self-reported by
    /// the party that benefits from under-reporting, so it can only ever raise
    /// `spent` and it is never the owner's only defence: `until`, the lease
    /// expiry and stopping renewal all hold without it. Reconciling it against
    /// the owner's own usage probe (`src/manager/probing.rs`) is out of scope
    /// here and is what would make it authoritative.
    #[serde(rename_all = "camelCase")]
    UsageHint {
        /// Which lease to charge. Hex on the wire, see [`id_hex`].
        #[serde(with = "id_hex")]
        lease_id: u128,
        /// The rise the borrower observed on the window's utilization, in the
        /// lease's own unit.
        spent: f64,
    },
    /// Liveness on an open stream, and nothing more: it carries no number and
    /// answers no question about a path.
    ///
    /// **Still not a failure detector.** [`Control::Probe`] measures what a
    /// live session costs, round trip and loss, and a peer that does not
    /// answer one is a row with a last-seen, exactly as before: no death is
    /// declared, no peer is evicted, and sleeping remains the normal state for
    /// a laptop.
    ///
    /// A unit variant on an internally tagged enum, and it stays one: adding a
    /// field here would make every frame this build writes unreadable to a peer
    /// that has not upgraded, which is why the measurement went into new
    /// variants instead.
    Ping,
    /// "How far away are you, right now?" One round trip on a live session.
    ///
    /// `nonce` matches an answer to its question, so a late
    /// [`Control::ProbeAck`] cannot be counted as the next probe's reply.
    /// `sent_ms` is the SENDER's clock and is echoed back untouched: it is a
    /// debugging aid and never the measurement, because two Macs' clocks differ
    /// by whatever NTP last left them. The round trip is measured by the asker,
    /// against its own clock, on the one timeline that has no skew.
    ///
    /// A build older than this one parses it as [`Control::Unknown`] and closes
    /// the session; the prober reads one such close as "this peer does not
    /// probe" and stops asking for the rest of that session.
    Probe {
        nonce: u64,
        sent_ms: i64,
    },
    /// The echo of a [`Control::Probe`]: the same `nonce` and the same
    /// `sent_ms`, carried back unexamined.
    ProbeAck {
        nonce: u64,
        sent_ms: i64,
    },
    /// "Meet me on the port our pair derives, at this slot."
    ///
    /// The one message a hole punch needs, and it carries no port: both ends
    /// compute that from the pairing secret and the slot number, so a reader
    /// of this frame who is not the pair learns an address that is already
    /// public and a number that is already a clock.
    ///
    /// It may be carried BLIND by a mutual friend, inside a TUNNEL, exactly as
    /// any other CONTROL frame is: the friend sees a nested Noise session and
    /// not this. That is the case it exists for, since a Mac nobody can dial
    /// is a Mac that cannot be told anything directly.
    ///
    /// A build older than this one parses it as [`Self::Unknown`] and refuses
    /// it, which costs the asker one punch nobody turned up to.
    #[serde(rename_all = "camelCase")]
    PunchAt {
        /// The slot to start in, chosen by the sender. The receiver takes this
        /// number rather than deriving one from its own clock, or the two
        /// would start in different slots whenever the frame crossed a
        /// boundary.
        slot: u64,
        /// The address the sender believes it is reachable at, learned from
        /// what its peers see it as. Routing advice, like every other address
        /// on this wire: identity is the handshake the punched socket then
        /// runs.
        public_addr: String,
    },
    /// A message this build does not know. Parse survives, handler refuses.
    #[serde(other)]
    Unknown,
}

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

/// `u16` big-endian length prefix, then one Noise message. Pure functions, no
/// codec crate, no async: everything they need is in the slice they are given.
///
/// Appends the prefix and the payload to `out`. Written rather than stubbed
/// because the whole of it is the sentence above, and because a stub here
/// would leave the two `todo!`s that decide a peer-controlled length as the
/// one place in this crate where a reader could not check the bounds.
pub fn encode_frame(payload: &[u8], out: &mut Vec<u8>) -> Result<(), FrameError> {
    let len = payload.len();
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLong { len });
    }
    // The cast cannot truncate: the bound above is exactly `u16::MAX`.
    out.extend_from_slice(&(len as u16).to_be_bytes());
    out.extend_from_slice(payload);
    Ok(())
}

/// Reads one frame off the front of `buf`.
///
/// Returns the payload and how many bytes of `buf` it consumed, or
/// [`FrameError::Incomplete`] when the caller must read more, never a partial
/// frame and never a panic on a short buffer, **because a peer controls the
/// length prefix.** That is why both bounds checks are explicit and why
/// neither indexes before comparing.
pub fn decode_frame(buf: &[u8]) -> Result<(&[u8], usize), FrameError> {
    let Some(prefix) = buf.get(..2) else {
        return Err(FrameError::Incomplete);
    };
    // Two bytes, so the array conversion cannot fail.
    let len = u16::from_be_bytes([prefix[0], prefix[1]]) as usize;
    match buf.get(2..2 + len) {
        Some(payload) => Ok((payload, 2 + len)),
        None => Err(FrameError::Incomplete),
    }
}

/// Why a frame could not be read or written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// Fewer bytes than the prefix promises. The caller reads more and retries;
    /// this is not an error condition on a stream.
    Incomplete,
    /// A payload above [`MAX_FRAME_BYTES`], which a Noise transport message
    /// cannot carry.
    TooLong {
        /// The payload length that was refused.
        len: usize,
    },
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Incomplete => write!(f, "frame: incomplete, read more bytes"),
            Self::TooLong { len } => write!(
                f,
                "frame: payload of {len} bytes exceeds the {MAX_FRAME_BYTES}-byte \
                 Noise transport message limit"
            ),
        }
    }
}

impl std::error::Error for FrameError {}

// ---------------------------------------------------------------------------
// The label sanitizer
// ---------------------------------------------------------------------------

/// The ONE sanitizer every label passes through, wherever a label is accepted:
/// `tcr peer invite --label`, `tcr peer join`, the peers-file loader, and the
/// discovery beacon.
///
/// One implementation rather than a check per entry point, because this
/// repository is public and a label is the one free-text field that reaches
/// another machine, a beacon and a fixture. A second copy would drift, and the
/// copy that drifts is the one on the entry point added later.
///
/// Refuses: an `@` (an email address), the `8-4-4-4-12` hex shape (a UUID),
/// and anything over the length cap. Every refusal names what it found,
/// because a refusal a reader cannot act on sends them to the source.
///
/// **The uuid check runs BEFORE the length cap**: a
/// canonical uuid is 36 bytes, over [`MAX_LABEL_BYTES`]'s 32, so a length
/// check that ran first would answer `TooLong` and its own `UuidShape`
/// refusal would never be reachable for the one input shape it names.
pub fn sanitize_label(raw: &str) -> Result<String, LabelRefusal> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(LabelRefusal::Empty);
    }
    if is_uuid_shape(trimmed) {
        return Err(LabelRefusal::UuidShape);
    }
    if trimmed.len() > MAX_LABEL_BYTES {
        return Err(LabelRefusal::TooLong { len: trimmed.len() });
    }
    for c in trimmed.chars() {
        if !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ' ')) {
            return Err(LabelRefusal::Character(c));
        }
    }
    Ok(trimmed.to_string())
}

/// The `8-4-4-4-12` hex shape: an account or organization uuid.
fn is_uuid_shape(s: &str) -> bool {
    const GROUP_LENS: [usize; 5] = [8, 4, 4, 4, 12];
    let groups: Vec<&str> = s.split('-').collect();
    groups.len() == GROUP_LENS.len()
        && groups
            .iter()
            .zip(GROUP_LENS)
            .all(|(group, len)| group.len() == len && group.chars().all(|c| c.is_ascii_hexdigit()))
}

/// The longest label that is still a label.
pub const MAX_LABEL_BYTES: usize = 32;

/// Why a label was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LabelRefusal {
    /// A character no label may carry, named so the operator can fix it.
    Character(char),
    /// The `8-4-4-4-12` hex shape. An account or organization uuid is
    /// upstream-issued identity and never travels between peers.
    UuidShape,
    /// Over [`MAX_LABEL_BYTES`].
    TooLong {
        /// The refused length, in bytes.
        len: usize,
    },
    /// Empty, or whitespace only.
    Empty,
}

impl std::fmt::Display for LabelRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Character(c) => write!(f, "label: {c:?} is not allowed in a peer label"),
            Self::UuidShape => write!(
                f,
                "label: this looks like a uuid, and an account or organization id \
                 never travels between peers"
            ),
            Self::TooLong { len } => {
                write!(f, "label: {len} bytes, the cap is {MAX_LABEL_BYTES}")
            }
            Self::Empty => write!(f, "label: empty"),
        }
    }
}

impl std::error::Error for LabelRefusal {}
