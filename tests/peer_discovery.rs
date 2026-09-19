//! Phase 3's gate on the one thing a beacon is allowed to say.
//!
//! The discovery beacon carries service presence, a port, the wire version, a
//! per-boot ephemeral instance id, the network-key tag when a key is set, and
//! (when the operator allows it), a display name. **It never carries the node
//! id, the static public key, or any other Noise material.** Identity is
//! learned inside the handshake, after the operator presses Accept and then
//! Trust, which is why a first pairing is always `Noise_XX` with a six-digit
//! compare.

use tcr_peer_wire::{InstanceId, PeerId, INSTANCE_ID_BYTES};
use teamclaude_rs::peer::config::NetworkKey;
use teamclaude_rs::peer::discovery::{self, Discovered, FoundList};

/// An instance id distinctive enough that a fragment of it could not appear by
/// chance in a TXT record.
fn test_instance() -> InstanceId {
    InstanceId([0x5A; INSTANCE_ID_BYTES])
}

/// **Nothing in the beacon identifies this node cryptographically.**
///
/// Three checks, because there are three spellings of the same mistake: the
/// display form of the id, the full wire form, and any raw byte run of the
/// static key. A test that only looked for the display form would pass a beacon
/// that shipped the key as base32.
///
/// Carries a POSITIVE CONTROL: the allowed name must be present when
/// announcing is on. Without it, a builder that returned an empty payload, or
/// one this test failed to call at all: would satisfy every absence below and
/// prove nothing.
///
/// Watch it fail by adding the node id under any TXT key.
///
/// `PeerId::display()`/`PeerId::to_wire()` are landed, so the `#[ignore]`
/// this carried while they were `todo!()` is gone. Confirmed by hand while
/// writing this un-ignore: with
/// `txt.push((TXT_NAME_KEY.to_string(), node.display()))` inserted into
/// `beacon_txt`, this test failed with "the beacon must not carry the node id
/// in display form"; reverted, and it passes again.
#[test]
fn beacon_carries_no_key_or_id() {
    // An obviously fake static key, distinctive enough that a fragment of it
    // could not appear by chance.
    let node = PeerId([0xAB_u8; 32]);
    let instance = test_instance();

    let txt = discovery::beacon_txt(&instance, 9600, Some("studio-mac"), None, 0);
    let flat = txt
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(";");

    // Positive control: the one thing it IS allowed to say.
    assert!(
        flat.contains("studio-mac"),
        "positive control failed: the beacon carried no name, so the absences \
         below prove nothing about what it would have carried"
    );

    assert!(
        !flat.contains(&node.display()),
        "the beacon must not carry the node id in display form"
    );
    assert!(
        !flat.contains(&node.to_wire()),
        "the beacon must not carry the node id in wire form"
    );
    // Any run of the raw key, in case it is ever hex-encoded, byte-stuffed or
    // pasted in some third shape nobody thought to name.
    assert!(
        !flat.as_bytes().windows(4).any(|w| w == [0xAB; 4]),
        "the beacon must not carry any bytes of the static public key"
    );

    // And with announcing off, the payload says the wire version and the
    // ephemeral instance id: presence and a port are carried by the service
    // registration, not by a TXT record.
    let off = discovery::beacon_txt(&instance, 9600, None, None, 0);
    assert_eq!(
        off,
        vec![
            (
                discovery::TXT_VERSION_KEY.to_string(),
                tcr_peer_wire::PROTO_VERSION.to_string()
            ),
            (discovery::TXT_INSTANCE_KEY.to_string(), instance.to_wire())
        ],
        "with the name withheld the TXT record says the version and the per-boot id"
    );
}

/// Blackbox cover for the same claim as `beacon_carries_no_key_or_id`, using
/// only its own [`discovery::beacon_txt`]: no `PeerId`, so it does
/// not depend on another file's symbol.
#[test]
fn beacon_txt_carries_only_version_instance_and_name() {
    let instance = test_instance();
    let txt = discovery::beacon_txt(&instance, 9600, Some("studio-mac"), None, 0);
    assert_eq!(
        txt.len(),
        3,
        "with no network key the beacon may say exactly three things: the version, the \
         per-boot instance id and the name: got {txt:?}"
    );
    assert_eq!(txt[0].0, discovery::TXT_VERSION_KEY);
    assert_eq!(txt[0].1, tcr_peer_wire::PROTO_VERSION.to_string());
    assert_eq!(txt[1].0, discovery::TXT_INSTANCE_KEY);
    assert_eq!(txt[1].1, instance.to_wire());
    assert_eq!(txt[2].0, discovery::TXT_NAME_KEY);
    assert_eq!(txt[2].1, "studio-mac");

    let off = discovery::beacon_txt(&instance, 9600, None, None, 0);
    assert_eq!(
        off.len(),
        2,
        "off means no name, not no TXT record: the version and the id still ride"
    );
    assert_eq!(off[0].0, discovery::TXT_VERSION_KEY);
    assert_eq!(off[1].0, discovery::TXT_INSTANCE_KEY);
}

// ---------------------------------------------------------------------------
// A FRESH config announces no name
// ---------------------------------------------------------------------------

