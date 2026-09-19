//! The dead drop: where two Macs that both moved leave each other an address.
//!
//! # What this is not
//!
//! It is not a relay and not a rendezvous server. Nothing of ours holds state
//! and nothing of ours learns who talks to whom: the operator points this at a
//! surface they already own, and to that surface a record is a random name
//! holding random bytes. `docs/peers.md` § "Reaching a Mac off your network"
//! says "There is no server anywhere in this", and this module is written to
//! keep that sentence true.
//!
//! # Where the key comes from
//!
//! One HKDF expand round over [`crate::peer::config::PeerRow::rendezvous_secret`],
//! under this module's own versioned domain string, exactly the shape
//! [`crate::peer::reach::port_secret`] uses over the handshake hash. Not from
//! the handshake hash itself: that value is memory-only by decision (see
//! `PeerRow::rendezvous_secret`'s doc, and `reach::port_secret`'s) and a drop
//! whose key dies on restart is useless in the one case it exists for.
//!
//! Three keys and not one. `name_key` answers "where does this pair write in
//! this hour" and `seal_key` answers "what do those bytes say", and keeping
//! them apart costs one keyed hash and makes the two questions two different
//! capabilities.
//!
//! # What a fetched address is worth
//!
//! One dial attempt and nothing else. A store can withhold, and a peer that
//! was forgotten on this Mac still holds the seal key it was given while it
//! was a peer, so nothing here is authorization: identity is re-proven by the
//! Noise handshake against the pinned static key, every time, the rule
//! `crate::peer::mod`'s invariant 2 states.
//!
//! # Nothing calls this yet
//!
//! This file is the crypto and the naming. There is no store client, no
//! publisher task and no endpoint source here: those are separate changes and
//! none of this runs at boot.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use tcr_peer_wire::PeerId;

use crate::peer::config::hmac_sha256;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// How long one drop name is valid: one hour.
///
/// Not [`crate::peer::reach::SLOT_SECONDS`] (30), and the difference is the
/// point: a port slot is computed locally and costs nothing, a drop slot costs
/// one write to somebody else's surface per friend. Thirty-second slots would
/// be 2,880 writes per friend per day.
pub const DROP_SLOT_SECONDS: u64 = 3_600;

/// The root domain separator. Versioned so a scheme change changes every name
/// rather than half of them, the rule `reach::PORT_INFO`'s doc states: two
/// derivations from one secret with no domain separator are one derivation
/// whose outputs are related.
const DROP_ROOT_INFO: &[u8] = b"tcr peer dead-drop root v1";

/// The name key's domain separator. See [`DROP_ROOT_INFO`].
const DROP_NAME_INFO: &[u8] = b"tcr peer dead-drop name v1";

/// The seal key's domain separator. See [`DROP_ROOT_INFO`].
const DROP_SEAL_INFO: &[u8] = b"tcr peer dead-drop seal v1";

/// The record's first four bytes, outside the sealed part on purpose, so "this
/// is not a tcr record at all" is a different refusal from "this did not
/// open". A surface serving an HTML error page must not read as a forgery.
const DROP_MAGIC: [u8; 4] = *b"TCRD";

/// The only format version.
const DROP_VERSION: u8 = 1;

/// ChaCha20-Poly1305's nonce width.
const NONCE_BYTES: usize = 12;

/// Poly1305's tag width, appended to the ciphertext by the AEAD.
const TAG_BYTES: usize = 16;

/// Magic, version and nonce: everything before the sealed bytes.
const HEADER_BYTES: usize = DROP_MAGIC.len() + 1 + NONCE_BYTES;

/// How old a record may be and still be applied.
pub const MAX_RECORD_AGE: Duration = Duration::from_secs(2 * DROP_SLOT_SECONDS);

/// How far into the future a publisher's clock may run before its record is
/// refused rather than believed.
pub const MAX_CLOCK_SKEW: Duration = Duration::from_secs(300);

/// How many addresses one record carries.
///
/// Capped so the sealed record stays small enough that a text-only surface
/// with a per-value size limit remains a possible backend later.
pub const MAX_RECORD_ENDPOINTS: usize = 4;

// ---------------------------------------------------------------------------
// Keys and names
// ---------------------------------------------------------------------------

/// The keys one pair's dead drop runs on.
///
/// Held by value and never logged: `Debug` prints the shape and no bytes, the
/// rule `crate::peer::config::NetworkKey`'s own `Debug` follows, because a
/// `{:?}` in a log line is how a shared secret ends up in a file. There is no
/// accessor for either key and neither is serialized anywhere.
pub struct DropKeys {
    name_key: [u8; 32],
    seal_key: [u8; 32],
}

