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
//! # Nothing is spawned yet
//!
//! This file is the crypto, the naming, the store client, and now the
//! publisher and fetch logic: [`own_endpoints`], [`publish_for`],
//! [`fetch_for`] and [`keep_drops_published`]. What is still not here is the
//! boot wiring: nothing calls [`keep_drops_published`] from
//! `server::boot_peer_listener`, nothing calls [`fetch_for`] from the dial
//! order, and there is no CLI verb to turn a store on. Those are a separate
//! change and none of this runs at boot yet.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use tcr_peer_wire::PeerId;

use crate::peer::config::{hmac_sha256, Endpoint, EndpointSource, Observed};

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
///
/// Visible to the rest of the peer module because [`seal_payload`] and
/// [`open_payload`] are, and a second module framing a sealed payload has to
/// size its own header off the same number rather than writing 12 again.
pub(crate) const NONCE_BYTES: usize = 12;

/// Poly1305's tag width, appended to the ciphertext by the AEAD. See
/// [`NONCE_BYTES`] for why it is not private.
pub(crate) const TAG_BYTES: usize = 16;

/// Magic and version: everything this module puts in front of the sealed
/// payload, which carries its own nonce ([`seal_payload`]).
const FRAME_BYTES: usize = DROP_MAGIC.len() + 1;

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
///
/// Visible to the rest of the peer module rather than private, so a sibling
/// deriving a key off the same [`crate::peer::config::PeerRow::rendezvous_secret`]
/// under its OWN versioned domain string calls this ladder instead of writing a
/// second one. Two spellings of RFC 5869 § 2.3 in one crate is the drift
/// `config::hmac_sha256`'s own doc warns about.
pub(crate) fn expand(pseudorandom_key: &[u8; 32], info: &[u8]) -> [u8; 32] {
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

// ---------------------------------------------------------------------------
// The AEAD body, shared with this module's siblings
// ---------------------------------------------------------------------------

/// Why a payload would not seal.
///
/// Two arms and not a string, because the two are a different kind of event: a
/// CSPRNG that refuses is the platform failing, and an AEAD that refuses at
/// this width cannot happen on inputs this code can build. Each caller writes
/// the sentence in its own vocabulary, which is why this type carries no
/// sentence of its own.
pub(crate) enum SealFailure {
    /// The platform CSPRNG would not fill the nonce.
    Csprng(getrandom::Error),
    /// The AEAD refused the inputs.
    Aead,
}

/// A fresh random nonce and the AEAD's output over `plaintext`, as one run of
/// bytes, authenticating `aad` without encrypting it.
///
/// The nonce is twelve RANDOM bytes from the platform CSPRNG and is never
/// derived: a derived nonce repeats the moment one key seals twice over
/// changed content, and a repeated ChaCha20-Poly1305 nonce under a fixed key is
/// a total break. Twelve random bytes against a handful of seals is nowhere
/// near a birthday problem.
///
/// What is NOT here: the magic, the version, and the meaning of `aad`. Framing
/// is each caller's own, so that "these are not our bytes at all" stays a
/// refusal a caller can make before any key is used.
pub(crate) fn seal_payload(
    seal_key: &[u8; 32],
    aad: &[u8],
    plaintext: &[u8],
) -> std::result::Result<Vec<u8>, SealFailure> {
    use chacha20poly1305::aead::{Aead as _, KeyInit as _, Payload};
    use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

    let mut nonce = [0_u8; NONCE_BYTES];
    getrandom::fill(&mut nonce).map_err(SealFailure::Csprng)?;

    let cipher = ChaCha20Poly1305::new(Key::from_slice(seal_key));
    let sealed = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        // The AEAD's own error carries nothing beyond "it failed", and at this
        // width it cannot: the key and nonce are fixed-size arrays.
        .map_err(|_| SealFailure::Aead)?;

    let mut payload = Vec::with_capacity(NONCE_BYTES + sealed.len());
    payload.extend_from_slice(&nonce);
    payload.extend_from_slice(&sealed);
    Ok(payload)
}

/// Why a payload did not open.
///
/// The two arms exist so a caller that wants to tell a cut-off paste apart from
/// a forgery can, and a caller that does not want to tell them apart says so by
/// mapping both to one refusal. Collapsing them here would take the choice away
/// from both.
pub(crate) enum OpenFailure {
    /// Fewer bytes than a nonce and a tag: there is nothing that could
    /// authenticate, whatever the key.
    TooShort,
    /// The bytes did not authenticate: a wrong key, wrong associated data, or
    /// bytes somebody edited.
    DidNotAuthenticate,
}

/// The inverse of [`seal_payload`]: the plaintext, or which of the two ways it
/// failed.
pub(crate) fn open_payload(
    seal_key: &[u8; 32],
    aad: &[u8],
    payload: &[u8],
) -> std::result::Result<Vec<u8>, OpenFailure> {
    use chacha20poly1305::aead::{Aead as _, KeyInit as _, Payload};
    use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

    let Some(nonce) = payload.get(..NONCE_BYTES) else {
        return Err(OpenFailure::TooShort);
    };
    let Some(body) = payload.get(NONCE_BYTES..) else {
        return Err(OpenFailure::TooShort);
    };
    if body.len() <= TAG_BYTES {
        // Tag and nothing to authenticate. The AEAD would refuse this too;
        // refusing it here keeps the length arithmetic in one place.
        return Err(OpenFailure::TooShort);
    }

    let cipher = ChaCha20Poly1305::new(Key::from_slice(seal_key));
    cipher
        .decrypt(Nonce::from_slice(nonce), Payload { msg: body, aad })
        .map_err(|_| OpenFailure::DidNotAuthenticate)
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

    let aad = associated_data(name, &record.publisher);
    let payload =
        seal_payload(&keys.seal_key, &aad, &plaintext).map_err(|failure| match failure {
            SealFailure::Csprng(err) => {
                anyhow::Error::new(err).context("dead drop: the platform CSPRNG refused")
            }
            SealFailure::Aead => anyhow!("dead drop: the record would not seal"),
        })?;

    let mut out = Vec::with_capacity(FRAME_BYTES + payload.len());
    out.extend_from_slice(&DROP_MAGIC);
    out.push(DROP_VERSION);
    out.extend_from_slice(&payload);
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

    let Some(payload) = sealed.get(FRAME_BYTES..) else {
        return Err(RecordRefusal::DidNotOpen);
    };

    let aad = associated_data(name, expected_publisher);
    let plaintext =
        open_payload(&keys.seal_key, &aad, payload).map_err(|failure| match failure {
            // One refusal for both, on purpose: a drop record comes off
            // somebody else's surface, where a short answer is as likely to be
            // a forgery as a truncation, so an operator could do nothing
            // differently with the two told apart. A caller whose bytes came
            // from a person's clipboard should decide otherwise.
            OpenFailure::TooShort | OpenFailure::DidNotAuthenticate => RecordRefusal::DidNotOpen,
        })?;

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

// ---------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------

/// How large a store's answer to `get` may be before it is refused unread.
///
/// A sealed record carrying [`MAX_RECORD_ENDPOINTS`] addresses is a few
/// hundred bytes. Sixty-four KiB is two orders of headroom above that and
/// still refuses a surface that answers with a log file instead of a record.
pub const MAX_STORE_BODY_BYTES: usize = 64 * 1024;

/// Why a store call did not succeed.
///
/// Four variants and not the three the design sketch carries: `TooLarge` is
/// a distinct diagnosis from `Status`, the same reason `RecordRefusal` has
/// seven variants rather than one, an operator reading a log needs to tell
/// them apart. Absence is never a variant here: it is `Ok(None)`, the
/// ordinary outcome `tcr peer reach` gives a router that declines.
#[derive(Debug, thiserror::Error)]
pub enum StoreRefusal {
    /// The call never got an answer: a DNS failure, a refused connection, a
    /// timeout.
    #[error("dead drop: the store could not be reached: {source}")]
    Unreachable {
        #[source]
        source: reqwest::Error,
    },
    /// The store answered, and not with success or an absence.
    #[error("dead drop: the store answered {status} for {name}")]
    Status { status: u16, name: String },
    /// No store is configured at all.
    #[error("dead drop: no store is configured; `tcr peer drop-store set` is the switch")]
    NotConfigured,
    /// The store's answer to `get` passed [`MAX_STORE_BODY_BYTES`] and the
    /// read stopped rather than holding whatever it kept sending.
    #[error("dead drop: the store's answer for {name} passed the {ceiling}-byte ceiling")]
    TooLarge { name: String, ceiling: usize },
}

/// A dumb public key-value surface: an opaque name in, opaque bytes out.
///
/// The seam is the transport and nothing above it. Naming, sealing, freshness
/// and admissibility are the shipped code in every test; a backend swaps only
/// what a remote store would have been. The same rule
/// [`crate::peer::reach::PunchNet`] is written to.
pub trait DeadDropStore {
    /// Place `record` at `name`, replacing whatever was there.
    fn put(
        &self,
        name: &DropName,
        record: &[u8],
    ) -> impl std::future::Future<Output = std::result::Result<(), StoreRefusal>> + Send;

    /// Read what is at `name`. `Ok(None)` is "nothing there", an ordinary
    /// outcome and not an error, the shape `reach_upnp` gives a router that
    /// declines.
    fn get(
        &self,
        name: &DropName,
    ) -> impl std::future::Future<Output = std::result::Result<Option<Vec<u8>>, StoreRefusal>> + Send;
}

/// The backend that ships first: one URL template, `PUT` and `GET`.
///
/// The client is built once, at construction, and held: no later call
/// re-checks the template and no later call builds a client.
pub struct HttpsTemplateStore {
    template: String,
    token: Option<String>,
    client: reqwest::Client,
}

impl HttpsTemplateStore {
    /// Build one from the configured template. Refuses a template with no
    /// `{name}` placeholder at construction, not at the first `put`.
    ///
    /// The client carries `no_proxy` (an ambient `HTTP_PROXY` very commonly
    /// points at tcr itself) and turns redirects off (a store that redirects
    /// a PUT is doing something this node did not ask for), the shape
    /// `reach_upnp`'s client is built in and for the reasons its comments
    /// give.
    pub fn new(template: &str, token: Option<&str>) -> Result<Self> {
        if !template.contains("{name}") {
            bail!("dead drop: the store template has no {{name}} placeholder: {template}");
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()
            .context("dead drop: the store's HTTP client would not build")?;
        Ok(Self {
            template: template.to_string(),
            token: token.map(str::to_string),
            client,
        })
    }

    /// The URL for one name.
    fn url_for(&self, name: &DropName) -> String {
        self.template.replace("{name}", &name.to_wire())
    }
}

impl DeadDropStore for HttpsTemplateStore {
    fn put(
        &self,
        name: &DropName,
        record: &[u8],
    ) -> impl std::future::Future<Output = std::result::Result<(), StoreRefusal>> + Send {
        let url = self.url_for(name);
        let name_wire = name.to_wire();
        let client = self.client.clone();
        let token = self.token.clone();
        let body = record.to_vec();
        async move {
            let mut request = client
                .put(&url)
                .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                .body(body);
            if let Some(token) = &token {
                request = request.bearer_auth(token);
            }
            let response = request
                .send()
                .await
                .map_err(|source| StoreRefusal::Unreachable { source })?;
            let status = response.status();
            if status.is_success() {
                Ok(())
            } else {
                Err(StoreRefusal::Status {
                    status: status.as_u16(),
                    name: name_wire,
                })
            }
        }
    }

    /// 200 with a body is `Ok(Some(bytes))`. 404 and 410 are `Ok(None)`:
    /// nothing there is an ordinary outcome. Anything else is `Status`, a
    /// redirect included, since the client turns redirects off and never
    /// follows one.
    ///
    /// The body is read under [`MAX_STORE_BODY_BYTES`], twice: a
    /// `Content-Length` above it is refused before a byte of body is read,
    /// and the read itself stops at the ceiling, which is the half that
    /// matters, since a promise is not a limit, the rule `reach_upnp::fetch`
    /// states.
    fn get(
        &self,
        name: &DropName,
    ) -> impl std::future::Future<Output = std::result::Result<Option<Vec<u8>>, StoreRefusal>> + Send
    {
        let url = self.url_for(name);
        let name_wire = name.to_wire();
        let client = self.client.clone();
        let token = self.token.clone();
        async move {
            let mut request = client.get(&url);
            if let Some(token) = &token {
                request = request.bearer_auth(token);
            }
            let response = request
                .send()
                .await
                .map_err(|source| StoreRefusal::Unreachable { source })?;
            let status = response.status();
            if status.as_u16() == 404 || status.as_u16() == 410 {
                return Ok(None);
            }
            if !status.is_success() {
                return Err(StoreRefusal::Status {
                    status: status.as_u16(),
                    name: name_wire,
                });
            }
            if let Some(promised) = response.content_length() {
                if promised > MAX_STORE_BODY_BYTES as u64 {
                    return Err(StoreRefusal::TooLarge {
                        name: name_wire,
                        ceiling: MAX_STORE_BODY_BYTES,
                    });
                }
            }
            let mut collected: Vec<u8> = Vec::new();
            let mut response = response;
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|source| StoreRefusal::Unreachable { source })?
            {
                if collected.len() + chunk.len() > MAX_STORE_BODY_BYTES {
                    return Err(StoreRefusal::TooLarge {
                        name: name_wire,
                        ceiling: MAX_STORE_BODY_BYTES,
                    });
                }
                collected.extend_from_slice(&chunk);
            }
            Ok(Some(collected))
        }
    }
}

// ---------------------------------------------------------------------------
// Publish and fetch
// ---------------------------------------------------------------------------

/// What this Mac believes it is reachable at, for a record to `peer`.
///
/// Built from [`crate::peer::reach::global_v6_addresses`],
/// [`crate::peer::reach::external_socket`] and
/// [`crate::peer::reach::observed_self_addresses`] filtered to `peer`, capped
/// at [`MAX_RECORD_ENDPOINTS`], de-duplicated, public IPv6 first: the three
/// sources are pushed in that order and a later duplicate of an address
/// already pushed is dropped rather than moved, so the order the sources are
/// read in is the order the record carries them.
///
/// `listen` supplies the port for the IPv6 addresses, which
/// `global_v6_addresses` does not carry: it answers which address the kernel
/// would source a connection from, not which port this Mac listens on.
///
/// An observed self address is published only when its PORT is one this Mac
/// actually accepts on: `listen.port()`, or a router mapping's external port.
/// `observed_self_addresses` is the source address a peer saw this Mac arrive
/// FROM, and this Mac dials out from an ephemeral port on every outbound
/// connection, so that source address routinely carries a port nothing is
/// listening on. Publishing it anyway spends a friend's dial budget on an
/// address that was never going to answer; a Mac reached over a mapped or
/// listening port instead teaches something that can.
pub fn own_endpoints(listen: SocketAddr, peer: &PeerId) -> Vec<SocketAddr> {
    let mut found: Vec<SocketAddr> = Vec::new();
    let mapped = crate::peer::reach::external_socket();

    for addr in crate::peer::reach::global_v6_addresses() {
        found.push(SocketAddr::new(IpAddr::V6(addr), listen.port()));
    }
    if let Some(mapped) = mapped {
        found.push(mapped);
    }
    let accepted_ports = [Some(listen.port()), mapped.map(|addr| addr.port())];
    for (node, addr) in crate::peer::reach::observed_self_addresses() {
        if &node == peer && accepted_ports.contains(&Some(addr.port())) {
            found.push(addr);
        }
    }

    let mut seen = std::collections::HashSet::new();
    found.retain(|addr| seen.insert(*addr));
    found.truncate(MAX_RECORD_ENDPOINTS);
    found
}

/// Publish this Mac's current address for one friend, in `slot`.
///
/// `Ok(false)` when nothing was published and that is not a failure: the
/// peer has no row here, the row lacks `allow.control.drop`, or the row has
/// no `rendezvous_secret` yet, the same three-way refusal
/// [`crate::peer::reach::rendezvous_ports`] gives a pair with no secret. The
/// cadence rules in the design doc (publish on slot roll or on address
/// change, a sixty-second floor between writes) are [`keep_drops_published`]'s
/// job, which calls this once per tick: this function itself always writes
/// when it is allowed to, so a caller deciding not to call it is the one
/// place the floor is enforced.
pub async fn publish_for<S: DeadDropStore + Sync>(
    store: &S,
    peers_path: &std::path::Path,
    us: &PeerId,
    friend: &PeerId,
    listen: SocketAddr,
    now_s: u64,
) -> Result<bool> {
    let file = crate::peer::config::read_or_default(peers_path)
        .context("dead drop: the peers file did not read, so nothing was published")?;
    let Some(row) = file.peers.iter().find(|row| &row.node == friend) else {
        return Ok(false);
    };
    if !row.allow.control.drop {
        return Ok(false);
    }
    let Some(secret) = row.rendezvous_secret else {
        return Ok(false);
    };

    let eps = own_endpoints(listen, friend);
    let slot = current_slot(now_s);
    let keys = DropKeys::derive(&secret);
    let name = DropName::for_slot(&keys, us, slot);
    let record = DropRecord {
        v: DROP_VERSION,
        publisher: *us,
        slot,
        at: now_s,
        eps,
    };
    let sealed = seal(&keys, &name, &record).context("dead drop: the record would not seal")?;
    store
        .put(&name, &sealed)
        .await
        .context("dead drop: the store refused the write")?;
    Ok(true)
}

/// Whether this process already fetched `friend`'s drop in `slot`.
///
/// A process-local register in `reach::port_secrets`'s shape, so a dial that
/// fails five times in one slot buys one store request and not
/// five: [`fetch_for`] checks this first and only calls the store when it
/// answers `false`, and records the attempt itself once it has decided there
/// is something to fetch.
pub fn fetched_this_slot(friend: &PeerId, slot: u64) -> bool {
    let held = match fetch_register().lock() {
        Ok(held) => held,
        Err(poisoned) => poisoned.into_inner(),
    };
    held.get(friend) == Some(&slot)
}

/// The process-local register [`fetched_this_slot`] reads and
/// [`remember_fetched`] writes.
fn fetch_register() -> &'static Mutex<HashMap<PeerId, u64>> {
    static REGISTER: OnceLock<Mutex<HashMap<PeerId, u64>>> = OnceLock::new();
    REGISTER.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record that this process fetched `friend`'s drop in `slot`, so the next
/// call in the same slot is answered from the register rather than the
/// store.
fn remember_fetched(friend: PeerId, slot: u64) {
    let mut held = match fetch_register().lock() {
        Ok(held) => held,
        Err(poisoned) => poisoned.into_inner(),
    };
    held.insert(friend, slot);
}

/// Fetch one friend's record and record what it holds.
///
/// Tries [`accepted_slots`] of `current_slot(now_s)` in order and stops at
/// the first record that opens. Returns the endpoints that were WRITTEN, not
/// how many the record carried: [`crate::peer::discovery::admissible_drop_endpoints`]
/// decides that.
///
/// The caller that just re-taught a locator needs to know WHICH one, not
/// merely that something changed: a dial that cooled that same locator down a
/// moment earlier has to clear that cooldown and no other, or the fetch looks
/// like it fetched nothing. A count would make the caller re-derive the list
/// by re-reading the row and guessing which entries are new; returning the
/// list itself is the one place that already knows it.
///
/// A store that answers nothing is `Ok(vec![])`, not an error. Absence is
/// never read as "the friend is gone".
///
/// **A peer this node does not pin is refused before the store is ever
/// asked.** The row is read first; a missing row, or one with no
/// `rendezvous_secret`, returns `Ok(vec![])` with zero calls made against
/// `store`. A fetch racing `tcr peer forget` therefore touches neither the
/// file (the eventual write goes through
/// [`crate::peer::config::observe_endpoints`], which independently refuses to
/// create a row) nor the store.
pub async fn fetch_for<S: DeadDropStore + Sync>(
    store: &S,
    peers_path: &std::path::Path,
    friend: &PeerId,
    now_s: u64,
) -> Result<Vec<Endpoint>> {
    let slot = current_slot(now_s);
    if fetched_this_slot(friend, slot) {
        return Ok(Vec::new());
    }

    let file = crate::peer::config::read_or_default(peers_path)
        .context("dead drop: the peers file did not read, so nothing was fetched")?;
    let Some(row) = file.peers.iter().find(|row| &row.node == friend) else {
        return Ok(Vec::new());
    };
    let Some(secret) = row.rendezvous_secret else {
        return Ok(Vec::new());
    };

    remember_fetched(*friend, slot);

    let keys = DropKeys::derive(&secret);
    for candidate_slot in accepted_slots(slot) {
        let name = DropName::for_slot(&keys, friend, candidate_slot);
        let sealed = match store.get(&name).await {
            Ok(Some(bytes)) => bytes,
            Ok(None) => continue,
            Err(err) => {
                tracing::debug!(
                    peer = %friend.display(),
                    error = %err,
                    "dead drop: the fetch failed; the dial continues to Via exactly as it \
                     does today"
                );
                continue;
            }
        };
        let record = match open(&keys, &name, candidate_slot, friend, &sealed, now_s) {
            Ok(record) => record,
            Err(refusal) => {
                tracing::debug!(
                    peer = %friend.display(),
                    error = %refusal,
                    "dead drop: the fetched record was refused"
                );
                continue;
            }
        };

        let observed_at_ms = i64::try_from(now_s.saturating_mul(1_000)).unwrap_or(i64::MAX);
        let learned: Vec<Endpoint> = record
            .eps
            .iter()
            .map(|addr| Endpoint::direct(*addr, observed_at_ms, EndpointSource::Drop))
            .collect();
        let admissible = crate::peer::discovery::admissible_drop_endpoints(row, &learned);
        if admissible.is_empty() {
            return Ok(Vec::new());
        }
        return match crate::peer::config::observe_endpoints(peers_path, friend, &admissible)? {
            Observed::Written { .. } => Ok(admissible),
            Observed::NothingDialable | Observed::NoRow => Ok(Vec::new()),
        };
    }
    Ok(Vec::new())
}

/// Keep every switched-on friend's drop current, until shutdown.
///
/// [`crate::peer::reach::keep_internet_mapping`]'s shape, spawned beside it
/// in `server::boot_peer_listener` as `supervise("peer-dead-drop", ...)`. No
/// shutdown branch here: this loop runs forever, and shutdown is the
/// caller's `tokio::select!` against its own stop signal, unit 3b's job and
/// not this one's.
pub async fn keep_drops_published<S: DeadDropStore + Sync>(
    store: S,
    peers_path: std::path::PathBuf,
    us: PeerId,
    listen: SocketAddr,
    tick: Duration,
) {
    let mut ticker = tokio::time::interval(tick);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;

        let file = match crate::peer::config::read_or_default(&peers_path) {
            Ok(file) => file,
            Err(err) => {
                tracing::warn!(
                    path = %peers_path.display(),
                    error = %err,
                    "dead drop: the peers file did not read, so this tick published nothing"
                );
                continue;
            }
        };
        if !file.dead_drop.is_live() {
            continue;
        }

        let now_ms = crate::now_ms().max(0);
        let now_s = u64::try_from(now_ms / 1_000).unwrap_or(0);
        let friends: Vec<PeerId> = file
            .peers
            .iter()
            .filter(|row| row.allow.control.drop)
            .map(|row| row.node)
            .collect();
        for friend in friends {
            if let Err(err) = publish_for(&store, &peers_path, &us, &friend, listen, now_s).await {
                tracing::warn!(
                    peer = %friend.display(),
                    error = %err,
                    "dead drop: publish failed; retrying at the next tick"
                );
            }
        }
    }
}
