//! Runtime state: `teamclaude/peer-state.json` in the cache directory.
//!
//! Operator INTENT lives in `tcr-peers.json` in the config directory
//! ([`crate::peer::config`]); what the process learned lives here. The split is
//! the same one the tree already draws between a config and a cache, and it
//! matters for one practical reason: deleting this file must cost nothing but a
//! cold start, and deleting the other one must revoke access.
//!
//! Persisted and restored at boot, TTL-bounded, on the precedent session
//! affinity already set, including its log line, which is the one thing that
//! tells an operator whether a restart cost them anything: how many rows were
//! restored, and how many were dropped as expired.
//!
//! # The version field is not to be bumped for a new field
//!
//! A format-version mismatch makes a loader ignore its file WHOLESALE. For
//! session affinity that would cold-start every live session on the first
//! upgrade, which is the most expensive event in this system, so this file
//! carries **its own** version, never a bump of `affinity.rs`'s, and a new
//! field arrives with a serde default and the version untouched.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tcr_peer_wire::{InstanceId, Lease, PeerId};

/// This file's own format version. See the module docs: additive fields do NOT
/// bump it.
pub const FORMAT_VERSION: u32 = 1;

/// `teamclaude/peer-state.json` in the cache directory, mode 0600.
///
/// Same base-dir resolution as [`crate::affinity::default_path`]
/// (`$XDG_CACHE_HOME`, else `$HOME/.cache`), and the same `teamclaude`
/// directory, a second cache dir would be a second thing to prune.
pub fn default_path() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".cache"))
        })
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("teamclaude").join("peer-state.json")
}

/// What survives a restart.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerState {
    /// The lender's ledger rows, with their ABSOLUTE deadlines. A restart
    /// inside a lease's deadline keeps it valid; one after it restores nothing,
    /// which is the honest outcome and not a failure.
    ///
    /// # This was declared and written by nobody
    ///
    /// The review's M2: this field existed, `load` pruned it, and no code path
    /// ever filled it, so every granted lease died silently on restart while
    /// two doc-comments promised a restore. It is written by
    /// [`crate::peer::lease::Ledger::persist`] and read by
    /// [`crate::peer::lease::Ledger::restored_from`] now, which is what makes
    /// the promise true.
    ///
    /// The element is a [`LeaseRow`] and not a bare [`Lease`], because a lease
    /// alone is not enough to restore: the grantee (the review's M1) and the
    /// scope live beside it in the ledger and never on the wire,
    /// and a restore that dropped them would bring every lease back as a bearer
    /// token drawing on the whole fleet. An `[]`, which is every file any build
    /// has ever written for this key, reads as an empty list under either
    /// shape.
    #[serde(default)]
    pub leases: Vec<LeaseRow>,
    /// Last time each peer answered, Unix milliseconds. Freshness, not
    /// liveness: sleeping is the normal state for a laptop and there is no
    /// peer-down event anywhere in this design.
    #[serde(default)]
    pub last_seen: Vec<(PeerId, i64)>,
    /// Collapse hints already tried and failed, with the instant they may be
    /// tried again.
    #[serde(default)]
    pub collapse_cooldowns: Vec<(PeerId, i64)>,
    /// When the operator opened this node's pairing window, and when it ends.
    ///
    /// Two keys rather than one because a deadline alone cannot tell a clock
    /// that moved from a window that is simply still open. See
    /// [`PairingDeadline`]. Read through [`Self::pairing_window`] and written
    /// through [`Self::open_pairing_window`] / [`Self::close_pairing_window`],
    /// which are the only three functions that touch either key: two keys are
    /// the WIRE shape, and one reader is what keeps them one fact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pairing_window_until_ms: Option<i64>,
    /// See [`Self::pairing_window_until_ms`]. Absent on a file written by a
    /// build that had only the deadline, which reads as a clock this node
    /// cannot bound, and therefore as closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pairing_window_opened_at_ms: Option<i64>,
    /// Knocks waiting on the operator: phase one of pairing.
    ///
    /// **Coalesced by SOURCE ADDRESS, never by instance id**, which is the
    /// whole of the id-changer defence: a rotating instance id from one address
    /// is one row that updates, and not 400 rows. The address is the one field
    /// a completed TCP handshake makes real.
    #[serde(default)]
    pub pending: Vec<Knock>,
    /// Addresses the operator pressed Ignore on, and when the mute lifts.
    #[serde(default)]
    pub muted: Vec<Mute>,
    /// Addresses (and static keys, when a handshake got far enough to learn
    /// one) the operator pressed Block on. No expiry: only `unblock` clears a
    /// ban.
    #[serde(default)]
    pub banned: Vec<Ban>,
    /// The windows an Accept opened, each keyed to ONE instance id at ONE
    /// address. See [`Self::accepted_window`].
    #[serde(default)]
    pub accepted: Vec<AcceptedInstance>,
    /// The leases this node has been GRANTED, beside the ledger rows it has
    /// granted. See [`BorrowedRow`].
    ///
    /// Its own key rather than a flag on [`LeaseRow`], because the two are
    /// opposite facts about opposite Macs: `leases` is what this Mac owes and
    /// funds, `borrowed` is what another Mac has promised it. A reader that
    /// had to check a boolean to know which way a row points is one `if` away
    /// from counting a borrowed lease in this lender's own `lent_fraction`.
    ///
    /// Pruned by [`load`] on the same absolute-deadline rule the ledger rows
    /// use, so no reader ever sees a borrowed lease whose TTL has run out.
    #[serde(default)]
    pub borrowed: Vec<BorrowedRow>,
    /// What each path to each peer cost, as the EWMA left it.
    ///
    /// A cache in the file that is already a cache: losing it costs the prober
    /// twenty minutes of samples and nothing else, which is why it lives here
    /// and not beside the operator's `paths` policy in the peers file. The
    /// split is the same one this module's own docs draw: intent in the config,
    /// what the process learned in the cache.
    ///
    /// **Not pruned by [`load`]**, unlike the leases above. A path's cost has no
    /// deadline: a measurement from before a sleep is stale advice, not an
    /// expired grant, and the first probe after a wake moves it. The
    /// `updated_at_ms` on each row is what a reader judges staleness by.
    #[serde(default)]
    pub paths: Vec<crate::peer::probe::PathStat>,
    /// What each path to each peer CARRIED inside the rolling hour, beside what
    /// it cost.
    ///
    /// Its own section rather than two more fields on [`PathStat`]: that row is
    /// the prober's EWMA and it is written by a different writer, on a different
    /// schedule, with different staleness rules (a cost has no deadline, an
    /// hour's traffic does). One writer per section is what lets both use the
    /// same locked read-modify-write without either publishing the other's
    /// figures as they looked one session ago.
    ///
    /// **Not pruned by [`load`]**, for [`PeerState::paths`]'s reason and one
    /// more of its own: a row whose `updated_at_ms` is older than the window has
    /// not expired, it has rolled off, and a reader that dropped it could not
    /// tell "this path carried nothing this hour" from "nothing here measures
    /// this path". [`crate::status::PathStatus::bytes_per_hour`] draws exactly
    /// that distinction, and it needs the row to draw it.
    #[serde(default)]
    pub path_traffic: Vec<PathTraffic>,
    /// The router mapping the SERVING process holds right now, or [`None`]
    /// when it holds none.
    ///
    /// Written here because the mapping lives in the memory of a process no
    /// CLI invocation can reach: the keeper is a thread inside a running
    /// `tcr`, and `tcr peer reach` is a separate process that would otherwise
    /// have to ask the router itself to answer "what am I mapped at". Asking
    /// the router is the thing this record exists to avoid, because a probe
    /// changes the mapping table it is reporting on.
    ///
    /// **Not pruned by [`load`], and a reader applies
    /// [`MappingRecord::expires_at_ms`] itself.** A record left behind by a
    /// process that died without deleting its mapping is not a live mapping,
    /// and dropping it here instead would lose the distinction between "the
    /// last mapping ended at 18:04" and "nothing ever mapped anything".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mapping: Option<MappingRecord>,
    /// Every key in this file that this build does not know.
    ///
    /// Kept so a read/modify/write here, `tcr peer pair` opening a two-minute
    /// window is exactly that, cannot drop a key a NEWER build wrote. Without
    /// it, the older build's save silently truncates the file and the newer
    /// one starts cold; with it, the two builds share a file.
    #[serde(flatten)]
    pub rest: serde_json::Map<String, serde_json::Value>,
}