/// **A fresh config's TXT record has no `name=`.**
///
/// `peer.announceName` used to default ON,
/// and it is now OFF, because "announce must send ephemeral data unless
/// otherwise configured" and a display name is the one field in the beacon that
/// is not ephemeral.
///
/// Driven through the DEFAULT `PeerFile` rather than a literal `false`, and
/// through the same `announce_name.then(...)` expression `tcr peer find on`
/// uses to decide what to pass: a test that hardcoded `None` for the name
/// would still pass with the default flipped back to `true`, which is exactly
/// the regression it exists to catch.
///
/// Watched red: with `#[serde(default = "default_announce_name")]` (returning
/// `true`) restored on `PeerFile::announce_name`, this fails on the first
/// assertion with a `name=studio-mac` pair present.
#[test]
fn a_fresh_config_announces_no_name() {
    let fresh = teamclaude_rs::peer::config::PeerFile::default();
    assert!(
        !fresh.announce_name,
        "announceName defaults OFF, so a fresh install announces ephemeral \
         data only"
    );

    // The caller's own expression, not a re-derivation of it.
    let name = fresh.announce_name.then(|| "studio-mac".to_string());
    let txt = discovery::beacon_txt(&test_instance(), 9600, name.as_deref(), None, 0);
    assert!(
        !txt.iter().any(|(key, _)| key == discovery::TXT_NAME_KEY),
        "a fresh config's beacon must carry no name= key, and this one carries {txt:?}"
    );

    // The positive control: with the operator having turned it on, the same
    // expression does produce a name. Without this, a `beacon_txt` that had
    // stopped emitting names at all would pass the assertion above.
    let asked = teamclaude_rs::peer::config::PeerFile {
        announce_name: true,
        ..teamclaude_rs::peer::config::PeerFile::default()
    };
    let name = asked.announce_name.then(|| "studio-mac".to_string());
    let txt = discovery::beacon_txt(&test_instance(), 9600, name.as_deref(), None, 0);
    assert!(
        txt.iter()
            .any(|(key, value)| key == discovery::TXT_NAME_KEY && value == "studio-mac"),
        "positive control: with announceName on the name must ride, got {txt:?}"
    );
}

// ---------------------------------------------------------------------------
// The network-key tag, and the untagged row
// ---------------------------------------------------------------------------

/// **An untagged announcement never becomes a row on a Mac that holds the
/// network key**: and a badly tagged one does not either.
///
/// Receivers with a key drop untagged or bad-tag rows before the
/// UI. Driven through `beacon_txt` on the sending side and the inbound row
/// builder on the receiving side, so the tag under test is the one this code
/// really computes rather than a literal this test wrote down.
///
/// Watched red: with the `if let Some(key) = network_key { … }` block removed
/// from `discovered_row`, both the untagged and the bad-tag assertions fail
/// (the rows come back as `Some`).
#[test]
fn an_untagged_row_never_appears_to_a_mac_with_the_network_key() {
    let key = NetworkKey::from_bytes([0x11; 32]);
    let instance = test_instance();
    let port = 9600_u16;
    // A fixed instant, so the minute the tag is over is the same on both
    // sides of this test without either of them reading a clock.
    let now = 1_700_000_000_i64;

    // The sending side, through the one builder.
    let txt = discovery::beacon_txt(&instance, port, None, Some(&key), now);
    let tag = txt
        .iter()
        .find(|(k, _)| k == discovery::TXT_TAG_KEY)
        .map(|(_, v)| v.clone())
        .expect("a beacon with a network key set carries a tag");

    // Positive control first: the real tag verifies, so the refusals below are
    // the check working and not a function that refuses everything.
    assert!(
        discovery::discovered_row(
            Some(&instance.to_wire()),
            Some(&tag),
            None,
            vec!["192.0.2.7".to_string()],
            port,
            Some(&key),
            now,
        )
        .is_some(),
        "positive control: a correctly tagged announcement must become a row"
    );

    // No tag at all: a `tcr` on the same LAN with no key set.
    assert!(
        discovery::discovered_row(
            Some(&instance.to_wire()),
            None,
            None,
            vec!["192.0.2.7".to_string()],
            port,
            Some(&key),
            now,
        )
        .is_none(),
        "an untagged announcement must be dropped before it becomes a row"
    );

    // A tag of the right shape and the wrong value.
    assert!(
        discovery::discovered_row(
            Some(&instance.to_wire()),
            Some("0000000000000000"),
            None,
            vec!["192.0.2.7".to_string()],
            port,
            Some(&key),
            now,
        )
        .is_none(),
        "a bad tag must be dropped before it becomes a row"
    );

    // A tag computed under a DIFFERENT network key: the case that matters on an
    // office network with two meshes on it.
    let other = NetworkKey::from_bytes([0x22; 32]);
    let other_tag = other.announcement_tag(&instance, port, now.div_euclid(60));
    assert!(
        discovery::discovered_row(
            Some(&instance.to_wire()),
            Some(&other_tag),
            None,
            vec!["192.0.2.7".to_string()],
            port,
            Some(&key),
            now,
        )
        .is_none(),
        "a tag under another network key must be dropped"
    );

    // And a Mac with NO key holds neither expectation: it takes the row
    // either way, which is what keeps one LAN usable by both configurations.
    assert!(
        discovery::discovered_row(
            Some(&instance.to_wire()),
            None,
            None,
            vec!["192.0.2.7".to_string()],
            port,
            None,
            now,
        )
        .is_some(),
        "a Mac with no network key must still see an untagged announcement"
    );
}

