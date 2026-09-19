//! Finding other Macs on this network. Off by default, behind two functions.
//!
//! # What the beacon carries, and what it must NEVER carry
//!
//! **Presence and a port. Plus the wire version, always, and a display name,
//! when the operator allows it. Nothing else.**
//!
//! Never the node id, never the static public key, never a fingerprint of
//! either, and no other Noise material. That is a decision, not a default, and
//! it is why [`advertise`] does not take a `PeerId` at all and why
//! [`Discovered`] does not carry one: a signature that cannot express the
//! forbidden thing is stronger than a comment asking nobody to add it.
//!
//! The consequence, stated so nobody re-derives it wrongly: **identity is
//! learned INSIDE the handshake, after the operator presses Trust.** So a
//! pairing that begins from a discovered row is always `Noise_XX` with a
//! six-digit compare, there is no prior key to run `Noise_IK` against, and
//! `Noise_IK` is for return visits to a key already pinned.
//!
//! (The one path that does begin with a key is a pasted join token, which
//! carries the registrar's static in the token itself and never in a beacon.
//! That is what lets a machine with no screen enrol at all.)
//!
//! # Two functions, on purpose
//!
//! [`advertise`] and [`browse`] are the entire surface. The implementation
//! behind them, `mdns-sd` today, a small UDP beacon in the recorded
//! alternative, can be swapped without touching trust, because **discovery
//! grants nothing**: a beacon is a hint about where to dial, identity is proved
//! by the handshake against a pinned static key, and a forged beacon buys an
//! attacker one refused handshake.
//!
//! # The name is the one free-text field here
//!
//! It is the operator's own display name for this machine
//! (`tcr peer name <name>`), it is announced only when
//! `peer.announceName` is on, and it goes through the one shared sanitizer,
//! which is a CHARACTER WHITELIST: `[A-Za-z0-9 ._-]`, 1 to 32 bytes, and no
//! uuid shape. This repository is public and a beacon payload is the kind of
//! thing that ends up in a fixture.
//!
//! The whitelist applies in BOTH directions and the inbound direction is the
//! one that matters: a name in an inbound TXT record is text a stranger on
//! this network chose, and it lands in a panel row, a `tcr peer ls` line and a
//! log line. [`browse_once`] runs every name it resolves through
//! [`sanitize_name`], so a control character, an ANSI escape, a newline or a
//! Unicode direction override never reaches any of the three.
//!
//! # Off by default
//!
//! A fresh config registers nothing and browses nothing, and there is a test
//! that asserts exactly that on a fresh config rather than trusting a field's
//! default. Discovery is also what the simple surface's first switch turns on,
//! so "off by default" and "one switch away" are both true.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

use crate::peer::config::{Endpoint, EndpointSource, PeerRow, PeerStore};

/// The DNS-SD service type this node registers and browses.
pub const SERVICE_TYPE: &str = "_tcr-peer._tcp.local.";

/// The TXT key the wire version rides under, always present: a reader on the
/// wrong version can say so instead of guessing at an unfamiliar payload.
pub const TXT_VERSION_KEY: &str = "v";

/// The TXT key the display name rides under. One key, so a reader can see the
/// whole payload in one line of `dns-sd` output.
pub const TXT_NAME_KEY: &str = "name";

/// The TXT key the per-boot instance id rides under, always present.
///
/// An announcement carries "a per-boot random 8-byte instance id,
/// the port, the wire version". **Ephemeral by construction**. See
/// [`tcr_peer_wire::InstanceId`]: it is minted fresh at every boot, never
/// persisted, and it is not identity. It is here because a found row, the knock
/// that Trust sends to it, and the 120-second window the far side's Accept
/// opens all have to name the same thing, and that name must not be a key.
pub const TXT_INSTANCE_KEY: &str = "id";

/// The TXT key the network-key tag rides under, present only when a key is set.
///
/// `HMAC-SHA256(network_key, instance_id ‖ port ‖ minute)[..8]`, per
/// [`crate::peer::config::NetworkKey::announcement_tag`]. A receiver that holds
/// the key drops a row whose tag does not verify **before it becomes a row**; a
/// receiver that holds none ignores this field entirely, which is what keeps
/// the two configurations on one LAN from breaking each other.
pub const TXT_TAG_KEY: &str = "t";

/// How many found rows are shown. `abuse-resistance.md` says:
/// twelve, newest first, with an "N more not shown" footer, so an
/// announcement flood costs a footer line and not a scrolling list.
pub const MAX_FOUND_ROWS: usize = 12;

/// How many found rows one source address may hold. Two: a Mac with two
/// interfaces on the same LAN is real, and 254 rows from one address is not.
pub const MAX_FOUND_PER_ADDRESS: usize = 2;

/// How long a row survives after its last announcement.
/// `abuse-resistance.md`: "a row that stops announcing leaves after 60 s".
pub const FOUND_TTL_MS: i64 = 60_000;

/// How long one [`browse`] call listens before returning what it has seen.
/// Short on purpose: a caller polls this repeatedly rather than holding a
/// long-lived stream, so "find off" only has to stop advertising and stop the
/// NEXT poll, never interrupt one in flight.
const BROWSE_WINDOW: Duration = Duration::from_millis(1500);

/// The whole beacon payload, as key/value pairs, built in ONE place so the gate
/// that asserts what it does not contain has something to read.
///
/// `name` is `None` when `peer.announceName` is off, and then the beacon is
/// presence and a port alone. Built here rather than at the registration call
/// so that `beacon_carries_no_key_or_id` (`tests/peer_discovery.rs`) has one
/// thing to read: a payload assembled at the call site is a payload no gate can
/// check.
pub fn beacon_txt(
    instance_id: &tcr_peer_wire::InstanceId,
    port: u16,
    name: Option<&str>,
    network_key: Option<&crate::peer::config::NetworkKey>,
    now_unix_secs: i64,
) -> Vec<(String, String)> {
    let mut txt = vec![
        (
            TXT_VERSION_KEY.to_string(),
            tcr_peer_wire::PROTO_VERSION.to_string(),
        ),
        (TXT_INSTANCE_KEY.to_string(), instance_id.to_wire()),
    ];
    if let Some(key) = network_key {
        txt.push((
            TXT_TAG_KEY.to_string(),
            key.announcement_tag(instance_id, port, now_unix_secs.div_euclid(60)),
        ));
    }
    if let Some(sanitized) = name.and_then(sanitize_name) {
        txt.push((TXT_NAME_KEY.to_string(), sanitized));
    }
    txt
}

