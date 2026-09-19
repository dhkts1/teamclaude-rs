//! Operator intent on disk: `tcr-peers.json` in the operator's config directory.
//!
//! # Why this is not in `teamclaude.json`
//!
//! Every field of the main config except group membership and `groupSettings`
//! is a BOOT-TIME SNAPSHOT (`Config`, `src/config.rs:785`), and that rule is
//! load-bearing: a whole evening was lost once to edits that applied perfectly
//! and were simply not visible yet. Peer policy must not inherit it. A grant
//! (`tcr peer allow`, `tcr peer lend`, `tcr peer share on`) has to take effect
//! without a restart, because a restart of the proxy costs the prompt cache,
//! which is the most expensive event in this system.
//!
//! So peers live in their own file with their own reload rule, and the main
//! config gains no new key and no new exception.
//!
//! **The split inside this file is the design: the SOCKET is boot-time, the
//! POLICY is hot.** [`PeerFile::listen`], [`PeerFile::discovery`] and
//! [`PeerFile::max_hops`] are read once at boot, turning a listener on is a
//! restart-worthy act and saying so is honest. Everything under
//! [`PeerRow::allow`] and [`PeerRow::lend`] is re-read whenever this file's
//! mtime moves, exactly the way `Manager::reload_groups_if_changed`
//! (`src/manager/mod.rs:1990`) already does it for groups. One mechanism, not a
//! second spelling of it.
//!
//! (The blueprint's §4 puts the socket fields in a new `peer` key in
//! `teamclaude.json` instead. Same two tiers, one fewer file to explain; this
//! skeleton keeps them here so nothing in `src/config.rs` changes at all, and
//! the coder who lands phase 2 can move them with Gil's answer in hand.)
//!
//! # Mode 0600, and what an outstanding invite really is
//!
//! [`PendingInvite::secret`] is PSK material the registrar must hold to
//! complete a handshake, so an outstanding invite is join-capable by anything
//! that can read this file. A stored hash cannot be both unusable by an
//! attacker and usable by the registrar; pretending otherwise would be the
//! comfortable lie. What actually bounds it: single use, a short TTL, mode
//! 0600, the row deleted on use, an explicit revoke, and a cap on how many may
//! be outstanding.

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tcr_peer_wire::{LendScope, PeerId, Window};

/// `tcr-peers.json` in the config directory, mode 0600.
pub fn default_path() -> PathBuf {
    crate::peer::id::default_config_dir().join("tcr-peers.json")
}

/// The whole file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerFile {
    /// Where the peer listener binds. **Absent means the feature is off**, and
    /// that is the default: a fresh install discloses nothing, opens no port
    /// and answers nobody. Boot-time.
    ///
    /// **Three verbs write it, and only on an explicit opt-in.** `tcr peer find
    /// on`, `tcr peer share on` and `tcr peer internet on` each need a port for
    /// their answer to mean anything, and each used to leave the operator to
    /// hand-edit this key: `find on` refused outright, and no walkthrough could
    /// get past its first command. When one of those verbs runs and this is
    /// absent, that verb writes [`default_listen`] and prints the line saying
    /// it did.
    ///
    /// The default itself is unchanged and stays [`None`]: a fresh install that
    /// runs no peer command still discloses nothing, opens no port and answers
    /// nobody, and a value the operator already chose is never overwritten.
    /// What moved is only that the opt-in is now one act instead of two, one of
    /// them undocumented.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen: Option<SocketAddr>,
    /// Multicast discovery. Off unless the operator turns finding on.
    /// Boot-time.
    #[serde(default)]
    pub discovery: bool,
    /// The operator's own display name for this machine (`tcr peer name
    /// <name>`), what another Mac shows on a discovered row, and the label
    /// this node offers when it enrols. Hot.
    ///
    /// `None` means "use the host name", resolved at use by
    /// [`PeerFile::display_name`] rather than written into this file as a
    /// serde default. Two reasons: a default that bakes the machine's host name
    /// into the file puts it in every copy of the file thereafter, including a
    /// fixture in a public repository; and a name the operator never chose
    /// should read as absent, so `tcr peer name` can tell "not set" from "set
    /// to the host name".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Whether the discovery beacon carries [`Self::name`]. **Off by default,
    /// and a later decision moved it**, row 9 said on, row 10 overrode it:
    /// "announce sends ephemeral data only unless otherwise configured … the
    /// display name ONLY when `peer.announceName` is on, and its default is now
    /// OFF". A name is the one field in the beacon that is not ephemeral, so it
    /// is the one field that needs the operator to ask for it.
    ///
    /// With it off the beacon is the per-boot instance id, the port and the
    /// wire version. **It is still not a privacy control for identity**,
    /// because the beacon never carried identity either way: no node id, no
    /// public key, no Noise material of any kind (see
    /// [`crate::peer::discovery`]). Hot.
    #[serde(default)]
    pub announce_name: bool,
    /// Whether this Mac may be reached from off its own LAN (`tcr peer
    /// internet on|off`). Off by default, which is the "all opt
    /// in per Mac, off by default".
    ///
    /// With it on, this node asks its router for a TCP mapping to the
    /// listener's port and renews it ([`crate::peer::reach::run_mapping`]).
    ///
    /// **It is not what decides whether a stranger is answered.** That is the
    /// class of the address the listener is BOUND to
    /// ([`crate::peer::listener::internet_admission`]): `listen: 0.0.0.0:7755`
    /// is a world-reachable socket whether or not this switch is on, and a
    /// gate that read the switch as permission answered it anyway.
    ///
    /// The switch is read at the accept gate for exactly one case, and only to
    /// NARROW a refusal: with it OFF, a first pairing or a knock from a source
    /// sharing one of this Mac's own global IPv6 `/64` prefixes is answered,
    /// which is two Macs on one home LAN whose ISP hands out global addresses.
    /// With it on, the operator has asked to be reachable from the internet
    /// and the internet rules apply in full.
    ///
    /// Boot-time for the mapping, which is started once beside the listener;
    /// hot for the accept gate, which is read per connection like every other
    /// policy field here.
    #[serde(default)]
    pub internet: bool,
    /// The opt-in network key: 32 bytes every Mac in one office pastes once.
    ///
    /// `None` is the default and the home-LAN case, where everything still
    /// holds, the caps, the mutes, the bans, the two-phase approval. What a
    /// key buys is the step beyond that: with it set, an announcement carries
    /// an HMAC tag a receiver verifies BEFORE the row exists
    /// ([`NetworkKey::announcement_tag`]) and a knock runs `NNpsk0` with the
    /// key as psk, so a Mac without it "sees nothing and can send nothing that
    /// reaches the UI".
    ///
    /// **It is not identity.** Identity stays the per-Mac static key and the
    /// six digits; this is an admission ticket to the beacon layer, shared by
    /// everyone who holds it. Stored in the clear for the same reason
    /// [`PendingInvite::secret`] is: the node must hold it to compute the tag.
    /// Boot-time for the beacon, hot for a knock. Read per connection like
    /// every other policy field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_key: Option<NetworkKey>,
    /// How many times a frame of ours may be forwarded. `1` is the default and
    /// `0` disables forwarding entirely, **which is also what makes
    /// `tcr peer forget` mesh-wide again**, see [`Allow::relay`]. Boot-time.
    #[serde(default = "default_max_hops")]
    pub max_hops: u8,
    /// How this Mac reaches the internet when its own path is dead
    /// (`tcr peer via auto|off|<peer>`). Hot: it is read per request, on the
    /// transport-failure arm, so turning a carry off takes effect without a
    /// restart, the same rule every other policy field here follows.
    ///
    /// The default is [`crate::peer::egress::ViaRoute::Auto`], and
    /// [`crate::peer::egress::ViaSetting`]'s own docs carry the reason that is
    /// not a disclosure decision: a carry is blind, and the Mac that carries is
    /// the one whose grant admits it.
    #[serde(default)]
    pub via: crate::peer::egress::ViaSetting,
    /// How long a borrow through [`crate::peer::serve::open_serve`] may take
    /// before the client is released, the operator's setting for
    /// [`crate::peer::serve::BORROW_TIMEOUT`], which is now only this field's
    /// default. Hot: `open_serve` reads it per call through the
    /// [`PeerStore`] it is already handed, so lowering it takes effect
    /// without a restart, the same rule every other policy field here
    /// follows.
    #[serde(default = "default_borrow_timeout_ms")]
    pub borrow_timeout_ms: u64,
    /// Which path to a peer is tried first, and what counts as too lossy to
    /// try early. The type is [`crate::peer::probe::PathsConfig`], which is
    /// where the sort it feeds lives. Hot: read per dial.
    #[serde(default)]
    pub paths: crate::peer::probe::PathsConfig,
    /// One row per pinned peer. Hot.
    #[serde(default)]
    pub peers: Vec<PeerRow>,
    /// Invites minted here and not yet used. Hot.
    #[serde(default)]
    pub pending_invites: Vec<PendingInvite>,
    /// The Sharing DEFAULTS row: the grant `tcr peer share on` writes onto
    /// every pinned Mac, including its scope ("the Sharing
    /// defaults sheet gets the same scope picker for the default lease").
    /// Hot.
    ///
    /// `None` until the operator turns sharing on once. It is a RECORD of what
    /// the defaults sheet should show, not a second enforcement point: what
    /// authorizes a lease is the per-peer [`PeerRow::lend`] entry, so a panel
    /// reading this never has to ask which of the two wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_lend: Option<LendGrant>,
    /// The dead drop: where this Mac leaves its current address for a friend
    /// that has also moved, and where it looks for that friend's.
    ///
    /// **Off by default**, the same rule every other peer switch here follows,
    /// and absent from a file that never turned it on: see
    /// [`DeadDropConfig::is_unset`] for why the key is not written in that
    /// case. Boot-time for the publisher, hot for the fetch gate.
    #[serde(default, skip_serializing_if = "DeadDropConfig::is_unset")]
    pub dead_drop: DeadDropConfig,
}

/// What the three opt-in verbs write into [`PeerFile::listen`] when it is
/// absent: `0.0.0.0:7755`. Not a serde default and not [`Default`], so a file
/// nobody opted in on still has no listener at all.
///
/// # A named port, and not `0` for an OS-assigned one
///
/// The other shape available was `0`, letting the kernel pick and persisting
/// the port once the socket is bound. It loses twice. Nothing writes a chosen
/// port back: the bind happens in the serving process (`src/server.rs`), the
/// write happens in a `tcr peer` process that has already exited, and building
/// that round trip would add a second writer for one number. And the port has
/// to be SAYABLE: the other Mac is paired with `tcr peer pair <host:port>`,
/// typed by a person reading the address off this one, so a port that is not
/// known until the proxy next boots cannot be printed by the verb that just
/// turned the feature on. A fixed port is known at the moment it is written,
/// which is the moment it has to be said out loud.
///
/// # `0.0.0.0`, and why the bind is not the admission decision
///
/// A peer on the same LAN arrives on a LAN interface, so loopback cannot be
/// the default, and this Mac's own address moves with its DHCP lease. What
/// keeps a stranger out is not the bind but
/// [`crate::peer::listener::internet_admission`], which refuses a knock and a
/// first pairing from off the LAN precisely BECAUSE a wide bind is not LAN
/// scope; the listener also logs one line at boot saying the socket is wide,
/// so an operator learns it from the running program and not from this file.
///
/// `7755` is the port this tree's own examples already write.
pub fn default_listen() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], 7755))
}