/// The tag is over the instance id, the port AND the minute, so changing any
/// one of the three changes it.
///
/// Three separate assertions rather than one, because a MAC computed over only
/// the first field would pass a test that varied only the first field: and the
/// port is the one an implementation is most likely to forget, since it is
/// already in the service registration.
#[test]
fn the_announcement_tag_covers_the_instance_the_port_and_the_minute() {
    let key = NetworkKey::from_bytes([0x33; 32]);
    let instance = test_instance();
    let base = key.announcement_tag(&instance, 9600, 100);

    let other_instance = InstanceId([0x5B; INSTANCE_ID_BYTES]);
    assert_ne!(
        base,
        key.announcement_tag(&other_instance, 9600, 100),
        "the tag must cover the instance id"
    );
    assert_ne!(
        base,
        key.announcement_tag(&instance, 9601, 100),
        "the tag must cover the port"
    );
    assert_ne!(
        base,
        key.announcement_tag(&instance, 9600, 101),
        "the tag must cover the minute"
    );

    // And the minute before is accepted, because a beacon composed at :59.9
    // arrives in the next minute: with the one before THAT refused, so the
    // window really is two minutes and not unbounded.
    let now = 6_060_i64; // minute 101, one second in
    assert!(
        key.tag_matches(
            &key.announcement_tag(&instance, 9600, 101),
            &instance,
            9600,
            now
        ),
        "this minute's tag must verify"
    );
    assert!(
        key.tag_matches(
            &key.announcement_tag(&instance, 9600, 100),
            &instance,
            9600,
            now
        ),
        "the previous minute's tag must verify, or a beacon that crossed a boundary is lost"
    );
    assert!(
        !key.tag_matches(
            &key.announcement_tag(&instance, 9600, 99),
            &instance,
            9600,
            now
        ),
        "two minutes back must NOT verify, or the replay window is unbounded"
    );
}

// ---------------------------------------------------------------------------
// The found-list caps
// ---------------------------------------------------------------------------

/// One announcement, as a row that has already passed the inbound checks.
fn row(instance: u8, addr: &str) -> Discovered {
    Discovered {
        instance_id: InstanceId([instance; INSTANCE_ID_BYTES]),
        name: None,
        addrs: vec![addr.to_string()],
        port: 9600,
    }
}

/// **The found list shows twelve and counts the rest.**
///
/// `abuse-resistance.md`'s announcement-flood row: "found list capped at 12
/// rows, newest first, 'N more not shown'". Driven with rows from 40 distinct
/// addresses, which is the shape of the attack: one host per row keeps the
/// per-address cap out of the way, so what this measures is the total cap.
///
/// Watched red: with `.take(MAX_FOUND_ROWS)` removed from `FoundList::shown`,
/// this fails with `shown 40, want 12`.
#[test]
fn the_found_list_shows_twelve_and_counts_the_rest() {
    let mut found = FoundList::new();
    let scan: Vec<Discovered> = (0..40_u8)
        .map(|n| row(n, &format!("192.0.2.{n}")))
        .collect();
    found.observe(scan, 1_000);

    assert_eq!(
        found.len(),
        40,
        "every row is held; the cap is on what SHOWS"
    );
    assert_eq!(
        found.shown().len(),
        discovery::MAX_FOUND_ROWS,
        "shown {}, want {}",
        found.shown().len(),
        discovery::MAX_FOUND_ROWS
    );
    assert_eq!(
        found.not_shown(),
        40 - discovery::MAX_FOUND_ROWS,
        "the footer's number must be the rows held back, so the UI can be honest about it"
    );
}

/// **Two rows per source address, and the third is dropped.**
///
/// The id-changer case at the discovery layer: one host rotating its instance
/// id gets two rows, not one per id. Watched red with the
/// `from_this_address >= MAX_FOUND_PER_ADDRESS` guard removed: the list then
/// holds all eight.
#[test]
fn one_address_holds_at_most_two_found_rows() {
    let mut found = FoundList::new();
    let scan: Vec<Discovered> = (0..8_u8).map(|n| row(n, "192.0.2.7")).collect();
    found.observe(scan, 1_000);
    assert_eq!(
        found.len(),
        discovery::MAX_FOUND_PER_ADDRESS,
        "one address may hold {} rows, and this list holds {}",
        discovery::MAX_FOUND_PER_ADDRESS,
        found.len()
    );

    // The positive control: a DIFFERENT address still gets its own rows, so
    // the cap above is per address and not a global two.
    found.observe(vec![row(9, "192.0.2.8")], 1_000);
    assert_eq!(
        found.len(),
        discovery::MAX_FOUND_PER_ADDRESS + 1,
        "the cap is per address, so a second address adds a row"
    );
}

