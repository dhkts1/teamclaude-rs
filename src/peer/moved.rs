//! The moved link: what one Mac sends one friend after it changed networks.
//!
//! # What this is and what it is not
//!
//! It is a sealed record a person hands to one other person over a chat they
//! already have. It says one thing, "here is where I am now", and it carries
//! nothing else: no name, no key, no grant, no invitation. Opening one proves
//! that whoever sealed it held this pair's shared secret, which means they were
//! a party to a completed handshake with this Mac at some point. It does not
//! prove that they still are that Mac, that the addresses inside are real, or
//! that the person who sent the message owns the Mac. Identity is re-proven by
//! the Noise handshake against the pinned static key, every time, the rule
//! `crate::peer::mod`'s invariant 2 states.
//!
//! It is not a share link. `tcr peer link` brings a NEW Mac onto a mesh; this
//! brings a pinned one a fresh address and can do nothing else.
//!
//! # Why it is not a dead drop record
//!
//! [`crate::peer::drop`] is the same shape and a different clock. A drop record
//! lives at a name that is a MAC over an hour-long slot, because a slot costs
//! one write to somebody else's surface per friend, so the slot count is a
//! write budget. A link costs no writes at all and is read when the friend next
//! looks at their phone, so its clock is a person's. Inheriting the slot would
//! make a link read as a forgery when the truth is that the friend was asleep.
//!
//! So this module shares that one's AEAD body ([`crate::peer::drop::seal_payload`])
//! and its key ladder ([`crate::peer::drop::expand`]), and shares none of its
//! policy: its own magic, its own versioned domain strings, its own age
//! ceiling, and refusals written for a person who pasted something.
//!
//! # Nothing calls this yet
//!
//! This file is the seal and the refusals. There is no verb, no URL parser, no
//! peers-file access and no clock of its own: `now_s` arrives as a parameter,
//! the way [`crate::peer::drop::open`] takes one. None of this runs at boot.
//!
//! # Nothing here prints key material or link text
//!
//! [`MovedKeys`] has no accessor, is serialized nowhere, and its `Debug` prints
//! the shape and no bytes. No refusal in this file carries a peer, an address
//! or any part of what was pasted, for the reason § "a link forwarded to the
//! wrong person" gives: a refusal that says which of this Mac's peers was
//! nearly a match tells a stranger who this Mac knows.

use std::net::SocketAddr;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tcr_peer_wire::{decode_bytes, encode_bytes, PeerId};