fn default_max_hops() -> u8 {
    1
}

/// Mirrors [`crate::peer::serve::BORROW_TIMEOUT`]: a fresh install behaves
/// exactly as it did before this field existed.
fn default_borrow_timeout_ms() -> u64 {
    10_000
}

/// Hand-written rather than derived: a derived `Default` fills every numeric
/// field with its type's zero, which for `max_hops` and `borrow_timeout_ms`
/// is not the operator's default, it is "forward nothing" and "wait no
/// time at all" respectively. Every field here reuses the same serde default
/// a missing key in the file gets, so a `PeerFile::default()` built in Rust
/// and a `{}` read off disk are the same value, not two.
impl Default for PeerFile {
    fn default() -> Self {
        Self {
            listen: None,
            discovery: false,
            name: None,
            announce_name: false,
            internet: false,
            network_key: None,
            max_hops: default_max_hops(),
            via: crate::peer::egress::ViaSetting::default(),
            borrow_timeout_ms: default_borrow_timeout_ms(),
            paths: crate::peer::probe::PathsConfig::default(),
            peers: Vec::new(),
            pending_invites: Vec::new(),
            default_lend: None,
            dead_drop: DeadDropConfig::default(),
        }
    }
}

/// The opt-in network key, and the two things it is used for.
///
/// A newtype rather than a bare `[u8; 32]` for the reason the wire crate gives
/// for [`PeerId`]: the two are the same SHAPE and completely different
/// MEANINGS, and a function that takes one must not accept the other. On the
/// wire (which here means a share link and an operator's paste buffer), it is
/// Crockford base32 through the one codec this tree has.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub struct NetworkKey([u8; 32]);

impl NetworkKey {
    /// Mint a fresh one through the same CSPRNG every other secret here comes
    /// from.
    pub fn mint() -> Result<Self> {
        Ok(Self(crate::peer::noise::random_secret()?))
    }

    /// Build one from raw bytes, for a share link, which carries the key in
    /// base32, and for a test.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The paste string: 52 Crockford base32 characters.
    pub fn to_paste_string(&self) -> String {
        tcr_peer_wire::encode_key32(&self.0)
    }

    /// The raw bytes, for the Noise `psk0` a knock mixes in.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The announcement tag: `HMAC-SHA256(key, instance_id ‖ port ‖ minute)`,
    /// truncated to the first eight bytes and rendered as lower-case hex.
    ///
    /// # Why this identifies nobody, and why a replay buys nothing
    ///
    /// Everything under the MAC is ephemeral: the instance id is fresh per boot
    /// ([`tcr_peer_wire::InstanceId`]), the port is public, and the minute
    /// moves. So the tag is not a stable name for a machine, and a captured
    /// announcement replayed inside its minute only refreshes a row that
    /// already exists for the 60 seconds the found list keeps it, it cannot
    /// create one for a machine that is not announcing, because the instance id
    /// in the tag is the one in the row.
    ///
    /// Eight bytes and not 32: this is a network admission check under a shared
    /// secret, not a signature, and the TXT record is read by eye with
    /// `dns-sd`. Forging one costs a 2^64 search against a key the forger does
    /// not have, and the thing it buys is one row in a list capped at 12.
    ///
    /// The minute is UTC Unix minutes (`unix_seconds / 60`), so two Macs with
    /// correct clocks agree without a timezone anywhere in the computation. A
    /// receiver checks the current minute and the one before it
    /// ([`Self::tag_matches`]), because a beacon sent at `:59.9` arrives in the
    /// next minute.
    pub fn announcement_tag(
        &self,
        instance_id: &tcr_peer_wire::InstanceId,
        port: u16,
        minute: i64,
    ) -> String {
        let mut message = Vec::with_capacity(tcr_peer_wire::INSTANCE_ID_BYTES + 2 + 8);
        message.extend_from_slice(instance_id.as_bytes());
        message.extend_from_slice(&port.to_be_bytes());
        message.extend_from_slice(&minute.to_be_bytes());
        let mac = hmac_sha256(&self.0, &message);
        let mut out = String::with_capacity(16);
        for byte in &mac[..8] {
            out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
            out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
        }
        out
    }

    /// Whether `tag` verifies for this minute or the one before it.
    ///
    /// Two minutes and not one: a beacon composed at `:59.900` is read by the
    /// other Mac in the next minute, and a one-minute check would drop every
    /// announcement that crossed a boundary, an intermittent, unreproducible
    /// "it sometimes does not see the other Mac". The cost of the second minute
    /// is that a captured announcement is replayable for under two minutes, and
    /// see [`Self::announcement_tag`] for why that buys nothing.
    ///
    /// # Constant time, in both dimensions
    ///
    /// The comparison is [`constant_time_eq`] and the two minutes are folded
    /// with `|` rather than short-circuited by `any`. A `==` on the hex string
    /// returns on the first differing byte, and `any` returns as soon as the
    /// current minute matches, so a sender on this network could have learned
    /// how much of a forged tag was right, and which minute answered, by timing
    /// the reply. This is the whole cost of removing that: two MACs computed
    /// per check instead of one or two, on a path that runs once per beacon.
    pub fn tag_matches(
        &self,
        tag: &str,
        instance_id: &tcr_peer_wire::InstanceId,
        port: u16,
        now_unix_secs: i64,
    ) -> bool {
        let minute = now_unix_secs.div_euclid(60);
        [minute, minute - 1].iter().fold(false, |matched, m| {
            constant_time_eq(
                self.announcement_tag(instance_id, port, *m).as_bytes(),
                tag.as_bytes(),
            ) | matched
        })
    }
}

/// Whether two byte strings are equal, in time that depends on their LENGTHS
/// and not on their contents.
///
/// The loop runs over the longer of the two either way, and the length
/// difference is folded into the accumulator as a `u64` so it cannot cancel
/// itself out the way an `xor` of two `u8` lengths could. `|=` and not `&&`:
/// there is no early exit, which is the entire point.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut difference = (a.len() as u64) ^ (b.len() as u64);
    for index in 0..a.len().max(b.len()) {
        let left = a.get(index).copied().unwrap_or(0);
        let right = b.get(index).copied().unwrap_or(0);
        difference |= u64::from(left ^ right);
    }
    difference == 0
}

impl std::fmt::Debug for NetworkKey {
    /// The bytes are deliberately not printed: this is a shared secret, and a
    /// `{:?}` in a log line is how one ends up in a file.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NetworkKey(set)")
    }
}

impl From<NetworkKey> for String {
    fn from(key: NetworkKey) -> Self {
        key.to_paste_string()
    }
}

impl TryFrom<String> for NetworkKey {
    type Error = tcr_peer_wire::Key32Refusal;

    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        tcr_peer_wire::decode_key32(value.trim()).map(Self)
    }
}

/// HMAC-SHA256, by hand over `sha2`, in the twenty lines RFC 2104 is.
///
/// # Why this is not the `hmac` crate
///
/// Measured rather than assumed: `sha2` 0.10.9 is already in `Cargo.lock`
/// (`snow`'s `use-sha2` resolves it), so calling it is one dependency EDGE and
/// zero new packages, the same precedent `getrandom` is in the root
/// `Cargo.toml` under. `hmac` is **not** in the lock, so reaching for it would
/// be a genuinely new package for the construction below. Re-derive with
/// `cargo tree -i sha2` and `cargo tree -i hmac` (the second prints nothing).
///
/// Verified against RFC 4231's test case 2 rather than against itself:
/// `hmac_sha256_matches_rfc_4231_case_2` in `tests/peer_pairing.rs`. A
/// hand-rolled MAC that is only ever compared to its own output is
/// self-consistent and agrees with nobody, including the next build of this
/// program, if the construction is ever replaced by the `hmac` crate.
///
/// `pub` and over `&[u8]` rather than private and over `&[u8; 32]`, for that
/// test's sake and for one reason beyond it: the RFC's vectors use keys of
/// several widths, and a signature that could only take this program's own key
/// width could only be checked against this program's own output. Every branch
/// the RFC names is therefore written, including the long-key one no caller
/// here reaches.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    use sha2::{Digest as _, Sha256};

    /// SHA-256's block size.
    const BLOCK: usize = 64;

    // RFC 2104: a key longer than the block is replaced by its own hash. No
    // caller in this tree reaches it, the network key is 32 bytes, but a
    // function that silently truncated a long key would be a function whose
    // agreement with the RFC held only for the inputs it was tested on.
    let mut shortened = [0_u8; 32];
    let key: &[u8] = if key.len() > BLOCK {
        shortened.copy_from_slice(&Sha256::digest(key));
        &shortened
    } else {
        key
    };

    let mut ipad = [0x36_u8; BLOCK];
    let mut opad = [0x5c_u8; BLOCK];
    for (slot, byte) in ipad.iter_mut().zip(key.iter()) {
        *slot ^= byte;
    }
    for (slot, byte) in opad.iter_mut().zip(key.iter()) {
        *slot ^= byte;
    }

    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(message);
    let inner = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner);
    outer.finalize().into()
}

impl PeerFile {
    /// The display name to announce and to enrol under: the operator's own, or
    /// the host name when they have not set one.
    ///
    /// Sanitized here, at the one place both callers read, rather than at each
    /// of them: a host name is free text this program did not choose, and it
    /// reaches another machine.
    pub fn display_name(&self) -> String {
        let raw = self.name.clone().unwrap_or_else(host_name);
        tcr_peer_wire::sanitize_label(&raw).unwrap_or_else(|_| "peer".to_string())
    }
}

/// How many endpoints one row keeps. Oldest are dropped first, so a peer that
/// has moved eight times is remembered at the eight places it answered from
/// and not at the twentieth.
pub const MAX_ENDPOINTS_PER_PEER: usize = 8;