impl std::fmt::Debug for DropKeys {
    /// The bytes are deliberately not printed. See the type docs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DropKeys(set)")
    }
}

impl DropKeys {
    /// Derive this pair's drop keys from the secret already on its row.
    ///
    /// `rendezvous_secret` is [`crate::peer::reach::port_secret`]'s output and
    /// therefore already a pseudorandom key, so one expand round is the whole
    /// ladder: RFC 5869 § 2.3 with `info` this module's own versioned string
    /// and the counter byte the RFC's first round carries. No second extract,
    /// for the reason `port_secret`'s doc gives.
    pub fn derive(rendezvous_secret: &[u8; 32]) -> Self {
        let root = expand(rendezvous_secret, DROP_ROOT_INFO);
        Self {
            name_key: expand(&root, DROP_NAME_INFO),
            seal_key: expand(&root, DROP_SEAL_INFO),
        }
    }
}

/// RFC 5869 § 2.3, one round: `T(1) = HMAC(prk, info || 0x01)`.
///
/// One round and not a loop, because 32 bytes is one SHA-256 block and the
/// counter never reaches two: a loop here would be dead code that the next
/// reader has to prove is dead.
fn expand(pseudorandom_key: &[u8; 32], info: &[u8]) -> [u8; 32] {
    let mut block = Vec::with_capacity(info.len() + 1);
    block.extend_from_slice(info);
    block.push(0x01);
    hmac_sha256(pseudorandom_key, &block)
}

/// One drop location: 16 bytes, rendered as 32 lower-case hex characters.
///
/// Hex and not Crockford base32, for the reason `config::secret32_hex` gives:
/// the Crockford codec is this tree's identity wire form and a drop name is
/// not an identity. The precedent for a hand-rolled hex wire form on a value
/// of this class is [`tcr_peer_wire::InstanceId::to_wire`].
///
/// To anyone without the pair's `name_key` this is sixteen uniform random
/// bytes, and two names from two slots share no structure: they are two MAC
/// outputs under one key over two different messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DropName([u8; 16]);

impl DropName {
    /// The name `publisher` writes to in `slot`.
    ///
    /// The publisher's id is in the derivation because a drop is
    /// DIRECTIONAL: each Mac publishes at its own name and reads the other's.
    /// Without it both ends would write to one location and overwrite each
    /// other on every slot.
    pub fn for_slot(keys: &DropKeys, publisher: &PeerId, slot: u64) -> Self {
        let wire = publisher.to_wire();
        let mut message = Vec::with_capacity(4 + 8 + wire.len());
        message.extend_from_slice(b"slot");
        message.extend_from_slice(&slot.to_be_bytes());
        message.extend_from_slice(wire.as_bytes());

        let mac = hmac_sha256(&keys.name_key, &message);
        let mut name = [0_u8; 16];
        name.copy_from_slice(&mac[..16]);
        Self(name)
    }

    /// The 32-character lower-case hex form, which is the only form.
    pub fn to_wire(&self) -> String {
        let mut out = String::with_capacity(32);
        for byte in &self.0 {
            out.push_str(&format!("{byte:02x}"));
        }
        out
    }

    /// The raw bytes, for the sealed record's associated data.
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

/// Which slot a unix timestamp falls in.
///
/// [`crate::peer::reach::current_slot`]'s shape over this module's own slot
/// length.
pub fn current_slot(unix_seconds: u64) -> u64 {
    unix_seconds / DROP_SLOT_SECONDS
}

/// The slots a reader accepts while in `slot`: the slot itself and the one
/// before it.
///
/// Two and not three, unlike [`crate::peer::reach::accepted_ports`]: a
/// publisher writes only to the slot it is in, so a future slot can hold
/// nothing yet, while the slot just ended is where a record written four
/// minutes ago still sits. Ordered current, previous, which is the order a
/// reader should try them in.
pub fn accepted_slots(slot: u64) -> [u64; 2] {
    [slot, slot.saturating_sub(1)]
}

// ---------------------------------------------------------------------------
// The record
// ---------------------------------------------------------------------------

/// What one Mac leaves for one friend in one slot.
///
/// `at` is the publisher's own clock and is used for exactly one thing,
/// refusing something too old. It never becomes an `Endpoint::observed_at_ms`:
/// that stays this node's own clock, the rule `Endpoint::observed_at_ms`'s doc
/// states and `config::observe_seen_address` repeats.
///
/// The field names are short because they are on somebody else's surface and
/// every byte is sealed, stored and fetched again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DropRecord {
    /// Format version inside the sealed plaintext, [`DROP_VERSION`].
    pub v: u8,
    /// The publisher, checked against the peer the reader expected.
    #[serde(rename = "pub")]
    pub publisher: PeerId,
    /// The slot, checked against the slot the name was derived for.
    pub slot: u64,
    /// The publisher's unix seconds.
    pub at: u64,
    /// Where the publisher believes it is reachable, at most
    /// [`MAX_RECORD_ENDPOINTS`] of them.
    pub eps: Vec<SocketAddr>,
}