/// One lease in the lender's ledger, as it survives a restart.
///
/// The three facts a lease cannot be restored without: the wire [`Lease`]
/// itself, **who it was granted to** and **what it draws from**. The last two
/// are the lender's alone, neither crosses the wire (see
/// [`tcr_peer_wire::LendScope`]), which is exactly why they have to be written
/// here: a restore that read the `Lease` back on its own would resurrect it as
/// a lease any pinned peer could spend, against the whole fleet's headroom.
///
/// **No build writes a row with no grantee, and the reader refuses one.** An
/// all-zero `peer` used to be written as the honest round-trip of a ledger row
/// that had none; it is not honest, because such a row can be spent by nobody
/// ([`crate::peer::lease::Ledger::enter_relay`] answers `NotTheGrantee` to
/// every peer) and yet still counts in `live` and `lent_fraction`, so
/// restoring one holds a slice of this lender's headroom for the lease's whole
/// TTL on behalf of nobody. [`crate::peer::lease::Ledger::persist`] drops such
/// a lease on the way out and
/// [`crate::peer::lease::Ledger::restored_from`] drops it on the way in, each
/// with a count in its log line; this file is hand-editable JSON, which is why
/// both ends check rather than one trusting the other.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseRow {
    pub lease: Lease,
    /// The peer this lease was granted to, the review's M1.
    pub peer: PeerId,
    /// What it draws from. Defaults to
    /// [`tcr_peer_wire::LendScope::All`], which is what every lease written
    /// before scopes existed already meant.
    #[serde(default)]
    pub scope: tcr_peer_wire::LendScope,
}

/// One lease this node has BORROWED, as it survives a restart.
///
/// The borrower's half of [`LeaseRow`], and the reason it exists at all: the
/// borrower's copy of a lease lived only in
/// [`crate::peer::lease::PeerLeaseProvider`]'s in-memory cache, so no CLI
/// process could read it and `tcr peer ls --json` printed `until: null` on
/// every Mac that was being lent to. The lease is the borrower's own view of
/// the lender's promise, a HINT, exactly as that cache's doc says, since the
/// lender's ledger is authoritative and a refusal frame drops the row here
/// rather than arguing.
///
/// Two fields and not three: there is no scope, because the scope
/// stays on the lender and never crosses the wire, and a borrower that wrote
/// one would be writing down a guess.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BorrowedRow {
    pub lease: Lease,
    /// The peer that GRANTED this lease, the Mac this node borrows from, and
    /// the row `tcr peer ls` draws the `until` on.
    pub lender: PeerId,
}

/// What ONE path to ONE peer carried inside the rolling hour, as the serving
/// process's meter last summed it.
///
/// # Why a summed row and not the window itself
///
/// The window lives in memory ([`crate::peer::tunnel::PathMeter`]) for the
/// reason the byte cap does: it is a record of when somebody else's machine was
/// working, and this file is read by any process on this Mac. What is written
/// is the two totals and the instant they were summed at, which is enough for a
/// reader to answer "how much, this hour" and nothing at all about when inside
/// the hour it moved.
///
/// # Reading one of these later
///
/// [`Self::updated_at_ms`] is the whole of the staleness rule and a reader must
/// apply it: the totals are the meter's window as it stood THEN, so a row a
/// full window old describes an hour that has entirely rolled off and its
/// figures are zero now, not stale. A reader that took the numbers at face
/// value would report an hour of traffic on a Mac that has been asleep since.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PathTraffic {
    /// The peer at the far end of the path.
    pub peer: PeerId,
    /// The path itself, spelled exactly as the peers file spells an endpoint,
    /// so a row here and an endpoint there can be compared by value.
    #[serde(flatten)]
    pub locator: crate::peer::config::Locator,
    /// Bytes this Mac carried over this path inside the hour ending at
    /// [`Self::updated_at_ms`].
    pub bytes_last_hour: u64,
    /// Tokens drawn over this path inside the same hour. Zero on every path
    /// whose leases are measured in fractions of a window: a fraction is not a
    /// token count, and converting one would be inventing the number.
    pub tokens_last_hour: u64,
    /// When the meter summed these two, Unix milliseconds.
    pub updated_at_ms: i64,
}

/// The router mapping a serving process holds, as it recorded it.
///
/// Every figure is the GATEWAY's answer and not the request's wish, the rule
/// [`crate::peer::reach::Mapping`] states: a router may hand back a different
/// external port and a shorter lifetime than the one asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MappingRecord {
    /// The socket a peer off this LAN can dial, or [`None`] when the router
    /// mapped the port and would not name its own external address.
    ///
    /// Absent rather than a guess: an external port with no address is not
    /// something anybody can dial, and the port below still says what was
    /// mapped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_address: Option<String>,
    /// The port on the router. The one to advertise.
    pub external_port: u16,
    /// The port on this Mac, which is what the listener is bound to.
    pub internal_port: u16,
    /// When the mapping lapses unless it is renewed, Unix milliseconds.
    ///
    /// The whole of the staleness rule: a reader compares it to its own clock
    /// and treats a passed deadline as no mapping, because a record outlives
    /// the process that wrote it and a crashed keeper renews nothing.
    pub expires_at_ms: i64,
}

/// One knock waiting on the operator, as the Peers tab renders it.
///
/// `addr` is the key: see [`PeerState::pending`]. `instance_id` and
/// `proposed_name` are what the row SHOWS, and both are claims, the name is
/// attacker-chosen text, already through
/// [`tcr_peer_wire::sanitize_label`] on arrival.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Knock {
    /// Where it knocked from, `ip:port`'s IP with the port dropped. See
    /// [`knock_address`]: a source PORT is ephemeral per connection, so keeping
    /// it would defeat the coalescing this row exists for.
    ///
    /// **This is the key, never the dial address.** The mutes, the bans and the
    /// accepted windows are all keyed on this same bare IP, and the listener
    /// compares it against the address a connection arrives on. What an
    /// operator dials to answer is [`Self::dial_address`], which is this plus
    /// [`Self::listen_port`] and is a rendering, not a second key.
    pub addr: String,
    /// The ephemeral id it knocked under. Updated in place when the same
    /// address knocks again with a different one.
    pub instance_id: InstanceId,
    /// The name it asked to be shown as, sanitized. `None` shows the address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_name: Option<String>,
    /// The wire version it claimed.
    pub wire_version: u16,
    /// The port the knocker said its own listener is bound to, when it said
    /// one. See [`tcr_peer_wire::Knock::listen_port`]: the source port this
    /// row's [`Self::addr`] drops is ephemeral, so this is the only number
    /// that can be dialled back.
    ///
    /// `None` on a knock from a build that predates the field, and on the
    /// placeholder row [`PeerState::reserve_knock_slot`] pushes, and then
    /// [`Self::dial_address`] is the bare address, which is what every answer
    /// had before this existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen_port: Option<u16>,
    /// When this address first knocked, Unix milliseconds.
    pub first_seen_ms: i64,
    /// When it last knocked. A row expires [`KNOCK_TTL_MS`] after this.
    pub last_seen_ms: i64,
}