/// Where a peer answers: a socket this node can open, or a peer that forwards.
///
/// Separate from [`Endpoint`] so the two facts every endpoint carries, when it
/// was observed and what observed it, are written once rather than once per
/// variant. Spelling it as one enum with two extra fields is a shape Rust
/// does not have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Locator {
    /// A socket this node opens itself.
    Direct { addr: SocketAddr },
    /// Reachable only through a peer that will forward, tried last, because a
    /// hop costs a second machine's bytes and its consent.
    Via { node: PeerId },
    /// Reached over a socket THAT MAC opened to a friend and parked there:
    /// the way in to a Mac nothing can dial at all.
    ///
    /// `node` is the friend holding the carrier, the same field `Via` carries
    /// and for the same reason, so a reader asking "which other Mac is this
    /// path's second end" gets one answer for both. What separates them is who
    /// opened the socket: a `Via` hop is dialled by this Mac when it is used,
    /// a `Reverse` one was already open before anybody wanted it, which is why
    /// it works for a peer with no address anywhere and a `Via` does not.
    Reverse { node: PeerId },
}

/// What taught this node an endpoint.
///
/// Recorded because the six differ in how much they are worth believing, not
/// for display: `Paired` and `Hello` come out of a completed handshake against
/// a pinned static key, `Beacon` from an unauthenticated LAN announcement,
/// `Mapping` from this node's own port-mapping request, `Brief` from a
/// trusted peer's word about a peer we already trust, and `Drop` from a record
/// left at a surface neither Mac owns. An endpoint is routing advice in every
/// case, identity is re-proven by the handshake, so a wrong one costs a
/// connect timeout and never a trust decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EndpointSource {
    /// Written at the pin itself, from the address the pairing ran over.
    Paired,
    /// Off a `Hello` from a session whose key checked out.
    Hello,
    /// Off a discovery beacon whose instance id matches this pinned row.
    Beacon,
    /// This node's own mapped external address.
    Mapping,
    /// Off a trusted peer's `NeighborBrief` about a peer THIS node already
    /// trusts. Never an introduction, only a fresher address for a mutual
    /// friend.
    Brief,
    /// Off a record this node fetched from the pair's dead drop, sealed under a
    /// key derived from the pair's own rendezvous secret.
    ///
    /// **The weakest band.** Unlike [`Self::Brief`], which a mutual friend
    /// vouched for over an authenticated session, this arrived through a
    /// surface nobody here owns, and a peer this node has since forgotten still
    /// holds the key that seals it. It buys one dial attempt and nothing else:
    /// see [`crate::peer::probe::order_endpoints`] for where that is enforced.
    Drop,
}

/// One place a peer was reached, when that was learned, and by what.
///
/// **The pinned static key is the identifier and this is the locator.** Moving
/// does not change who a peer is, which is why [`PeerRow::node`] is the
/// authorization and this list is advice, the split RFC 7401 (HIPv2) makes
/// between a host identity and any address it answers on. `config.rs` has said
/// so in prose since the skeleton; this type is where the prose became a type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Endpoint {
    #[serde(flatten)]
    pub locator: Locator,
    /// Unix milliseconds, from the observation and never from the peer's claim.
    pub observed_at_ms: i64,
    pub source: EndpointSource,
}

impl Endpoint {
    /// A socket endpoint observed now.
    pub fn direct(addr: SocketAddr, observed_at_ms: i64, source: EndpointSource) -> Self {
        Self {
            locator: Locator::Direct { addr },
            observed_at_ms,
            source,
        }
    }

    /// A forwarded endpoint through `node`.
    pub fn via(node: PeerId, observed_at_ms: i64, source: EndpointSource) -> Self {
        Self {
            locator: Locator::Via { node },
            observed_at_ms,
            source,
        }
    }

    /// The socket to open, or `None` for a hop this node cannot dial itself.
    pub fn direct_addr(&self) -> Option<SocketAddr> {
        match self.locator {
            Locator::Direct { addr } => Some(addr),
            // Neither one is a socket this node opens: a `Via` hop is dialled
            // at the forwarder and a `Reverse` one was opened by the far Mac
            // before anybody asked.
            Locator::Via { .. } | Locator::Reverse { .. } => None,
        }
    }

    /// Whether this endpoint needs another machine to carry it.
    pub fn is_via(&self) -> bool {
        matches!(self.locator, Locator::Via { .. })
    }
}

/// One pinned peer: who it is, how to reach it, and exactly what it may do.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", from = "PeerRowOnDisk")]
pub struct PeerRow {
    /// The pinned static key. **This is the authorization.** A changed key on
    /// a pinned node is a refusal that names both fingerprints, never a prompt,
    /// the SSH host-key rule.
    pub node: PeerId,
    /// The operator's label for it, already through the shared sanitizer.
    pub label: String,
    /// Endpoints last known to work, newest first. Routing advice only:
    /// identity is re-proven by the handshake every time.
    ///
    /// **Written through [`PeerRow::observe_endpoint`] and nowhere else**, so
    /// "how did this node learn where that peer is" has one answer. An earlier
    /// build wrote a `addrs: ["host:port"]` key here instead, and
    /// [`PeerRowOnDisk`] reads it.
    #[serde(default)]
    pub endpoints: Vec<Endpoint>,
    /// Unix milliseconds.
    pub added_at: i64,
    #[serde(default)]
    pub allow: Allow,
    /// What this peer may borrow: one entry per lease, so a Mac can hold
    /// several at once ("attic-nuc: 20 % of the `work` group's
    /// 7-day, and all of account A's Fable weekly" is one row with two
    /// leases). An empty list is "nothing".
    ///
    /// **A file written before scopes existed held ONE grant object here**, and it
    /// reads as a one-element list rather than as a parse error. See
    /// [`one_or_many_grants`]. An operator's grant is not something to lose to a
    /// shape change.
    #[serde(default, deserialize_with = "one_or_many_grants")]
    pub lend: Vec<LendGrant>,
    /// The pair's rendezvous secret, so a restarted node can still meet this
    /// peer on a port neither end ever names.
    ///
    /// **Not the handshake hash, and one keyed hash away from it.** This is
    /// [`crate::peer::reach::port_secret`]'s output: HKDF-SHA256 over the
    /// pair's completed handshake hash under its own domain string, which is
    /// the same value both ends already derive independently and the same one
    /// the process-local register holds. The hash itself is what the six-digit
    /// pairing compare is built from, and it stays in memory only (decision
    /// row 17, 2026-09-18).
    ///
    /// Written on every completed session and read at boot. Absent means the
    /// row predates this key, or no session has completed since it was pinned,
    /// and absent is not an error: the derived port is the fallback for a peer
    /// that MOVED, so a row without one has exactly the reach it had before.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "secret32_hex"
    )]
    pub rendezvous_secret: Option<[u8; 32]>,
    /// The address this peer last said it sees THIS Mac at, and when it said
    /// so: `(address, observed_at_ms)`, this node's own clock.
    ///
    /// A Mac behind a NAT cannot see its own public address and the peer that
    /// just answered it can, so this is the one fact about this machine that
    /// only somebody else holds. It is what a punch aims at
    /// ([`crate::peer::reach::punch_target`]), and holding it only in memory
    /// meant a restart could not say where it is until each pair spoke again,
    /// which is the same restart the derived port already learned to survive
    ///
    /// **An address is not a secret**, which is why it is written in the clear
    /// beside every peer's endpoints on a file that already lists them, and
    /// why this needed no new disclosure decision: what it adds is one line
    /// saying where THIS Mac is seen, on a file only this Mac reads.
    ///
    /// Recorded as told, like every endpoint: the session carrying it proved
    /// the pinned static key, and a peer that lies costs one failed punch at
    /// an address nobody is behind. The TIME is this node's clock and never
    /// anything off the wire, the rule [`endpoints_from_hello`] states.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sees_us_at: Option<(String, u64)>,
}

impl PeerRow {
    /// Record one endpoint: newest first, at most [`MAX_ENDPOINTS_PER_PEER`],
    /// one entry per locator.
    ///
    /// **This is the only writer of [`Self::endpoints`]**, which is what makes
    /// the list auditable: every caller reached it holding either a completed
    /// handshake or a beacon that matched a pinned instance id, and a reader
    /// checks that by grepping for this name instead of for an assignment.
    ///
    /// A locator already on the row is REFRESHED rather than duplicated, and
    /// refreshing moves it to the front: the same socket observed again is one
    /// fact with a newer timestamp, not two facts.
    pub fn observe_endpoint(&mut self, endpoint: Endpoint) {
        self.endpoints
            .retain(|existing| existing.locator != endpoint.locator);
        self.endpoints.insert(0, endpoint);
        self.endpoints
            .sort_by_key(|entry| std::cmp::Reverse(entry.observed_at_ms));
        self.endpoints.truncate(MAX_ENDPOINTS_PER_PEER);
    }

    /// Whether this row has any way back to its peer.
    ///
    /// The question four candidate gates ask, spelled once: a row a session
    /// completed against and that has no endpoint is invisible to all of them,
    /// which is the bug the endpoint writer exists to stop.
    pub fn has_endpoint(&self) -> bool {
        !self.endpoints.is_empty()
    }

    /// Which of this row's locators a connection from `source` arrived over.
    ///
    /// Matched on the HOST and never on the port: the recorded endpoint is the
    /// port a peer LISTENS on, and the source of an inbound connection carries
    /// the ephemeral port the kernel gave its dial. Comparing both would
    /// attribute nothing, ever.
    ///
    /// [`None`] when no recorded endpoint has that host, and that is the honest
    /// answer rather than the first locator on the row: a borrow this node
    /// could not attribute must be charged to no path at all
    /// ([`crate::peer::lease::Ledger::note_lease_path`] says the same thing
    /// from the other side), because a guessed path is a figure an operator
    /// would read as measured.
    ///
    /// A `Via` locator never matches: a forwarded connection arrives from the
    /// FORWARDER's socket, so the host on the wire is the hop's and not the
    /// peer's, and saying otherwise would credit the wrong Mac.
    pub fn locator_from(&self, source: SocketAddr) -> Option<Locator> {
        self.endpoints
            .iter()
            .find(|endpoint| {
                endpoint
                    .direct_addr()
                    .is_some_and(|addr| addr.ip() == source.ip())
            })
            .map(|endpoint| endpoint.locator)
    }
}

/// Record endpoints against one pinned row, on disk, under the file lock.
///
/// Returns whether the row was there. A peer that is not pinned is NOT
/// created: this function is called from a session that has already proved a
/// pinned static key, so an absent row means the operator forgot the peer
/// while the session was open, and re-creating it here would undo a
/// revocation. That is the one direction `forget` must never fail in.
///
/// Locked for the reason [`crate::peer::pair::forget`] gives: this is a
/// read-modify-write of the operator's file, and an unlocked one loses
/// whichever write finishes second, here that would be a grant or a
/// revocation, in exchange for a routing hint.
///
/// **No write at all when nothing changed.** An endpoint already on the row at
/// the same locator with the same source is still re-dated, which does move
/// the file's mtime, so the caller decides how often to call this; every
/// caller today is once per session and not once per frame.
pub fn observe_endpoints(path: &Path, peer: &PeerId, endpoints: &[Endpoint]) -> Result<bool> {
    if endpoints.is_empty() {
        return Ok(false);
    }
    let _lock = FileLock::acquire(path)?;
    let mut file = read_or_default(path)?;
    let Some(row) = file.peers.iter_mut().find(|row| &row.node == peer) else {
        return Ok(false);
    };
    for endpoint in endpoints {
        row.observe_endpoint(*endpoint);
    }
    save(path, &file)?;
    Ok(true)
}

