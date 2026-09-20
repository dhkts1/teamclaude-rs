//! The sealed exchange: a public key that names nothing, and a reply that
//! seals a real join key to it.
//!
//! # What this is and what it is not
//!
//! An [`Ask`] is a one-time public key, pasted into a chat, that names no
//! address and grants nothing. Whoever holds it mints a one-use, ten-minute
//! join key of their own (the same [`crate::peer::pair::mint_invite`] the
//! `invite` verb runs) and seals it to the ask; this Mac then joins with that
//! key immediately. It is not a credential itself: it does not go in the
//! peers file, and it is not counted against
//! [`crate::peer::pair::MAX_OUTSTANDING_INVITES`].
//!
//! A [`Reply`] is what comes back: one Mac's freshly minted `JoinToken`
//! (v3's three fields), sealed under Noise `N`
//! ([`crate::peer::noise::PATTERN_SEAL`]) to one ask's public key. It opens
//! against exactly one live row, once. Opening it runs the join on the spot,
//! `Enrol` (`IKpsk1`), the same path a pasted key already takes: pinned and
//! trusted on both sides the moment the handshake completes, no knock and no
//! six-digit compare.
//!
//! # Why a key and not a bare address, the way phase 2 first shipped it
//!
//! `src/peer/listener.rs::internet_admission` refuses a knock or a first
//! pairing (`NN`/`XX`) from any source outside LAN scope, by design: a
//! tailnet address is deliberately not LAN scope
//! (`listener::is_lan_scope_v4`'s own doc). Only `Return`/`Enrol` (`IK`,
//! `IKpsk1`, a key this node minted or already pinned) is answered off-LAN.
//! So a reply that only taught an address would still fail at the knock that
//! used to follow it, for exactly the friend mode B exists for: one who is
//! not on this Wi-Fi. Sealing a join key instead means the dial that follows
//! opening the reply is the `Enrol` path, which off-LAN admission already
//! answers.
//!
//! # Nothing here logs an address
//!
//! This module takes no `tracing` import at all, deliberately: a refusal
//! never names a Mac, an address or a key, the rule `MovedRefusal` states at
//! `src/peer/moved.rs:262-270`. A forwarded blob's reader learns nothing more
//! from watching this Mac refuse it than from watching it stay silent.
//!
//! # Framing
//!
//! Both wire forms frame exactly like [`crate::peer::moved`]: four magic
//! bytes outside the seal, then a version byte, then the payload. The length
//! is checked before any AEAD runs, so a chat client that truncated a paste
//! is told it was cut short rather than called a forgery
//! (`src/peer/moved.rs:106`, `:430`).

use tcr_peer_wire::{decode_bytes, encode_bytes};

use crate::peer::noise::{self, KEY_BYTES};
use crate::peer::pair::JoinToken;
use crate::peer::state::PendingAsk;

/// What every ask starts with.
pub const ASK_PREFIX: &str = "tcr-invite:v1:";

/// The four magic bytes in front of an ask's version byte.
const ASK_MAGIC: [u8; 4] = *b"TCRA";

/// What every reply starts with.
pub const REPLY_PREFIX: &str = "tcr-reply:v1:";

/// The four magic bytes in front of a reply's version byte.
const REPLY_MAGIC: [u8; 4] = *b"TCRP";

/// The only wire version either shape has yet.
const WIRE_VERSION: u8 = 1;

/// The sealed plaintext's own version byte, one layer inside the AEAD: what
/// [`seal_key`] writes and [`open_reply`] reads back before trusting the
/// payload type byte after it.
const PAYLOAD_VERSION: u8 = 1;

/// The only payload type this build produces or reads: a join key, in v3's
/// compact spelling ([`JoinToken::to_v3_body`]).
///
/// A byte rather than an assumption, the same reason [`JoinToken::parse`]
/// checks a version rather than guessing: a future payload type is refused by
/// name, never silently read as this one.
const PAYLOAD_TYPE_KEY: u8 = 1;

/// A one-time public key, named to nobody, that a friend can seal an answer
/// to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ask {
    /// The public half of a keypair minted for this exchange alone.
    pub key: [u8; KEY_BYTES],
}

impl Ask {
    /// Render the ask for pasting: magic, version, the raw 32-byte key,
    /// through the one base32 codec this tree has. 74 characters.
    pub fn to_string_wire(&self) -> String {
        let mut body = Vec::with_capacity(ASK_MAGIC.len() + 1 + KEY_BYTES);
        body.extend_from_slice(&ASK_MAGIC);
        body.push(WIRE_VERSION);
        body.extend_from_slice(&self.key);
        format!("{ASK_PREFIX}{}", encode_bytes(&body))
    }