impl Knock {
    /// Whether this row is still an untouched reservation placeholder, the row
    /// [`PeerState::reserve_knock_slot`] pushes before a handshake has told it
    /// anything.
    ///
    /// The ONE answer to "is this a placeholder", read by
    /// [`PeerState::release_knock_reservation`] and by
    /// [`PeerState::visible_pending`]. Two spellings of an all-zero test is how
    /// a row comes to be hidden from the operator and never released, or
    /// released out from under a real knock.
    ///
    /// An all-zero instance id alone is not enough: a real knock's frame could
    /// carry one (nothing on the wire forbids it), and it would arrive with a
    /// non-zero `wire_version`, [`tcr_peer_wire::PROTO_VERSION`] is never zero,
    /// which is why all three fields are compared.
    pub fn is_reservation_placeholder(&self) -> bool {
        self.instance_id == InstanceId([0_u8; tcr_peer_wire::INSTANCE_ID_BYTES])
            && self.proposed_name.is_none()
            && self.wire_version == 0
    }

    /// What to dial to answer this knock: `host:port` when the knocker said
    /// which port it listens on, the bare host when it did not.
    ///
    /// **The one place the answer's address is built**, read by every surface
    /// that shows a pending row, because an operator, and the panel, hand this
    /// string straight to `tcr peer pair`. Before it existed each surface
    /// printed [`Self::addr`] alone, every answer went to whatever was
    /// listening on the default port, and a Mac that listens anywhere else
    /// could not be answered at all: on one machine the dial came back to that
    /// machine's own listener and the pairing digits never appeared.
    ///
    /// An IPv6 host is bracketed, because `fe80::1:7766` is not an address any
    /// parser can split and `[fe80::1]:7766` is. An [`Self::addr`] that is not
    /// an IP address at all, which nothing in this process writes, gets no
    /// port appended rather than a string built around a value this cannot
    /// read.
    pub fn dial_address(&self) -> String {
        let Some(port) = self.listen_port else {
            return self.addr.clone();
        };
        match self.addr.parse::<std::net::IpAddr>() {
            Ok(std::net::IpAddr::V6(host)) => format!("[{host}]:{port}"),
            Ok(std::net::IpAddr::V4(host)) => format!("{host}:{port}"),
            Err(_) => self.addr.clone(),
        }
    }
}

/// An address the operator pressed Ignore on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mute {
    /// The address, in [`knock_address`]'s form.
    pub addr: String,
    /// Absolute deadline, Unix milliseconds. A mute lifts on its own; a ban
    /// does not.
    pub until_ms: i64,
}

/// An address, and its static key when one was learned, that the operator
/// pressed Block on.
///
/// **Both, and that is the answer**: DHCP moves addresses, so a
/// banned Mac that re-knocks from a new address with the same static key has to
/// stay banned; and a Mac whose key was never learned (a knock never reveals
/// one) can still be banned by address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ban {
    /// The address, in [`knock_address`]'s form.
    pub addr: String,
    /// The static key, when a handshake got far enough to learn it. `None` on a
    /// ban that came from a knock, which reveals no static key at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<PeerId>,
    /// When it was banned, Unix milliseconds.
    pub since_ms: i64,
    /// Why, so Settings > Advanced > Blocked can say it.
    pub reason: BanReason,
}

/// Why an address is in [`PeerState::banned`]. A typed enum rather than free
/// text, so the panel row and a `tcr peer ls --json` field cannot disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BanReason {
    /// The operator pressed Block on a pending row.
    Blocked,
    /// A pinned peer the operator blocked rather than merely forgot.
    ForgottenAndBlocked,
}

impl std::fmt::Display for BanReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Blocked => f.write_str("blocked"),
            Self::ForgottenAndBlocked => f.write_str("forgotten-and-blocked"),
        }
    }
}

/// One Accept: a 120-second window for ONE instance id at ONE address.
///
/// The rule is: "on Accept, the responder opens a 120 s window for
/// THAT instance id only". Both halves are checked, because each alone is
/// forgeable in a way the other is not: the address is real (a TCP handshake
/// completed) but shared by every process behind one NAT, and the id is
/// unshared but chosen by the caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcceptedInstance {
    /// The instance id the operator accepted.
    pub instance_id: InstanceId,
    /// The address it knocked from, in [`knock_address`]'s form.
    pub addr: String,
    /// Absolute deadline, Unix milliseconds.
    pub until_ms: i64,
    /// When the operator accepted, so a clock set BACKWARD closes the window
    /// rather than extending it, the same rule
    /// [`PeerState::pairing_window`] documents.
    pub opened_at_ms: i64,
    /// The static key the `XX` handshake inside this window learned, once it
    /// has run.
    ///
    /// `None` until then, which is the honest state: a knock reveals no static
    /// key at all, so an address accepted and then blocked before it dialled
    /// can only be banned by address. This field is what makes the other half
    /// of "ban scope: both" reachable. See
    /// [`PeerState::ban`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub learned_key: Option<PeerId>,
}

/// How long a knock sits in [`PeerState::pending`] before it expires on its
/// own: ten minutes. Nothing else leaves PENDING without the
/// operator.
pub const KNOCK_TTL_MS: i64 = 600_000;

/// How long Ignore mutes an address: one hour.
pub const MUTE_MS: i64 = 3_600_000;

/// How many CLOSED accepted windows are kept for the static key their
/// handshake revealed. See [`PeerState::expire`], which keeps them, and
/// [`PeerState::key_learned_at`], which is the only reader.
///
/// A number because the rows no longer age out on a deadline: without one, a
/// Mac that pairs often would grow this list for the life of the file.
pub const MAX_LEARNED_KEYS: usize = 32;

/// How many knocks may be outstanding at once. `abuse-resistance.md` says
/// eight, and the ninth is refused with zero bytes, a
/// flood from a `/24` of addresses costs eight rows, not 254.
pub const MAX_PENDING_KNOCKS: usize = 8;

/// The coalescing key for a knock: the peer's IP, with the ephemeral source
/// port dropped.
///
/// **Dropping the port is the coalescing.** A source port is fresh on every TCP
/// connection, so keeping it would file each knock from one machine under a new
/// key and the eight-row cap would be eight knocks rather than eight machines,
/// which is the flood this exists to bound.
pub fn knock_address(addr: &std::net::SocketAddr) -> String {
    addr.ip().to_string()
}

/// Whether this node is currently willing to answer a first pairing, and, on
/// the open arm, the deadline it is willing until.
///
/// Mirrors [`crate::peer::pair::PairingWindow`], which is the type the
/// listener takes; this one is what the state file can answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingDeadline {
    /// An operator asked, `now_ms` is inside the window, and this is its end.
    Open {
        /// Absolute deadline, Unix milliseconds.
        until_ms: i64,
    },
    /// Nobody asked, the deadline has passed, or the clock no longer agrees
    /// with the two stamps in the file.
    Closed,
}

impl PeerState {
    /// Whether a first pairing may be answered at `now_ms`.
    ///
    /// **A clock jump closes the window, in either direction**, which is the
    /// fail-closed half of the invite TTL's rule ("a clock that moved is not a
    /// second chance"). The window is open only while `now_ms` lies inside the
    /// interval the operator's own act defines: at or after the instant they
    /// opened it, and strictly before its deadline. So a clock set BACKWARD
    /// (now before the open stamp) reads closed rather than extending a
    /// stranger's chance to harvest this node's static key, and a clock set
    /// forward past the deadline reads closed as it always did.
    ///
    /// An interval rather than a monotonic clock because the writer and the
    /// reader are two processes: `Instant` is not comparable across them and
    /// has no serialized form, so the honest cross-process version of "the
    /// clock did not move" is "both stamps still bracket now".
    pub fn pairing_window(&self, now_ms: i64) -> PairingDeadline {
        let (Some(opened_at_ms), Some(until_ms)) = (
            self.pairing_window_opened_at_ms,
            self.pairing_window_until_ms,
        ) else {
            return PairingDeadline::Closed;
        };
        if now_ms >= opened_at_ms && now_ms < until_ms {
            PairingDeadline::Open { until_ms }
        } else {
            PairingDeadline::Closed
        }
    }