/// Record the pair's rendezvous secret on a pinned row.
///
/// `Ok(false)` for a peer this node does not pin, which is not an error: a
/// handshake completes with joiners and with keys pinned a moment ago, and a
/// secret for a row that does not exist has nothing to be for.
///
/// Idempotent by value: the secret is a function of the handshake, so two
/// sessions with the same peer in one boot write the same 32 bytes and the
/// file is left alone the second time rather than rewritten for nothing.
pub fn observe_rendezvous_secret(path: &Path, peer: &PeerId, secret: [u8; 32]) -> Result<bool> {
    let _lock = FileLock::acquire(path)?;
    let mut file = read_or_default(path)?;
    let Some(row) = file.peers.iter_mut().find(|row| &row.node == peer) else {
        return Ok(false);
    };
    if row.rendezvous_secret == Some(secret) {
        return Ok(false);
    }
    row.rendezvous_secret = Some(secret);
    save(path, &file)?;
    Ok(true)
}

/// Record the address `peer` says it sees this Mac at, on that pinned row.
///
/// [`observe_rendezvous_secret`]'s shape and for the same reason: a fact this
/// process learned from a completed session, written where a restart can read
/// it back. `Ok(false)` for a peer this node does not pin, and for a value
/// already on the row, so two sessions in one boot do not rewrite the file for
/// nothing.
///
/// The TIME is the caller's clock. A peer telling this Mac when it was seen
/// would be a peer deciding how fresh its own advice looks.
pub fn observe_seen_address(
    path: &Path,
    peer: &PeerId,
    addr: SocketAddr,
    observed_at_ms: u64,
) -> Result<bool> {
    let _lock = FileLock::acquire(path)?;
    let mut file = read_or_default(path)?;
    let Some(row) = file.peers.iter_mut().find(|row| &row.node == peer) else {
        return Ok(false);
    };
    let seen = addr.to_string();
    if row
        .sees_us_at
        .as_ref()
        .is_some_and(|(held, _)| held == &seen)
    {
        return Ok(false);
    }
    row.sees_us_at = Some((seen, observed_at_ms));
    save(path, &file)?;
    Ok(true)
}

/// The endpoints in a `Hello`, as this node is willing to believe them.
///
/// A `Hello` arrives inside a session whose static key checked out, so the
/// SENDER is proved, but the addresses in it are still a claim about the
/// world, and one this node cannot check. That is safe because an endpoint
/// grants nothing: a wrong one costs a connect timeout and the handshake
/// re-proves identity at the far end regardless. What is NOT safe is letting
/// the claim carry a time, so `observed_at_ms` is this node's own clock and
/// never anything off the wire.
///
/// An entry that is not a socket address is skipped, because the wire type is
/// `Vec<String>` and this node cannot dial a string it cannot parse. Skipped
/// and not refused: a peer running a build that announces something new must
/// not be able to make its `Hello` unreadable.
pub fn endpoints_from_hello(addrs: &[String], observed_at_ms: i64) -> Vec<Endpoint> {
    addrs
        .iter()
        .filter_map(|addr| addr.parse::<SocketAddr>().ok())
        .map(|addr| Endpoint::direct(addr, observed_at_ms, EndpointSource::Hello))
        .collect()
}

/// [`PeerRow`] exactly as it sits on disk, including the key an earlier build
/// wrote.
///
/// A READER and not a rewrite pass, for the reason [`one_or_many_grants`]
/// gives: the next [`save`] writes the new shape anyway, and a migration that
/// ran at read time would have to answer what happens on a read-only file.
///
/// `addrs` held `"host:port"` strings written from a `SocketAddr` at the pin
/// ([`crate::peer::pair`]) and at a lease handshake. An entry that does not
/// parse as a socket address is DROPPED with a warning naming it, rather than
/// refusing the file: the field is routing advice, the next handshake re-learns
/// it, and an operator locked out of their own grants by an unparseable
/// routing hint would be the worse failure.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PeerRowOnDisk {
    node: PeerId,
    label: String,
    #[serde(default)]
    endpoints: Vec<Endpoint>,
    #[serde(default)]
    addrs: Vec<String>,
    added_at: i64,
    #[serde(default)]
    allow: Allow,
    #[serde(default, deserialize_with = "one_or_many_grants")]
    lend: Vec<LendGrant>,
    #[serde(default, with = "secret32_hex")]
    rendezvous_secret: Option<[u8; 32]>,
    #[serde(default)]
    sees_us_at: Option<(String, u64)>,
}

impl From<PeerRowOnDisk> for PeerRow {
    fn from(disk: PeerRowOnDisk) -> Self {
        let mut row = Self {
            node: disk.node,
            label: disk.label,
            endpoints: disk.endpoints,
            added_at: disk.added_at,
            allow: disk.allow,
            lend: disk.lend,
            rendezvous_secret: disk.rendezvous_secret,
            sees_us_at: disk.sees_us_at,
        };
        // The legacy key carried no timestamp of its own, so the row's own
        // `addedAt` is the honest answer to "when was this observed": it is
        // the instant the pin that wrote the address was taken.
        for legacy in &disk.addrs {
            match legacy.parse::<SocketAddr>() {
                Ok(addr) => row.observe_endpoint(Endpoint::direct(
                    addr,
                    disk.added_at,
                    EndpointSource::Paired,
                )),
                Err(err) => tracing::warn!(
                    peer = %row.node.display(),
                    value = %legacy,
                    error = %err,
                    "peers file: dropping a legacy `addrs` entry that is not a socket \
                     address; the next handshake with this peer re-learns where it answers"
                ),
            }
        }
        row
    }
}

/// `lend` as either one grant object (the pre-decision-12 shape) or a list.
///
/// The migration is a READER, not a rewrite pass: nothing walks the file to
/// convert it, because the next [`save`] writes the list shape anyway and a
/// migration that ran at read time would have to answer what happens when the
/// file is read-only.
///
/// # Why this is a visitor and not `#[serde(untagged)]`
///
/// It was an untagged enum, and an untagged enum BUFFERS its input into
/// serde's own `Content` tree before trying each arm. So the refusal was
/// `data did not match any variant of untagged enum OneOrMany`, positioned
/// where the BUFFER ran out rather than where the bad field is: measured on a
/// two-grant list whose second `fraction` is a string, it named line 11,
/// the line the list closes on, for a defect on line 9, and said nothing
/// about `fraction` at all. [`read_peer_file`] promises the operator a line and
/// a column; a refusal that names the wrong line and the wrong problem is
/// worse than one that admits it has neither, because the operator reads the
/// line it names.
///
/// A visitor streams instead: `deserialize_any` hands the sequence or the map
/// straight to `LendGrant`'s own derived impl, so a bad field is refused by
/// the format's own deserializer, at the byte it sits on, in that field's own
/// words (`unknown field`, `invalid type`), with the position `serde_json`
/// attaches.
fn one_or_many_grants<'de, D>(deserializer: D) -> std::result::Result<Vec<LendGrant>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    /// One or a list, and in that order: a single object is a map and a list
    /// is a sequence, so the two shapes cannot both match one input.
    struct OneOrMany;

    impl<'de> serde::de::Visitor<'de> for OneOrMany {
        type Value = Vec<LendGrant>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a list of lend grants, or one grant object")
        }

        fn visit_seq<A>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            let mut grants = Vec::with_capacity(seq.size_hint().unwrap_or_default());
            // `?` and not a filter: one malformed grant in a list is the whole
            // file refused, with ITS line, rather than a peers file that
            // silently lends less than the operator wrote.
            while let Some(grant) = seq.next_element::<LendGrant>()? {
                grants.push(grant);
            }
            Ok(grants)
        }

        fn visit_map<A>(self, map: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            LendGrant::deserialize(serde::de::value::MapAccessDeserializer::new(map))
                .map(|grant| vec![grant])
        }
    }

    deserializer.deserialize_any(OneOrMany)
}

/// Every grant, all default false, so a bare pin can do nothing but say hello.
///
/// Two of these are one direction each, and that asymmetry is the point:
/// [`Self::inspect`] is "I will read THEIR requests" and
/// [`Self::allow_disclose`] is "they may read MINE". A lease therefore needs an
/// explicit act on both machines, and an unconfigured mesh is blind-only.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Allow {
    /// This peer may ask us to forward to a peer WE have pinned.
    ///
    /// **A forward grant is transitive, and the honest consequence is stated
    /// rather than denied:** because a terminal solves its own onward problem
    /// recursively, the node two hops out reaches our gateway with this peer's
    /// authority, bounded by this peer's byte cap. So `tcr peer forget` does
    /// NOT revoke egress reachable through a still-trusted relay. The three
    /// things that do bound it: revoke THIS grant on the intermediate peer, the
    /// gateway byte cap (charged to this peer), and `maxHops: 0`.
    #[serde(default)]
    pub relay: bool,
    /// This peer may ask us to carry bytes out to an allow-listed origin. Blind
    /// by construction: we hold no key for what we carry.
    #[serde(default)]
    pub gateway: bool,
    /// We may ask this peer to carry us out. The inverse of [`Self::gateway`],
    /// kept as a separate field rather than reading `gateway` both ways: one
    /// act (pinning a peer and setting one grant) must not silently hand it
    /// authority over both directions of the carry.
    #[serde(default)]
    pub carry: bool,
    /// This peer may open a SERVE to us, which means **we read its requests in
    /// full** and serve them on our own account.
    #[serde(default)]
    pub inspect: bool,
    /// We may open a SERVE to this peer, which means **it reads our requests in
    /// full**, prompts included.
    #[serde(default)]
    pub allow_disclose: bool,
    /// This peer may hand us an account. Off, confirmed, and last, because it
    /// is the only grant in the design under which a credential crosses a host
    /// boundary at all.
    #[serde(default)]
    pub accept_move: bool,
    /// What our `Hello` tells this peer.
    #[serde(default)]
    pub control: ControlGrants,
}