/// **A row that stops announcing leaves after sixty seconds**, and one that
/// keeps announcing does not.
///
/// Watched red by widening the `retain` in `FoundList::observe` to
/// `FOUND_TTL_MS * 10`: the stale row survives and the first assertion fails.
#[test]
fn a_row_that_stops_announcing_leaves_after_sixty_seconds() {
    let mut found = FoundList::new();
    found.observe(vec![row(1, "192.0.2.7"), row(2, "192.0.2.8")], 1_000);
    assert_eq!(found.len(), 2);

    // One keeps announcing, the other does not, one millisecond past the TTL.
    let later = 1_000 + discovery::FOUND_TTL_MS + 1;
    found.observe(vec![row(1, "192.0.2.7")], later);
    let shown = found.shown();
    assert_eq!(
        shown.len(),
        1,
        "the silent row must be gone and the announcing one must stay: {shown:?}"
    );
    assert_eq!(shown[0].instance_id, InstanceId([1; INSTANCE_ID_BYTES]));

    // And re-announcing inside the TTL refreshes rather than duplicating.
    found.observe(vec![row(1, "192.0.2.7")], later + 1);
    assert_eq!(
        found.len(),
        1,
        "the same instance at the same address is one row that updates"
    );
}

// ---------------------------------------------------------------------------
// A beacon is the last fact left about a peer that moved
// ---------------------------------------------------------------------------

/// A scratch directory per test, per process and per thread, so two tests here,
/// and several lanes at once, never share a peers file.
fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tcr-peer-discovery-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the scratch dir");
    dir
}

/// One pinned row whose only endpoint is `addr`.
fn pinned_row(node: PeerId, addr: std::net::SocketAddr) -> teamclaude_rs::peer::config::PeerRow {
    let mut row = teamclaude_rs::peer::config::PeerRow {
        node,
        label: "studio-mac".to_string(),
        endpoints: Vec::new(),
        added_at: 1_000,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: teamclaude_rs::peer::config::Allow::default(),
        lend: Vec::new(),
    };
    row.observe_endpoint(teamclaude_rs::peer::config::Endpoint::direct(
        addr,
        1_000,
        teamclaude_rs::peer::config::EndpointSource::Paired,
    ));
    row
}

/// **A pinned Mac moves on the LAN, and its next beacon is enough to reach it
/// again.**
///
/// This is the case no session can fix, which is why the beacon is wired to
/// the endpoint list at all: the peer changed address, so every endpoint on
/// its row is dead, and a `Hello` cannot teach the new one because no session
/// can be started to carry it. The announcement is the only fact left.
///
/// The dead port is a POSITIVE CONTROL and not scenery: the first assertion
/// proves the row really is unreachable before the scan, so the last one is
/// about the beacon and not about a port that was already open.
///
/// Both refusals are checked in the same test rather than in two, because
/// what is being asserted is a FILTER and a filter is only as good as what it
/// drops: an unbound beacon (a stranger on the LAN) and a binding for a key
/// this node never pinned both write nothing at all.
///
/// Watched red: with the `pinned.iter().any(...)` guard and the binding lookup
/// intact but `EndpointSource::Beacon` endpoints never written, that is, with
/// the `observe_endpoints` call in `discovery::observe_beacons` removed, this
/// fails at "A dials the address B announced", because A's row still holds
/// only the dead port.
#[tokio::test]
async fn a_pinned_mac_that_moved_is_reached_at_the_address_its_beacon_announced() {
    let dir = scratch("beacon-endpoint");
    let peers = dir.join("tcr-peers.json");

    // B, after the move: a real listener on a kernel-chosen loopback port.
    let moved = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the address B moved to");
    let moved_addr = moved.local_addr().expect("the bound address");

    // The port B used to answer on, bound only to learn a number and dropped.
    let dead = {
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a port to learn its number");
        probe.local_addr().expect("the bound address")
    };

    let b = PeerId([0x11; 32]);
    let stranger = PeerId([0x22; 32]);
    let file = teamclaude_rs::peer::config::PeerFile {
        peers: vec![pinned_row(b, dead)],
        ..Default::default()
    };
    teamclaude_rs::peer::config::save(&peers, &file).expect("write the peers file");

    let b_instance = InstanceId([0xB1; INSTANCE_ID_BYTES]);
    let beacon = Discovered {
        instance_id: b_instance,
        name: None,
        addrs: vec![moved_addr.ip().to_string()],
        port: moved_addr.port(),
    };

    // ---- control: before the scan, A cannot reach B at all.
    let stale = teamclaude_rs::peer::config::read_or_default(&peers)
        .expect("the peers file reads")
        .peers
        .remove(0);
    assert!(
        teamclaude_rs::peer::serve::dial_peer(&stale)
            .await
            .is_none(),
        "the control: A's only endpoint for B is a port nobody listens on, so the dial \
         must fail before the beacon, otherwise the last assertion proves nothing"
    );

    // ---- refusal 1: a beacon nothing has bound is a stranger, and writes nothing.
    let unbound = discovery::observe_beacons(&peers, std::slice::from_ref(&beacon), &[], 5_000)
        .expect("an unbound scan is not an error");
    assert_eq!(
        unbound, 0,
        "an announcement from an instance no session has bound to a pinned key must not \
         touch any row: it is a found-list row to Trust, not an edit to a trusted one"
    );

    // ---- refusal 2: a binding for a key this node never pinned writes nothing.
    let unpinned = discovery::observe_beacons(
        &peers,
        std::slice::from_ref(&beacon),
        &[discovery::InstanceBinding {
            instance_id: b_instance,
            node: stranger,
        }],
        5_000,
    )
    .expect("an unpinned binding is not an error");
    assert_eq!(
        unpinned, 0,
        "a beacon must never CREATE a row: a key this node has not pinned has no endpoints"
    );

    // ---- the case this exists for: B's own beacon, one scan, and A reaches it.
    let moved_rows = discovery::observe_beacons(
        &peers,
        std::slice::from_ref(&beacon),
        &[discovery::InstanceBinding {
            instance_id: b_instance,
            node: b,
        }],
        5_000,
    )
    .expect("the scan records");
    assert_eq!(moved_rows, 1, "exactly B's row moved");

    let refreshed = teamclaude_rs::peer::config::read_or_default(&peers)
        .expect("the peers file reads")
        .peers
        .remove(0);
    assert_eq!(
        refreshed.added_at, stale.added_at,
        "and without re-pinning: the row is the same row, so every grant on it survived"
    );
    let learned = refreshed
        .endpoints
        .iter()
        .find(|endpoint| endpoint.direct_addr() == Some(moved_addr))
        .expect("the announced address is on the row");
    assert_eq!(
        learned.source,
        teamclaude_rs::peer::config::EndpointSource::Beacon,
        "recorded as what taught it, so a reader can tell an unauthenticated announcement \
         from an address a handshake proved"
    );
    assert_eq!(
        learned.observed_at_ms, 5_000,
        "timed by this node's clock and never by anything in the announcement"
    );
    assert!(
        teamclaude_rs::peer::serve::dial_peer(&refreshed)
            .await
            .is_some(),
        "A dials the address B announced and gets a connection: {:?}",
        refreshed.endpoints
    );
    // The dead port is still remembered, behind the fresh one: an endpoint list
    // is a history, and the newest is tried first.
    assert!(
        refreshed
            .endpoints
            .iter()
            .any(|endpoint| endpoint.direct_addr() == Some(dead)),
        "the old endpoint is kept as history rather than replaced"
    );
    assert_eq!(
        refreshed.endpoints.first().map(|e| e.direct_addr()),
        Some(Some(moved_addr)),
        "and the newest observation leads the dial order"
    );
    drop(moved);
}