    /// Open the window at `now_ms` and report its deadline.
    pub fn open_pairing_window(&mut self, now_ms: i64, window_secs: i64) -> i64 {
        let until_ms = now_ms.saturating_add(window_secs.saturating_mul(1000));
        self.pairing_window_opened_at_ms = Some(now_ms);
        self.pairing_window_until_ms = Some(until_ms);
        until_ms
    }

    /// Close it, so a completed or abandoned pairing does not leave the
    /// remainder of its two minutes standing. Reports whether anything was
    /// open to close.
    /// Drop every knock, mute and accepted window that has aged out at
    /// `now_ms`, and report how many rows went.
    ///
    /// Bans are NOT touched: a ban has no deadline by design, and only
    /// `tcr peer unblock` clears one.
    pub fn expire(&mut self, now_ms: i64) -> usize {
        let before = self.pending.len() + self.muted.len() + self.accepted.len();
        self.pending
            .retain(|knock| now_ms.saturating_sub(knock.last_seen_ms) < KNOCK_TTL_MS);
        self.muted.retain(|mute| mute.until_ms > now_ms);
        // A closed window whose handshake LEARNED A STATIC KEY is kept as a
        // record, and it authorizes nothing: [`Self::accepted_window`] reads
        // the clock, so an expired row admits no pairing either way.
        //
        // What it buys is the other half of a ban. The key a first pairing
        // reveals is written here and nowhere else, the window lasts two
        // minutes, and an operator who blocks the Mac afterwards, which is the
        // ordinary case, since the reason to block is usually the digits not
        // matching, used to record an address-only ban and read "no handshake
        // ever revealed a static key". A new DHCP lease then walked straight
        // past it.
        self.accepted.retain(|window| {
            (now_ms >= window.opened_at_ms && now_ms < window.until_ms)
                || window.learned_key.is_some()
        });
        // Bounded, because the kept rows no longer age out on their own: the
        // newest [`MAX_LEARNED_KEYS`] closed ones, by when their window opened.
        let mut closed: Vec<usize> = self
            .accepted
            .iter()
            .enumerate()
            .filter(|(_, window)| now_ms >= window.until_ms)
            .map(|(index, _)| index)
            .collect();
        if closed.len() > MAX_LEARNED_KEYS {
            closed.sort_by_key(|index| self.accepted[*index].opened_at_ms);
            // OVER THE CLOSED ROWS, which is what the bound is counted in. It
            // dropped `self.accepted.len() - MAX_LEARNED_KEYS`, and that length
            // counts the OPEN windows too, so every window still open cost one
            // extra learned key: the oldest keys went early and an operator who
            // blocked that Mac got an address-only ban.
            let doomed_count = closed.len().saturating_sub(MAX_LEARNED_KEYS);
            let doomed: std::collections::BTreeSet<usize> =
                closed.into_iter().take(doomed_count).collect();
            let mut index = 0;
            self.accepted.retain(|_| {
                let keep = !doomed.contains(&index);
                index += 1;
                keep
            });
        }
        before - (self.pending.len() + self.muted.len() + self.accepted.len())
    }

    /// Whether `addr` is muted right now.
    pub fn is_muted(&self, addr: &str, now_ms: i64) -> bool {
        self.muted
            .iter()
            .any(|mute| mute.addr == addr && mute.until_ms > now_ms)
    }

    /// Whether `addr` is banned.
    pub fn is_address_banned(&self, addr: &str) -> bool {
        self.banned.iter().any(|ban| ban.addr == addr)
    }

    /// Whether this static key is banned, **from any address**.
    ///
    /// The reason ban scope is both halves: DHCP moves addresses, so a banned
    /// Mac that comes back on a new lease with the same static key must still
    /// be refused. Checked after the handshake has learned the key, which is
    /// the earliest moment it can be.
    pub fn is_key_banned(&self, key: &PeerId) -> bool {
        self.banned.iter().any(|ban| ban.key.as_ref() == Some(key))
    }

    /// Whether a knock from `addr` would be refused, without recording
    /// anything.
    ///
    /// Mirrors the early refusals in [`Self::record_knock`] exactly, banned,
    /// muted, or the queue full for a NEW address, because all three depend
    /// only on `addr` and the queue's current rows, never on the knock's own
    /// payload (its instance id, proposed name, or wire version). That is
    /// what lets a caller check this BEFORE the responder has written NN
    /// message 2: `listener::serve_knock` calls it right after the rate
    /// bucket, before `noise::finish_responder`, so a muted or over-cap
    /// source is closed with zero bytes written rather than being refused
    /// only after message 2 (and the ack it earns nothing for) already went
    /// out.
    pub fn refusal_for_knock_source(&self, addr: &str, now_ms: i64) -> Option<KnockRefusal> {
        if self.is_address_banned(addr) {
            return Some(KnockRefusal::Banned);
        }
        if self.is_muted(addr, now_ms) {
            return Some(KnockRefusal::Muted);
        }
        // An existing row for this address coalesces in `record_knock`
        // rather than being refused, so it must not count against the cap
        // here either.
        if self.pending.iter().any(|knock| knock.addr == addr) {
            return None;
        }
        if self.pending.len() >= MAX_PENDING_KNOCKS {
            return Some(KnockRefusal::QueueFull {
                pending: self.pending.len(),
            });
        }
        None
    }

    /// Record a knock, coalescing on its address.
    ///
    /// `Ok(true)` means a NEW row was added, `Ok(false)` that an existing row
    /// for this address was updated in place. `Err` is the refusal, and its
    /// arms are the whole of what a stranger can make this queue do.
    pub fn record_knock(
        &mut self,
        addr: &str,
        instance_id: InstanceId,
        proposed_name: Option<String>,
        wire_version: u16,
        listen_port: Option<u16>,
        now_ms: i64,
    ) -> Result<bool, KnockRefusal> {
        if let Some(refusal) = self.refusal_for_knock_source(addr, now_ms) {
            return Err(refusal);
        }
        if let Some(existing) = self.pending.iter_mut().find(|knock| knock.addr == addr) {
            // An id changer is one row that updates. That is the defence, not
            // a convenience: coalescing by instance id instead would let one
            // address hold all eight slots by rotating its id eight times.
            existing.instance_id = instance_id;
            existing.proposed_name = proposed_name;
            existing.wire_version = wire_version;
            // The newest knock's port, including back to `None`: a Mac that
            // moved its listener, or that downgraded to a build which says no
            // port at all, must not be answered at the port it used to be on.
            existing.listen_port = listen_port;
            existing.last_seen_ms = now_ms;
            return Ok(false);
        }
        // `refusal_for_knock_source` already confirmed the queue has room for
        // a new address; this is the same check again to keep the invariant
        // local should it ever run without that guard.
        if self.pending.len() >= MAX_PENDING_KNOCKS {
            return Err(KnockRefusal::QueueFull {
                pending: self.pending.len(),
            });
        }
        self.pending.push(Knock {
            addr: addr.to_string(),
            instance_id,
            proposed_name,
            wire_version,
            listen_port,
            first_seen_ms: now_ms,
            last_seen_ms: now_ms,
        });
        Ok(true)
    }