/// What a peer is told about us, assembled from these booleans rather than
/// filtered on the way out.
///
/// A bare pin receives booleans and addresses: no countable figure, no build
/// identity, no view of our other peers. Without these three, pinning a peer
/// would silently subscribe it to per-window lendable amounts with account
/// counts, our build sha, our boot id and a list of our neighbours.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlGrants {
    /// Adds one level of neighbour briefs.
    #[serde(default)]
    pub briefs: bool,
    /// Adds per-window lendable amounts, their backing account counts, and how
    /// many hops we are from egress.
    #[serde(default)]
    pub lendable: bool,
    /// Adds our build sha and boot id.
    #[serde(default)]
    pub diag: bool,
    /// This peer and this Mac leave each other addresses at a dead drop.
    ///
    /// **Read on both sides**, the symmetry rule
    /// [`crate::peer::discovery::observe_neighbor_briefs`] states for
    /// [`Self::briefs`] and for the same reason: a peer this Mac does not
    /// publish for does not get to write endpoints onto its rows either. Off by
    /// default, like every other switch here.
    #[serde(default)]
    pub drop: bool,
}

/// How long one drop name is valid: one hour.
///
/// Both Macs must agree on it, so it is a number in the file and not a guess
/// per side; a mismatch means the two never meet. One hour and not
/// [`crate::peer::reach::SLOT_SECONDS`] (30) because a port slot is computed
/// locally and costs nothing, while a drop slot costs one write per friend:
/// 30-second slots would be 2,880 writes per friend per day.
pub const DROP_SLOT_SECONDS: u64 = 3_600;

fn default_drop_slot_seconds() -> u64 {
    DROP_SLOT_SECONDS
}

/// The `deadDrop` object: where this Mac leaves its current address for a
/// friend that has also moved, and where it looks for that friend's.
///
/// **Off in every field by default**, so a file with no `deadDrop` key and a
/// `DeadDropConfig::default()` built in Rust are the same value, the rule
/// [`PeerFile`]'s hand-written [`Default`] states. Nothing is published and
/// nothing is fetched until the operator turns it on AND points it at a
/// surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeadDropConfig {
    /// The switch. Boot-time for the publisher, hot for the fetch gate.
    #[serde(default)]
    pub enabled: bool,
    /// The surface to publish on. Absent means off regardless of
    /// [`Self::enabled`], which is what [`Self::is_live`] exists to say once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<StoreConfig>,
    /// How long one drop name is valid, in seconds.
    #[serde(default = "default_drop_slot_seconds")]
    pub slot_seconds: u64,
}

impl Default for DeadDropConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            store: None,
            slot_seconds: default_drop_slot_seconds(),
        }
    }
}

impl DeadDropConfig {
    /// Whether this Mac publishes and fetches at all: the switch AND a surface.
    ///
    /// One function rather than two reads, so "on but pointed at nothing"
    /// cannot mean one thing to the publisher and another to the fetcher.
    pub fn is_live(&self) -> bool {
        self.enabled && self.store.is_some()
    }

    /// Whether this is the value a file with no `deadDrop` key reads as.
    ///
    /// The `skip_serializing_if` predicate for [`PeerFile::dead_drop`]: a Mac
    /// that never turned the dead drop on writes a peers file with no such key,
    /// byte-identical to the one it wrote before this field existed. A key that
    /// appeared on the next save would be a shape change every reader of that
    /// file pays for, in exchange for saying "off" twice.
    pub fn is_unset(&self) -> bool {
        self == &Self::default()
    }
}

/// Which surface a dead drop uses, and what that surface needs.
///
/// The credential lives here, in the peers file, and never in
/// `teamclaude.json`: this is the file whose 0600 mode is checked on read, the
/// same rule [`PeerFile::network_key`] and `PeerRow::rendezvous_secret`
/// already follow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum StoreConfig {
    /// Any HTTPS surface that takes a PUT and answers a GET: `url` is a
    /// template containing `{name}`, used for both.
    Https {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
    },
    /// One gist, written and read through the API.
    Gist { gist_id: String, token: String },
}

/// How a grant's account reaches Anthropic: over the owner's Mac, or on the
/// borrower's own.
///
/// The rule, verbatim: "**No master role.** Sharing stays per grant, and
/// a grant gains a `mode`: `serve` (today: the borrower's requests go over the
/// owner's Mac and out on the owner's IP; the owner reads them) or `hand` (the
/// owner hands the borrower the account's short-lived access token over the
/// authenticated session, the borrower sends locally on its own IP, the owner
/// keeps the refresh token and pushes renewals; revocation is to stop renewing,
/// plus `until`)."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LendMode {
    /// The owner serves the request itself and sees the plaintext. The shape
    /// every grant written before row 15 already had, which is why it is the
    /// default.
    #[default]
    Serve,
    /// The owner hands the borrower a short-lived access token; the request
    /// leaves the borrower's own machine.
    Hand,
}

impl LendMode {
    /// Whether this is the default.
    ///
    /// Named rather than `== Serve` so every reader asks the question the same
    /// way. It was a `skip_serializing_if` predicate until the panel started
    /// reading the mode: see [`LendGrant::mode`] for why the key is now always
    /// written.
    pub fn is_serve(&self) -> bool {
        matches!(self, Self::Serve)
    }
}

/// What this peer may borrow on one window, from which of this node's
/// accounts, and until when.
///
/// # Not `Copy` any more, and the reason is [`Self::scope`]
///
/// A scope names a group or a set of account labels ([`LendScope`]), so it owns
/// a `String` and a `Vec<String>`. Every caller that took a grant by `Copy`
/// clones instead; the alternative (an index into a table of scopes) would be
/// a second place the operator's own words live.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LendGrant {
    /// The wire lease id this grant mints leases under: the handle
    /// `tcr peer lend --revoke <lease-id>` and `--relend <lease-id>` name, and
    /// the id the lender's own ledger keys the lease by.
    ///
    /// **Rendered as lower-case hex and never as a JSON number.** A `u128` in
    /// JSON is a number no double can hold, so every reader that parses JSON
    /// into doubles (the panel among them) would silently round it and then
    /// revoke a lease nobody minted. A `0` id is a grant written before ids
    /// existed, and [`Self::ensure_id`] mints one the next time the file is
    /// written.
    #[serde(default, with = "lease_id_hex")]
    pub id: u128,
    /// Whether the owner serves this grant's requests or hands the borrower a
    /// token. See [`LendMode`]; defaults to
    /// [`LendMode::Serve`], which is what every grant written before modes
    /// existed already meant.
    ///
    /// **Always serialized**, for the reason [`Self::ended`] is. `serve` and
    /// `hand` are two different disclosure decisions, and the panel draws them
    /// differently; an absent key would be read as "serve" by a reader that
    /// knows the default and as "unknown" by one that does not, and the two
    /// readings of one file is exactly the drift a hand-mode grant cannot
    /// afford. It skipped the default until the panel started reading it,
    /// which cost an existing file nothing and a reader one key.
    #[serde(default)]
    pub mode: LendMode,
    /// What this grant lends FROM. Defaults to
    /// [`LendScope::All`], which is what every grant written before scopes
    /// existed already meant.
    ///
    /// It never crosses the wire: see [`LendScope`]'s own doc.
    #[serde(default)]
    pub scope: LendScope,
    pub window: Window,
    /// The ceiling on any one lease, as a fraction of the window.
    ///
    /// **Clamped where it is READ, by [`lend_fraction`], so the ceiling is a
    /// property of this field and not of the one command that happened to
    /// write it.** It used to be clamped only in `tcr peer lend`'s own argv
    /// handling, so a file written by an older build, or edited by hand, or
    /// copied between Macs, reached the sizing arithmetic with any value at all
    /// and this sentence was simply untrue of it.
    #[serde(deserialize_with = "deserialize_lend_fraction")]
    pub fraction: f64,
    /// How long a granted lease lives, in seconds.
    pub ttl_s: u32,
    /// How many relayed requests may be in flight against it at once.
    pub max_inflight: u8,
    /// The absolute end of the LENDING, in unix SECONDS, or `None`
    /// for "no end", which is the default an operator gets by not asking.
    ///
    /// Distinct from [`Self::ttl_s`], which is the renewal deadline of one
    /// lease: a borrower that reaches the ttl asks again, and a borrower that
    /// reaches `until` is done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<u64>,
    /// The daily window, local time, or `None` for every hour.
    ///
    /// Stored as the string the operator typed (`"22:00-08:00"`) and parsed
    /// into two times of day at read, so a hand-edited file is refused at load
    /// with the bad value named rather than half-applied at grant time.
    /// Distinct from [`Self::until`], which ends the lending once: this one
    /// closes and re-opens every day.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub between: Option<crate::peer::schedule::Between>,
    /// The days [`Self::between`] may START on, `None` for every day. A window
    /// that crosses midnight is charged to the day it opens, so a Friday
    /// grant is open into Saturday morning. See [`crate::peer::schedule`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub days: Option<crate::peer::schedule::Days>,
    /// Whether [`Self::until`] has passed, **derived on read and never trusted
    /// from the file**.
    ///
    /// `skip_deserializing`, so a hand-planted (or stale) `"ended": true` on a
    /// live grant reads as `false` and cannot switch lending off by a text
    /// edit; the one answer is [`Self::has_ended`] against a clock.
    /// [`PeerStore::peers`], [`PeerStore::row`] and [`read_or_default`] each
    /// derive it as they hand a row out, so every surface, the picker, the
    /// `lentTo` line, `tcr peer ls --json`, reads the same fact.
    ///
    /// Always serialized (no `skip_serializing_if`), because the panel greys an
    /// ended row and an absent key would read as "still running".
    #[serde(default, skip_deserializing)]
    pub ended: bool,
    /// When the bearer a `hand` grant hands over stops working, in unix
    /// SECONDS, or `None` when this grant hands nothing over right now.
    ///
    /// `skip_deserializing` for the same reason [`Self::ended`] is: it is a
    /// fact about a short-lived credential and a clock, never about the file,
    /// and a hand-planted value must not be able to tell a reader that a dead
    /// key is live. Absent rather than `null` when there is nothing to hand,
    /// which is the common case and every `serve` grant.
    ///
    /// The one writer is `tcr peer ls --json`, which derives it from the
    /// accounts the grant's scope covers; the running proxy answers the same
    /// question through `Manager::handoff_bearer`, and the CLI's answer is
    /// deliberately the narrower of the two (it cannot see an account the
    /// proxy has marked errored at runtime). A borrower's own copy is in
    /// `peer::lease::HandedTokens` and is not this.
    #[serde(default, skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub handed_key_until: Option<u64>,
}