use crate::peer::drop::{
    expand, open_payload, seal_payload, OpenFailure, SealFailure, MAX_CLOCK_SKEW,
    MAX_RECORD_ENDPOINTS, NONCE_BYTES, TAG_BYTES,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// The root domain separator, versioned for the reason `drop::DROP_ROOT_INFO`
/// states: two derivations from one secret with no domain separator are one
/// derivation whose outputs are related. This root is what keeps a link's key
/// and a drop's key unrelated even though both grow from one pair secret.
const MOVED_ROOT_INFO: &[u8] = b"tcr peer moved root v1";

/// The seal key's domain separator. See [`MOVED_ROOT_INFO`].
const MOVED_SEAL_INFO: &[u8] = b"tcr peer moved seal v1";

/// The context string the AEAD authenticates in front of the publisher.
///
/// This is what stops a drop record and a moved link being interchangeable even
/// if some future edit made the two seal keys agree.
const MOVED_CONTEXT: &[u8] = b"tcr peer moved v1";

/// The link's first four bytes, outside the seal on purpose, so "this is not
/// one of ours at all" is a different refusal from "this did not open".
const MOVED_MAGIC: [u8; 4] = *b"TCRM";

/// The only format version, outside the seal and inside it.
pub const MOVED_VERSION: u8 = 1;

/// Magic and version: everything in front of the sealed payload, which carries
/// its own nonce.
const FRAME_BYTES: usize = MOVED_MAGIC.len() + 1;

/// The fewest bytes a link can be and still hold something that could open:
/// the frame, a nonce, a tag, and at least one byte under the tag.
///
/// Checked BEFORE the AEAD runs, which is the whole of telling a paste that got
/// cut off apart from a forgery. Chat clients wrap, truncate at a character
/// limit, and sometimes linkify a URL and eat the last character, and the person
/// that happens to needs to be told about their paste rather than about an
/// attacker.
const MIN_LINK_BYTES: usize = FRAME_BYTES + NONCE_BYTES + TAG_BYTES + 1;

/// How long a link stays good: 24 hours.
///
/// **This is a policy value, not a fact about the crypto**, and it is the one
/// number in this file somebody may rule differently. It is here once, as one
/// constant, so a ruling changes this line and nothing else.
///
/// Why a day: the channel is a chat message read when the friend next picks up
/// their phone, so anything under a few hours makes "ask them to send another
/// one" the common outcome, which is the failure this exists to end. Past a day
/// the sender has probably moved again, so an old link starts teaching an
/// address that is no longer true. What it costs is a replay window of the same
/// length for anyone holding the link, bounded to weak endpoints on a row that
/// was already pinned, worth at most a wasted connect attempt.
pub const MAX_MOVED_AGE: Duration = Duration::from_secs(24 * 60 * 60);

// Two more ceilings apply and are deliberately NOT spelled again here: how far
// ahead a publisher's clock may run ([`crate::peer::drop::MAX_CLOCK_SKEW`]) and
// how many addresses one sealed record may carry
// ([`crate::peer::drop::MAX_RECORD_ENDPOINTS`]). Both are the same question
// asked of a sealed record, and one question with two spellings is two values
// that drift. They are used below under their own names.

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

/// The key one pair's moved links are sealed under.
///
/// Held by value and never logged: `Debug` prints the shape and no bytes, the
/// rule `crate::peer::config::NetworkKey`'s own `Debug` follows, because a
/// `{:?}` in a log line is how a shared secret ends up in a file. There is no
/// accessor and it is serialized nowhere.
///
/// One key and not two, unlike [`crate::peer::drop::DropKeys`]: a drop has a
/// LOCATION and therefore needs a name key. A link is handed over directly and
/// has no location, so a name key would be a capability with nothing to name.
pub struct MovedKeys {
    seal_key: [u8; 32],
}

impl std::fmt::Debug for MovedKeys {
    /// The bytes are deliberately not printed. See the type docs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MovedKeys(set)")
    }
}

impl MovedKeys {
    /// This pair's link key, from the secret on its row, or a refusal naming
    /// the one case where there is no key to derive.
    ///
    /// # The option is the argument, on purpose
    ///
    /// [`crate::peer::config::PeerRow::rendezvous_secret`] is an `Option` and
    /// its own doc says absent is not an error: it means the row predates the
    /// key, or no session has completed since the row was pinned. There is no
    /// safe default to reach for. `unwrap_or_default()` here would seal every
    /// such link under thirty-two zero bytes, which is a key anybody can guess
    /// and a total break, so the option is taken whole and refused by name.
    ///
    /// A caller cannot skip this: it is the only way to get a [`MovedKeys`].
    pub fn for_row(rendezvous_secret: Option<&[u8; 32]>) -> Result<Self, MintRefusal> {
        let secret = rendezvous_secret.ok_or(MintRefusal::NoSharedSecret)?;
        let root = expand(secret, MOVED_ROOT_INFO);
        Ok(Self {
            seal_key: expand(&root, MOVED_SEAL_INFO),
        })
    }
}

// ---------------------------------------------------------------------------
// The record
// ---------------------------------------------------------------------------

/// What one Mac tells one friend about where it is now.
///
/// `at` is the publisher's own clock and is used for exactly one thing,
/// refusing something too old. It never becomes an `Endpoint::observed_at_ms`:
/// that stays this node's own clock, the rule `Endpoint::observed_at_ms`'s doc
/// states.
///
/// # There is no publisher field and no label
///
/// This is the one place the record deliberately differs from
/// [`crate::peer::drop::DropRecord`]. A drop record is fetched from a name the
/// reader derived for one specific peer, so a publisher field inside it is a
/// cross-check. A link is tried against every row this Mac has pinned, so the
/// row whose key worked IS the answer and a field claiming one could never
/// disagree with it. The publisher is still bound: it goes in the associated
/// data, supplied by the reader from the row being tried.
///
/// No label either. The receiving Mac already holds the operator's own word for
/// that peer, and a name the link asserts would be a second, worse source.
///
/// The field names are short because every byte is sealed and then spelled into
/// base32 that a person has to paste.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MovedRecord {
    /// Format version inside the sealed plaintext, [`MOVED_VERSION`].
    pub v: u8,
    /// The publisher's unix seconds.
    pub at: u64,
    /// Where the publisher believes it is reachable, at most
    /// [`crate::peer::drop::MAX_RECORD_ENDPOINTS`] of them.
    pub eps: Vec<SocketAddr>,
}