    /// Atomically check the ban/mute/queue-full refusal AND, if admitted,
    /// reserve the row for `addr`, under whatever lock the caller holds
    /// while calling this.
    ///
    /// **Closes the TOCTOU between the pre-handshake refusal check and the
    /// post-handshake [`Self::record_knock`] write.** A caller that only
    /// READ this state to decide whether to proceed (as `listener::serve_knock`
    /// used to, unlocked) leaves a window in which several concurrent knocks
    /// all read the same "there is room" snapshot before any of them writes,
    /// so all of them run the Noise handshake and earn a real message 2 on the
    /// wire, and only the queue-full ones are refused afterwards, once it is
    /// too late to have written nothing. Calling this UNDER the same
    /// [`crate::peer::config::FileLock`] the caller later re-acquires for
    /// [`Self::record_knock`], and reserving the row right here rather than
    /// only checking, removes that window: an over-cap or banned/muted knock
    /// is refused before the handshake ever starts, with the same "zero bytes
    /// written" guarantee the ban and mute checks already had.
    ///
    /// The reservation is a placeholder row, a real instance id is not known
    /// until the handshake decrypts the knock frame, with `instance_id` all
    /// zero and no name. [`Self::record_knock`] coalesces on `addr` exactly
    /// like a second knock from an address already in the queue, so the
    /// placeholder is filled in with the real details once the handshake
    /// completes; if it never completes, [`Self::release_knock_reservation`]
    /// undoes it, or (failing that) the row ages out after
    /// [`KNOCK_TTL_MS`] like any other abandoned knock.
    ///
    /// `Ok(true)` means a NEW placeholder row was pushed and the caller owns
    /// releasing it on failure; `Ok(false)` means an existing pending row
    /// already covered this address (a real earlier knock, or a sibling
    /// reservation that won the race first) and the caller must NOT release
    /// it, that row is not this caller's to remove.
    pub fn reserve_knock_slot(&mut self, addr: &str, now_ms: i64) -> Result<bool, KnockRefusal> {
        if let Some(refusal) = self.refusal_for_knock_source(addr, now_ms) {
            return Err(refusal);
        }
        if self.pending.iter().any(|knock| knock.addr == addr) {
            // Already reserved (or pending) for this address, nothing new
            // to hold. `record_knock` will update the existing row in place.
            return Ok(false);
        }
        self.pending.push(Knock {
            addr: addr.to_string(),
            instance_id: InstanceId([0_u8; tcr_peer_wire::INSTANCE_ID_BYTES]),
            proposed_name: None,
            wire_version: 0,
            // Nothing has been decrypted yet, so there is no port to hold
            // either. `record_knock` fills it with the real one.
            listen_port: None,
            first_seen_ms: now_ms,
            last_seen_ms: now_ms,
        });
        Ok(true)
    }

    /// The pending knocks an OPERATOR should see: every row except an
    /// unfilled reservation placeholder.
    ///
    /// The defect this exists for is a real one: a reservation is a
    /// placeholder row with an all-zero instance id and no name, pushed BEFORE
    /// the handshake so the cap is taken atomically, and it lands in
    /// [`Self::pending`], which `tcr peer pending` and the panel render
    /// verbatim. So a connection that opened and stalled showed the operator a
    /// pairing request from nobody, with a blank name and an instance id of
    /// zeros, that no Accept could complete.
    ///
    /// A FILTER on the read surfaces and not a drop at load: the row IS the
    /// reservation, and the cap it holds is the whole reason it exists
    /// ([`Self::reserve_knock_slot`]). The listener must keep counting it and
    /// the operator must not be shown it; those are two different questions
    /// about one row, which is why this is a second reader rather than a
    /// narrower field.
    ///
    /// The same all-zero test [`Self::release_knock_reservation`] uses, through
    /// [`Knock::is_reservation_placeholder`] so the two cannot drift about what
    /// a placeholder is.
    pub fn visible_pending(&self) -> Vec<Knock> {
        self.pending
            .iter()
            .filter(|knock| !knock.is_reservation_placeholder())
            .cloned()
            .collect()
    }

    /// Undo a reservation [`Self::reserve_knock_slot`] made, because the
    /// handshake that would have earned it real details never completed.
    ///
    /// Removes the row for `addr` ONLY if it is still exactly the untouched
    /// placeholder that reservation created, all-zero instance id, no name,
    /// `wire_version` zero, and `first_seen_ms` equal to `reserved_at_ms`,
    /// so a real knock that coalesced onto the row in the meantime (the
    /// reserving connection's own retry, or a sibling from the same address)
    /// is never destroyed by this connection's failure.
    pub fn release_knock_reservation(&mut self, addr: &str, reserved_at_ms: i64) {
        self.pending.retain(|knock| {
            !(knock.addr == addr
                && knock.is_reservation_placeholder()
                && knock.first_seen_ms == reserved_at_ms)
        });
    }

    /// The pending row for an `instance|addr` selector, which is what every
    /// operator verb takes.
    ///
    /// One resolver for all four verbs (`accept`, `ignore`, `block`,
    /// `pending`), so they cannot disagree about what `tcr peer accept
    /// 10.0.1.24` means. An instance id is tried first, it is the unambiguous
    /// form, and the address second.
    /// **A reservation placeholder never matches.** It is a row with an
    /// all-zero instance id that [`Self::reserve_knock_slot`] pushed to hold
    /// the cap before any handshake earned it a name, and [`Self::visible_pending`]
    /// already hides it from every screen. It used to match here, so
    /// `tcr peer accept <address>` against a stalled connection opened a
    /// 120-second window keyed to an instance id of zeros: a window no real
    /// pairing can ever use, over an address the operator now believes is
    /// admitted. The two readers agree again.
    pub fn find_pending(&self, selector: &str) -> Option<&Knock> {
        let selector = selector.trim();
        if let Ok(id) = InstanceId::parse(selector) {
            if let Some(row) = self
                .pending
                .iter()
                .find(|knock| knock.instance_id == id && !knock.is_reservation_placeholder())
            {
                return Some(row);
            }
        }
        // The bare key, OR the `host:port` form the same row is printed under
        // ([`Knock::dial_address`]). Every string a surface shows for a row
        // has to select that row: `tcr peer pending` prints the dial address,
        // and an operator who pastes back what they just read must not be told
        // it matches no request.
        self.pending.iter().find(|knock| {
            (knock.addr == selector || knock.dial_address() == selector)
                && !knock.is_reservation_placeholder()
        })
    }

    /// Whether `text` is the address form every row in this file is keyed on:
    /// an IP address with no port, which is what [`knock_address`] writes.
    ///
    /// The selector verbs need it because a target that matches no pending row
    /// is still allowed to name an address, and one that is not an address at
    /// all must not be stored as a ban or a mute the listener's comparison can
    /// never equal.
    pub fn is_knock_address(text: &str) -> bool {
        text.trim().parse::<std::net::IpAddr>().is_ok()
    }

    /// Accept one pending knock: drop its row and open a window keyed to its
    /// id and address. Reports the row that was accepted, or `None` when the
    /// selector matched nothing.
    pub fn accept_knock(
        &mut self,
        selector: &str,
        now_ms: i64,
        window_secs: i64,
    ) -> Option<AcceptedInstance> {
        let knock = self.find_pending(selector)?.clone();
        self.pending.retain(|row| row.addr != knock.addr);
        // A key an earlier handshake from this address revealed is carried
        // forward rather than dropped: the row below REPLACES that one, and
        // `tcr peer block` reads the key off whatever row is there.
        let learned_key = self.key_learned_at(&knock.addr);
        let window = AcceptedInstance {
            instance_id: knock.instance_id,
            addr: knock.addr,
            until_ms: now_ms.saturating_add(window_secs.saturating_mul(1000)),
            opened_at_ms: now_ms,
            learned_key,
        };
        // One accepted window at a time per address: a second Accept for the
        // same address replaces the first rather than stacking, so "how long is
        // this address welcome for" has one answer.
        self.accepted.retain(|open| open.addr != window.addr);
        self.accepted.push(window.clone());
        Some(window)
    }

    /// Whether an `XX` message 1 from `addr` claiming `instance_id` is inside
    /// an operator-opened window.
    ///
    /// **This is the whole of the authorization for a first pairing**, and it
    /// replaces the node-wide `pairing_window` the direct-`XX` path used: a
    /// window that admitted any `XX` from anyone let a stranger on the port
    /// harvest this node's static key for two minutes, and the window is keyed
    /// to the one machine the operator said yes to. A clock set backward closes
    /// it, for the reason [`Self::pairing_window`] gives.
    pub fn accepted_window(&self, addr: &str, instance_id: &InstanceId, now_ms: i64) -> bool {
        self.accepted.iter().any(|open| {
            open.addr == addr
                && &open.instance_id == instance_id
                && now_ms >= open.opened_at_ms
                && now_ms < open.until_ms
        })
    }