/// 32 bytes as 64 lower-case hex characters, for [`PeerRow::rendezvous_secret`].
///
/// Hex rather than the Crockford base32 a [`PeerId`] uses, because that codec
/// is the identity wire form and this value is not an identity: one glance at
/// a peers file must not make a secret look like a key somebody could paste
/// into `tcr peer pair`.
mod secret32_hex {
    use serde::de::Error as _;
    use serde::{Deserialize as _, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(
        secret: &Option<[u8; 32]>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match secret {
            Some(bytes) => {
                let mut out = String::with_capacity(64);
                for byte in bytes {
                    out.push_str(&format!("{byte:02x}"));
                }
                serializer.serialize_str(&out)
            }
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<[u8; 32]>, D::Error> {
        let Some(raw) = Option::<String>::deserialize(deserializer)? else {
            return Ok(None);
        };
        let trimmed = raw.trim();
        if trimmed.len() != 64 {
            return Err(D::Error::custom(format!(
                "rendezvous secret: expected 64 hex characters and got {}",
                trimmed.len()
            )));
        }
        let mut out = [0_u8; 32];
        for (slot, pair) in out.iter_mut().zip(trimmed.as_bytes().chunks(2)) {
            let text = std::str::from_utf8(pair).map_err(D::Error::custom)?;
            *slot = u8::from_str_radix(text, 16).map_err(|err| {
                D::Error::custom(format!("rendezvous secret: {text:?} is not hex: {err}"))
            })?;
        }
        Ok(Some(out))
    }
}

/// A `u128` lease id as lower-case hex. See [`LendGrant::id`] for why it is
/// not a JSON number.
/// The ceiling on one lease's [`LendGrant::fraction`].
///
/// It mirrors the clamp the main config applies to its own control reserve
/// rather than inventing a second number. Public and read by both ends of the
/// field's life, the `--fraction` flag that accepts one and
/// [`lend_fraction`] that parses one off disk, because a ceiling the writer
/// enforces and the reader does not is a ceiling only for files this build
/// wrote.
pub const MAX_LEND_FRACTION: f64 = 0.5;

/// `raw` as a lend fraction: inside the ceiling, and a number the sizing
/// arithmetic can use.
///
/// A non-finite value answers 0.0, a grant that lends nothing, rather than
/// propagating: `NaN` compares false against both clamp bounds, so it survives
/// a bare `clamp` untouched and then makes every comparison downstream of it
/// answer false, which is a grant that is neither refused nor enforced.
pub fn lend_fraction(raw: f64) -> f64 {
    if !raw.is_finite() {
        return 0.0;
    }
    raw.clamp(0.0, MAX_LEND_FRACTION)
}

/// Read [`LendGrant::fraction`] with the ceiling already applied.
fn deserialize_lend_fraction<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<f64, D::Error> {
    use serde::Deserialize as _;
    Ok(lend_fraction(f64::deserialize(deserializer)?))
}

mod lease_id_hex {
    use serde::de::Error as _;
    use serde::{Deserialize as _, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(id: &u128, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("{id:032x}"))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u128, D::Error> {
        let raw = String::deserialize(deserializer)?;
        let trimmed = raw.trim();
        let trimmed = trimmed.strip_prefix("0x").unwrap_or(trimmed);
        u128::from_str_radix(trimmed, 16).map_err(|err| {
            D::Error::custom(format!(
                "lease id: {raw:?} is not a lease id (32 hex characters, as \
                 `tcr peer lend --list` prints it): {err}"
            ))
        })
    }
}

/// Parse a lease id the way [`LendGrant::id`] is written, for
/// `--revoke`/`--relend` argv.
///
/// Shared with the serde form above rather than written twice: an operator
/// pastes what `--list` printed, so the two spellings must be the one spelling.
pub fn parse_lease_id(raw: &str) -> Result<u128> {
    let trimmed = raw.trim();
    let hex = trimmed.strip_prefix("0x").unwrap_or(trimmed);
    u128::from_str_radix(hex, 16).with_context(|| {
        format!("{raw:?} is not a lease id; paste the one `tcr peer lend --list` printed")
    })
}

/// Render a lease id the way [`LendGrant::id`] serializes it.
pub fn lease_id_string(id: u128) -> String {
    format!("{id:032x}")
}

impl LendGrant {
    /// A grant with today's defaults and no end: every account, one window.
    pub fn new(window: Window, fraction: f64, ttl_s: u32, max_inflight: u8) -> Self {
        Self {
            id: 0,
            mode: LendMode::Serve,
            scope: LendScope::All,
            window,
            fraction,
            ttl_s,
            max_inflight,
            until: None,
            between: None,
            days: None,
            ended: false,
            handed_key_until: None,
        }
    }

    /// The schedule this grant names, or `None` when it names neither a window
    /// nor a set of days.
    ///
    /// One reader for the two keys, so the CLI's "what did I just grant" line,
    /// the panel and [`crate::peer::lease::Ledger::grant`]'s own refusal
    /// cannot disagree about whether a grant is scheduled at all.
    pub fn schedule(&self) -> Option<crate::peer::schedule::Schedule> {
        crate::peer::schedule::Schedule::from_parts(self.between, self.days.as_ref())
    }

    /// Whether the LENDING has ended at `now_s`. The one answer to that
    /// question; [`Self::ended`] is this, cached at read time.
    pub fn has_ended(&self, now_s: u64) -> bool {
        self.until.is_some_and(|until| until <= now_s)
    }

    /// This grant with [`Self::ended`] derived against `now_s`.
    pub fn with_derived_end(mut self, now_s: u64) -> Self {
        self.ended = self.has_ended(now_s);
        self
    }

    /// Mint an id if this grant has none, and answer whether one was minted.
    ///
    /// Called on the WRITE path only ([`save`]), so an id is minted once and
    /// then never moves: a `--revoke` handle that changed when the file was
    /// re-read would revoke a different lease every time.
    pub fn ensure_id(&mut self) -> Result<bool> {
        if self.id != 0 {
            return Ok(false);
        }
        self.id = u128::from_be_bytes(
            crate::peer::noise::random_secret()?[..16]
                .try_into()
                .map_err(|_| anyhow::anyhow!("a 32-byte secret has 16 bytes in it"))?,
        );
        // A minted id of exactly zero would read as "no id" forever. One in
        // 2^128, and answering it costs one line.
        if self.id == 0 {
            self.id = 1;
        }
        Ok(true)
    }
}

/// An invite minted here and not yet spent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingInvite {
    pub id: u64,
    /// The label the joiner will be pinned under, already sanitized.
    pub label: String,
    /// The 32-byte join secret, used as the Noise PSK. See the module docs for
    /// why this is stored in the clear and what actually bounds it.
    pub secret: [u8; 32],
    /// Absolute deadline, Unix milliseconds.
    pub expires_at_ms: i64,
    /// How many joins this invite may still admit. Deleted at zero.
    pub uses_left: u8,
}

/// The live view of the file: the boot-time half taken once, the policy half
/// re-read when the file's mtime moves.
///
/// One store rather than a read per call site, for the reason the group reload
/// already documents: two spellings of "has this changed?" drift, and the one
/// that drifts is the one added later.
pub struct PeerStore {
    path: PathBuf,
    /// The whole file, as of the last successful read. Boot-time fields
    /// ([`PeerFile::listen`], [`PeerFile::discovery`], [`PeerFile::max_hops`])
    /// are trusted only from the FIRST read, taken in [`Self::open`]; a later
    /// [`Self::reload_if_changed`] still holds the whole struct here (one
    /// shape, matching the module docs' "one mechanism" reasoning), but no
    /// caller reads the boot-time fields off anything but the value `open`
    /// produced, so a later edit to `listen`/`discovery`/`maxHops` on disk has
    /// no effect until a restart, exactly as documented.
    file: Mutex<PeerFile>,
    /// The file's mtime as of the last successful (or unmodified-since) read.
    /// `None` before the first successful read.
    mtime: Mutex<Option<SystemTime>>,
    /// The runtime state beside the peers file, read at most ONCE per store and
    /// then held. See [`Self::state`]. `None` until something asks for it, so
    /// a store opened by a caller that never needs the state (the SERVE path,
    /// `Ledger::grant`) reads no second file.
    state: Mutex<Option<crate::peer::state::PeerState>>,
    /// Which state file [`Self::state`] reads. Derived from [`Self::path`] by
    /// [`crate::peer::serve::peer_state_path`], the one derivation this tree
    /// has, and overridable with [`Self::with_state_path`] for the callers
    /// that carry their own (`SessionContext` holds both paths, and
    /// `listener::serve` pairs a temp-dir peers file with the DEFAULT state
    /// file, so deriving it here unconditionally would read the wrong file).
    state_path: PathBuf,
}

impl PeerStore {
    /// Take the boot-time snapshot and hold the path for later reloads.
    ///
    /// A missing file is the default config, a fresh install has pinned
    /// nothing and the listener is off. A malformed file is a refusal that
    /// names the path and the line, never a silent default: a config that
    /// exists but cannot be read is not the same fact as no config at all.
    pub fn open(path: &Path) -> Result<Self> {
        let file = read_or_default(path)?;
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();

        Ok(Self {
            path: path.to_path_buf(),
            file: Mutex::new(file),
            mtime: Mutex::new(mtime),
            state: Mutex::new(None),
            state_path: crate::peer::serve::peer_state_path(path),
        })
    }

    /// Read [`Self::state`] from `path` instead of the one derived beside the
    /// peers file. Takes effect only before the first [`Self::state`] call,
    /// which is the only call that reads.
    #[must_use]
    pub fn with_state_path(mut self, path: PathBuf) -> Self {
        self.state_path = path;
        self
    }

    /// Re-read the policy half if and only if the file's mtime moved. A cheap
    /// no-op otherwise, so it is safe to call on the serving path, which is
    /// the whole reason `tcr peer allow` costs no restart.
    ///
    /// Read failures (a hand-edit that broke the JSON, a file that vanished)
    /// keep the last-known-good policy in place and are logged, never
    /// propagated: the serving path must not go dark over a bad edit to a
    /// hot-reloaded file.
    pub fn reload_if_changed(&self) {
        let current_mtime = match std::fs::metadata(&self.path).and_then(|m| m.modified()) {
            Ok(mtime) => mtime,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    path = %self.path.display(),
                    "peer config reload: could not stat the peers file, keeping current policy"
                );
                return;
            }
        };

        {
            let last = self.mtime.lock().expect("peer store mtime lock poisoned");
            if *last == Some(current_mtime) {
                return;
            }
        }

        // THROUGH `read_or_default`, not `read_peer_file`, the review's M6.
        //
        // `read_peer_file` reads and parses with no `metadata` check at all, so
        // the 0600 refusal held for the file as it was at boot and for no later
        // edit, and the later edit is the one that matters, since this whole
        // file exists to be hot. The rows reloaded here ARE the authorization:
        // `store.row(peer)` and `row.allow.inspect` are what `handle_serve` and
        // `Ledger::grant` decide on.
        //
        // A read failure still keeps the current policy in place (see this
        // function's doc), which is what makes a mode refusal safe to apply
        // here: the serving path does not go dark, it goes on answering with
        // the last policy this program trusted.
        match read_or_default(&self.path) {
            Ok(fresh) => {
                *self.file.lock().expect("peer store file lock poisoned") = fresh;
                *self.mtime.lock().expect("peer store mtime lock poisoned") = Some(current_mtime);
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    path = %self.path.display(),
                    "peer config reload: peers file is unreadable/malformed, keeping current policy"
                );
            }
        }
    }