    /// Parse a pasted ask, or a refusal naming what was wrong. Never a paste's
    /// bytes: the caller is a person who just pasted a live invite into a
    /// terminal that keeps scrollback.
    pub fn parse(pasted: &str) -> Result<Self, AskRefusal> {
        let pasted = pasted.trim();
        let body = pasted
            .strip_prefix(ASK_PREFIX)
            .ok_or(AskRefusal::NotAnAsk)?;
        let bytes = decode_bytes(body).map_err(|_| AskRefusal::CutShort)?;
        let min_len = ASK_MAGIC.len() + 1 + KEY_BYTES;
        if bytes.len() < min_len {
            return Err(AskRefusal::CutShort);
        }
        if bytes[..ASK_MAGIC.len()] != ASK_MAGIC {
            return Err(AskRefusal::NotAnAsk);
        }
        let version = bytes[ASK_MAGIC.len()];
        if version != WIRE_VERSION {
            return Err(AskRefusal::UnknownVersion { version });
        }
        let key_start = ASK_MAGIC.len() + 1;
        let key: [u8; KEY_BYTES] = bytes[key_start..key_start + KEY_BYTES]
            .try_into()
            .expect("checked length");
        Ok(Self { key })
    }
}

/// Why a pasted string did not read as an [`Ask`]. Never carries a key or any
/// part of what was pasted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskRefusal {
    /// The prefix, the magic, or both did not match: this is not one of ours.
    NotAnAsk,
    /// Fewer bytes than a whole ask, what a paste cut short looks like.
    CutShort,
    /// A version this build does not know.
    UnknownVersion { version: u8 },
}

impl std::fmt::Display for AskRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAnAsk => write!(f, "this is not an ask (it must start with {ASK_PREFIX:?})"),
            Self::CutShort => write!(
                f,
                "this ask is shorter than it should be, which is what a paste cut short looks like"
            ),
            Self::UnknownVersion { version } => {
                write!(
                    f,
                    "this ask names format version {version}, which this build does not know"
                )
            }
        }
    }
}

impl std::error::Error for AskRefusal {}

/// Seal `token` (a freshly minted one-use join key) to `ask`.
///
/// `token` is spent the moment the inviter's `--reply --stdin` opens this and
/// runs the join, the same one-use rule every join key already carries: the
/// seal adds no second grant on top of it.
pub fn seal_key(ask: &Ask, token: &JoinToken) -> anyhow::Result<Reply> {
    let mut plaintext = Vec::new();
    plaintext.push(PAYLOAD_VERSION);
    plaintext.push(PAYLOAD_TYPE_KEY);
    plaintext.extend_from_slice(&token.to_v3_body());
    let sealed = noise::seal_to(&ask.key, &plaintext)?;
    let mut body = Vec::with_capacity(REPLY_MAGIC.len() + 1 + sealed.len());
    body.extend_from_slice(&REPLY_MAGIC);
    body.push(WIRE_VERSION);
    body.extend_from_slice(&sealed);
    Ok(Reply { body })
}

/// A sealed reply, still opaque: the frame and the magic are checked, the
/// AEAD is not opened yet, because opening needs the matching row.
#[derive(Debug, Clone)]
pub struct Reply {
    body: Vec<u8>,
}

/// The fewest bytes a reply can be and still hold something that could open:
/// the frame, a 32-byte ephemeral public key, a tag, and at least one byte
/// under the tag. [`crate::peer::moved::MIN_LINK_BYTES`]'s own shape.
const MIN_REPLY_BYTES: usize = REPLY_MAGIC.len() + 1 + 32 + 16 + 1;

impl Reply {
    /// Render the reply for pasting.
    pub fn to_string_wire(&self) -> String {
        format!("{REPLY_PREFIX}{}", encode_bytes(&self.body))
    }

    /// Parse a pasted reply's frame, or a refusal naming what was wrong. The
    /// AEAD has not run yet: this only checks the magic, the version and the
    /// length.
    fn parse(pasted: &str) -> Result<Self, ReplyRefusal> {
        let pasted = pasted.trim();
        let body = pasted
            .strip_prefix(REPLY_PREFIX)
            .ok_or(ReplyRefusal::NotOurs)?;
        let bytes = decode_bytes(body).map_err(|_| ReplyRefusal::CutShort)?;
        if bytes.len() < MIN_REPLY_BYTES {
            return Err(ReplyRefusal::CutShort);
        }
        if bytes[..REPLY_MAGIC.len()] != REPLY_MAGIC {
            return Err(ReplyRefusal::NotOurs);
        }
        let version = bytes[REPLY_MAGIC.len()];
        if version != WIRE_VERSION {
            return Err(ReplyRefusal::UnknownVersion { version });
        }
        Ok(Self { body: bytes })
    }

    /// The sealed bytes, past the frame: what [`noise::open_sealed`] takes.
    fn sealed(&self) -> &[u8] {
        &self.body[REPLY_MAGIC.len() + 1..]
    }
}

/// What a reply opened into: a join key, ready for [`crate::peer::pair::join`]
/// to run immediately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Opened {
    /// The answering Mac's freshly minted, one-use join key.
    Key(JoinToken),
}