    /// The static key, if any, that a handshake from `addr` has revealed.
    ///
    /// Read by `tcr peer block <addr>`, which is why it exists: a ban is
    /// address plus key when the key is known, and this is the only place a
    /// key learned during a pairing that never became a pin is kept.
    pub fn key_learned_at(&self, addr: &str) -> Option<PeerId> {
        self.accepted
            .iter()
            .find(|open| open.addr == addr)
            .and_then(|open| open.learned_key)
    }

    /// Mute an address for [`MUTE_MS`] and drop its pending row.
    pub fn mute(&mut self, addr: &str, now_ms: i64) {
        self.pending.retain(|knock| knock.addr != addr);
        self.muted.retain(|mute| mute.addr != addr);
        self.muted.push(Mute {
            addr: addr.to_string(),
            until_ms: now_ms.saturating_add(MUTE_MS),
        });
    }

    /// Ban an address, and its static key when one is known. Drops the pending
    /// row and any mute, which a ban supersedes.
    pub fn ban(&mut self, addr: &str, key: Option<PeerId>, reason: BanReason, now_ms: i64) {
        self.pending.retain(|knock| knock.addr != addr);
        self.muted.retain(|mute| mute.addr != addr);
        self.accepted.retain(|open| open.addr != addr);
        // A re-block that now knows the key upgrades the row rather than
        // adding a second one: "is this banned" must have one answer.
        if let Some(existing) = self.banned.iter_mut().find(|ban| ban.addr == addr) {
            if key.is_some() {
                existing.key = key;
            }
            existing.reason = reason;
            return;
        }
        self.banned.push(Ban {
            addr: addr.to_string(),
            key,
            since_ms: now_ms,
            reason,
        });
    }

    /// Lift a ban by address. Reports whether anything was banned to lift.
    pub fn unblock(&mut self, addr: &str) -> bool {
        let before = self.banned.len();
        self.banned.retain(|ban| ban.addr != addr);
        self.banned.len() != before
    }

    pub fn close_pairing_window(&mut self) -> bool {
        let was_open =
            self.pairing_window_until_ms.is_some() || self.pairing_window_opened_at_ms.is_some();
        self.pairing_window_until_ms = None;
        self.pairing_window_opened_at_ms = None;
        was_open
    }
}

/// Why a knock never reached [`PeerState::pending`]. **Every arm gets zero
/// bytes on the socket**, so a stranger cannot tell one from another, which
/// is deliberate: telling a flooder which cap it hit is telling it what to
/// change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnockRefusal {
    /// The address is banned. Cleared only by `tcr peer unblock`.
    Banned,
    /// The address is muted, because the operator pressed Ignore.
    Muted,
    /// [`MAX_PENDING_KNOCKS`] are already outstanding.
    QueueFull {
        /// How many were outstanding.
        pending: usize,
    },
}

impl std::fmt::Display for KnockRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Banned => write!(f, "peer knock refused: this address is blocked"),
            Self::Muted => write!(
                f,
                "peer knock refused: this address was ignored and is muted for the hour"
            ),
            Self::QueueFull { pending } => write!(
                f,
                "peer knock refused: {pending} pairing requests are already waiting, \
                 which is the cap (MAX_PENDING_KNOCKS)"
            ),
        }
    }
}

impl std::error::Error for KnockRefusal {}

/// The on-disk shape: this file's own [`FORMAT_VERSION`] alongside the
/// flattened state. A separate wrapper rather than a field on [`PeerState`]
/// itself, so every in-memory caller holds the state and nothing else.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PeerStateFile {
    version: u32,
    #[serde(flatten)]
    state: PeerState,
}

/// Read the file, dropping every row whose deadline has passed.
///
/// Logs `restored=N expired=M`, because a silent restore is indistinguishable
/// from a file that was never read. Missing is the ordinary first-boot case
/// and costs nothing but a cold start. A corrupt file, or one carrying a
/// format version this build does not understand, is renamed aside (never
/// deleted. See the module docs) and reported; the caller still gets a
/// usable empty state rather than a propagated error, because a cache that
/// can fail a boot is a downgrade on the memory-only version it replaces.
///
/// [`load_with_origin`] is this function plus the answer to "was that an empty
/// state or an empty state because the file could not be trusted", which is a
/// distinction a WRITER has to make and a reader does not.
pub fn load(path: &Path, now_ms: i64) -> Result<PeerState> {
    load_with_origin(path, now_ms).map(|(state, _)| state)
}

/// Where the state a caller just got came from.
///
/// [`load`] answers a cold start and a trusted read with the same
/// [`PeerState::default`]-shaped value, which is right for every READER: the
/// contract of this file is that losing it costs a cold start and nothing more.
/// It is wrong for a WRITER. [`save_leases`] does a read-modify-write of a file
/// that also carries the knock queue, the mutes, the bans and the accepted
/// windows, and writing an empty state's worth of those keys back over a file
/// that was merely UNTRUSTED (not absent) publishes a partial state as if it
/// were the truth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateOrigin {
    /// The file was read and trusted.
    Trusted,
    /// There is no file yet: the ordinary first boot.
    Missing,
    /// The file could not be trusted and was renamed aside. `aside` is `None`
    /// when even the rename failed, which [`quarantine_corrupt`] logs.
    Quarantined {
        /// Where the evidence went.
        aside: Option<PathBuf>,
        /// Why it was not trusted, in the words the log line used.
        reason: String,
    },
}

/// The six numbers the "peer state restored" line below reports, held so the
/// next tick can be compared against this one.
///
/// `PartialEq` is the whole point: two ticks with the same six numbers are,
/// as far as an operator reading the log cares, the same event, and the
/// second one saying so again is the spam this type exists to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RestoreCounts {
    restored: usize,
    borrowed: usize,
    expired: usize,
    pending: usize,
    muted: usize,
    banned: usize,
}

/// The last tick's [`RestoreCounts`] this process logged at INFO, per PATH.
///
/// [`load_with_origin`] runs on every poll of `tcr status` (once a second
/// from the panel), not only at boot, so logging its numbers unconditionally
/// turns one boot line into one line a second. Keyed by path for the same
/// reason [`crate::peer::config::PEERS_FILE_OPENS`] is: a test binary runs
/// its tests concurrently in one process, and a single global slot would
/// compare one test's first tick against whatever another test's temp file
/// last wrote.
static LAST_RESTORE_LOG: std::sync::OnceLock<
    Mutex<std::collections::HashMap<PathBuf, RestoreCounts>>,
> = std::sync::OnceLock::new();

/// Whether `counts` differs from the last tick this process logged for
/// `path`, or there was no last tick at all (first call for this path, the
/// boot case). Updates the cache to `counts` either way, so the comparison on
/// the NEXT call is always against the tick that just ran, not against
/// whatever was last logged.
fn restore_counts_changed(path: &Path, counts: RestoreCounts) -> bool {
    let mut cache = LAST_RESTORE_LOG
        .get_or_init(|| Mutex::new(std::collections::HashMap::new()))
        .lock()
        .expect("restore-log cache lock poisoned");
    let previous = cache.insert(path.to_path_buf(), counts);
    previous != Some(counts)
}