    /// The pinned rows, as of the last reload, with every grant's
    /// [`LendGrant::ended`] derived against the clock NOW.
    ///
    /// Derived here and not at the reload, because a grant ends on a wall clock
    /// and the file's mtime does not move when 18:00 arrives: a row cached as
    /// "not ended" at reload time would read as live for as long as nobody
    /// touched the file.
    pub fn peers(&self) -> Vec<PeerRow> {
        let now_s = now_unix_secs();
        self.file
            .lock()
            .expect("peer store file lock poisoned")
            .peers
            .iter()
            .cloned()
            .map(|row| row.with_derived_ends(now_s))
            .collect()
    }

    /// The row for one peer, or `None` when it is not pinned, which is the
    /// same answer as "not authorized for anything".
    pub fn row(&self, node: &PeerId) -> Option<PeerRow> {
        let now_s = now_unix_secs();
        self.file
            .lock()
            .expect("peer store file lock poisoned")
            .peers
            .iter()
            .find(|row| &row.node == node)
            .cloned()
            .map(|row| row.with_derived_ends(now_s))
    }

    /// The WHOLE file as of the last read, with every grant's
    /// [`LendGrant::ended`] derived against the clock now. **This opens
    /// nothing**, which is the whole point of it.
    ///
    /// # The review's L2
    ///
    /// [`Self::peers`] hands back the rows and the accept loop needs more than
    /// the rows, [`crate::peer::listener::unauthenticated_allowance`] reads a
    /// cap off the file itself, and the enrolment path reads
    /// [`PeerFile::pending_invites`]. So every caller that wanted the whole
    /// struct called [`read_or_default`] a second time and opened the file
    /// again, on a path that had just read it: two opens per accepted
    /// connection, one of them for bytes already in memory.
    ///
    /// A caller that wants the file as it is on DISK right now calls
    /// [`Self::reload_if_changed`] first, which stats and re-reads only when
    /// the mtime moved. That is the one spelling of "has this changed?" this
    /// module allows, and it keeps a revoke's one-frame latency while costing
    /// nothing when nobody edited anything.
    pub fn file(&self) -> PeerFile {
        let now_s = now_unix_secs();
        let mut file = self
            .file
            .lock()
            .expect("peer store file lock poisoned")
            .clone();
        file.peers = file
            .peers
            .into_iter()
            .map(|row| row.with_derived_ends(now_s))
            .collect();
        file
    }

    /// The runtime state beside this peers file, read ONCE per store and then
    /// cached, [`Self::file`]'s rule for the other file this listener reads.
    ///
    /// `now_ms` is the instant the rows are expired against
    /// ([`crate::peer::state::load`]) and it is taken by the FIRST caller: a
    /// second call inside the same connection gets that read back, which is the
    /// point. A caller that must see another process's write, every
    /// load-modify-save under [`FileLock`], takes the lock and calls
    /// [`crate::peer::state::load`] itself; this accessor is for the READ path
    /// only and says so rather than pretending to be fresh.
    pub fn state(&self, now_ms: i64) -> Result<crate::peer::state::PeerState> {
        let mut cached = self.state.lock().expect("peer store state lock poisoned");
        if let Some(state) = cached.as_ref() {
            return Ok(state.clone());
        }
        let state = crate::peer::state::load(&self.state_path, now_ms)?;
        *cached = Some(state.clone());
        Ok(state)
    }