/// Why a reply did not open. Never carries a peer, a label, an address or any
/// part of what was pasted, the rule `MovedRefusal` states at
/// `src/peer/moved.rs:262-270`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyRefusal {
    /// The prefix or the magic did not match: not one of ours.
    NotOurs,
    /// A version this build does not know.
    UnknownVersion { version: u8 },
    /// Fewer bytes than a whole reply, what a paste cut short looks like.
    /// Checked before the AEAD ever runs, so a truncated paste is never
    /// called a forgery.
    CutShort,
    /// The AEAD did not open against any live row on this Mac: sealed to a
    /// different ask, already spent, or past its deadline. These three read
    /// identically on purpose: a refusal that told them apart would teach a
    /// forwarded blob's reader which one it was.
    DidNotOpen,
    /// It opened, but the plaintext named a payload type this build does not
    /// know.
    UnknownPayload { payload_type: u8 },
}

impl std::fmt::Display for ReplyRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotOurs => write!(f, "this is not a reply (it must start with {REPLY_PREFIX:?})"),
            Self::UnknownVersion { version } => {
                write!(f, "this reply names format version {version}, which this build does not know")
            }
            Self::CutShort => write!(f, "this reply is shorter than it should be, which is what a paste cut short looks like"),
            Self::DidNotOpen => write!(
                f,
                "this reply did not open against anything outstanding here; it may answer an \
                 ask that already expired, was already spent, or belongs to a different Mac"
            ),
            Self::UnknownPayload { payload_type } => write!(
                f,
                "this reply opened, but it carries a payload type ({payload_type}) this build \
                 does not know"
            ),
        }
    }
}

impl std::error::Error for ReplyRefusal {}

/// Try `pasted` against every row in `rows` that is still live at `now_ms`,
/// and on the first one whose private key opens it, decode the plaintext and
/// hand back which row matched.
///
/// Eight rows at most ([`crate::peer::state::MAX_OUTSTANDING_ASKS`]), so eight
/// `N`-pattern opens, the same trial shape
/// [`noise::read_message_1_matching`] already runs over outstanding invites.
pub fn open_reply(
    rows: &[PendingAsk],
    pasted: &str,
    now_ms: i64,
) -> Result<(Opened, u64), ReplyRefusal> {
    let reply = Reply::parse(pasted)?;
    for row in rows.iter().filter(|row| now_ms < row.until_ms) {
        let Ok(plaintext) = noise::open_sealed(&row.private, reply.sealed()) else {
            continue;
        };
        return decode_opened(&plaintext).map(|opened| (opened, row.id));
    }
    Err(ReplyRefusal::DidNotOpen)
}

/// The inverse of the plaintext [`seal_key`] builds.
fn decode_opened(plaintext: &[u8]) -> Result<Opened, ReplyRefusal> {
    let &[_version, payload_type, ref rest @ ..] = plaintext else {
        return Err(ReplyRefusal::DidNotOpen);
    };
    match payload_type {
        PAYLOAD_TYPE_KEY => {
            let token = JoinToken::from_v3_body(rest).map_err(|_| ReplyRefusal::DidNotOpen)?;
            Ok(Opened::Key(token))
        }
        other => Err(ReplyRefusal::UnknownPayload {
            payload_type: other,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tcr_peer_wire::PeerId;

    fn keypair() -> ([u8; KEY_BYTES], [u8; KEY_BYTES]) {
        noise::generate_static().expect("a test keypair")
    }

    fn some_token() -> JoinToken {
        JoinToken::new(
            vec![
                "192.0.2.10:7755".parse().expect("a test address"),
                "198.51.100.20:7755".parse().expect("a test address"),
            ],
            PeerId([4_u8; 32]),
            [8_u8; 32],
        )
    }

    #[test]
    fn an_ask_round_trips() {
        let (_secret, public) = keypair();
        let ask = Ask { key: public };
        let rendered = ask.to_string_wire();
        assert!(rendered.starts_with(ASK_PREFIX));
        assert_eq!(Ask::parse(&rendered).expect("it parses"), ask);
    }

    #[test]
    fn a_reply_opens_against_its_own_ask_and_yields_the_key_that_went_in() {
        let (ask_secret, ask_public) = keypair();
        let ask = Ask { key: ask_public };
        let token = some_token();

        let reply = seal_key(&ask, &token).expect("it seals");
        let rendered = reply.to_string_wire();
        assert!(rendered.starts_with(REPLY_PREFIX));

        let rows = vec![PendingAsk {
            id: 1,
            public: ask_public,
            private: ask_secret,
            until_ms: crate::peer::pair::now_ms() + 600_000,
        }];
        let (opened, id) =
            open_reply(&rows, &rendered, crate::peer::pair::now_ms()).expect("it opens");
        assert_eq!(id, 1);
        assert_eq!(opened, Opened::Key(token));
    }
}