// ---------------------------------------------------------------------------
// Decision row 16: a brief refreshes a mutual friend and never introduces one
// ---------------------------------------------------------------------------

/// A share brief naming `node` at `addr`, the whole of what
/// [`discovery::neighbor_briefs`] is allowed to put in one.
fn brief(node: PeerId, addr: std::net::SocketAddr) -> tcr_peer_wire::NeighborBrief {
    tcr_peer_wire::NeighborBrief {
        node,
        caps: tcr_peer_wire::Caps::default(),
        addrs: vec![addr.to_string()],
    }
}

/// One pinned row whose only endpoint is `addr` and which the operator swaps
/// neighbour lists with, so its briefs are applied.
fn briefing_row(node: PeerId, addr: std::net::SocketAddr) -> teamclaude_rs::peer::config::PeerRow {
    let mut row = pinned_row(node, addr);
    row.allow.control.briefs = true;
    row
}

/// **A trusts B and C; B also trusts C; C moves; A's row for C gains C's new
/// locator through B's brief.**
///
/// This is the case a brief exists for: no session can carry the new address
/// to A directly, because a session needs an address to dial and every one A
/// holds for C is dead. B's `Hello` is the only fact left, and it is safe to
/// believe because A already trusts C: the handshake re-proves the key when a
/// dial against the new address lands.
///
/// Watched red: with the `pinned.iter().any(...)` guard's true branch left in
/// but the loop body that pushes an endpoint deleted from
/// `neighbor_brief_endpoints`, this fails at the last assertion because A's
/// row for C never gains the new locator. Confirmed by hand while writing
/// this test: with that line removed, `observe_neighbor_briefs` returns `0`
/// and the assertion on `refreshed.endpoints` fails to find the moved
/// address; restored, it returns `1` and the address is present.
#[tokio::test]
async fn a_brief_from_a_mutual_friend_refreshes_that_friends_locator() {
    let dir = scratch("brief-mutual-friend");
    let peers = dir.join("tcr-peers.json");

    let b = PeerId([0x11; 32]);
    let c = PeerId([0x22; 32]);
    let old_addr: std::net::SocketAddr = "192.0.2.7:9600".parse().expect("a literal address");
    let new_addr: std::net::SocketAddr = "192.0.2.7:9601".parse().expect("a literal address");

    let file = teamclaude_rs::peer::config::PeerFile {
        peers: vec![briefing_row(b, old_addr), pinned_row(c, old_addr)],
        ..Default::default()
    };
    teamclaude_rs::peer::config::save(&peers, &file).expect("write the peers file");

    let briefs = vec![brief(c, new_addr)];
    let moved = discovery::observe_neighbor_briefs(&peers, &b, &briefs, 5_000).expect("records");
    assert_eq!(moved, 1, "exactly C's row moved, from B's brief about it");

    let refreshed = teamclaude_rs::peer::config::read_or_default(&peers)
        .expect("the peers file reads")
        .peers
        .into_iter()
        .find(|row| row.node == c)
        .expect("C is still a pinned row, not re-created");
    let learned = refreshed
        .endpoints
        .iter()
        .find(|endpoint| endpoint.direct_addr() == Some(new_addr))
        .expect("C's new address, learned through B's brief, is on the row");
    assert_eq!(
        learned.source,
        teamclaude_rs::peer::config::EndpointSource::Brief,
        "recorded as what taught it: a trusted peer's word, not a session A itself completed"
    );
}