    /// Where this store reads from. Kept so a log line can name the file
    /// without a second source of truth for the path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Read `path` for a one-shot CLI mutation (`tcr peer name`, `tcr peer allow`,
/// …), or the default file when it does not exist yet. Same mode check and
/// same "missing is the default, malformed is a named refusal" rule as
/// [`PeerStore::open`], this just is not on the hot serving path, so a fresh
/// read per CLI invocation costs nothing and needs no reload machinery.
pub fn read_or_default(path: &Path) -> Result<PeerFile> {
    match std::fs::metadata(path) {
        Ok(meta) => {
            let mode = meta.permissions().mode() & 0o777;
            if mode != 0o600 {
                anyhow::bail!(
                    "{} has mode {mode:o}, expected 0600: refusing to trust a peers file \
                     this program did not write",
                    path.display()
                );
            }
            let mut file = read_peer_file(path)?;
            // Same derivation `PeerStore::peers` does, for the same reason:
            // every surface reads one answer to "has this lease ended", and a
            // CLI that read `ended` off the file would read whatever was last
            // written there.
            let now_s = now_unix_secs();
            for row in &mut file.peers {
                for grant in &mut row.lend {
                    grant.ended = grant.has_ended(now_s);
                }
            }
            Ok(file)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(PeerFile::default()),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

/// An exclusive advisory lock over the peers file, held for one
/// read-mutate-save.
///
/// # The bug this exists to close
///
/// [`crate::peer::pair::accept_enrolment`] reads the file, spends one use of an
/// invite, writes the file. Two joiners presenting the SAME one-use key at the
/// same instant both read `usesLeft: 1`, both decide they may enrol, and both
/// write, and that was measured at **40 double-accepts out
/// of 40 trials**. A one-use key that admits two machines is the whole failure:
/// `save` being atomic does not help, because atomicity is about the file never
/// being half-written, and this is two complete writes each built on a stale
/// read.
///
/// # Why a lockfile and not `flock(2)`
///
/// This crate is `#![forbid(unsafe_code)]` (`src/lib.rs:8`), so `libc::flock`
/// is not reachable from here and neither is any other raw syscall; `fs2` is
/// not in `Cargo.lock` and a new package for one call needs an approval nobody
/// has given. `OpenOptions::create_new` is `O_EXCL|O_CREAT` in safe
/// std and is atomic on every filesystem this ships to, which is exactly the
/// primitive a lockfile needs.
///
/// # What it is honest about
///
/// It is ADVISORY and it is cross-process: it stops two `tcr` processes racing
/// the same file, which is the real case (a listener enrolling a joiner while
/// the operator runs `tcr peer allow`). It does not stop a process that never
/// takes it, and it does not survive `kill -9`, a stale lock is broken after
/// [`LOCK_STALE_MS`] rather than deadlocking the listener for ever, and the
/// break is logged, because a lock that can wedge a proxy is worse than the
/// race it prevents.
pub struct FileLock {
    path: PathBuf,
}

/// How long a caller waits for the lock before giving up. A peers-file write is
/// a few milliseconds; this is a bound on a crashed holder, not a queue.
pub const LOCK_WAIT_MS: u64 = 5_000;

/// How old a lockfile must be before it is treated as abandoned. Well above
/// any real hold, well below the wait above, so a `kill -9`'d holder costs one
/// slow enrolment and not a wedged listener.
pub const LOCK_STALE_MS: u64 = 2_000;

impl FileLock {
    /// Take the lock for `peers_path`, waiting up to [`LOCK_WAIT_MS`].
    pub fn acquire(peers_path: &Path) -> Result<Self> {
        let mut file_name = peers_path.file_name().unwrap_or_default().to_os_string();
        file_name.push(".lock");
        let path = peers_path.with_file_name(file_name);

        let started = std::time::Instant::now();
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Ok(Self { path }),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(err) => {
                    return Err(err)
                        .with_context(|| format!("peers file lock: creating {}", path.display()))
                }
            }

            // A holder that died with the file in place must not wedge the
            // listener. Age is measured on the lockfile itself, so a live
            // holder that is simply slow is never broken in under
            // LOCK_STALE_MS.
            if let Ok(age) = std::fs::metadata(&path).and_then(|m| m.modified()) {
                if age.elapsed().map_or(0, |e| e.as_millis()) > u128::from(LOCK_STALE_MS) {
                    tracing::warn!(
                        path = %path.display(),
                        stale_ms = LOCK_STALE_MS,
                        "peers file lock: breaking a lock older than the stale bound; \
                         its holder is gone"
                    );
                    std::fs::remove_file(&path).with_context(|| {
                        format!("peers file lock: breaking stale {}", path.display())
                    })?;
                    continue;
                }
            }

            if started.elapsed().as_millis() > u128::from(LOCK_WAIT_MS) {
                anyhow::bail!(
                    "peers file lock: {} is held by another process after {}ms; refusing \
                     rather than writing over somebody else's read-modify-write",
                    path.display(),
                    LOCK_WAIT_MS
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_file(&self.path) {
            // Not a silent drop: a lockfile left behind is broken after
            // LOCK_STALE_MS by the next acquirer, and this line is how an
            // operator learns why an enrolment was slow.
            tracing::warn!(
                error = %err,
                path = %self.path.display(),
                "peers file lock: could not release; the next acquirer breaks it as stale"
            );
        }
    }
}

/// Write `file` to `path` at mode 0600, atomically. The one writer every
/// `tcr peer` mutation goes through.
///
/// **A lease id is minted here and nowhere else.** Every grant on its way to
/// disk gets one if it has none ([`LendGrant::ensure_id`]), so the handle
/// `--revoke` and `--relend` name is stable from the moment the grant exists and
/// a caller cannot forget to mint it. Doing it on the READ path instead would
/// hand out a different id per read, which is the same as no handle at all.
pub fn save(path: &Path, file: &PeerFile) -> Result<()> {
    let mut file = file.clone();
    for row in &mut file.peers {
        for grant in &mut row.lend {
            grant.ensure_id()?;
        }
    }
    if let Some(default) = file.default_lend.as_mut() {
        default.ensure_id()?;
    }
    let json = serde_json::to_string_pretty(&file)?;
    crate::config::write_atomic(path, &json)?;
    Ok(())
}

/// How many times this process has opened each peers file, counted at the one
/// place the bytes are actually read.
///
/// A counter and not a log line, because the fact it exists for is a NUMBER
/// per accepted connection (the review's L2: it was two, it must be one), and
/// a test that grepped a log would be counting what this program chose to say
/// rather than what it did.
///
/// **Per PATH, not one global count.** A test binary runs its tests
/// concurrently in one process, so a single counter would be measuring every
/// other test's temp file at the same time and would read whatever the
/// scheduler handed it. Keyed by path, a test counts its own temp file and
/// nobody else's. The map is bounded by the number of distinct peers files a
/// process touches, which is one in production.
///
/// Every route into the file passes through [`read_peer_file`],
/// [`PeerStore::open`], [`PeerStore::reload_if_changed`] and
/// [`read_or_default`], so a regression that re-reads by any of them is
/// counted, not only the one this counter was aimed at.
static PEERS_FILE_OPENS: std::sync::OnceLock<Mutex<std::collections::HashMap<PathBuf, u64>>> =
    std::sync::OnceLock::new();

fn count_peers_file_open(path: &Path) {
    let mut counts = PEERS_FILE_OPENS
        .get_or_init(|| Mutex::new(std::collections::HashMap::new()))
        .lock()
        .expect("peers-file open counter lock poisoned");
    *counts.entry(path.to_path_buf()).or_insert(0) += 1;
}

/// How many times this process has opened `path`. See [`PEERS_FILE_OPENS`].
pub fn peers_file_opens(path: &Path) -> u64 {
    PEERS_FILE_OPENS
        .get_or_init(|| Mutex::new(std::collections::HashMap::new()))
        .lock()
        .expect("peers-file open counter lock poisoned")
        .get(path)
        .copied()
        .unwrap_or(0)
}

/// Read and parse `path`, naming both the path and the line/column on a
/// malformed file, a refusal a reader cannot act on sends them to the
/// source, and "the JSON is bad" with no coordinates does not.
fn read_peer_file(path: &Path) -> Result<PeerFile> {
    count_peers_file_open(path);
    let data =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&data).map_err(|err| {
        anyhow::anyhow!(
            "{} is not valid tcr-peers.json: {err} (line {}, column {})",
            path.display(),
            err.line(),
            err.column()
        )
    })
}

/// The wall clock in unix SECONDS, which is the unit [`LendGrant::until`] is
/// in. Floored, and never negative on any clock this program runs on.
fn now_unix_secs() -> u64 {
    u64::try_from(crate::now_ms().max(0) / 1_000).unwrap_or(0)
}

impl PeerRow {
    /// This row with every grant's [`LendGrant::ended`] derived against
    /// `now_s`.
    fn with_derived_ends(mut self, now_s: u64) -> Self {
        for grant in &mut self.lend {
            grant.ended = grant.has_ended(now_s);
        }
        self
    }

    /// The grant this peer would mint a lease from on `window`: the first one
    /// that is on that window, has not ended, and whose scope this lender can
    /// actually hold a request inside.
    ///
    /// One reader for the picker, the ledger and the CLI, because "which of
    /// this peer's several leases pays for this request" answered two ways is
    /// two different accounts being charged.
    ///
    /// # Why `enforceable`, and why it is passed in
    ///
    /// A grant list is POSITIONAL ("a trusted Mac may hold
    /// several leases at once, one per scope"), so the first row on the window
    /// wins. Without this predicate a `--scope account:bob` row that this
    /// lender cannot serve, `bob` names no account here, or the group is a
    /// spill group the picker would widen, shadowed every row beneath it: the
    /// lease minted against the dead scope and then refused at serve time
    /// (`ScopeRestriction::Unenforceable`), while an `all` grant sat one line
    /// below, able to serve, never reached. An operator who wrote two rows
    /// meant the second one to carry what the first cannot.
    ///
    /// The answer comes from the caller because it is a fact about the
    /// LENDER'S FLEET and this file knows nothing about accounts. Its one
    /// production source is
    /// [`crate::peer::serve::WindowUtilization::scope_restriction`], the same
    /// function the serving leg asks, so a grant skipped here and a request
    /// refused there can never disagree.
    pub fn grant_for(
        &self,
        window: Window,
        now_s: u64,
        enforceable: &dyn Fn(&tcr_peer_wire::LendScope) -> bool,
    ) -> Option<&LendGrant> {
        self.lend
            .iter()
            .filter(|grant| grant.window == window && !grant.has_ended(now_s))
            .find(|grant| {
                if enforceable(&grant.scope) {
                    return true;
                }
                // One line per skipped grant, at warn: this is an operator's
                // own row doing nothing, and the two reasons it can happen,
                // a label that names no account here, a group that is spill
                // and not reserved, are both things only they can fix.
                tracing::warn!(
                    lease = %lease_id_string(grant.id),
                    scope = %grant.scope,
                    window = ?grant.window,
                    "peer lend: this grant's scope cannot be held by this Mac's picker, so it \
                     is skipped and the next grant on this window is offered instead"
                );
                false
            })
    }
}

fn host_name() -> String {
    sysinfo::System::host_name().unwrap_or_else(|| "peer".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **[`constant_time_eq`] agrees with `==` on every case that matters, and
    /// a length difference cannot cancel itself out.**
    ///
    /// The TIMING property is not asserted here and saying so is the honest
    /// part: a wall-clock comparison on a laptop under five other test
    /// binaries measures the scheduler. What is asserted is the thing a
    /// constant-time compare most often gets wrong, folding the lengths in
    /// with an `u8` xor, so two strings whose lengths differ by exactly 256
    /// compare equal. The `a` / `a`-plus-256-more case below is that bug.
    ///
    /// Watch it fail by folding the lengths as `(a.len() ^ b.len()) as u8`:
    /// the 256-apart case then answers true.
    #[test]
    fn constant_time_eq_agrees_with_equality_including_on_lengths() {
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"deadbeefdeadbeef", b"deadbeefdeadbeef"));
        assert!(!constant_time_eq(b"deadbeefdeadbeef", b"deadbeefdeadbeee"));
        // Differs in the FIRST byte, which an early-exit compare answers in
        // one step and this one does not.
        assert!(!constant_time_eq(b"deadbeefdeadbeef", b"0eadbeefdeadbeef"));
        assert!(!constant_time_eq(b"short", b"longer-than-that"));

        // Lengths 1 and 257 differ by exactly 256, and the bytes are ZERO,
        // so the short side's padding xors to zero too and the length fold is
        // the only thing left to catch it. With `b'a'` here the padding would
        // catch it and this would pass for a build with the 8-bit bug.
        let one = vec![0_u8; 1];
        let two_hundred_fifty_seven = vec![0_u8; 257];
        assert!(
            !constant_time_eq(&one, &two_hundred_fifty_seven),
            "lengths 1 and 257 differ by 256, which an 8-bit length fold reads as equal"
        );
    }

    /// **A lease id round-trips through the hex form the file and the CLI
    /// share**, and it is a STRING in JSON.
    ///
    /// A `u128` written as a JSON number is silently rounded by every reader
    /// that parses into a double, the panel among them, so an id that went
    /// out as a number would come back naming a lease nobody minted.
    ///
    /// Watch it fail by serializing `LendGrant::id` without `with =
    /// "lease_id_hex"`: the JSON then carries a bare number and the
    /// `starts_with('"')` assertion fails.
    #[test]
    fn a_lease_id_is_hex_in_the_file_and_parses_back_from_what_the_cli_prints() {
        let id = 0x1eaf_0000_0000_0000_0000_0000_0000_0001_u128;
        let printed = lease_id_string(id);
        assert_eq!(printed.len(), 32, "{printed}");
        assert_eq!(parse_lease_id(&printed).expect("round trip"), id);
        // The two spellings an operator might paste.
        assert_eq!(
            parse_lease_id(&format!(" 0x{printed} ")).expect("0x and spaces"),
            id
        );
        assert!(parse_lease_id("not-an-id").is_err());

        let mut grant = LendGrant::new(Window::SevenDay, 0.20, 300, 2);
        grant.id = id;
        let json = serde_json::to_string(&grant).expect("a grant serializes");
        assert!(
            json.contains(&format!("\"id\":\"{printed}\"")),
            "the id is a JSON string, never a number no double can hold: {json}"
        );
        let back: LendGrant = serde_json::from_str(&json).expect("and reads back");
        assert_eq!(back.id, id);
    }

    /// A grant written before schedules existed has neither key, and that is
    /// "open at every hour", not a grant that refuses everything.
    ///
    /// Watch it fail by making [`crate::peer::schedule::Schedule::from_parts`]
    /// answer `Some(Schedule::always())` instead of `None` when a grant names
    /// neither key: the `schedule()` assertion then reads a schedule where
    /// there is none, which is the difference between a grant the refusal path
    /// never touches and one it evaluates on every lease.
    ///
    /// Dropping `#[serde(default)]` from the two fields does NOT break this:
    /// measured, not assumed: serde's derive already treats a missing
    /// `Option<T>` as `None`. The attribute is kept because every other
    /// optional key on this struct carries it and a reader should not have to
    /// know that rule to see that the field is optional.
    #[test]
    fn a_grant_written_before_schedules_reads_as_no_schedule() {
        let old = r#"{"window":"sevenDay","fraction":0.2,"ttlS":300,"maxInflight":2}"#;
        let grant: LendGrant = serde_json::from_str(old).expect("an older grant still loads");
        assert_eq!(grant.between, None);
        assert_eq!(grant.days, None);
        assert_eq!(
            grant.schedule(),
            None,
            "no keys means no schedule, which is what every grant meant before row 14"
        );
    }

    /// The two keys are stored as the strings an operator types, and the
    /// grant answers with one schedule built from both.
    #[test]
    fn a_scheduled_grant_round_trips_through_the_strings_it_stores() {
        let mut grant = LendGrant::new(Window::SevenDay, 0.20, 300, 2);
        grant.between = Some("22:00-08:00".parse().expect("a window parses"));
        grant.days = Some("fri,sat".parse().expect("days parse"));
        let json = serde_json::to_string(&grant).expect("a grant serializes");
        assert!(
            json.contains(r#""between":"22:00-08:00""#),
            "the window is stored as the operator wrote it: {json}"
        );
        assert!(
            json.contains(r#""days":"fri,sat""#),
            "so are the days: {json}"
        );
        let back: LendGrant = serde_json::from_str(&json).expect("and reads back");
        let schedule = back.schedule().expect("a scheduled grant has a schedule");
        assert_eq!(
            schedule.between,
            Some((
                crate::peer::schedule::Hhmm::new(22, 0).expect("22:00"),
                crate::peer::schedule::Hhmm::new(8, 0).expect("08:00"),
            ))
        );
        assert_eq!(
            schedule.days,
            Some(std::collections::HashSet::from([
                time::Weekday::Friday,
                time::Weekday::Saturday
            ]))
        );
    }

    /// A hand-edited file with a window that is not a window is refused at
    /// LOAD, naming the value, never accepted and then quietly ignored at
    /// grant time, which would read to the operator as a schedule that does
    /// not work.
    #[test]
    fn a_grant_with_an_unparseable_window_is_refused_naming_the_value() {
        let bad = r#"{"window":"sevenDay","fraction":0.2,"ttlS":300,"maxInflight":2,
                      "between":"ten to six"}"#;
        let err = serde_json::from_str::<LendGrant>(bad)
            .expect_err("a window that is not two times of day must be refused");
        assert!(
            err.to_string().contains("ten to six"),
            "the refusal must name the value: {err}"
        );
    }
}