/// [`load`], and where the state came from. See [`StateOrigin`].
pub fn load_with_origin(path: &Path, now_ms: i64) -> Result<(PeerState, StateOrigin)> {
    // **The review's L1: the mode tripwire the peers file has, on the file that
    // carries the accepted-pairing windows and the ban list.**
    //
    // `accepted` is described one screen up as "the whole of the authorization
    // for a first pairing" and `banned` is the block list, so both are security
    // inputs and a file this program did not write must not decide either.
    // `crate::config::write_atomic` creates it 0600, so the ordinary case never
    // sees this.
    //
    // The consequence of a bad mode is a COLD START and not a refusal, unlike
    // the peers file's hard `bail!`: this is a cache whose whole contract is
    // that deleting it costs nothing but a cold start, and refusing to boot a
    // proxy over one would be the downgrade the module docs already rule out.
    // The file is renamed aside rather than read, so the evidence survives.
    if let Ok(meta) = std::fs::metadata(path) {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = meta.permissions().mode() & 0o777;
        if mode != 0o600 {
            let reason = format!("mode {mode:o}, expected 0600");
            let aside = quarantine_corrupt(path, now_ms, &reason);
            tracing::warn!(
                restored = 0,
                expired = 0,
                path = %path.display(),
                mode = format!("{mode:o}"),
                "peer state: refusing to trust a state file this program did not write \
                 (expected mode 0600); renamed aside, starting cold"
            );
            return Ok((
                PeerState::default(),
                StateOrigin::Quarantined { aside, reason },
            ));
        }
    }

    let data = match std::fs::read_to_string(path) {
        Ok(data) => data,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(restored = 0, expired = 0, "peer state: no file yet");
            return Ok((PeerState::default(), StateOrigin::Missing));
        }
        Err(err) => {
            return Err(err).with_context(|| format!("reading {}", path.display()));
        }
    };

    let parsed = match serde_json::from_str::<PeerStateFile>(&data) {
        Ok(parsed) => parsed,
        Err(err) => {
            let reason = err.to_string();
            let aside = quarantine_corrupt(path, now_ms, &reason);
            tracing::warn!(
                restored = 0,
                expired = 0,
                path = %path.display(),
                error = %err,
                "peer state: file is corrupt, renamed aside, starting cold"
            );
            return Ok((
                PeerState::default(),
                StateOrigin::Quarantined { aside, reason },
            ));
        }
    };

    if parsed.version != FORMAT_VERSION {
        let reason = format!(
            "format version {} is not the {FORMAT_VERSION} this build understands",
            parsed.version
        );
        let aside = quarantine_corrupt(path, now_ms, &reason);
        tracing::warn!(
            restored = 0,
            expired = 0,
            path = %path.display(),
            file_version = parsed.version,
            "peer state: unknown format version, renamed aside, starting cold"
        );
        return Ok((
            PeerState::default(),
            StateOrigin::Quarantined { aside, reason },
        ));
    }

    let mut state = parsed.state;
    let total = state.leases.len();
    state.leases.retain(|row| row.lease.expires_at_ms > now_ms);
    let restored = state.leases.len();
    // The borrowed rows age out on the ledger rows' rule and are counted into
    // the same two numbers: a borrower whose lease TTL ran out while the
    // process was down has to re-ask, which is the same fact "expired" already
    // names on the lending side. `borrowed` is reported separately in the log
    // line so an operator can tell which direction a restore came back in.
    let borrowed_total = state.borrowed.len();
    state
        .borrowed
        .retain(|row| row.lease.expires_at_ms > now_ms);
    let borrowed = state.borrowed.len();
    // Knocks, mutes and accepted windows age out on the same absolute-deadline
    // rule the leases do, and dropping them HERE is what makes every reader of
    // this file agree: the listener, `tcr peer pending` and the panel all load
    // through this function, so an expired row is never something one of them
    // sees and another does not.
    let aged_out = state.expire(now_ms);
    let expired = total - restored + aged_out + (borrowed_total - borrowed);
    let counts = RestoreCounts {
        restored,
        borrowed,
        expired,
        pending: state.pending.len(),
        muted: state.muted.len(),
        banned: state.banned.len(),
    };
    // `tcr status` polls this function once a second, so an unconditional
    // `info!` here is not a boot line, it is a busy loop's worth of identical
    // lines burying whatever else the log holds. Only a first read for this
    // path (the actual boot event) or a tick whose numbers moved earns INFO;
    // an unchanged tick still logs, but at `debug!`, so the fact is still
    // there for anyone who raised the filter to look for it.
    if restore_counts_changed(path, counts) {
        tracing::info!(
            restored = counts.restored,
            borrowed = counts.borrowed,
            expired = counts.expired,
            pending = counts.pending,
            muted = counts.muted,
            banned = counts.banned,
            "peer state restored"
        );
    } else {
        tracing::debug!(
            restored = counts.restored,
            borrowed = counts.borrowed,
            expired = counts.expired,
            pending = counts.pending,
            muted = counts.muted,
            banned = counts.banned,
            "peer state restored"
        );
    }
    Ok((state, StateOrigin::Trusted))
}

/// Rename a file this build could not trust aside to `<name>.corrupt-<now_ms>`,
/// so the evidence survives the next boot's cold start. Best-effort: a failed
/// rename is logged and does not stop the caller from starting cold anyway.
///
/// Answers where the file went, or `None` when the rename itself failed, so
/// [`StateOrigin::Quarantined`] can name it to an operator.
fn quarantine_corrupt(path: &Path, now_ms: i64, reason: &str) -> Option<PathBuf> {
    let mut file_name = path.file_name().unwrap_or_default().to_os_string();
    file_name.push(format!(".corrupt-{now_ms}"));
    let quarantined = path.with_file_name(file_name);
    match std::fs::rename(path, &quarantined) {
        Ok(()) => {
            tracing::warn!(
                from = %path.display(),
                to = %quarantined.display(),
                reason,
                "peer state: renamed a file this build could not trust aside"
            );
            Some(quarantined)
        }
        Err(err) => {
            tracing::warn!(
                error = %err,
                path = %path.display(),
                reason,
                "peer state: could not rename an untrusted file aside; leaving it in place"
            );
            None
        }
    }
}

/// Replace ONLY the lease rows in the state file, keeping every other key.
///
/// The lender's ledger is process state that has to survive a restart, and it
/// shares its file with the pairing window, the knock queue, the mutes and the
/// bans, every one of which a concurrent `tcr peer accept` or an inbound knock
/// may be writing at the same instant. So this is a **locked** read-modify-write
/// over [`crate::peer::config::FileLock`], the same lock the knock verbs and the
/// listener already take on this path: a whole-state `save` from the relay path
/// would write back a snapshot of the queue as it looked when the lease was
/// granted and lose an Accept made since.
///
/// Called on the relay path, so a failure is reported to the caller and never
/// panics: [`crate::peer::lease::Ledger::persist`] logs it and answers the
/// borrower anyway.
///
/// # A quarantined file is refused, not rebuilt
///
/// [`load`] answers an untrusted file with an EMPTY state, which is right for a
/// reader: this file is a cache and losing it costs a cold start. For this
/// writer it was a data-loss bug. A read-modify-write whose read was
/// quarantined writes back an empty knock queue, no mutes, no bans and no
/// accepted windows, with the lease rows on top, so the result looks like a
/// healthy file rather than a cold start. The mutes and bans are the operator's
/// own decisions and the accepted windows are, in this file's own words, "the
/// whole of the authorization for a first pairing".
///
/// So a [`StateOrigin::Quarantined`] read refuses here, naming both paths. The
/// next boot's [`load`] starts cold on its own terms and says so in its log
/// line; nothing this function does is what publishes that. A MISSING file is
/// not this case: there is nothing to lose and a first grant has to be able to
/// create the file.
pub fn save_leases(path: &Path, leases: &[LeaseRow]) -> Result<()> {
    let leases = leases.to_vec();
    replace_section(path, "lease rows", move |state| state.leases = leases)
}

/// Replace ONLY the borrowed rows in the state file, keeping every other key,
/// [`save_leases`] for the other direction, and the same locked
/// read-modify-write for the same reasons. See [`PeerState::borrowed`].
///
/// Called by [`crate::peer::lease::PeerLeaseProvider`] when the set of live
/// borrowed leases CHANGES, a lease granted, or a refused one dropped, and
/// never per relayed request: the borrower's copy of `spent` is the lender's
/// figure and this file is not where it is kept.
pub fn save_borrowed(path: &Path, borrowed: &[BorrowedRow]) -> Result<()> {
    let borrowed = borrowed.to_vec();
    replace_section(path, "borrowed lease rows", move |state| {
        state.borrowed = borrowed;
    })
}