/// Why this Mac could not mint a link for a peer.
///
/// Separate from [`MovedRefusal`] because the two are answered by different
/// people: everything here is about this Mac and the row in front of the person
/// minting, and everything there is about bytes that arrived from somewhere
/// else. One enum over both would give every reader of an opened link an arm
/// that can never fire.
#[derive(Debug, thiserror::Error)]
pub enum MintRefusal {
    /// The row carries no shared secret yet, so there is no key only these two
    /// Macs hold. The sentence carries the remedy, because this is the one
    /// refusal here that a person can fix and would otherwise read as a bug.
    #[error(
        "moved link: this pair has no shared secret yet, so there is nothing only the two of \
         you can read; let the two Macs complete one session together and try again"
    )]
    NoSharedSecret,
    /// The record names a version this build does not write.
    #[error("moved link: refusing to seal a version {found} record, this build writes {expected}")]
    Version { found: u8, expected: u8 },
    /// More addresses than one record carries.
    #[error("moved link: refusing to seal {found} addresses, the ceiling is {max}")]
    TooManyEndpoints { found: usize, max: usize },
    /// The record would not serialize. The error is not carried: at this width
    /// it can only be the platform, and what it would say is about `serde_json`
    /// rather than about anything a person can act on.
    #[error("moved link: the record would not serialize")]
    WouldNotSerialize,
    /// The platform CSPRNG would not fill the nonce. Sealing without a random
    /// nonce is not an option, so this refuses rather than degrades.
    #[error("moved link: the platform CSPRNG refused")]
    Csprng(#[source] getrandom::Error),
    /// The AEAD refused. At this width it cannot: the key and the nonce are
    /// fixed-size arrays.
    #[error("moved link: the record would not seal")]
    WouldNotSeal,
}

/// Why a link that arrived was not applied.
///
/// Six variants because four of the six things that go wrong with a link are
/// the person's own and each has a different fix: a link meant for somebody
/// else, one that sat too long, one a chat app cut in half, and one from a
/// build that is not this one. A refusal that answers one word for all of them
/// is one nobody can act on, which is [`crate::peer::drop::RecordRefusal`]'s
/// own argument applied to a different reader.
///
/// # No variant names anything
///
/// No peer, no label, no address, no part of what was pasted. A link forwarded
/// into the wrong group chat must teach its reader nothing about whose Mac it
/// was for or whether this Mac was close to a match. That is why there is no
/// `found` field beside the magic, which is the one place this differs from
/// [`crate::peer::drop::RecordRefusal::NotARecord`]: a drop's bytes came off a
/// surface an operator owns and printing them helps; a link's bytes came out of
/// somebody's chat window.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MovedRefusal {
    /// The paste is not a whole link: fewer bytes than the shortest one that
    /// could open, or base32 that stops in the middle of a symbol. Decided
    /// before the AEAD runs, so a paste that got cut off never reads as a
    /// forgery.
    ///
    /// It carries no count. The two ways to get here are counted in different
    /// units, bytes and characters, and neither is a number the person can do
    /// anything with: what they can do is paste the whole line again.
    #[error(
        "moved link: that is not a whole link and looks cut off; copy the whole line and \
         paste it again"
    )]
    CutShort,
    /// The first four bytes are not [`MOVED_MAGIC`].
    #[error("moved link: those bytes are not a tcr moved link")]
    NotALink,
    /// The version names a format this build does not have.
    #[error(
        "moved link: link version {found} is not {expected}; one of the two Macs needs updating"
    )]
    Version { found: u8, expected: u8 },
    /// The sealed bytes did not authenticate under this pair's key: a link for
    /// a different pair of Macs, or bytes somebody edited.
    #[error("moved link: that link was not meant for this Mac")]
    NotForThisMac,
    /// It opened, and what was inside is not a record this build can read.
    /// Past the AEAD the bytes can only have been written by a holder of this
    /// pair's key, so this is not an attack: it is this pair's own writer
    /// producing something this reader cannot read, refused rather than
    /// guessed at.
    #[error("moved link: that link opened but this build cannot read what is inside it")]
    Unreadable,
    /// It opened, and is dated further ahead than
    /// [`crate::peer::drop::MAX_CLOCK_SKEW`] allows.
    #[error(
        "moved link: that link is dated {ahead_s}s in the future; check the clock on both Macs"
    )]
    FromTheFuture { ahead_s: u64 },
    /// It opened, and is older than [`MAX_MOVED_AGE`].
    #[error(
        "moved link: that link is {age_s}s old, past the {max_s}s ceiling; ask for a fresh one"
    )]
    Stale { age_s: u64, max_s: u64 },
}