/// Why a fetched record was not applied.
///
/// Seven variants and not one, because "the surface served an HTML error
/// page", "the surface served a forged record" and "the surface served last
/// week's record" are three different diagnoses, and an operator reading a log
/// needs to tell them apart. A refusal that answers one word for all three is
/// one nobody can act on.
#[derive(Debug, thiserror::Error)]
pub enum RecordRefusal {
    /// The first four bytes are not [`DROP_MAGIC`]. Reported with what was
    /// found, zero-padded when there were fewer than four bytes to read.
    #[error("dead drop: those bytes are not a tcr record (magic {found:?})")]
    NotARecord { found: [u8; 4] },
    /// The header's version byte names a format this build does not have.
    #[error("dead drop: record version {found} is not {expected}")]
    Version { found: u8, expected: u8 },
    /// The sealed bytes did not authenticate: a wrong key, a wrong name, a
    /// wrong publisher, or bytes somebody edited.
    #[error("dead drop: the record at that name did not open under this pair's key")]
    DidNotOpen,
    /// It opened, and names somebody else.
    #[error("dead drop: the record names peer {found} and this name is {expected}'s")]
    WrongPublisher { found: String, expected: String },
    /// It opened, and claims a slot this name does not belong to.
    #[error("dead drop: the record is from slot {found} and this name is slot {expected}'s")]
    WrongSlot { found: u64, expected: u64 },
    /// It opened, and is older than [`MAX_RECORD_AGE`].
    #[error("dead drop: the record is {age_s}s old, past the {max_s}s ceiling")]
    Stale { age_s: u64, max_s: u64 },
    /// It opened, and is dated further ahead than [`MAX_CLOCK_SKEW`] allows.
    #[error("dead drop: the record is dated {ahead_s}s in the future")]
    FromTheFuture { ahead_s: u64 },
}

/// What the AEAD authenticates but does not encrypt: the name and the
/// publisher.
///
/// # Why the slot is not a third term here
///
/// It is already bound, and binding it twice would cost a diagnosis. The name
/// IS a MAC over the slot, so a record sealed for one slot cannot open at
/// another slot's name: that is [`RecordRefusal::DidNotOpen`], and it is the
/// defence. Feeding the record's own `slot` field in as well would make a
/// record whose inner slot disagrees with its name indistinguishable from a
/// forgery, and [`RecordRefusal::WrongSlot`] would be unreachable. The check
/// that fires instead is the explicit one in [`open`], after the bytes have
/// authenticated, which is the reading an operator can act on.
fn associated_data(name: &DropName, publisher: &PeerId) -> Vec<u8> {
    let wire = publisher.to_wire();
    let mut aad = Vec::with_capacity(name.0.len() + wire.len());
    aad.extend_from_slice(&name.0);
    aad.extend_from_slice(wire.as_bytes());
    aad
}

/// Seal `record` for `name`.
///
/// The nonce is twelve RANDOM bytes from the platform CSPRNG and is never
/// derived: a derived nonce repeats the moment a Mac publishes twice in one
/// slot because its address changed, and a repeated ChaCha20-Poly1305 nonce
/// under a fixed key is a total break. Twelve random bytes against a handful
/// of writes per slot is nowhere near a birthday problem.
///
/// The associated data is the name and the publisher, which is what stops the
/// surface moving one pair's record to another name and having it believed.
pub fn seal(keys: &DropKeys, name: &DropName, record: &DropRecord) -> Result<Vec<u8>> {
    use chacha20poly1305::aead::{Aead as _, KeyInit as _, Payload};
    use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

    if record.v != DROP_VERSION {
        bail!(
            "dead drop: refusing to seal a version {} record, this build writes {DROP_VERSION}",
            record.v
        );
    }
    if record.eps.len() > MAX_RECORD_ENDPOINTS {
        bail!(
            "dead drop: refusing to seal {} addresses, the ceiling is {MAX_RECORD_ENDPOINTS}",
            record.eps.len()
        );
    }

    let plaintext =
        serde_json::to_vec(record).context("dead drop: the record would not serialize")?;

    let mut nonce = [0_u8; NONCE_BYTES];
    getrandom::fill(&mut nonce).context("dead drop: the platform CSPRNG refused")?;

    let cipher = ChaCha20Poly1305::new(Key::from_slice(&keys.seal_key));
    let aad = associated_data(name, &record.publisher);
    let sealed = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &plaintext,
                aad: &aad,
            },
        )
        // The AEAD's own error carries nothing beyond "it failed", and at this
        // width it cannot: the key and nonce are fixed-size arrays.
        .map_err(|_| anyhow!("dead drop: the record would not seal"))?;

    let mut out = Vec::with_capacity(HEADER_BYTES + sealed.len());
    out.extend_from_slice(&DROP_MAGIC);
    out.push(DROP_VERSION);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&sealed);
    Ok(out)
}