/// Replace ONLY the measured path costs, keeping every other key.
///
/// [`save_leases`] for the prober's figures, and the same locked
/// read-modify-write for the same reasons: the prober writes while a knock or
/// an Accept may be writing the same file, and a whole-state `save` from here
/// would publish the knock queue as it looked when this session opened.
///
/// Called when a probing session ENDS rather than per probe: a sample is worth
/// one EWMA step and not one file write, and a write per probe per peer per
/// minute would move the file's mtime often enough to make every `PeerStore`
/// on this Mac re-read its config for nothing.
pub fn save_paths(path: &Path, paths: &[crate::peer::probe::PathStat]) -> Result<()> {
    let paths = paths.to_vec();
    replace_section(path, "measured path costs", move |state| {
        state.paths = paths
    })
}

/// Replace ONLY the held-mapping record, keeping every other key.
///
/// The same locked read-modify-write as [`save_paths`], and its own section
/// for the same reason: the keeper thread writes this while a knock or an
/// Accept may be writing the same file, and a whole-state `save` from a
/// mapping renewal would publish the knock queue as it looked when the
/// serving process booted.
///
/// `None` clears it, which is what a keeper does on its way out: leaving the
/// last record behind would have `tcr peer reach` report a mapping that was
/// deleted on purpose, for as long as its old deadline had left to run.
pub fn save_mapping(path: &Path, mapping: Option<MappingRecord>) -> Result<()> {
    replace_section(path, "the held mapping", move |state| {
        state.mapping = mapping;
    })
}

/// Replace ONLY the per-path traffic totals, keeping every other key.
///
/// The same locked read-modify-write as [`save_paths`], and a SEPARATE section
/// for the reason [`PeerState::path_traffic`] gives: the prober owns the costs
/// and the meter owns the totals, and a writer that published the other's
/// section would publish it as it looked when this process last read the file.
///
/// Called when a window's figures are wanted on disk, at the end of a carry or
/// on whatever interval the serving process sums its meter at, and never per
/// charge: a file write per carried chunk would move this file's mtime often
/// enough to make every `PeerStore` on this Mac re-read its config for nothing,
/// which is the same trade [`save_paths`] makes and for the same reason.
pub fn save_path_traffic(path: &Path, traffic: &[PathTraffic]) -> Result<()> {
    let traffic = traffic.to_vec();
    replace_section(path, "per-path traffic totals", move |state| {
        state.path_traffic = traffic;
    })
}

/// The locked read-modify-write every section writer above shares, so the lock,
/// the quarantine refusal and the atomic write are one implementation rather
/// than three that drift.
fn replace_section(
    path: &Path,
    what: &str,
    apply: impl FnOnce(&mut PeerState) + Send,
) -> Result<()> {
    let _lock = crate::peer::config::FileLock::acquire(path)
        .with_context(|| format!("peer state: locking {} to write {what}", path.display()))?;
    // `crate::now_ms()` and not a parameter: the only thing the clock decides
    // here is which rows `load` prunes on the way in, and a caller that passed
    // one would be choosing how much of its OWN file to drop.
    let (mut state, origin) = load_with_origin(path, crate::now_ms())?;
    if let StateOrigin::Quarantined { aside, reason } = origin {
        anyhow::bail!(
            "peer state: {} could not be trusted ({reason}) and was renamed to {}; refusing \
             to write {what} over a state this process never read, the knock queue, the \
             mutes, the bans and the accepted pairing windows would be published as empty",
            path.display(),
            aside.map_or_else(
                || String::from("nowhere: the rename failed too"),
                |aside| aside.display().to_string()
            )
        );
    }
    apply(&mut state);
    save(path, &state)
}

/// Write the file at mode 0600, atomically.
pub fn save(path: &Path, state: &PeerState) -> Result<()> {
    let file = PeerStateFile {
        version: FORMAT_VERSION,
        state: state.clone(),
    };
    let json = serde_json::to_string_pretty(&file)?;
    crate::config::write_atomic(path, &json)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt as _;

    /// One captured tracing event: its level and its message, which is all
    /// this test needs to tell an INFO "peer state restored" apart from a
    /// DEBUG one.
    #[derive(Clone, Default)]
    struct Capture(std::sync::Arc<Mutex<Vec<(tracing::Level, String)>>>);

    struct Message(Option<String>);

    impl tracing::field::Visit for Message {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.0 = Some(format!("{value:?}"));
            }
        }
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut message = Message(None);
            event.record(&mut message);
            if let Some(message) = message.0 {
                self.0
                    .lock()
                    .expect("capture lock")
                    .push((*event.metadata().level(), message));
            }
        }
    }

    impl Capture {
        fn count(&self, level: tracing::Level, needle: &str) -> usize {
            self.0
                .lock()
                .expect("capture lock")
                .iter()
                .filter(|(event_level, message)| *event_level == level && message.contains(needle))
                .count()
        }
    }

    /// A scratch state-file path unique to this thread, so two tests run
    /// concurrently in this binary never share a [`LAST_RESTORE_LOG`] cache
    /// entry (that cache is keyed by path for exactly this reason).
    fn scratch_state_path() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tcr-peer-state-restored-log-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        drop(std::fs::remove_dir_all(&dir));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir.join("peer-state.json")
    }

    /// **A tick that restores the same nothing twice logs the fact ONCE at
    /// INFO, not twice.**
    ///
    /// `tcr status` polls [`load_with_origin`] once a second, so the boot
    /// line this function documents becomes a line-a-second poll unless a
    /// tick whose six numbers match the last one it logged stays at
    /// `debug!`. Watched red by reverting the `if restore_counts_changed`
    /// branch back to an unconditional `tracing::info!`: the second call
    /// then adds a second INFO line and this test's `count(INFO, ..) == 1`
    /// fails with 2.
    #[test]
    fn a_tick_that_restored_nothing_new_stays_quiet_the_second_time() {
        let path = scratch_state_path();
        save(&path, &PeerState::default()).expect("write scratch state file");
        let now = crate::now_ms();

        let capture = Capture::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        tracing::subscriber::with_default(subscriber, || {
            load_with_origin(&path, now).expect("first tick reads the file just written");
            load_with_origin(&path, now).expect("second tick reads the same, unchanged file");
        });

        assert_eq!(
            capture.count(tracing::Level::INFO, "peer state restored"),
            1,
            "two identical ticks must log the restore ONCE at INFO, not once per tick"
        );
        assert_eq!(
            capture.count(tracing::Level::DEBUG, "peer state restored"),
            1,
            "the second, unchanged tick must still say what it found, at debug"
        );
    }

    /// A tick whose counts differ from the last one this process logged for
    /// the same path earns INFO again, because that IS new information: an
    /// operator restarted, or a knock landed between the two reads.
    #[test]
    fn a_tick_whose_counts_moved_logs_at_info_again() {
        let path = scratch_state_path();
        save(&path, &PeerState::default()).expect("write scratch state file");
        let now = crate::now_ms();

        let capture = Capture::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        tracing::subscriber::with_default(subscriber, || {
            load_with_origin(&path, now).expect("first tick reads the file just written");

            // Something changed on disk between the two ticks: a knock landed
            // and the pending queue is no longer empty.
            let mut state = PeerState::default();
            state
                .reserve_knock_slot("198.51.100.1:4242", now)
                .expect("reserve a knock slot for the fixture");
            save(&path, &state).expect("write the changed scratch state file");

            load_with_origin(&path, now).expect("second tick reads the changed file");
        });

        assert_eq!(
            capture.count(tracing::Level::INFO, "peer state restored"),
            2,
            "a tick whose numbers moved must log again at INFO, not stay quiet"
        );
    }
}