// ---------------------------------------------------------------------------
// Sealing and opening
// ---------------------------------------------------------------------------

/// What the AEAD authenticates and does not encrypt: this module's context
/// string and the publisher.
///
/// The context string is what stops a drop record and a moved link being
/// interchangeable. The publisher is what stops a link minted by one friend
/// being replayed against another friend's row on a Mac that pins both: the
/// reader supplies it from the row it is trying, so a link only opens against
/// the row it was sealed for.
fn associated_data(publisher: &PeerId) -> Vec<u8> {
    let wire = publisher.to_wire();
    let mut aad = Vec::with_capacity(MOVED_CONTEXT.len() + wire.len());
    aad.extend_from_slice(MOVED_CONTEXT);
    aad.extend_from_slice(wire.as_bytes());
    aad
}

/// Seal `record`, from `publisher`, into the framed bytes a link carries.
///
/// `publisher` is the peer this link is FOR, as the sender knows it: the
/// receiver supplies its own side of the same pair when it opens. It is
/// authenticated and not encrypted, and it does not appear in the plaintext.
pub fn seal_link(
    keys: &MovedKeys,
    publisher: &PeerId,
    record: &MovedRecord,
) -> Result<Vec<u8>, MintRefusal> {
    if record.v != MOVED_VERSION {
        return Err(MintRefusal::Version {
            found: record.v,
            expected: MOVED_VERSION,
        });
    }
    if record.eps.len() > MAX_RECORD_ENDPOINTS {
        return Err(MintRefusal::TooManyEndpoints {
            found: record.eps.len(),
            max: MAX_RECORD_ENDPOINTS,
        });
    }

    let plaintext = serde_json::to_vec(record).map_err(|_| MintRefusal::WouldNotSerialize)?;
    let aad = associated_data(publisher);
    let payload =
        seal_payload(&keys.seal_key, &aad, &plaintext).map_err(|failure| match failure {
            SealFailure::Csprng(err) => MintRefusal::Csprng(err),
            SealFailure::Aead => MintRefusal::WouldNotSeal,
        })?;

    let mut out = Vec::with_capacity(FRAME_BYTES + payload.len());
    out.extend_from_slice(&MOVED_MAGIC);
    out.push(MOVED_VERSION);
    out.extend_from_slice(&payload);
    Ok(out)
}

/// The same thing in the form a link's `r=` field carries: Crockford base32,
/// unpadded, over the same codec the rest of this wire uses.
///
/// Here and not at the caller so that one encoder answers for both directions:
/// a sender that picked its own would be a second spelling of this wire form
/// for the reader to fail against.
pub fn seal_link_field(
    keys: &MovedKeys,
    publisher: &PeerId,
    record: &MovedRecord,
) -> Result<String, MintRefusal> {
    seal_link(keys, publisher, record).map(|sealed| encode_bytes(&sealed))
}