/// **A trusts B only; B also trusts D; A's trusted rows and A's peers file
/// never mention D.** The positive control in the same test: C's locator,
/// carried in the same batch of briefs, DOES appear in those same file bytes,
/// so the absence of D is the filter working and not an `observe_neighbor_briefs`
/// call that silently writes nothing at all.
///
/// Watched red: with the `pinned.iter().any(...)` guard removed from
/// `neighbor_brief_endpoints`, D's key appears in the saved file's raw bytes
/// and the first assertion fails.
#[tokio::test]
async fn a_brief_never_introduces_a_key_a_did_not_already_trust() {
    let dir = scratch("brief-no-introduction");
    let peers = dir.join("tcr-peers.json");

    let b = PeerId([0x33; 32]);
    let c = PeerId([0x44; 32]);
    let d = PeerId([0x55; 32]); // B trusts D; A never has.
    let b_addr: std::net::SocketAddr = "192.0.2.10:9600".parse().expect("a literal address");
    let c_addr: std::net::SocketAddr = "192.0.2.11:9601".parse().expect("a literal address");
    let c_moved: std::net::SocketAddr = "192.0.2.11:9611".parse().expect("a literal address");
    let d_addr: std::net::SocketAddr = "192.0.2.12:9602".parse().expect("a literal address");

    // A pins B and C; D was never pinned.
    let file = teamclaude_rs::peer::config::PeerFile {
        peers: vec![briefing_row(b, b_addr), pinned_row(c, c_addr)],
        ..Default::default()
    };
    teamclaude_rs::peer::config::save(&peers, &file).expect("write the peers file");

    // B's Hello carries a brief about C (a mutual friend) and one about D (a
    // stranger to A). Both arrive in the same call, so the filter has to tell
    // them apart rather than merely reject the whole batch.
    // C's entry names an address C's row does NOT already hold: a brief that
    // repeated a locator the row already has from a completed handshake is
    // refused on purpose now, so repeating `c_addr` here would make the
    // positive control below true for the wrong reason.
    let briefs = vec![brief(c, c_moved), brief(d, d_addr)];
    let moved = discovery::observe_neighbor_briefs(&peers, &b, &briefs, 5_000).expect("records");
    assert_eq!(
        moved, 1,
        "only C's row moved; D was never pinned so it never becomes one"
    );

    let raw = std::fs::read_to_string(&peers).expect("read the peers file's raw bytes");
    assert!(
        !raw.contains(&d.to_wire()) && !raw.contains(&d.display()),
        "D's key must not appear in the peers file in any encoding: {raw}"
    );
    assert!(
        !raw.contains(&d_addr.to_string()),
        "D's address must not appear in the peers file either: {raw}"
    );

    // Positive control: C's moved address, carried in the same batch, is in
    // the same file. Without this, an `observe_neighbor_briefs` that wrote
    // nothing at all would pass the two refusals above for the wrong reason.
    assert!(
        raw.contains(&c_moved.to_string()),
        "positive control: C's locator must be in the file, or the absences above prove \
         nothing about the filter: {raw}"
    );

    let rows = teamclaude_rs::peer::config::read_or_default(&peers)
        .expect("the peers file reads")
        .peers;
    assert_eq!(
        rows.len(),
        2,
        "still exactly A's two original pins: B and C, never D"
    );
    assert!(
        rows.iter().all(|row| row.node != d),
        "D must never appear as a trusted row"
    );
}