/// Open and check a record fetched from `name` for `slot`, against `now_s`.
///
/// Every check [`RecordRefusal`] names, in that order, and the refusal says
/// which one fired.
pub fn open(
    keys: &DropKeys,
    name: &DropName,
    slot: u64,
    expected_publisher: &PeerId,
    sealed: &[u8],
    now_s: u64,
) -> std::result::Result<DropRecord, RecordRefusal> {
    use chacha20poly1305::aead::{Aead as _, KeyInit as _, Payload};
    use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

    // The magic and the version are read before anything else and outside the
    // AEAD, so a surface answering with a login page or an error document is
    // told apart from a surface answering with bytes somebody forged.
    let mut found_magic = [0_u8; 4];
    for (place, byte) in found_magic.iter_mut().zip(sealed.iter()) {
        *place = *byte;
    }
    if found_magic != DROP_MAGIC {
        return Err(RecordRefusal::NotARecord { found: found_magic });
    }

    let Some(&version) = sealed.get(DROP_MAGIC.len()) else {
        // Four bytes of magic and nothing after them: the magic matched, so
        // this is not "some other document", it is a truncated record.
        return Err(RecordRefusal::DidNotOpen);
    };
    if version != DROP_VERSION {
        return Err(RecordRefusal::Version {
            found: version,
            expected: DROP_VERSION,
        });
    }

    let Some(nonce) = sealed.get(DROP_MAGIC.len() + 1..HEADER_BYTES) else {
        return Err(RecordRefusal::DidNotOpen);
    };
    let Some(body) = sealed.get(HEADER_BYTES..) else {
        return Err(RecordRefusal::DidNotOpen);
    };
    if body.len() <= TAG_BYTES {
        // Tag and nothing to authenticate. The AEAD would refuse this too;
        // refusing it here keeps the length arithmetic in one place.
        return Err(RecordRefusal::DidNotOpen);
    }

    let cipher = ChaCha20Poly1305::new(Key::from_slice(&keys.seal_key));
    let aad = associated_data(name, expected_publisher);
    let plaintext = cipher
        .decrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: body,
                aad: &aad,
            },
        )
        .map_err(|_| RecordRefusal::DidNotOpen)?;

    // Past this line the bytes authenticated, so what follows can only have
    // been written by a holder of this pair's seal key. A plaintext that is
    // not a record is therefore not an attack, it is this pair's own writer
    // producing something this reader cannot read, and it is refused rather
    // than guessed at.
    let record: DropRecord =
        serde_json::from_slice(&plaintext).map_err(|_| RecordRefusal::DidNotOpen)?;

    if record.v != DROP_VERSION {
        return Err(RecordRefusal::Version {
            found: record.v,
            expected: DROP_VERSION,
        });
    }
    if record.publisher != *expected_publisher {
        return Err(RecordRefusal::WrongPublisher {
            found: record.publisher.display(),
            expected: expected_publisher.display(),
        });
    }
    if record.slot != slot {
        return Err(RecordRefusal::WrongSlot {
            found: record.slot,
            expected: slot,
        });
    }
    if record.at > now_s.saturating_add(MAX_CLOCK_SKEW.as_secs()) {
        return Err(RecordRefusal::FromTheFuture {
            ahead_s: record.at.saturating_sub(now_s),
        });
    }
    let age_s = now_s.saturating_sub(record.at);
    if age_s > MAX_RECORD_AGE.as_secs() {
        return Err(RecordRefusal::Stale {
            age_s,
            max_s: MAX_RECORD_AGE.as_secs(),
        });
    }

    Ok(record)
}