/// Unix seconds, for the minute the announcement tag is computed over.
fn now_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| i64::try_from(since.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

/// The one shared sanitizer, plus the two beacon-specific refusals it has no
/// reason to know about: [`PeerId::display`]'s own `tcr-xxxxxxxxxx` shape and
/// the full 52-character wire form. Both are id shapes, not display names, and
/// a beacon is exactly where an id-shaped "name" would be mistaken for
/// identity. See the module docs.
///
/// **`tcr_peer_wire::sanitize_label` is a CHARACTER WHITELIST**
/// (`[A-Za-z0-9 ._-]`, 1 to 32 bytes, no uuid shape), which is why the local
/// copy of the trim/length/`@`/uuid rules that used to live here was deleted
/// in favour of calling it. The whitelist is the point: an inbound beacon's
/// TXT record is attacker-chosen text that lands in a panel row, a
/// `tcr peer ls` line and a log line, and a denylist would have to enumerate
/// every hostile shape a renderer acts on, a control character, an `ESC[`
/// sequence that rewrites the line an operator is reading, a newline that
/// forges a second row, a `U+202E` override that displays the name backwards.
/// A whitelist refuses all four, and everything nobody has thought of yet. It
/// also refuses the `@` this function used to name on its own: an `@` is
/// simply not in the set.
///
/// A measurement proved a beacon could carry `PeerId::display()`'s
/// own `tcr-xxxxxxxxxx` shape and pass every check here, because nothing
/// refused it. Watch that fail by removing `is_display_id_shaped`/
/// `is_wire_id_shaped` from the condition below: `sanitize_name_rejects_the_\
/// tcr_display_shape_and_the_wire_form` (below) goes red, confirmed by hand
/// while writing this fix.
///
/// For the whitelist: `a_hostile_beacon_name_is_refused` (below)
/// goes red if the `sanitize_label` call is replaced by the old
/// `trimmed.contains('@')` condition, confirmed by running exactly that.
///
/// `pub` rather than the initially suggested `pub(crate)`, `pub(crate)` restricts
/// visibility to THIS crate, and an integration test under `tests/` is a
/// SEPARATE crate that links against this one, so `pub(crate)` would still
/// refuse it. `discovery` is already `pub mod` and other integration tests
/// already reach into it (`tests/peer_discovery.rs`), so this is the one
/// visibility token this test needed and nothing wider.
pub fn sanitize_name(raw: &str) -> Option<String> {
    let name = tcr_peer_wire::sanitize_label(raw).ok()?;
    if is_display_id_shaped(&name) || is_wire_id_shaped(&name) {
        return None;
    }
    Some(name)
}

/// [`PeerId::display`]'s exact shape: `tcr-` plus ten alphanumeric
/// characters. Checked case-insensitively, an attacker choosing what looks
/// like their own display id does not get to pick the case that slips past
/// this refusal.
fn is_display_id_shaped(s: &str) -> bool {
    if !s.is_ascii() || s.len() < 4 || !s[..4].eq_ignore_ascii_case("tcr-") {
        return false;
    }
    let rest = &s[4..];
    rest.chars().count() == 10 && rest.chars().all(|c| c.is_ascii_alphanumeric())
}

/// [`PeerId::to_wire`]'s exact length: 52 Crockford base32 characters. Length
/// alone is the check, not the alphabet, Crockford excludes I/L/O/U but a
/// name that happens to avoid four letters and hits exactly 52 alphanumeric
/// characters is close enough to the wire form to refuse rather than parse.
fn is_wire_id_shaped(s: &str) -> bool {
    s.chars().count() == 52 && s.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Register `SERVICE_TYPE` on `daemon` with `beacon_txt(name)` and `port`. The
/// seam [`advertise`] delegates to, so a whitebox test (`mod tests` below) can
/// drive it against a real, throwaway [`ServiceDaemon`] without needing a
/// [`PeerStore`].
///
/// `instance` is the mDNS instance label. It carries no identity: a random
/// nonce, never the node id (see the module docs, the same rule the TXT
/// record follows).
///
/// A measurement proved this advertised the literal `127.0.0.1`,
/// which is unreachable from any other Mac, a beacon nobody but this box
/// could ever dial. Fixed by handing `ServiceInfo::new` no address at all
/// (`()`, which [`mdns_sd::AsIpAddrs`] resolves to an empty set) and calling
/// [`ServiceInfo::enable_addr_auto`], so `mdns-sd` fills in this host's real
/// interface addresses and keeps them current if they change.
fn register_beacon(
    daemon: &ServiceDaemon,
    instance: &str,
    name: Option<&str>,
    port: u16,
    network_key: Option<&crate::peer::config::NetworkKey>,
) -> Result<String> {
    let info = build_beacon_info(instance, name, port, network_key)?;
    let fullname = info.get_fullname().to_string();
    daemon
        .register(info)
        .context("registering the discovery beacon with the mDNS daemon")?;
    Ok(fullname)
}

/// The `ServiceInfo` [`register_beacon`] hands to the daemon, split out so a
/// test can inspect it (addresses, `is_addr_auto`) without a running
/// [`ServiceDaemon`] to receive it.
fn build_beacon_info(
    instance: &str,
    name: Option<&str>,
    port: u16,
    network_key: Option<&crate::peer::config::NetworkKey>,
) -> Result<ServiceInfo> {
    let txt = beacon_txt(
        &crate::peer::id::boot_instance_id(),
        port,
        name,
        network_key,
        now_unix_secs(),
    );
    let host = format!("{instance}.local.");
    let info = ServiceInfo::new(SERVICE_TYPE, instance, &host, (), port, &txt[..])
        .context("building the discovery beacon's ServiceInfo")?
        .enable_addr_auto();
    Ok(info)
}

/// Log-and-drop for a best-effort teardown call whose failure is not fatal to
/// the caller, every call site below says why. Real handling, not a silent
/// `let _ =`: the failure still surfaces, in the log the caller cannot wait
/// on but an operator debugging a stuck beacon can read.
fn log_teardown_err<T, E: std::fmt::Display>(result: std::result::Result<T, E>, what: &str) {
    if let Err(err) = result {
        tracing::warn!(error = %err, "{what}");
    }
}

/// Stop advertising `fullname` on `daemon`. Errors here are non-fatal to the
/// caller's own shutdown, a beacon that outlives its owner by a few seconds
/// until its TTL expires is a stale row, not a security problem (see the
/// module docs: a forged or stale beacon buys nothing but a refused
/// handshake).
fn unregister_beacon(daemon: &ServiceDaemon, fullname: &str) {
    log_teardown_err(
        daemon.unregister(fullname),
        "unregistering the discovery beacon",
    );
}

/// One resolved mDNS row, turned into a [`Discovered`] this node is willing to
/// show, or `None` when there is nothing to show.
///
/// Every decision the inbound path makes about untrusted beacon data is here,
/// in one function a test can call: the name goes through `sanitize_name`
/// (the whitelist, see its docs for what an unsanitized TXT record does to a
/// panel row), and a row with no address is dropped because there is nothing
/// to dial. A name that fails the whitelist drops the NAME and keeps the row:
/// the row is still a machine an operator may want to trust, and it shows its
/// address instead, which is the same rendering a node that announces no name
/// at all gets.
pub fn discovered_row(
    instance_id: Option<&str>,
    tag: Option<&str>,
    name: Option<&str>,
    addrs: Vec<String>,
    port: u16,
    network_key: Option<&crate::peer::config::NetworkKey>,
    now_unix_secs: i64,
) -> Option<Discovered> {
    if addrs.is_empty() {
        return None;
    }
    // The instance id is required and parsed, never assumed: it is what Trust
    // knocks under and what the far side's Accept keys its window to, so a row
    // without a readable one is a row whose Trust button could not work.
    let instance_id = tcr_peer_wire::InstanceId::parse(instance_id?).ok()?;

    // **The tag is checked before the row exists, which is the whole point.**
    // The rule is: "a receiver with the key drops an announcement whose tag
    // does not verify BEFORE it becomes a row". A node with no key ignores the
    // field, so one LAN can carry both configurations.
    if let Some(key) = network_key {
        let tag = tag?;
        if !key.tag_matches(tag, &instance_id, port, now_unix_secs) {
            return None;
        }
    }

    Some(Discovered {
        instance_id,
        name: name.and_then(sanitize_name),
        addrs,
        port,
    })
}

/// Browse `SERVICE_TYPE` on `daemon` for [`BROWSE_WINDOW`] and return every
/// resolved row seen, sanitized, with malformed rows dropped. The seam
/// [`browse`] delegates to, for the same reason [`register_beacon`] exists.
fn browse_once(
    daemon: &ServiceDaemon,
    window: Duration,
    network_key: Option<&crate::peer::config::NetworkKey>,
) -> Result<Vec<Discovered>> {
    let receiver = daemon
        .browse(SERVICE_TYPE)
        .context("starting an mDNS browse for the discovery service")?;
    let deadline = std::time::Instant::now() + window;
    let mut found = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match receiver.recv_timeout(remaining) {
            Ok(ServiceEvent::ServiceResolved(resolved)) => {
                let addrs = resolved
                    .get_addresses_v4()
                    .into_iter()
                    .map(|ip| ip.to_string())
                    .collect::<Vec<_>>();
                if let Some(row) = discovered_row(
                    resolved.get_property_val_str(TXT_INSTANCE_KEY),
                    resolved.get_property_val_str(TXT_TAG_KEY),
                    resolved.get_property_val_str(TXT_NAME_KEY),
                    addrs,
                    resolved.get_port(),
                    network_key,
                    now_unix_secs(),
                ) {
                    found.push(row);
                }
            }
            Ok(_) => continue,
            Err(_) => break, // timed out or the daemon closed the channel
        }
    }
    log_teardown_err(
        daemon.stop_browse(SERVICE_TYPE),
        "stopping the mDNS browse after one scan",
    );
    Ok(found)
}

/// One [`ServiceDaemon`] per process, shared by [`advertise`] and [`browse`]
/// so [`stop_all`] can actually reach what [`advertise`] registered. mdns-sd
/// runs the daemon on its own background thread regardless, so sharing one
/// costs nothing a fresh daemon per call would not have paid anyway.
static DAEMON: std::sync::Mutex<Option<std::sync::Arc<ServiceDaemon>>> =
    std::sync::Mutex::new(None);

/// The fullname [`advertise`] last registered, so [`stop_all`] unregisters
/// exactly that row and nothing another process's announcement.
static ANNOUNCED_FULLNAME: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// The mDNS instance label [`advertise`] last registered under.
///
/// Kept so a RE-STAMP is an update of one row rather than a second row: the
/// keyed tag in the payload is only valid for the minute it was computed in
/// ([`crate::peer::config::NetworkKey::announcement_tag`], and a receiver
/// accepts this minute and the one before), so an announcer has to re-register
/// while it keeps announcing. A fresh label each time would leave the LAN
/// holding one stale row per minute until each aged out.
static ANNOUNCED_INSTANCE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// How many times this process has registered a beacon, re-stamps included.
///
/// The one fact that says an announcer is running HERE rather than in some
/// other process: `tcr peer find on` used to announce from the CLI process and
/// exit, which took the mDNS daemon with it, so no serving process ever
/// announced anything.
static ANNOUNCEMENTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many beacons this process has registered ([`ANNOUNCEMENTS`]).
pub fn announcements() -> u64 {
    ANNOUNCEMENTS.load(std::sync::atomic::Ordering::SeqCst)
}

/// The fullname this process is announcing under, or [`None`] when it is not
/// announcing.
pub fn announced_beacon() -> Option<String> {
    match ANNOUNCED_FULLNAME.lock() {
        Ok(held) => held.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// How often an announcer wakes: to re-stamp the keyed tag, and to re-read
/// `peer.find`.
///
/// Twenty seconds, against a tag that is valid for the minute it was computed
/// in plus the one before. Two wakes per minute at worst, so a re-stamp can be
/// missed once and the beacon still verifies.
pub const BEACON_RESTAMP_INTERVAL: Duration = Duration::from_secs(20);

/// What an announcer should do on this wake.
///
/// A pure decision, so the loop that drives it is three lines and the thing
/// worth testing is testable without an mDNS daemon, a LAN or a clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnounceStep {
    /// Nothing to do: announcing and the stamp is still this minute, or not
    /// announcing and `peer.find` is off.
    Idle,
    /// Start announcing.
    Start,
    /// Re-register the beacon so its keyed tag names the current minute.
    Restamp,
    /// Stop announcing: `peer.find` went off.
    Stop,
}

/// What an announcer should do, given the switch, the minute it last stamped
/// a beacon in (or [`None`] when it is not announcing), and the clock.
///
/// The minute is the grain because the tag is
/// (`HMAC(network_key, instance ‖ port ‖ minute)`): a receiver holding the key
/// accepts the current minute and the one before, so a beacon stamped once at
/// registration stops verifying about two minutes later and the node goes
/// undiscoverable while still announcing.
pub fn announce_step(
    find_on: bool,
    stamped_minute: Option<i64>,
    now_unix_secs: i64,
) -> AnnounceStep {
    let now_minute = now_unix_secs.div_euclid(60);
    match (find_on, stamped_minute) {
        (true, None) => AnnounceStep::Start,
        (true, Some(minute)) if minute != now_minute => AnnounceStep::Restamp,
        (true, Some(_)) => AnnounceStep::Idle,
        (false, Some(_)) => AnnounceStep::Stop,
        (false, None) => AnnounceStep::Idle,
    }
}

/// The minute a beacon registered now would be stamped in.
pub fn current_stamp_minute() -> i64 {
    now_unix_secs().div_euclid(60)
}

fn lock_poisoned(what: &str) -> anyhow::Error {
    anyhow::anyhow!("{what} lock poisoned, a prior panic left it locked")
}

fn shared_daemon() -> Result<std::sync::Arc<ServiceDaemon>> {
    let mut guard = DAEMON.lock().map_err(|_| lock_poisoned("mDNS daemon"))?;
    if let Some(daemon) = guard.as_ref() {
        return Ok(std::sync::Arc::clone(daemon));
    }
    let daemon = std::sync::Arc::new(ServiceDaemon::new().context("starting the mDNS daemon")?);
    *guard = Some(std::sync::Arc::clone(&daemon));
    Ok(daemon)
}

/// Start advertising this node: presence, the port, and the name if allowed.
///
/// Takes no identity argument. See the module docs: the beacon carries no node
/// id and no key, so there is nothing about identity to pass in.
///
/// `store` is accepted for the same reason every other function in this
/// module's sibling files takes one, a stable shape across `src/peer/`, but
/// today's implementation does not read policy off it: `peer.find` on/off and
/// `peer.announceName` are already resolved by the caller before it decides
/// whether to call this function at all, and what to pass for `name`.
pub async fn advertise(store: &PeerStore, name: Option<&str>, port: u16) -> Result<()> {
    let daemon = shared_daemon()?;
    // The SAME label on a re-stamp, so the daemon updates the row it already
    // holds instead of publishing a second one. See [`ANNOUNCED_INSTANCE`].
    let instance = {
        let mut held = ANNOUNCED_INSTANCE
            .lock()
            .map_err(|_| lock_poisoned("mDNS announced-instance"))?;
        held.get_or_insert_with(|| format!("tcr-peer-{:08x}", rand_u32()))
            .clone()
    };
    let network_key = crate::peer::config::read_or_default(store.path())?.network_key;
    let fullname = register_beacon(&daemon, &instance, name, port, network_key.as_ref())?;
    ANNOUNCEMENTS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let mut guard = ANNOUNCED_FULLNAME
        .lock()
        .map_err(|_| lock_poisoned("mDNS announced-fullname"))?;
    *guard = Some(fullname);
    Ok(())
}

/// Browse for other nodes. A no-op when discovery is off.
///
/// Every result is UNTRUSTED input: a row the panel may offer a Trust button
/// for, and nothing more. It is not a peer until a handshake proves a key.
pub async fn browse(store: &PeerStore) -> Result<Vec<Discovered>> {
    let daemon = shared_daemon()?;
    let network_key = crate::peer::config::read_or_default(store.path())?.network_key;
    browse_once(&daemon, BROWSE_WINDOW, network_key.as_ref())
}

/// Which per-boot instance id belongs to which pinned static key.
///
/// # Why a beacon cannot say this on its own, and why this type exists instead
///
/// A beacon carries the ephemeral instance id, the port, the wire version and
/// optionally a display name. **It carries no key, in any encoding**, and
/// `beacon_carries_no_key_or_id` (`tests/peer_discovery.rs`) is the gate that
/// holds it to that. So an announcement can say "something is listening here"
/// and can never say "and it is the Mac you pinned".
///
/// The binding therefore comes from the only place it can: a session that
/// PROVED the static key and read the instance id out of the handshake
/// payload ([`crate::peer::noise::Handshake`]'s message-1 payload, which is
/// exactly this value). One authenticated session teaches this node which
/// beacon on the LAN is that peer's, for as long as that peer stays booted.
///
/// **Per boot, and deliberately not persisted here.** The instance id is
/// minted fresh at every boot, so a binding is worth nothing after the peer
/// restarts, and writing it into the operator's peers file would put process
/// state in the file that holds intent. A caller holds these for as long as it
/// is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstanceBinding {
    /// The per-boot id that peer announces and knocks under.
    pub instance_id: tcr_peer_wire::InstanceId,
    /// The pinned static key a session proved was behind it.
    pub node: tcr_peer_wire::PeerId,
}

/// The endpoints a scan teaches about PINNED peers, and nothing about anyone
/// else.
///
/// Pure, so the filter is testable without a LAN: what it decides is which of
/// three kinds of beacon is allowed to reach a row at all. A beacon whose
/// instance nothing has bound is ignored: it is a stranger, and a stranger's
/// announcement is a found-list row for the operator to Trust, never an edit to
/// a trusted row. A binding naming a key this node has not pinned is ignored
/// too, because a row that does not exist must not be created from an
/// announcement.
///
/// What a matched beacon buys is the case the pin path cannot cover: the peer
/// MOVED, so every endpoint on its row is dead and no session can start to
/// tell this node the new one. The beacon is the only fact left. Believing it
/// is safe for the reason every endpoint is advice: it grants nothing, the
/// handshake re-proves the key against the pin when the dial lands, and a
/// forged beacon costs a connect that fails a pin check.
pub fn beacon_endpoints(
    scan: &[Discovered],
    bindings: &[InstanceBinding],
    pinned: &[PeerRow],
    now_ms: i64,
) -> Vec<(tcr_peer_wire::PeerId, Endpoint)> {
    let mut learned = Vec::new();
    for found in scan {
        let Some(binding) = bindings
            .iter()
            .find(|binding| binding.instance_id == found.instance_id)
        else {
            continue;
        };
        if !pinned.iter().any(|row| row.node == binding.node) {
            continue;
        }
        for addr in &found.addrs {
            // A beacon's address is text off the network. It is parsed and
            // never trusted to be a socket: `addrs` holds bare addresses and
            // the port arrives as its own field, so the pair is assembled here
            // rather than anywhere a string could be dialled unparsed.
            let Ok(ip) = addr.parse::<std::net::IpAddr>() else {
                continue;
            };
            learned.push((
                binding.node,
                Endpoint::direct(
                    SocketAddr::new(ip, found.port),
                    now_ms,
                    EndpointSource::Beacon,
                ),
            ));
        }
    }
    learned
}

/// Record one scan against the peers file and return how many rows it moved.
///
/// The write goes through [`crate::peer::config::observe_endpoints`], which is
/// the file's one endpoint writer and refuses to create a row, so "discovery
/// writes nothing for a peer that is not pinned" holds twice over, here by the
/// filter in [`beacon_endpoints`] and there by the writer.
///
/// `now_ms` is the caller's clock and never a beacon's, for the same reason
/// the `Hello` path takes one: a timestamp off the wire is the one field of an
/// endpoint that could be used to push a stale address to the front of the
/// dial order.
pub fn observe_beacons(
    peers_path: &std::path::Path,
    scan: &[Discovered],
    bindings: &[InstanceBinding],
    now_ms: i64,
) -> Result<usize> {
    let pinned = crate::peer::config::read_or_default(peers_path)
        .context("discovery: the peers file did not read, so no beacon could be recorded")?
        .peers;
    let mut moved: Vec<tcr_peer_wire::PeerId> = Vec::new();
    for (node, endpoint) in beacon_endpoints(scan, bindings, &pinned, now_ms) {
        if crate::peer::config::observe_endpoints(peers_path, &node, &[endpoint])?
            && !moved.contains(&node)
        {
            moved.push(node);
        }
    }
    Ok(moved.len())
}

// ---------------------------------------------------------------------------
// Neighbour briefs: what a trusted peer's `Hello.briefs` may teach this node
// ---------------------------------------------------------------------------

/// Reduce this node's own trusted rows to what a `NeighborBrief` may ever say
/// about them: the key and its endpoint locators, nothing this node was told
/// in confidence.
///
/// The rule, verbatim: "friends through friends are not supported
/// unless you both support the same friend." A brief is useful only for a Mac
/// BOTH sides already trust, so this function's whole job is producing one
/// side of that: it reads `trusted`, which is by construction the set of keys
/// this node already pinned, and there is no argument here through which an
/// unpinned or merely-discovered row could arrive.
///
/// `exclude` is the peer this brief set is about to be sent TO. A brief
/// telling B about B is a line B already knows better than this node does, so
/// it is dropped here, the way `Hello.briefs`' own doc comment describes
/// split horizon: never back a fact out the link it would be re-advertised
/// on.
///
/// `Caps::default()` on every entry, by decision and not by gap. A brief is an
/// address hint for a mutual friend, nothing more: the receiver's own honest
/// view of what that friend can do comes from connecting to the friend
/// directly and reading its `Hello`, which is the one place caps are actually
/// current. This node's own peers file records where a trusted peer answers,
/// not what it can do right now, so `neighbor_briefs` has nothing true to fill
/// caps from, and inventing a value here would be a guess dressed as a fact
/// forwarded on a chain of trust the wire never promised to keep fresh.
pub fn neighbor_briefs(
    trusted: &[PeerRow],
    exclude: tcr_peer_wire::PeerId,
) -> Vec<tcr_peer_wire::NeighborBrief> {
    trusted
        .iter()
        .filter(|row| row.node != exclude)
        .map(|row| tcr_peer_wire::NeighborBrief {
            node: row.node,
            caps: tcr_peer_wire::Caps::default(),
            addrs: row
                .endpoints
                .iter()
                .filter_map(Endpoint::direct_addr)
                .map(|addr| addr.to_string())
                .collect(),
        })
        .collect()
}

/// The endpoints one incoming `Hello.briefs` teaches this node, about peers it
/// already pinned, and nothing about anyone else.
///
/// This is the enforcement half of that rule. The filter is the whole
/// of the rule, and it runs before a brief entry is anything but a local
/// variable: `pinned.iter().any(...)` is checked first, so a brief naming a
/// key this node has not pinned never reaches a socket address, a log line
/// beyond the one below, or this function's return value. That mirrors
/// [`beacon_endpoints`]'s own shape for the same reason: a row that does not
/// exist must not be created from something a peer merely said.
///
/// A dropped brief still earns exactly one debug log line naming the key, and
/// nothing else: no row, no pending entry, no file write, no screen. An
/// operator who wants to see the mesh's shadow beyond their own pins has no
/// surface here to see it from.
///
/// # Both caps are enforced HERE, on receipt
///
/// [`tcr_peer_wire::MAX_NEIGHBOR_BRIEFS`] and [`MAX_BRIEF_ADDRS`] bound what
/// one incoming `Hello` can ever teach, and they are applied on the reading
/// side because the sending side is the half this node does not run. The
/// send-side `take(MAX_NEIGHBOR_BRIEFS)` in `hello_for_peer` shapes what an
/// honest peer offers; it says nothing about what a peer that wants to fill
/// this node's dial order offers, and a slice arriving off a socket is
/// attacker-chosen in both its length and each entry's address count.
pub fn neighbor_brief_endpoints(
    briefs: &[tcr_peer_wire::NeighborBrief],
    pinned: &[PeerRow],
    now_ms: i64,
) -> Vec<(tcr_peer_wire::PeerId, Endpoint)> {
    let mut learned = Vec::new();
    for brief in briefs.iter().take(tcr_peer_wire::MAX_NEIGHBOR_BRIEFS) {
        if !pinned.iter().any(|row| row.node == brief.node) {
            tracing::debug!(
                peer = %brief.node.display(),
                "discovery: dropping a neighbor brief for a peer this node has not pinned; \
                 a brief is not an introduction"
            );
            continue;
        }
        for addr in brief.addrs.iter().take(MAX_BRIEF_ADDRS) {
            // Parsed and never trusted to be a socket, the same rule
            // `beacon_endpoints` follows for the same reason: this is text a
            // trusted peer sent about a THIRD machine, one step further from
            // proof than that peer's own `Hello.addrs`.
            let Ok(addr) = addr.parse::<SocketAddr>() else {
                continue;
            };
            learned.push((
                brief.node,
                Endpoint::direct(addr, now_ms, EndpointSource::Brief),
            ));
        }
    }
    learned
}

/// How many addresses one incoming `NeighborBrief` may carry.
///
/// A brief exists to say "this mutual friend moved, try here". Four is more
/// than that sentence needs, and the number is a cap rather than a guess at a
/// typical value: the field is a `Vec<String>` off a socket, so without a
/// number here one entry can carry as many addresses as fit in a frame.
pub const MAX_BRIEF_ADDRS: usize = 4;

/// How many of a row's [`crate::peer::config::MAX_ENDPOINTS_PER_PEER`] slots
/// a brief may ever hold.
///
/// The weakest source in [`EndpointSource`] does not get to own the dial
/// order. Two slots leave six for endpoints a completed handshake, this
/// node's own mapping, or a matching beacon taught, which is what the peer is
/// actually reached at.
pub const MAX_BRIEF_ENDPOINTS_PER_PEER: usize = 2;

/// Record one incoming `Hello.briefs` against the peers file and return how
/// many rows it moved.
///
/// Mirrors [`observe_beacons`] exactly: the write goes through
/// [`crate::peer::config::observe_endpoints`], the file's one endpoint
/// writer, which refuses to create a row it does not already hold. So "a
/// brief writes nothing for a peer that is not pinned" holds twice over, once
/// in the filter inside [`neighbor_brief_endpoints`] and once in the writer.
///
/// `now_ms` is the caller's clock, never anything off the wire, for the same
/// reason [`endpoints_from_hello`](crate::peer::config::endpoints_from_hello)
/// takes one: a timestamp a peer supplied is the one field of an endpoint
/// that could be used to push a stale address to the front of the dial
/// order.
///
/// # `sender` is the peer whose `Hello` this was, and it is a gate
///
/// `allow.control.briefs` is the operator's answer to "do this Mac and I
/// swap neighbour lists", and it was previously read in one direction only:
/// `hello_for_peer` consulted it when BUILDING a `Hello` while the applying
/// side took whatever arrived. So any pinned peer, including one deliberately
/// granted nothing, could write endpoints into this node's rows. The flag is
/// read on both sides now, which makes it symmetric: a peer this node would
/// not brief does not get to brief this node.
///
/// A sender with no row, or with the flag off, is `Ok(0)` and one debug line,
/// the same shape an unpinned subject gets: refusing a hint is not an error.
///
/// # One read, and one write per node
///
/// The peers file is read once here and written at most once per node named
/// in the batch, rather than once per address. The old shape ran a locked
/// read-modify-write of the operator's file per address, so a peer that sent
/// 500 addresses in one `Hello` bought 500 rewrites of a file every other
/// `tcr peer` mutation contends for. Endpoints are grouped by node and handed
/// to [`crate::peer::config::observe_endpoints`] in one call.
///
/// # A brief never evicts a proven endpoint
///
/// [`crate::peer::config::PeerRow::observe_endpoint`] keeps the newest
/// [`crate::peer::config::MAX_ENDPOINTS_PER_PEER`] and drops the oldest, and
/// a brief's endpoint carries this node's clock, so it is always the newest:
/// unbounded, eight addresses from one stranger's brief would push out every
/// endpoint a handshake proved. Three rules hold instead, all decided from
/// the row as it is on disk before the write:
///
/// - a locator already on the row from a STRONGER source is left alone, never
///   re-dated and never downgraded to [`EndpointSource::Brief`];
/// - a locator already on the row as a brief is refreshed, which evicts
///   nothing;
/// - a locator new to the row is written only while the row has both a free
///   slot and fewer than [`MAX_BRIEF_ENDPOINTS_PER_PEER`] briefs on it.
pub fn observe_neighbor_briefs(
    peers_path: &std::path::Path,
    sender: &tcr_peer_wire::PeerId,
    briefs: &[tcr_peer_wire::NeighborBrief],
    now_ms: i64,
) -> Result<usize> {
    let pinned = crate::peer::config::read_or_default(peers_path)
        .context("discovery: the peers file did not read, so no neighbor brief could be recorded")?
        .peers;
    let Some(sender_row) = pinned.iter().find(|row| &row.node == sender) else {
        tracing::debug!(
            peer = %sender.display(),
            "discovery: dropping a neighbor brief batch from a peer with no row here"
        );
        return Ok(0);
    };
    if !sender_row.allow.control.briefs {
        tracing::debug!(
            peer = %sender.display(),
            "discovery: dropping a neighbor brief batch from a peer this node does not \
             swap neighbour lists with; `tcr peer allow <peer> briefs` is the switch"
        );
        return Ok(0);
    }

    let mut by_node: Vec<(tcr_peer_wire::PeerId, Vec<Endpoint>)> = Vec::new();
    for (node, endpoint) in neighbor_brief_endpoints(briefs, &pinned, now_ms) {
        match by_node.iter_mut().find(|(known, _)| known == &node) {
            Some((_, endpoints)) => endpoints.push(endpoint),
            None => by_node.push((node, vec![endpoint])),
        }
    }

    let mut moved = 0;
    for (node, endpoints) in by_node {
        let Some(row) = pinned.iter().find(|row| row.node == node) else {
            continue;
        };
        let admissible = admissible_brief_endpoints(row, &endpoints);
        if admissible.is_empty() {
            continue;
        }
        if crate::peer::config::observe_endpoints(peers_path, &node, &admissible)? {
            moved += 1;
        }
    }
    Ok(moved)
}

/// Which of one node's brief endpoints may be written, read off the row as it
/// stands: the three rules in [`observe_neighbor_briefs`]'s doc, in order.
fn admissible_brief_endpoints(row: &PeerRow, learned: &[Endpoint]) -> Vec<Endpoint> {
    let mut briefs_held = row
        .endpoints
        .iter()
        .filter(|endpoint| endpoint.source == EndpointSource::Brief)
        .count();
    let mut free_slots = crate::peer::config::MAX_ENDPOINTS_PER_PEER.saturating_sub(
        row.endpoints
            .len()
            .min(crate::peer::config::MAX_ENDPOINTS_PER_PEER),
    );
    let mut admissible: Vec<Endpoint> = Vec::new();
    for endpoint in learned {
        if admissible
            .iter()
            .any(|kept| kept.locator == endpoint.locator)
        {
            continue;
        }
        match row
            .endpoints
            .iter()
            .find(|held| held.locator == endpoint.locator)
        {
            Some(held) if held.source == EndpointSource::Brief => admissible.push(*endpoint),
            Some(_) => continue,
            None => {
                if briefs_held >= MAX_BRIEF_ENDPOINTS_PER_PEER || free_slots == 0 {
                    continue;
                }
                briefs_held += 1;
                free_slots -= 1;
                admissible.push(*endpoint);
            }
        }
    }
    admissible
}

/// The bounded, newest-first found list the UI renders, and the count it could
/// not show.
///
/// # Why a list with caps and not the raw scan
///
/// An announcement costs nothing to send, proves nothing, and is trivially
/// forgeable down to its source address, so the found list is the one surface
/// in this design a stranger can write to directly. Unbounded, a laptop on an
/// open network renders whatever a script feels like putting there.
///
/// Three caps, each from `abuse-resistance.md`'s "announcement flood" row:
/// [`MAX_FOUND_ROWS`] shown with the remainder counted in
/// [`Self::not_shown`], at most [`MAX_FOUND_PER_ADDRESS`] rows per source
/// address, and a row dropped [`FOUND_TTL_MS`] after its last announcement.
///
/// **Nothing an announcement says is written to disk.** This type is process
/// state, held by whatever is rendering; there is no found-list file, which is
/// why a flood costs memory bounded by the caps above and nothing else.
#[derive(Debug, Clone, Default)]
pub struct FoundList {
    rows: Vec<(Discovered, i64)>,
}

impl FoundList {
    /// An empty list.
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one scan's rows in, dropping what has aged out.
    ///
    /// Coalescing is by instance id AND address together: an id changer from
    /// one address is bounded by the per-address cap, and one id appearing from
    /// two addresses is two rows because it really is two places to dial.
    pub fn observe(&mut self, scan: Vec<Discovered>, now_ms: i64) {
        self.rows
            .retain(|(_, seen)| now_ms.saturating_sub(*seen) < FOUND_TTL_MS);
        for row in scan {
            let key = row.addrs.first().cloned().unwrap_or_default();
            if let Some(existing) = self
                .rows
                .iter_mut()
                .find(|(seen, _)| seen.instance_id == row.instance_id && seen.addrs == row.addrs)
            {
                existing.0 = row;
                existing.1 = now_ms;
                continue;
            }
            let from_this_address = self
                .rows
                .iter()
                .filter(|(seen, _)| seen.addrs.first().map(String::as_str) == Some(key.as_str()))
                .count();
            if from_this_address >= MAX_FOUND_PER_ADDRESS {
                continue;
            }
            self.rows.push((row, now_ms));
        }
        // Newest first, which is what `abuse-resistance.md` asks for and also
        // the only order that makes the cap useful: the Mac somebody just
        // turned on is the one they are looking for.
        self.rows.sort_by_key(|(_, seen)| std::cmp::Reverse(*seen));
    }

    /// The rows to show: newest first, at most [`MAX_FOUND_ROWS`].
    pub fn shown(&self) -> Vec<Discovered> {
        self.rows
            .iter()
            .take(MAX_FOUND_ROWS)
            .map(|(row, _)| row.clone())
            .collect()
    }

    /// How many rows are held but not shown, the footer line's number.
    ///
    /// Counted rather than discarded so the UI can be honest: "12 shown, N more
    /// not shown" tells an operator their Mac is on a noisy network, and a
    /// silently truncated list tells them their Mac is broken.
    pub fn not_shown(&self) -> usize {
        self.rows.len().saturating_sub(MAX_FOUND_ROWS)
    }

    /// How many rows are held in total.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether nothing has been found.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// Stop advertising and stop browsing: unregisters whatever [`advertise`]
/// last registered and drops this process's shared mDNS daemon, so a second
/// [`advertise`]/[`browse`] call starts a fresh one.
///
/// `pub`, but not part of the two-function TRUST surface the module docs
/// describe, that claim is about what implementing the mesh's trust model
/// needs (nothing calls `stop_all` to decide whether to trust a row), and
/// `run_peer`'s `find off` body (`src/main.rs`, the `tcr` binary crate) is the
/// only caller, which is why this could not stay `pub(crate)`: the binary and
/// the library are separate crates here. Deliberately synchronous: mdns-sd's
/// `unregister`/`stop_browse`/`shutdown` hand back a completion receiver
/// rather than blocking, so returning once they are *requested* is what makes
/// `find off` answer within one second; the daemon finishes tearing
/// down its background thread shortly after.
pub fn stop_all() -> Result<()> {
    let mut daemon_guard = DAEMON.lock().map_err(|_| lock_poisoned("mDNS daemon"))?;
    let Some(daemon) = daemon_guard.take() else {
        return Ok(()); // never started; nothing to stop
    };
    let mut announced = ANNOUNCED_FULLNAME
        .lock()
        .map_err(|_| lock_poisoned("mDNS announced-fullname"))?;
    if let Some(fullname) = announced.take() {
        unregister_beacon(&daemon, &fullname);
    }
    match ANNOUNCED_INSTANCE.lock() {
        Ok(mut held) => *held = None,
        Err(poisoned) => *poisoned.into_inner() = None,
    }
    log_teardown_err(
        daemon.stop_browse(SERVICE_TYPE),
        "stopping the mDNS browse on stop_all",
    );
    log_teardown_err(daemon.shutdown(), "shutting down the shared mDNS daemon");
    Ok(())
}

/// A small, non-cryptographic nonce for the mDNS instance label. Not identity
/// (see [`register_beacon`]), just enough to keep two announcers on the same
/// box from colliding on one instance name.
fn rand_u32() -> u32 {
    use std::hash::{BuildHasher, Hash, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    std::time::Instant::now().hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    hasher.finish() as u32
}

/// One machine seen on the network and not yet trusted.
///
/// **Carries no identity**, because the beacon carries none: a name the
/// operator chose, where to dial, and nothing that could be mistaken for proof
/// of who answered. The key arrives from the `Noise_XX` handshake the operator
/// starts by pressing Trust, and the six-digit compare is what binds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    /// The ephemeral per-boot id it announced. **Not identity**. See
    /// [`tcr_peer_wire::InstanceId`]. It is what Trust knocks under, and what
    /// the far side's Accept keys its 120-second window to, so a row without
    /// one is dropped: its Trust button could not work.
    pub instance_id: tcr_peer_wire::InstanceId,
    /// The display name it announced, already through the sanitizer. `None`
    /// when it announces no name, which is a setting and not a fault: the row
    /// then shows its address.
    pub name: Option<String>,
    /// Where it says it can be reached.
    pub addrs: Vec<String>,
    /// The peer port it says it listens on.
    pub port: u16,
}

#[cfg(test)]
mod tests {
    //! Whitebox tests against the private seams ([`register_beacon`],
    //! [`browse_once`], [`stop_all`]) rather than [`advertise`]/[`browse`]
    //! themselves: those two take `&PeerStore`, and `PeerStore` is only
    //! constructible through `crate::peer::config::PeerStore::open`, which is
    //! `todo!()` at the time this file was written (phase 1/4, in
    //! `src/peer/config.rs`). The logic under test is identical either
    //! way, `advertise`/`browse` are thin wrappers that add nothing but the
    //! `store` parameter this crate does not yet know how to construct in a
    //! test. See the report for the panic this produces if called directly.

    use std::time::Duration;

    use mdns_sd::ServiceDaemon;
    use tcr_peer_wire::PeerId;

    use super::*;

    #[test]
    fn sanitize_name_rejects_email_and_uuid_shape() {
        assert_eq!(sanitize_name("studio-mac"), Some("studio-mac".to_string()));
        assert_eq!(
            sanitize_name("  studio-mac  "),
            Some("studio-mac".to_string())
        );
        assert_eq!(sanitize_name("gil@example.com"), None);
        assert_eq!(sanitize_name("ab6f2c1e-1a2b-3c4d-5e6f-0123456789ab"), None);
        assert_eq!(sanitize_name(""), None);
        assert_eq!(sanitize_name("   "), None);
        assert_eq!(
            sanitize_name(&"x".repeat(tcr_peer_wire::MAX_LABEL_BYTES + 1)),
            None
        );
    }

    /// The name in an inbound TXT record is text a stranger on
    /// this network chose, and [`browse_once`] puts it in a panel row, a
    /// `tcr peer ls` line and a log line. The whitelist is what makes that
    /// safe, so every shape a denylist would have had to enumerate is named
    /// here as a refusal: a C0 control character, an ANSI escape that
    /// rewrites the line an operator is reading, a newline that forges a
    /// second row, a `U+202E RIGHT-TO-LEFT OVERRIDE` that displays the name
    /// backwards, and a tab.
    ///
    /// Watched red: with the whitelist replaced by the older
    /// condition (`trimmed.contains('@')` and the length cap), all six of
    /// these are accepted and this test fails on the first one.
    #[test]
    fn a_hostile_beacon_name_is_refused() {
        for hostile in [
            "studio\u{0}mac",      // a NUL
            "studio\u{7}mac",      // BEL
            "studio\u{1b}[31mmac", // an ANSI colour escape
            "studio\nmac",         // a forged second row
            "studio\u{202e}mac",   // RIGHT-TO-LEFT OVERRIDE
            "studio\tmac",         // a tab
        ] {
            assert_eq!(
                sanitize_name(hostile),
                None,
                "a beacon name carrying {hostile:?} must be refused, not rendered"
            );
        }

        // And the control: a name made only of whitelisted characters is
        // still accepted, so the refusals above are the whitelist working and
        // not a sanitizer that refuses everything.
        assert_eq!(
            sanitize_name("Studio Mac_2.0-b"),
            Some("Studio Mac_2.0-b".to_string())
        );
    }

    /// The whitelist has to apply to what ARRIVES, not only to what this node
    /// announces: the inbound path reads the name out of a TXT record a
    /// stranger wrote, and a control character there reaches a panel row, a
    /// `tcr peer ls` line and a log line.
    ///
    /// Driven through [`discovered_row`], which is the whole of what
    /// [`browse_once`] does with one resolved row, a hostile name cannot be
    /// driven through a real daemon without a second machine writing the TXT
    /// record, and the accepting half of the same call site is already end to
    /// end in `two_announcers_on_loopback_see_each_other`.
    ///
    /// Watched red: with `sanitize_name` dropped from `discovered_row`, the
    /// first assertion fails with the escape sequence still in the name.
    #[test]
    fn an_inbound_hostile_name_is_dropped_and_the_row_survives() {
        let instance = tcr_peer_wire::InstanceId([1; tcr_peer_wire::INSTANCE_ID_BYTES]);
        let row = discovered_row(
            Some(&instance.to_wire()),
            None,
            Some("studio\u{1b}[31mmac"),
            vec!["192.0.2.7".to_string()],
            9600,
            None,
            0,
        )
        .expect("a row with an address is still dialable");
        assert_eq!(
            row.name, None,
            "a name this node will not render is no name at all, the row shows its address"
        );
        assert_eq!(row.addrs, vec!["192.0.2.7".to_string()]);

        // The accepting case, so the assertion above is the sanitizer and not
        // a function that drops every name.
        let good = discovered_row(
            Some(&instance.to_wire()),
            None,
            Some("studio-mac"),
            vec!["192.0.2.7".to_string()],
            9600,
            None,
            0,
        )
        .expect("a row with an address");
        assert_eq!(good.name.as_deref(), Some("studio-mac"));

        // And a row with nothing to dial is dropped outright.
        assert_eq!(
            discovered_row(
                Some(&instance.to_wire()),
                None,
                Some("studio-mac"),
                Vec::new(),
                9600,
                None,
                0
            ),
            None
        );

        // A row with no readable instance id is dropped too: Trust knocks
        // under that id, so a row without one has a button that cannot work.
        assert_eq!(
            discovered_row(
                None,
                None,
                Some("studio-mac"),
                vec!["192.0.2.7".to_string()],
                9600,
                None,
                0
            ),
            None
        );
    }

    /// A measurement proved `PeerId::display()`'s own shape passed
    /// this sanitizer today. Both the exact form and a lowercased one (a
    /// name is not case-sensitive to an attacker picking what to paste), and
    /// the 52-character wire form for the same reason, even though the
    /// length cap alone already refuses it, the explicit shape check stays
    /// so the refusal does not silently stop working if the cap ever moves.
    #[test]
    fn sanitize_name_rejects_the_tcr_display_shape_and_the_wire_form() {
        let node = PeerId([0xAB_u8; 32]);
        assert_eq!(sanitize_name(&node.display()), None);
        assert_eq!(sanitize_name(&node.display().to_lowercase()), None);
        assert_eq!(sanitize_name(&node.to_wire()), None);

        // A name that merely starts with "tcr-" but is the wrong length is
        // still a normal display name.
        assert_eq!(sanitize_name("tcr-office"), Some("tcr-office".to_string()));
    }

    /// The beacon carries the wire version and the per-boot instance id
    /// always, the name only when allowed, and the network-key tag only when a
    /// key is set, and it carries no node id and no public key in any
    /// configuration, which is the claim the ephemeral rule turns on.
    #[test]
    fn beacon_txt_carries_version_and_instance_always_and_name_only_when_allowed() {
        let version_pair = (
            TXT_VERSION_KEY.to_string(),
            tcr_peer_wire::PROTO_VERSION.to_string(),
        );
        let instance = tcr_peer_wire::InstanceId([0xAB; tcr_peer_wire::INSTANCE_ID_BYTES]);
        let instance_pair = (TXT_INSTANCE_KEY.to_string(), instance.to_wire());

        assert_eq!(
            beacon_txt(&instance, 9600, None, None, 0),
            vec![version_pair.clone(), instance_pair.clone()]
        );
        assert_eq!(
            beacon_txt(&instance, 9600, Some("studio-mac"), None, 0),
            vec![
                version_pair.clone(),
                instance_pair.clone(),
                (TXT_NAME_KEY.to_string(), "studio-mac".to_string())
            ]
        );
        // A name that fails the sanitizer collapses to the same "nothing to
        // say" as no name at all, rather than shipping the raw string, the
        // version is still there either way.
        assert_eq!(
            beacon_txt(&instance, 9600, Some("gil@example.com"), None, 0),
            vec![version_pair.clone(), instance_pair.clone()]
        );

        // With a network key set the tag appears, and it is the one
        // `NetworkKey::announcement_tag` computes rather than anything this
        // test re-derives.
        let key = crate::peer::config::NetworkKey::from_bytes([7; 32]);
        let tagged = beacon_txt(&instance, 9600, None, Some(&key), 120);
        assert_eq!(
            tagged,
            vec![
                version_pair,
                instance_pair,
                (
                    TXT_TAG_KEY.to_string(),
                    key.announcement_tag(&instance, 9600, 2)
                )
            ]
        );
    }

    /// `peer.find off` (the default) registers nothing: browsing sees no row.
    ///
    /// # The proof this doc-comment used to claim, and what it really measures
    ///
    /// An earlier version of this comment said a stray `register_beacon`
    /// before the browse had been confirmed by hand to fail the assertion with
    /// `left: 1, right: 0`. **That was measured and it is not
    /// true at the window this test used.** Run with `register_beacon`
    /// inserted and a 300 ms window: `found=0`, the test stays green while a
    /// beacon is live, because a resolve takes longer than 300 ms to come
    /// back. The same probe at 3 s: `found=10` (ten `ServiceResolved` events
    /// as `mdns-sd` learns this box's interface addresses one by one).
    ///
    /// So the window here is the 3 s the instrument actually needs, and the
    /// positive control is a separate test rather than a sentence:
    /// `two_announcers_on_loopback_see_each_other` runs the SAME
    /// [`browse_once`] at the SAME window and finds a row. An absence
    /// measured by an instrument nothing has shown can detect presence is not
    /// evidence, which is the whole reason that sentence had to go.
    ///
    /// The claim is also narrowed to THIS test's port. A 3 s browse on a real
    /// network can legitimately resolve somebody else's `_tcr-peer._tcp` row,
    /// another Mac, or a `tcr` on this box with finding on, and "no rows at
    /// all" would make this test fail for a reason that has nothing to do with
    /// what it measures. What it measures is that a daemon which never
    /// advertised does not conjure ITS row.
    #[test]
    fn find_off_registers_nothing() {
        /// A port this test never registers. Every other test in this file
        /// uses its own, so a row carrying this one could only come from a
        /// registration nobody made.
        const NEVER_REGISTERED_PORT: u16 = 47814;

        let daemon = ServiceDaemon::new().expect("test daemon");
        let found = browse_once(&daemon, Duration::from_secs(3), None).expect("browse_once");
        assert!(
            !found.iter().any(|row| row.port == NEVER_REGISTERED_PORT),
            "a daemon that never advertised must not find its own row: {found:?}"
        );
        log_teardown_err(daemon.shutdown(), "shutting down test mDNS daemon");
    }

    /// A measurement proved [`register_beacon`] advertised the
    /// literal `127.0.0.1`, unreachable from any other Mac. Checked on the
    /// built [`ServiceInfo`] rather than a live daemon, so this does not need
    /// multicast: no address baked in, and auto-detect turned on so
    /// `mdns-sd` fills in this host's real interface addresses.
    ///
    /// Watch it fail: put `"127.0.0.1"` back as the fourth argument to
    /// `ServiceInfo::new` in `build_beacon_info` and this fails on the first
    /// assertion (`get_addresses()` is no longer empty), confirmed by hand
    /// while writing this fix.
    #[test]
    fn beacon_info_carries_no_hardcoded_address() {
        let info =
            build_beacon_info("tcr-peer-test-addr", Some("studio-mac"), 47813, None).expect("info");
        assert!(
            info.get_addresses().is_empty(),
            "no address should be baked into the ServiceInfo: got {:?}",
            info.get_addresses()
        );
        assert!(
            info.is_addr_auto(),
            "auto-detect must be on so mdns-sd fills in the real interface addresses"
        );
    }

    /// `peer.find on` starts both announce and browse: a second daemon on
    /// loopback sees the row, with the sanitized name, the right port, and no
    /// identity in it.
    #[test]
    fn two_announcers_on_loopback_see_each_other() {
        let announcer = ServiceDaemon::new().expect("announcer daemon");
        register_beacon(
            &announcer,
            "tcr-peer-test-1",
            Some("studio-mac"),
            47811,
            None,
        )
        .expect("register_beacon");

        let browser = ServiceDaemon::new().expect("browser daemon");
        let found = browse_once(&browser, Duration::from_secs(3), None).expect("browse_once");

        assert!(
            found
                .iter()
                .any(|row| row.port == 47811 && row.name.as_deref() == Some("studio-mac")),
            "expected a row for the loopback announcer, got {found:?}"
        );

        log_teardown_err(
            announcer.unregister(&format!("tcr-peer-test-1.{SERVICE_TYPE}")),
            "unregistering test beacon",
        );
        log_teardown_err(announcer.shutdown(), "shutting down test announcer daemon");
        log_teardown_err(browser.shutdown(), "shutting down test browser daemon");
    }

    /// `peer.find off` stops both within about a second: after `stop_all`,
    /// [`ANNOUNCED_FULLNAME`] is clear and the shared daemon is gone, so the
    /// next `advertise`/`browse` starts a fresh one rather than reusing a
    /// half-torn-down handle.
    #[test]
    fn stop_all_clears_shared_state() {
        {
            let mut guard = DAEMON.lock().expect("daemon lock");
            *guard = None;
        }
        {
            let mut guard = ANNOUNCED_FULLNAME.lock().expect("fullname lock");
            *guard = None;
        }

        let daemon = shared_daemon().expect("shared_daemon");
        let fullname = register_beacon(&daemon, "tcr-peer-test-2", Some("studio-mac"), 47812, None)
            .expect("register");
        *ANNOUNCED_FULLNAME.lock().expect("fullname lock") = Some(fullname);

        stop_all().expect("stop_all");

        assert!(
            DAEMON.lock().expect("daemon lock").is_none(),
            "stop_all must drop the shared daemon"
        );
        assert!(
            ANNOUNCED_FULLNAME.lock().expect("fullname lock").is_none(),
            "stop_all must clear what it unregistered"
        );
    }

    /// A forged beacon buys nothing: a [`Discovered`] row is data, never
    /// authorization, there is no method on this type that could create a
    /// pin. Compile-time coverage: if a future edit adds one, this doc test's
    /// claim goes stale and a reader filling in the trust store has to look
    /// here first.
    #[test]
    fn discovered_carries_no_pinning_capability() {
        let row = Discovered {
            instance_id: tcr_peer_wire::InstanceId([2; tcr_peer_wire::INSTANCE_ID_BYTES]),
            name: Some("studio-mac".to_string()),
            addrs: vec!["127.0.0.1".to_string()],
            port: 47811,
        };
        // The only thing `Discovered` supports is reading its own fields back.
        assert_eq!(row.name.as_deref(), Some("studio-mac"));
        assert_eq!(row.port, 47811);
    }
}