/// **A brief carrying five hundred addresses costs one file write and leaves
/// C's proven endpoint at the front of the dial order.**
///
/// A failing input, verbatim: a pinned peer sends one `Hello` whose
/// single brief names five hundred addresses for a mutual friend. Three
/// things went wrong at once before this change and each is asserted
/// separately here, because fixing any two of them still leaves a usable
/// attack:
///
/// 1. every address ran its own locked read-modify-write of the operator's
///    peers file, so one frame bought five hundred rewrites of the file every
///    `tcr peer` mutation contends for. Counted with
///    [`teamclaude_rs::peer::config::peers_file_opens`], the counter the
///    accepted-connection test already uses: one read to decide, one inside
///    the writer, and nothing per address.
/// 2. nothing capped a brief's address list, so the eight endpoint slots on
///    C's row filled with addresses a peer chose.
/// 3. a brief endpoint carries this node's clock and is therefore always the
///    newest, so it evicted the address the pairing itself proved.
///
/// Watched red, one mutation at a time, each restored from a byte copy:
/// deleting the `take(MAX_BRIEF_ADDRS)` in `neighbor_brief_endpoints` fails
/// the parsed-length assertion at 500 and NOTHING else, which is why that
/// assertion exists; replacing the `free_slots`/`briefs_held` guard in
/// `admissible_brief_endpoints` with `if false` fails the "two brief slots"
/// assertion at 4.
#[tokio::test]
async fn five_hundred_brief_addresses_write_once_and_never_outrank_the_paired_endpoint() {
    let dir = scratch("brief-flood");
    let peers = dir.join("tcr-peers.json");

    let b = PeerId([0x66; 32]);
    let c = PeerId([0x77; 32]);
    let b_addr: std::net::SocketAddr = "192.0.2.20:9600".parse().expect("a literal address");
    let c_paired: std::net::SocketAddr = "192.0.2.21:9601".parse().expect("a literal address");

    let file = teamclaude_rs::peer::config::PeerFile {
        peers: vec![briefing_row(b, b_addr), pinned_row(c, c_paired)],
        ..Default::default()
    };
    teamclaude_rs::peer::config::save(&peers, &file).expect("write the peers file");

    let flood = tcr_peer_wire::NeighborBrief {
        node: c,
        caps: tcr_peer_wire::Caps::default(),
        addrs: (0..500)
            .map(|index| format!("198.51.100.9:{}", 10_000 + index))
            .collect(),
    };

    // The address cap, asserted where it acts rather than through the write:
    // the row's slot rules below would hold the file to two endpoints even if
    // all five hundred addresses were parsed and carried, so the write count
    // and the endpoint count say nothing about this cap. Measured by hand:
    // with `take(MAX_BRIEF_ADDRS)` deleted, every assertion further down still
    // passes and this one reads 500.
    let pinned = teamclaude_rs::peer::config::read_or_default(&peers)
        .expect("the peers file reads")
        .peers;
    let parsed = discovery::neighbor_brief_endpoints(std::slice::from_ref(&flood), &pinned, 5_000);
    assert_eq!(
        parsed.len(),
        teamclaude_rs::peer::discovery::MAX_BRIEF_ADDRS,
        "one brief may teach four addresses, and the other 496 are never parsed, held or \
         carried; this batch produced {} endpoints",
        parsed.len()
    );

    let before = teamclaude_rs::peer::config::peers_file_opens(&peers);
    let moved = discovery::observe_neighbor_briefs(&peers, &b, &[flood], 5_000).expect("records");
    let opened = teamclaude_rs::peer::config::peers_file_opens(&peers) - before;

    assert_eq!(moved, 1, "one node was named, so one row moved");
    assert_eq!(
        opened, 2,
        "five hundred addresses opened the peers file {opened} times; it is one read to \
         decide what is admissible and one inside the single write, per node and never \
         per address"
    );

    let row = teamclaude_rs::peer::config::read_or_default(&peers)
        .expect("the peers file reads")
        .peers
        .into_iter()
        .find(|row| row.node == c)
        .expect("C is still pinned");
    assert!(
        row.endpoints.len() <= teamclaude_rs::peer::config::MAX_ENDPOINTS_PER_PEER,
        "the row keeps its cap: {:?}",
        row.endpoints
    );
    let from_brief = row
        .endpoints
        .iter()
        .filter(|endpoint| endpoint.source == teamclaude_rs::peer::config::EndpointSource::Brief)
        .count();
    assert_eq!(
        from_brief,
        teamclaude_rs::peer::discovery::MAX_BRIEF_ENDPOINTS_PER_PEER,
        "at most two of the eight slots are a peer's word about a third machine, so six \
         stay with the endpoints this node proved: {:?}",
        row.endpoints
    );
    assert!(
        row.endpoints
            .iter()
            .any(|endpoint| endpoint.direct_addr() == Some(c_paired)),
        "the address the pairing proved is still on the row: {:?}",
        row.endpoints
    );

    let order = teamclaude_rs::peer::probe::order_endpoints(
        &row,
        &teamclaude_rs::peer::probe::PathTable::default(),
    );
    assert_eq!(
        order
            .first()
            .and_then(teamclaude_rs::peer::config::Endpoint::direct_addr),
        Some(c_paired),
        "C's paired endpoint is dialled first, ahead of anything a brief added: {order:?}"
    );
}

/// **A pinned peer that was never granted `briefs` changes nothing here.**
///
/// `allow.control.briefs` is the operator's answer to "do this Mac and I swap
/// neighbour lists", and it used to be read in one direction only: consulted
/// when this node BUILT a `Hello` and ignored when one arrived. So a peer
/// pinned with no grants at all could still write endpoints onto this node's
/// rows.
///
/// Asserted on the file's bytes rather than on the return value alone,
/// because a count of moved rows is this function's own word for what it did.
/// The positive control is the test above: the same call with the grant set
/// writes, so a zero here is the grant and not a call that never works.
///
/// Watched red: with the `sender_row.allow.control.briefs` check deleted from
/// `observe_neighbor_briefs`, `moved` is 1 and the file's bytes gain the
/// injected address.
#[tokio::test]
async fn a_sender_without_the_briefs_grant_writes_nothing() {
    let dir = scratch("brief-not-granted");
    let peers = dir.join("tcr-peers.json");

    let b = PeerId([0x88; 32]);
    let c = PeerId([0x99; 32]);
    let b_addr: std::net::SocketAddr = "192.0.2.30:9600".parse().expect("a literal address");
    let c_paired: std::net::SocketAddr = "192.0.2.31:9601".parse().expect("a literal address");
    let injected: std::net::SocketAddr = "198.51.100.40:9999".parse().expect("a literal address");

    // B is pinned and has no grants: `pinned_row` leaves `Allow::default()`.
    let file = teamclaude_rs::peer::config::PeerFile {
        peers: vec![pinned_row(b, b_addr), pinned_row(c, c_paired)],
        ..Default::default()
    };
    teamclaude_rs::peer::config::save(&peers, &file).expect("write the peers file");
    let before = std::fs::read_to_string(&peers).expect("the peers file's bytes");

    let briefs = vec![brief(c, injected)];
    let moved = discovery::observe_neighbor_briefs(&peers, &b, &briefs, 5_000).expect("records");

    assert_eq!(moved, 0, "a peer without the grant moves no row");
    let after = std::fs::read_to_string(&peers).expect("the peers file's bytes");
    assert_eq!(
        before, after,
        "the peers file is byte-identical: an ungranted brief is not a smaller write, it \
         is no write"
    );
    assert!(
        !after.contains(&injected.to_string()),
        "the address B tried to teach must not be on any row: {after}"
    );
}