/// Open and check a link's bytes against `now_s`, for the row `expected_publisher`
/// names.
///
/// Every check [`MovedRefusal`] names, in that order, and the refusal says
/// which one fired. The order is what makes the refusals worth having: the
/// length is decided before the AEAD so a cut paste is never called a forgery,
/// and the age is decided after it, from a field INSIDE the seal, so "too old"
/// is its own answer rather than collapsing into "did not open".
pub fn open_link(
    keys: &MovedKeys,
    expected_publisher: &PeerId,
    sealed: &[u8],
    now_s: u64,
) -> Result<MovedRecord, MovedRefusal> {
    // Length first, before the magic and before the AEAD. See
    // [`MIN_LINK_BYTES`].
    //
    // Before the magic and not after it, which is the opposite of
    // [`crate::peer::drop::open`]'s order, because the two readers get short
    // input from different places. A drop's short answer came off a surface
    // that might be serving an error document, so reading the magic first is
    // what tells a document apart from a record. A link's short input came out
    // of a chat window, where by far the likeliest three bytes are the front of
    // a link that got cut, and telling that person "those bytes are not a tcr
    // moved link" sends them to look for a broken app. Somebody who really did
    // paste three bytes of something else is told to paste the whole line
    // again, which costs them nothing.
    if sealed.len() < MIN_LINK_BYTES {
        return Err(MovedRefusal::CutShort);
    }

    // Outside the AEAD, so something that was never one of ours is told apart
    // from something that is and did not open.
    let mut found_magic = [0_u8; 4];
    for (place, byte) in found_magic.iter_mut().zip(sealed.iter()) {
        *place = *byte;
    }
    if found_magic != MOVED_MAGIC {
        return Err(MovedRefusal::NotALink);
    }

    let Some(&version) = sealed.get(MOVED_MAGIC.len()) else {
        // Unreachable: the length check above already guarantees this byte.
        // Spelled as a refusal rather than an index so there is no panic here
        // whatever a later edit does to the order of these checks.
        return Err(MovedRefusal::CutShort);
    };
    if version != MOVED_VERSION {
        return Err(MovedRefusal::Version {
            found: version,
            expected: MOVED_VERSION,
        });
    }

    let Some(payload) = sealed.get(FRAME_BYTES..) else {
        return Err(MovedRefusal::CutShort);
    };

    let aad = associated_data(expected_publisher);
    let plaintext =
        open_payload(&keys.seal_key, &aad, payload).map_err(|failure| match failure {
            // Both are one refusal here, and it is the quiet one: a link that did
            // not authenticate is a link for two other Macs, and the reader must
            // not learn anything else about it. The short case cannot reach this
            // line, because the length was decided above.
            OpenFailure::TooShort | OpenFailure::DidNotAuthenticate => MovedRefusal::NotForThisMac,
        })?;

    let record: MovedRecord =
        serde_json::from_slice(&plaintext).map_err(|_| MovedRefusal::Unreadable)?;

    if record.v != MOVED_VERSION {
        return Err(MovedRefusal::Version {
            found: record.v,
            expected: MOVED_VERSION,
        });
    }
    if record.at > now_s.saturating_add(MAX_CLOCK_SKEW.as_secs()) {
        return Err(MovedRefusal::FromTheFuture {
            ahead_s: record.at.saturating_sub(now_s),
        });
    }
    let age_s = now_s.saturating_sub(record.at);
    if age_s > MAX_MOVED_AGE.as_secs() {
        return Err(MovedRefusal::Stale {
            age_s,
            max_s: MAX_MOVED_AGE.as_secs(),
        });
    }

    Ok(record)
}

/// Open a link's `r=` field: decode the base32, then [`open_link`].
///
/// A field that will not decode is [`MovedRefusal::CutShort`] and not a
/// character complaint. The codec's own refusal names a peer id, because that
/// is what the rest of this wire carries, and re-spelling it at a person who
/// pasted a link would send them hunting for something that was never there.
/// What actually happens to a link in a chat window is that it gets wrapped,
/// cut at a character limit, or linkified with the last character eaten, and
/// every one of those lands here.
pub fn open_link_field(
    keys: &MovedKeys,
    expected_publisher: &PeerId,
    field: &str,
    now_s: u64,
) -> Result<MovedRecord, MovedRefusal> {
    let sealed = decode_bytes(field).map_err(|_| MovedRefusal::CutShort)?;
    open_link(keys, expected_publisher, &sealed, now_s)
}