// ---------------------------------------------------------------------------
// The keyed tag is only good for the minute it was stamped in
// ---------------------------------------------------------------------------

/// **A beacon is re-stamped every minute it keeps announcing.**
///
/// The keyed tag is `HMAC(network_key, instance ‖ port ‖ minute)` and a
/// receiver holding the key accepts the current minute and the one before
/// (`crate::peer::config`'s window). The tag used to be computed once, at
/// registration, so a node that turned `find` on kept announcing a payload
/// that stopped verifying about two minutes later: it went undiscoverable to
/// exactly the neighbours that share its key, while every log line said it was
/// announcing.
///
/// Watch it fail: make `announce_step` answer `Idle` where it answers
/// `Restamp` and the second row below goes red.
#[test]
fn a_beacon_is_restamped_every_minute_it_keeps_announcing() {
    use discovery::AnnounceStep;

    // One minute, in the unix seconds `announce_step` reads.
    let minute: i64 = 27_000;
    let now = minute * 60;

    assert_eq!(
        discovery::announce_step(true, None, now),
        AnnounceStep::Start,
        "`find` on and nothing announcing yet is a start"
    );
    assert_eq!(
        discovery::announce_step(true, Some(minute - 1), now),
        AnnounceStep::Restamp,
        "a beacon stamped in the previous minute has to be re-registered, or its tag stops \
         verifying while it is still being announced"
    );
    assert_eq!(
        discovery::announce_step(true, Some(minute), now + 59),
        AnnounceStep::Idle,
        "and inside one minute there is nothing to do: a re-registration per wake would be \
         mDNS traffic with no reader"
    );
    assert_eq!(
        discovery::announce_step(false, Some(minute), now),
        AnnounceStep::Stop,
        "`find` off with a beacon held is what withdraws it, which is how `tcr peer find off` \
         reaches a running server at all"
    );
    assert_eq!(
        discovery::announce_step(false, None, now),
        AnnounceStep::Idle,
        "and `find` off with nothing announcing is not a second stop every wake"
    );

    // The wake interval has to be inside the window the tag is good for, or
    // the decision above is right and the loop still misses it.
    assert!(
        discovery::BEACON_RESTAMP_INTERVAL < std::time::Duration::from_secs(60),
        "an announcer that wakes less often than once a minute can stamp a tag that is \
         already two minutes old"
    );

    // And the tag really does change with the minute, or nothing above matters.
    let key = NetworkKey::from_bytes([9_u8; 32]);
    let stamped_now = discovery::beacon_txt(&test_instance(), 9_600, None, Some(&key), now);
    let stamped_later = discovery::beacon_txt(&test_instance(), 9_600, None, Some(&key), now + 60);
    let tag = |txt: &Vec<(String, String)>| {
        txt.iter()
            .find(|(k, _)| k == discovery::TXT_TAG_KEY)
            .map(|(_, v)| v.clone())
            .expect("a keyed beacon carries a tag")
    };
    assert_ne!(
        tag(&stamped_now),
        tag(&stamped_later),
        "the tag is minute-keyed, which is why a beacon has to be re-stamped at all"
    );
}

/// **What a flood can actually make this node hold.**
///
/// The found list's doc named three caps, and a reader could take them for one
/// total bound of twelve rows. Only two of the three bound MEMORY:
/// `MAX_FOUND_ROWS` is where the display stops, every row is kept, and what a
/// flooder can hold is `MAX_FOUND_PER_ADDRESS` rows per distinct source address
/// inside one `FOUND_TTL_MS` window. This is that number, measured, so the
/// sentence in the doc is a claim somebody checked.
///
/// Watch it fail by truncating `FoundList::observe` to `MAX_FOUND_ROWS`, which
/// is the total cap the old wording implied: the held count drops to twelve.
#[test]
fn a_flood_holds_two_rows_per_address_and_nothing_survives_the_ttl() {
    let addresses = 30_u8;
    let mut found = FoundList::new();

    // Three announcements per address, each under its own instance id: the
    // third is the one the per-address cap refuses.
    let mut scan = Vec::new();
    for host in 0..addresses {
        for nth in 0..3_u8 {
            scan.push(row(host * 3 + nth, &format!("192.0.2.{host}")));
        }
    }
    found.observe(scan, 1_000);

    assert_eq!(
        found.len(),
        usize::from(addresses) * discovery::MAX_FOUND_PER_ADDRESS,
        "the memory a flood costs is two rows per distinct source address, not a total of {}",
        discovery::MAX_FOUND_ROWS
    );

    // And the other half of the real bound: the window.
    found.observe(Vec::new(), 1_000 + discovery::FOUND_TTL_MS);
    assert_eq!(
        found.len(),
        0,
        "a flooder that stops announcing holds nothing a minute later"
    );
}
