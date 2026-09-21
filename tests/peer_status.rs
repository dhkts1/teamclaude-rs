//! The peers block on the status payload: the panel's vocabulary, the two
//! files it is derived from, and the fields that must stay honestly absent.
//!
//! # Why this suite exists
//!
//! The Peers tab decodes sixteen fields and, before this block, thirteen of
//! them had no Rust writer anywhere in the tree. Every Swift field is
//! `decodeIfPresent`, so the tab rendered an empty row and both suites stayed
//! green, the failure a passing test cannot see. So the assertions below are
//! about the JSON BYTES and their key names, not about a Rust struct: the seam
//! this guards is a cross-language one where the keys cross as strings.
//!
//! Nothing here opens a socket, spawns a process or reads the operator's own
//! config directory: every fixture is a temp-file peers file and peer-state
//! file with obviously-fake values, and the derivation under test is pure.

use std::path::Path;

use tcr_peer_wire::{Lease, LeaseUnit, PeerId, Window};
use teamclaude_rs::peer::config::{PeerFile, PeerRow, PeerStore};
use teamclaude_rs::peer::state::{BorrowedRow, LeaseRow, PeerState};
use teamclaude_rs::status::{peer_status_rows, PathKind, PeerStatusRow};

/// Five minutes of milliseconds, the unit almost every fixture below counts in.
const FIVE_MINUTES_MS: i64 = 5 * 60 * 1_000;

fn node(byte: u8) -> PeerId {
    PeerId([byte; 32])
}

/// A pinned row with the two endpoints and grants a caller wants, written
/// through the real writer and read back through the real reader, so a shape
/// this test believes is a shape `tcr` itself can load.
fn write_peers(path: &Path, rows: Vec<PeerRow>) -> Vec<PeerRow> {
    let file = PeerFile {
        peers: rows,
        ..PeerFile::default()
    };
    teamclaude_rs::peer::config::save(path, &file).expect("the peers file writes");
    let store = PeerStore::open(path).expect("the peers file opens");
    store.peers()
}

/// `addrs` are still written as strings, which is what every caller here has:
/// one literal socket address, or none. The row's own field is
/// `Vec<Endpoint>`, so the conversion is here and not at twenty call sites,
/// each address becomes a `Direct` locator observed at the row's own
/// `added_at`, with source `Paired`, which is what a pin writes.
fn row(byte: u8, label: &str, addrs: &[&str]) -> PeerRow {
    use teamclaude_rs::peer::config::{Endpoint, EndpointSource};

    PeerRow {
        node: node(byte),
        label: label.to_string(),
        endpoints: addrs
            .iter()
            .map(|addr| {
                let parsed = addr
                    .parse()
                    .unwrap_or_else(|err| panic!("{addr:?} is not a socket address: {err}"));
                Endpoint::direct(parsed, 1, EndpointSource::Paired)
            })
            .collect(),
        added_at: 1,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: Default::default(),
        lend: Vec::new(),
    }
}

/// A lease measured in tokens, as the lender's ledger holds it.
fn tokens_lease(amount: u64, spent: f64, granted_at_ms: i64, expires_at_ms: i64) -> Lease {
    Lease {
        lease_id: 0x2a,
        window: Window::SevenDay,
        unit: LeaseUnit::Tokens(amount),
        granted_at_ms,
        expires_at_ms,
        spent,
        max_inflight: 2,
        until: None,
    }
}

fn one_row(rows: &[PeerStatusRow]) -> &PeerStatusRow {
    assert_eq!(rows.len(), 1, "one pinned row in, one row out");
    &rows[0]
}

/// The whole point of the block: the keys the panel decodes are the keys the
/// payload emits, and the fields nothing measures yet are ABSENT rather than
/// zero.
///
/// Watch it fail by renaming `last_seen_ms` back to the file's own vocabulary
/// (`lastSeen`, or dropping the `rename_all` on `PeerStatusRow`): the
/// `lastSeenMs` assertion goes red with `key not found`, which is exactly the
/// silent mismatch that rendered an empty row for three waves.
#[test]
fn the_row_speaks_the_panels_vocabulary_and_omits_what_it_cannot_measure() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut pinned = row(7, "studio-mac", &["127.0.0.1:7749"]);
    pinned.allow.carry = true;
    pinned.allow.allow_disclose = true;
    pinned.allow.gateway = true;
    let rows = write_peers(&dir.path().join("tcr-peers.json"), vec![pinned]);

    let now_ms = 1_700_000_000_000;
    let state = PeerState {
        last_seen: vec![(node(7), now_ms - FIVE_MINUTES_MS)],
        ..PeerState::default()
    };
    let derived = peer_status_rows(&rows, &state, now_ms);
    let wire = serde_json::to_value(one_row(&derived)).expect("the row serializes");
    let object = wire.as_object().expect("a row is a JSON object");

    // The three keys the two vocabularies did not overlap on at all.
    //
    // `id` is the WIRE form and `display` the short one. This assertion used to
    // require `tcr-…` on `id`, and that is the shape it was written to pin: the
    // panel merges this read with `tcr peer ls --json` on `id`, that read
    // carries the wire form, and the two halves of one Mac never matched.
    assert_eq!(
        object.get("id").and_then(|v| v.as_str()),
        Some(node(7).to_wire().as_str()),
        "the panel joins on `id`, so it is the wire form `peer ls --json` writes: {wire}"
    );
    assert!(
        object
            .get("display")
            .and_then(|v| v.as_str())
            .is_some_and(|short| short.starts_with("tcr-")),
        "the short form a person reads is its own key: {wire}"
    );
    assert_eq!(
        object.get("name").and_then(|v| v.as_str()),
        Some("studio-mac"),
        "the panel decodes `name`, never the file's `label`: {wire}"
    );
    assert_eq!(
        object.get("address").and_then(|v| v.as_str()),
        Some("127.0.0.1:7749"),
        "the panel decodes one `address`, never the file's `addrs` list: {wire}"
    );
    assert_eq!(
        object.get("lastSeenMs").and_then(|v| v.as_i64()),
        Some(now_ms - FIVE_MINUTES_MS),
        "freshness, in the panel's key: {wire}"
    );
    assert_eq!(object.get("trusted").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(object.get("carries").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(object.get("serves").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(
        object.get("byteCapPerHour").and_then(|v| v.as_u64()),
        Some(teamclaude_rs::peer::tunnel::DEFAULT_MAX_TUNNEL_BYTES_PER_HOUR),
        "a row this Mac carries for names the ceiling it is measured against: {wire}"
    );

    // ABSENT, not zero. A `0` here reads as a measurement and there is no
    // measurement: in-flight lives in the running ledger, the byte counter in
    // the running budget, and the per-path round trip has no writer until the
    // prober lands.
    for absent in ["inFlight", "bytesPerHour", "noHeadroom"] {
        assert!(
            !object.contains_key(absent),
            "`{absent}` has no writer on this path and must be absent, not zero: {wire}"
        );
    }

    let paths = object
        .get("paths")
        .and_then(|v| v.as_array())
        .expect("one endpoint is one path");
    assert_eq!(paths.len(), 1, "one endpoint, one path: {wire}");
    let path = paths[0].as_object().expect("a path is an object");
    assert_eq!(
        path.get("endpoint").and_then(|v| v.as_str()),
        Some("127.0.0.1:7749")
    );
    assert_eq!(path.get("kind").and_then(|v| v.as_str()), Some("direct"));
    for absent in ["rttMs", "lossPct", "bytesPerHour", "tokensPerHour"] {
        assert!(
            !path.contains_key(absent),
            "`{absent}` on a path is measured by the prober, not by a file: {wire}"
        );
    }
}

/// A label that is an email, a uuid or an org name never reaches a screen, a
/// screenshot or a bug report through this payload. This repository is public
/// and the peers file is hand-editable JSON.
#[test]
fn a_label_that_is_not_a_label_comes_out_masked() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let rows = write_peers(
        &dir.path().join("tcr-peers.json"),
        vec![
            row(1, "alice@example.com", &[]),
            row(2, "11111111-1111-1111-1111-111111111111", &[]),
            row(3, "studio-mac", &[]),
        ],
    );
    let derived = peer_status_rows(&rows, &PeerState::default(), 1_700_000_000_000);
    let names: Vec<&str> = derived.iter().map(|row| row.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["[masked]", "[masked]", "studio-mac"],
        "two masked, and the plain label untouched, a mask that swallows \
         everything is the failure a lone masking assertion cannot see"
    );
}

/// The item's own gate: a peers file with one lent lease reports a token rate,
/// and the number comes off the ledger rather than out of the air.
///
/// One million tokens granted, half of them spent, two hours ago: 250 000 per
/// hour. Watch it fail by dropping the `spent` factor in
/// `status::tokens_per_hour`, the rate then reads 500 000, twice the traffic
/// that ever happened.
#[test]
fn a_lent_lease_in_tokens_reports_a_rate_from_the_ledger() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let rows = write_peers(
        &dir.path().join("tcr-peers.json"),
        vec![row(9, "attic-nuc", &["127.0.0.1:7750"])],
    );
    let now_ms = 1_700_000_000_000;
    let two_hours = 2 * 60 * 60 * 1_000;
    let state = PeerState {
        leases: vec![LeaseRow {
            lease: tokens_lease(1_000_000, 0.5, now_ms - two_hours, now_ms + two_hours),
            peer: node(9),
            scope: tcr_peer_wire::LendScope::All,
        }],
        ..PeerState::default()
    };

    let derived = peer_status_rows(&rows, &state, now_ms);
    let row = one_row(&derived);
    assert_eq!(
        row.tokens_per_hour,
        Some(250_000),
        "1 000 000 granted x 0.5 spent over 2 h: {row:?}"
    );
    assert_eq!(
        row.lease_spent,
        Some(0.5),
        "the meter reads the ledger's own fraction: {row:?}"
    );
    assert_eq!(
        row.lease_ttl_seconds,
        Some(two_hours / 1_000),
        "seconds left before the borrower asks again: {row:?}"
    );
}

/// The negative control for the rate, and the reason it is an `Option`: a lease
/// measured as a utilization FRACTION carries no token count anywhere in this
/// tree, and a `0` there would read as "this Mac drew nothing".
#[test]
fn a_fraction_lease_reports_no_token_rate_rather_than_zero() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let rows = write_peers(
        &dir.path().join("tcr-peers.json"),
        vec![row(9, "attic-nuc", &[])],
    );
    let now_ms = 1_700_000_000_000;
    let mut lease = tokens_lease(1_000_000, 0.5, now_ms - 3_600_000, now_ms + 3_600_000);
    lease.unit = LeaseUnit::Fraction(0.2);
    let state = PeerState {
        leases: vec![LeaseRow {
            lease,
            peer: node(9),
            scope: tcr_peer_wire::LendScope::All,
        }],
        ..PeerState::default()
    };

    let derived = peer_status_rows(&rows, &state, now_ms);
    assert_eq!(
        one_row(&derived).tokens_per_hour,
        None,
        "no token count exists for a fraction lease, so no rate is reported"
    );
}

/// The second reason the rate is an `Option`, and the one that was wrong: a
/// token lease granted a moment ago has not run long enough to BE a rate.
///
/// The doc on `status::tokens_per_hour` promises `None` "before the first
/// window", and the code answered `Some(0)`: it marked the row measured before
/// it checked whether enough time had passed, so a lease minted in the last
/// second reported that the borrower had drawn nothing. On a panel that is not
/// an absence, it is a claim, the same false zero this whole block exists to
/// end.
///
/// Watch it fail by setting `measured = true` before the elapsed check: the
/// first assertion then reads `Some(0)`.
#[test]
fn a_token_lease_younger_than_its_first_window_reports_no_rate_yet() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let rows = write_peers(
        &dir.path().join("tcr-peers.json"),
        vec![row(11, "just-lent", &[])],
    );
    let now_ms = 1_700_000_000_000;
    // Granted 400 ms ago and half spent: a real draw, over a window too short
    // to divide by.
    let lease = tokens_lease(1_000_000, 0.5, now_ms - 400, now_ms + 3_600_000);
    let state = PeerState {
        leases: vec![LeaseRow {
            lease,
            peer: node(11),
            scope: tcr_peer_wire::LendScope::All,
        }],
        ..PeerState::default()
    };

    let derived = peer_status_rows(&rows, &state, now_ms);
    assert_eq!(
        one_row(&derived).tokens_per_hour,
        None,
        "a lease this young is not a rate yet, and `Some(0)` would read as `drew nothing`"
    );
    let wire = serde_json::to_value(one_row(&derived)).expect("the row serializes");
    assert!(
        !wire
            .as_object()
            .expect("a row is an object")
            .contains_key("tokensPerHour"),
        "and it is ABSENT on the wire, not a zero the panel would draw: {wire}"
    );

    // One second later the same lease IS a rate, so the `None` above is
    // "not yet" and never "never".
    let later = peer_status_rows(&rows, &state, now_ms + 1_000);
    assert!(
        later[0].tokens_per_hour.is_some_and(|rate| rate > 0),
        "once the window has passed the same lease reports a real rate: {:?}",
        later[0].tokens_per_hour
    );
}

/// One timestamp cannot name which of two paths answered.
///
/// `peer-state.json` records one `last_seen` per PEER. With one endpoint that
/// is the only path it could have arrived over; with two it is unattributable,
/// and claiming the same success on both would draw two live paths where one
/// may be dead.
#[test]
fn a_single_endpoint_carries_the_last_success_and_two_do_not() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let rows = write_peers(
        &dir.path().join("tcr-peers.json"),
        vec![
            row(4, "one-way", &["127.0.0.1:7751"]),
            row(5, "two-ways", &["127.0.0.1:7752", "127.0.0.1:7753"]),
        ],
    );
    let now_ms = 1_700_000_000_000;
    let seen = now_ms - FIVE_MINUTES_MS;
    let state = PeerState {
        last_seen: vec![(node(4), seen), (node(5), seen)],
        ..PeerState::default()
    };

    let derived = peer_status_rows(&rows, &state, now_ms);
    assert_eq!(derived[0].paths.len(), 1);
    assert_eq!(
        derived[0].paths[0].last_ok_ms,
        Some(seen),
        "one endpoint: the only path the peer could have answered over"
    );
    assert_eq!(derived[1].paths.len(), 2, "both endpoints are drawn");
    assert!(
        derived[1]
            .paths
            .iter()
            .all(|path| path.last_ok_ms.is_none()),
        "two endpoints and one timestamp: neither path may claim it, {:?}",
        derived[1].paths
    );
    // The peer-level freshness is still reported on both rows, the absence
    // above is about ATTRIBUTION, not about losing the fact.
    assert_eq!(derived[1].last_seen_ms, Some(seen));
    assert!(derived
        .iter()
        .all(|row| row.paths.iter().all(|path| path.kind == PathKind::Direct)));
}

/// The row-level end, over a lease this Mac BORROWS: a row whose
/// borrowed lease is still running is not an ended row, and the `until` it
/// draws is that lease's own end.
#[test]
fn a_borrowed_lease_still_running_is_not_an_ended_row() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let rows = write_peers(
        &dir.path().join("tcr-peers.json"),
        vec![row(6, "studio-mac", &[])],
    );
    let now_ms = 1_700_000_000_000;
    let now_s = u64::try_from(now_ms / 1_000).expect("a positive clock");
    let mut lease = tokens_lease(10, 0.0, now_ms - FIVE_MINUTES_MS, now_ms + FIVE_MINUTES_MS);
    lease.until = Some(now_s + 300);
    let state = PeerState {
        borrowed: vec![BorrowedRow {
            lease,
            lender: node(6),
        }],
        ..PeerState::default()
    };

    let derived = peer_status_rows(&rows, &state, now_ms);
    let row = one_row(&derived);
    assert!(!row.ended, "the lease it borrows is still running: {row:?}");
    assert_eq!(
        row.until,
        Some(now_s + 300),
        "the row counts down to the borrowed lease's own end: {row:?}"
    );
}

/// A row with no lease at all names no end, and is not ended.
///
/// The pair matters: `ended: true` greys a Mac out in the panel, and a bare pin
/// that has never lent or borrowed anything must not be drawn as finished.
#[test]
fn a_row_with_no_lease_names_no_end_and_is_not_ended() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let rows = write_peers(
        &dir.path().join("tcr-peers.json"),
        vec![row(8, "fresh-pin", &["127.0.0.1:7754"])],
    );
    let derived = peer_status_rows(&rows, &PeerState::default(), 1_700_000_000_000);
    let row = one_row(&derived);
    assert_eq!(row.until, None);
    assert!(!row.ended);
    assert_eq!(
        row.lease_spent, None,
        "no lease is not a spent meter at zero"
    );
}

/// The skew rule this block is allowed to rely on, in both directions, with no
/// `STATUS_KIND` bump.
///
/// An OLD server's payload has no `peers` key at all and a NEW client must read
/// an empty list, the truth, rather than fail the parse and drop to the
/// all-zeros offline snapshot. Watch it fail by removing `#[serde(default)]`
/// from `StatusPayload::peers`: the deserialize then errors with `missing field
/// peers`, which is the fabricated-healthy-fleet failure the endpoint exists to
/// end.
#[test]
fn a_payload_from_a_server_with_no_peers_block_still_parses() {
    let old = serde_json::json!({
        "kind": teamclaude_rs::status::STATUS_KIND,
        "accounts": [],
    });
    let payload: teamclaude_rs::status::StatusPayload =
        serde_json::from_value(old).expect("an old server's payload still parses");
    assert!(
        payload.peers.is_empty(),
        "absent reads as an empty mesh, never as a fabricated one"
    );
    assert_eq!(
        payload.kind,
        teamclaude_rs::status::STATUS_KIND,
        "and the block must NOT have bumped the kind: an old client rejecting \
         the payload would render the structural zeros this endpoint ended"
    );
}

/// The peers block round-trips through the wire unchanged, keys and all.
#[test]
fn the_peers_block_round_trips_through_the_status_payload() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut pinned = row(3, "attic-nuc", &["127.0.0.1:7749", "127.0.0.1:7750"]);
    pinned.allow.carry = true;
    let rows = write_peers(&dir.path().join("tcr-peers.json"), vec![pinned]);
    let now_ms = 1_700_000_000_000;
    let state = PeerState {
        last_seen: vec![(node(3), now_ms - FIVE_MINUTES_MS)],
        ..PeerState::default()
    };

    let derived = peer_status_rows(&rows, &state, now_ms);
    let payload = serde_json::json!({
        "kind": teamclaude_rs::status::STATUS_KIND,
        "accounts": [],
        "peers": derived,
    });
    let text = serde_json::to_string(&payload).expect("the payload serializes");
    let back: teamclaude_rs::status::StatusPayload =
        serde_json::from_str(&text).expect("and parses back");
    assert_eq!(back.peers.len(), 1);
    assert_eq!(
        serde_json::to_value(&back.peers).expect("re-serializes"),
        serde_json::to_value(&derived).expect("re-serializes"),
        "byte-for-byte the same block after a full round trip"
    );

    // The no-secret invariant, asserted on the BYTES rather than on a struct,
    // the same way `status_endpoint_leaks_no_secrets` does: no credential
    // material has any business in a peers block.
    for forbidden in [
        "access_token",
        "refresh_token",
        "authorization",
        "sk-ant",
        "joinKey",
    ] {
        assert!(
            !text.to_lowercase().contains(&forbidden.to_lowercase()),
            "the peers block must carry no credential material: {forbidden} in {text}"
        );
    }
}

// MARK: The cross-language fixture the panel decodes
//
// The keys in the peers block cross into Swift as STRINGS and no compiler
// checks that seam, the same gap `cli::tests::status_contract_fixture_matches_committed`
// exists to close for the accounts array. So the block is rendered here, pinned
// as committed bytes, and decoded on the other side by
// `PeersStatusBlockTests.testCommittedPeersFixtureDecodes`: a renamed key turns
// this test red, and regenerating the fixture to satisfy it turns the Swift one
// red in the same breath.

/// Where the committed fixture lives, from this test file's own path.
///
/// Beside `tests/fixtures/status-contract.json` and no longer inside the Swift
/// package: it used to live in `apps/macos/Tests/…/Fixtures/`, which made the
/// producer's pin a file the consumer owned and could hand-edit into agreement
/// with itself. Both golden files a panel decodes are written here by the real
/// Rust serializers, and the Swift suite reads THESE bytes.
fn peers_fixture_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/peer-status.json")
}

/// The two rows the fixture holds, and why it takes two.
///
/// Row one is the real derivation: every measured field is absent, which is the
/// state every build ships until the prober lands, and the one the panel must
/// draw without inventing a zero. Row two is hand-built with every measured
/// field FILLED, no writer produces those values yet, and that is exactly why
/// they need pinning: a key the derivation never populates is a key a rename
/// could not turn red. The numbers are illustrative and the doc-comment on the
/// Swift side says so.
fn peers_fixture_rows() -> Vec<PeerStatusRow> {
    let dir = tempfile::tempdir().expect("a temp dir");
    let now_ms = 1_700_000_000_000;
    let rows = write_peers(
        &dir.path().join("tcr-peers.json"),
        vec![{
            let mut pinned = row(7, "studio-mac", &["127.0.0.1:7749", "127.0.0.1:7750"]);
            pinned.allow.carry = true;
            pinned.allow.allow_disclose = true;
            pinned.allow.gateway = true;
            pinned
        }],
    );
    let state = PeerState {
        last_seen: vec![(node(7), now_ms - FIVE_MINUTES_MS)],
        leases: vec![LeaseRow {
            lease: tokens_lease(
                1_000_000,
                0.5,
                now_ms - 2 * 60 * 60 * 1_000,
                now_ms + 3_600_000,
            ),
            peer: node(7),
            scope: tcr_peer_wire::LendScope::All,
        }],
        ..PeerState::default()
    };

    let mut derived = peer_status_rows(&rows, &state, now_ms);
    let measured = PeerStatusRow {
        id: node(9).to_wire(),
        display: node(9).display(),
        name: "attic-nuc".to_string(),
        address: Some("127.0.0.1:7751".to_string()),
        trusted: true,
        last_seen_ms: Some(now_ms - 2_000),
        carries: true,
        serves: false,
        in_flight: Some(2),
        lease_spent: Some(0.34),
        lease_ttl_seconds: Some(240),
        bytes_per_hour: Some(41_943_040),
        byte_cap_per_hour: Some(2_147_483_648),
        tokens_per_hour: Some(90_000),
        no_headroom: Some(false),
        until: Some(1_700_003_600),
        ended: false,
        lend: Vec::new(),
        paths: vec![
            teamclaude_rs::status::PathStatus {
                endpoint: "127.0.0.1:7751".to_string(),
                kind: PathKind::Direct,
                rtt_ms: Some(18.5),
                loss_pct: Some(0.0),
                bytes_per_hour: Some(41_943_040),
                tokens_per_hour: Some(90_000),
                last_ok_ms: Some(now_ms - 2_000),
            },
            teamclaude_rs::status::PathStatus {
                // The wire id, not `display()`: `PeerStatusRow::id` is the
                // wire form and the panel resolves a via/reverse endpoint's
                // name by looking `PathStatus::endpoint` up in that map.
                endpoint: node(7).to_wire(),
                kind: PathKind::Via,
                rtt_ms: Some(96.0),
                loss_pct: Some(0.02),
                bytes_per_hour: None,
                tokens_per_hour: None,
                last_ok_ms: Some(now_ms - 30_000),
            },
        ],
    };
    derived.push(measured);
    derived
}

/// THE CROSS-LANGUAGE CONTRACT PIN for the peers block.
#[test]
fn the_committed_peers_fixture_matches_what_the_payload_renders() {
    let path = peers_fixture_path();
    let rendered = format!(
        "{}\n",
        serde_json::to_string_pretty(&peers_fixture_rows()).expect("the rows serialize")
    );

    if std::env::var_os("TCR_UPDATE_FIXTURES").is_some() {
        std::fs::create_dir_all(path.parent().expect("fixture dir")).expect("create fixture dir");
        std::fs::write(&path, &rendered).expect("write fixture");
        eprintln!("regenerated {}", path.display());
        return;
    }

    let committed = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "cannot read the committed peers fixture at {}: {e}. Regenerate with \
             TCR_UPDATE_FIXTURES=1 cargo test --test peer_status \
             the_committed_peers_fixture_matches_what_the_payload_renders",
            path.display()
        )
    });
    assert_eq!(
        committed,
        rendered,
        "the peers block no longer renders the committed fixture at {}. If the change is \
         intended, regenerate it with TCR_UPDATE_FIXTURES=1 and then run the Swift decode \
         suite (apps/macos: swift test). That is the half of the contract this process \
         cannot check.",
        path.display()
    );
}

/// The fixture is only worth pinning if it covers BOTH shapes: the derivation's
/// honest absences and a fully measured row. A fixture of one shape would pin
/// one shape, and the absent half is the one that regressed.
#[test]
fn the_committed_peers_fixture_covers_the_absent_and_the_measured_row() {
    let committed = std::fs::read_to_string(peers_fixture_path())
        .expect("the committed peers fixture is readable");
    let rows: Vec<serde_json::Value> =
        serde_json::from_str(&committed).expect("the fixture is a bare JSON array");
    assert_eq!(rows.len(), 2, "one derived row, one fully measured row");

    let derived = rows[0].as_object().expect("a row is an object");
    for absent in ["rttMs", "lossPct", "inFlight", "bytesPerHour", "noHeadroom"] {
        assert!(
            !derived.contains_key(absent),
            "the derived row's `{absent}` has no writer yet and must be absent: {}",
            rows[0]
        );
    }
    assert!(
        derived.contains_key("tokensPerHour"),
        "the ledger CAN answer a token rate, so the derived row carries one: {}",
        rows[0]
    );

    let measured = rows[1].as_object().expect("a row is an object");
    for present in [
        "rttMs",
        "lossPct",
        "inFlight",
        "bytesPerHour",
        "byteCapPerHour",
        "noHeadroom",
        "tokensPerHour",
    ] {
        let present_here = measured.contains_key(present)
            || measured["paths"][0]
                .as_object()
                .is_some_and(|path| path.contains_key(present));
        assert!(
            present_here,
            "`{present}` must appear somewhere in the measured row, or a rename of it \
             could not turn this pin red: {}",
            rows[1]
        );
    }
    let kinds: Vec<&str> = rows
        .iter()
        .flat_map(|row| row["paths"].as_array().expect("paths is an array"))
        .map(|path| path["kind"].as_str().expect("kind is a string"))
        .collect();
    assert!(kinds.contains(&"direct"), "{kinds:?}");
    assert!(
        kinds.contains(&"via"),
        "a forwarded path is the other kind the panel draws differently: {kinds:?}"
    );
}

// MARK: `tcr peer graph --json`
//
// The graph is served as a PAGE by one nominated Mac, which makes its shape a
// contract with a reader written in another language again, so it is checked
// the way the peers block is: against a schema this file spells out, key by
// key, and against a set of hand-broken documents the schema MUST reject. A
// validator that only ever sees a valid document proves nothing about what it
// would catch, which is the whole defect the peers block existed to fix.

use teamclaude_rs::status::{peer_graph, PeerGraph, PEER_GRAPH_KIND};

/// What a JSON value is allowed to be, in the schema below.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Shape {
    Str,
    Int,
    Float,
    /// One of a fixed set of string tokens: a typed discriminant on the wire.
    Token(&'static [&'static str]),
}

/// One key of one object: its name, whether it must be present, and its shape.
struct Field(&'static str, bool, Shape);

const NODE_FIELDS: &[Field] = &[
    Field("id", true, Shape::Str),
    Field("name", true, Shape::Str),
    Field("role", true, Shape::Token(&["self", "peer"])),
    Field("lastSeenMs", false, Shape::Int),
];

const PATH_EDGE_FIELDS: &[Field] = &[
    Field("from", true, Shape::Str),
    Field("to", true, Shape::Str),
    Field("kind", true, Shape::Token(&["path"])),
    Field("endpoint", true, Shape::Str),
    Field("pathKind", true, Shape::Token(&["direct", "via"])),
    Field("through", false, Shape::Str),
    Field("rttMs", false, Shape::Float),
    Field("lossPct", false, Shape::Float),
    Field("lastOkMs", false, Shape::Int),
];

const LEASE_EDGE_FIELDS: &[Field] = &[
    Field("from", true, Shape::Str),
    Field("to", true, Shape::Str),
    Field("kind", true, Shape::Token(&["lease"])),
    Field("leaseId", true, Shape::Str),
    Field("spent", true, Shape::Float),
    Field("expiresAtMs", true, Shape::Int),
    Field("until", false, Shape::Int),
];

const DOCUMENT_FIELDS: &[Field] = &[
    Field("kind", true, Shape::Token(&[PEER_GRAPH_KIND])),
    Field("generatedAtMs", true, Shape::Int),
    // `nodes` and `edges` are walked with their own tables below rather than
    // named here, because an array of objects has no single shape.
];

/// Check one object against its table: every required key present and of the
/// right shape, and NO key the table does not name.
///
/// The unknown-key half is the one that earns its keep: a renamed field passes
/// every "is the value a string" check in the world while the reader on the
/// other side reads nothing at all.
fn check_object(what: &str, value: &serde_json::Value, fields: &[Field], out: &mut Vec<String>) {
    let Some(object) = value.as_object() else {
        out.push(format!("{what} is not a JSON object: {value}"));
        return;
    };
    for Field(name, required, shape) in fields {
        match object.get(*name) {
            None if *required => out.push(format!("{what}: `{name}` is required: {value}")),
            None => {}
            Some(found) => {
                let ok = match shape {
                    Shape::Str => found.is_string(),
                    Shape::Int => found.is_i64(),
                    // An integer IS an acceptable float on this wire: serde
                    // writes `0.0` as `0.0` but a hand-built document may say
                    // `0`, and refusing that would be a schema stricter than
                    // JSON itself.
                    Shape::Float => found.is_f64() || found.is_i64(),
                    Shape::Token(allowed) => {
                        found.as_str().is_some_and(|token| allowed.contains(&token))
                    }
                };
                if !ok {
                    out.push(format!("{what}: `{name}` has the wrong shape: {found}"));
                }
            }
        }
    }
    for key in object.keys() {
        if !fields.iter().any(|Field(name, _, _)| name == key) {
            out.push(format!(
                "{what}: `{key}` is not in this document's schema, so no reader was told \
                 about it: {value}"
            ));
        }
    }
}

/// THE SCHEMA. Every complaint, not just the first, so a broken document names
/// everything wrong with it in one run.
fn graph_schema_errors(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    check_object("the document", value, DOCUMENT_FIELDS, &mut out);
    let Some(object) = value.as_object() else {
        return out;
    };
    // The two array keys, checked here so `check_object`'s unknown-key sweep
    // does not report them as unschema'd.
    out.retain(|error| !error.contains("`nodes`") && !error.contains("`edges`"));

    let mut ids: Vec<&str> = Vec::new();
    let mut selves = 0;
    match object.get("nodes").and_then(|nodes| nodes.as_array()) {
        None => out.push("the document: `nodes` must be an array".to_string()),
        Some(nodes) => {
            for node in nodes {
                check_object("a node", node, NODE_FIELDS, &mut out);
                if let Some(id) = node.get("id").and_then(|id| id.as_str()) {
                    ids.push(id);
                }
                if node.get("role").and_then(|role| role.as_str()) == Some("self") {
                    selves += 1;
                }
            }
        }
    }
    if selves != 1 {
        out.push(format!(
            "exactly one node is the Mac that answered; found {selves}"
        ));
    }

    match object.get("edges").and_then(|edges| edges.as_array()) {
        None => out.push("the document: `edges` must be an array".to_string()),
        Some(edges) => {
            for edge in edges {
                match edge.get("kind").and_then(|kind| kind.as_str()) {
                    Some("path") => check_object("a path edge", edge, PATH_EDGE_FIELDS, &mut out),
                    Some("lease") => {
                        check_object("a lease edge", edge, LEASE_EDGE_FIELDS, &mut out)
                    }
                    other => out.push(format!(
                        "an edge's `kind` must be `path` or `lease`; got {other:?}: {edge}"
                    )),
                }
                // Every endpoint of every edge is a DECLARED node. A dangling
                // edge is the one defect a page cannot draw at all, and no
                // per-field check can see it.
                for end in ["from", "to", "through"] {
                    if let Some(id) = edge.get(end).and_then(|id| id.as_str()) {
                        if end == "through" && !ids.contains(&id) {
                            // A via endpoint is a node id; a direct one is an
                            // address and is not checked here.
                            out.push(format!("`through` names no declared node: {edge}"));
                        } else if end != "through" && !ids.contains(&id) {
                            out.push(format!("`{end}` names no declared node: {edge}"));
                        }
                    }
                }
            }
        }
    }
    out
}

/// One Mac, two peers, two endpoints on one of them, a lease each way.
fn graph_fixture(now_ms: i64) -> PeerGraph {
    let dir = tempfile::tempdir().expect("a temp dir");
    let rows = write_peers(
        &dir.path().join("tcr-peers.json"),
        vec![
            {
                let mut pinned = row(7, "studio-mac", &["127.0.0.1:7749", "127.0.0.1:7750"]);
                pinned.allow.carry = true;
                pinned
            },
            row(9, "attic-nuc", &["127.0.0.1:7751"]),
        ],
    );
    let state = PeerState {
        last_seen: vec![
            (node(7), now_ms - FIVE_MINUTES_MS),
            (node(9), now_ms - 2_000),
        ],
        leases: vec![LeaseRow {
            lease: tokens_lease(1_000_000, 0.5, now_ms - 3_600_000, now_ms + 3_600_000),
            peer: node(7),
            scope: tcr_peer_wire::LendScope::All,
        }],
        borrowed: vec![BorrowedRow {
            lease: tokens_lease(200_000, 0.1, now_ms - 600_000, now_ms + 1_800_000),
            lender: node(9),
        }],
        ..PeerState::default()
    };
    peer_graph(&node(1), "this-mac", &rows, &state, now_ms)
}

/// THE GATE: what `tcr peer graph --json` renders validates against the schema
/// above, with no complaint at all.
#[test]
fn the_graph_validates_against_the_schema() {
    let graph = graph_fixture(1_700_000_000_000);
    let wire = serde_json::to_value(&graph).expect("the graph serializes");
    let errors = graph_schema_errors(&wire);
    assert!(errors.is_empty(), "{errors:#?}\nfrom {wire:#}");

    // And it round-trips: a reader in this language gets the same value back,
    // which is what makes the flattened edge tag honest rather than
    // write-only.
    let back: PeerGraph = serde_json::from_value(wire).expect("the graph deserializes");
    assert_eq!(back, graph);
}

/// THE POSITIVE CONTROL FOR THE SCHEMA ITSELF.
///
/// Five hand-broken documents, each breaking one thing the schema exists to
/// catch, every one of which a per-field type check alone would wave through.
/// Without this the test above would pass just as happily against a validator
/// that returned an empty list.
#[test]
fn the_schema_rejects_every_document_it_exists_to_reject() {
    let valid = serde_json::to_value(graph_fixture(1_700_000_000_000)).expect("serializes");
    assert!(
        graph_schema_errors(&valid).is_empty(),
        "the control is valid"
    );

    let mutate = |name: &str, edit: &dyn Fn(&mut serde_json::Value)| {
        let mut broken = valid.clone();
        edit(&mut broken);
        let errors = graph_schema_errors(&broken);
        assert!(
            !errors.is_empty(),
            "the schema waved through `{name}`, so it is not checking it: {broken:#}"
        );
    };

    mutate("a renamed node key", &|graph| {
        let node = &mut graph["nodes"][0];
        let name = node["name"].take();
        node.as_object_mut().expect("an object").remove("name");
        node["label"] = name;
    });
    mutate("a renamed edge figure", &|graph| {
        let edge = &mut graph["edges"][0];
        let endpoint = edge["endpoint"].take();
        edge.as_object_mut().expect("an object").remove("endpoint");
        edge["addr"] = endpoint;
    });
    mutate("a dangling edge", &|graph| {
        graph["edges"][0]["to"] = serde_json::json!("tcr-nosuchmac");
    });
    mutate("a second answering Mac", &|graph| {
        graph["nodes"][1]["role"] = serde_json::json!("self");
    });
    mutate("an untyped path kind", &|graph| {
        graph["edges"][0]["pathKind"] = serde_json::json!("whatever");
    });
    mutate("a wrong document kind", &|graph| {
        graph["kind"] = serde_json::json!("tcr.status.v1");
    });
}

/// The graph is the mesh, so it says which way the quota flows: the LENDER is
/// always `from`, and a reader never consults a direction field.
#[test]
fn a_lease_edge_points_from_the_lender_to_the_borrower() {
    let now_ms = 1_700_000_000_000;
    let graph = graph_fixture(now_ms);
    let me = PeerId([1; 32]).display();
    let lent: Vec<(&str, &str)> = graph
        .edges
        .iter()
        .filter(|edge| {
            matches!(
                edge.detail,
                teamclaude_rs::status::GraphEdgeDetail::Lease { .. }
            )
        })
        .map(|edge| (edge.from.as_str(), edge.to.as_str()))
        .collect();
    assert_eq!(
        lent,
        vec![
            (me.as_str(), PeerId([7; 32]).display().as_str()),
            (PeerId([9; 32]).display().as_str(), me.as_str()),
        ],
        "one lease lent to studio-mac and one borrowed from attic-nuc, and the arrow \
         turns around: {:#?}",
        graph.edges
    );
}

/// An EXPIRED lease is not an edge. A graph of the mesh as it is now cannot
/// carry a delegation that has already lapsed: the reader would see quota
/// flowing where none is.
#[test]
fn an_expired_lease_is_not_an_edge() {
    let now_ms = 1_700_000_000_000;
    let dir = tempfile::tempdir().expect("a temp dir");
    let rows = write_peers(
        &dir.path().join("tcr-peers.json"),
        vec![row(7, "studio-mac", &["127.0.0.1:7749"])],
    );
    let state = PeerState {
        leases: vec![LeaseRow {
            // Granted an hour ago, dead a minute ago.
            lease: tokens_lease(1_000_000, 0.5, now_ms - 3_600_000, now_ms - 60_000),
            peer: node(7),
            scope: tcr_peer_wire::LendScope::All,
        }],
        ..PeerState::default()
    };
    let graph = peer_graph(&node(1), "this-mac", &rows, &state, now_ms);
    assert!(
        graph.edges.iter().all(|edge| matches!(
            edge.detail,
            teamclaude_rs::status::GraphEdgeDetail::Path { .. }
        )),
        "only the path edge survives an expired lease: {:#?}",
        graph.edges
    );
    assert_eq!(
        graph.nodes.len(),
        2,
        "the Mac is still a node: {:#?}",
        graph.nodes
    );
}

/// A label that is an address or a uuid never reaches a page. This graph is the
/// one shape a nominated Mac SERVES, and the repository is public.
#[test]
fn a_graph_node_label_is_masked() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let rows = write_peers(
        &dir.path().join("tcr-peers.json"),
        vec![row(7, "alice@example.com", &["127.0.0.1:7749"])],
    );
    let graph = peer_graph(
        &node(1),
        "bob@example.com",
        &rows,
        &PeerState::default(),
        1_700_000_000_000,
    );
    assert_eq!(
        graph
            .nodes
            .iter()
            .map(|n| n.name.as_str())
            .collect::<Vec<_>>(),
        vec!["[masked]", "[masked]"],
        "both the answering Mac's own label and the peer's go through the sanitizer"
    );
}

// ---------------------------------------------------------------------------
// The block the SERVING process assigns, read off the two files
// ---------------------------------------------------------------------------

/// `peers_block` opens the peers file and the peer-state file beside it and
/// answers the same rows `peer_status_rows` derives from their contents.
///
/// This is the seam the serving process calls, and it is the one that was
/// missing: `peer_status_rows` had no production caller at all, so a correct
/// derivation sat behind a payload that always carried `"peers": []`.
///
/// Watch it fail by returning `Vec::new()` from `peers_block`: the row count
/// drops to zero while every assertion about the derivation stays green, which
/// is exactly the shape of the defect this closes.
#[test]
fn the_serving_process_block_reads_both_files_off_disk() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let peers_path = dir.path().join("tcr-peers.json");
    write_peers(&peers_path, vec![row(7, "studio-mac", &["127.0.0.1:7749"])]);

    let now_ms = 1_700_000_000_000;
    let state_path = teamclaude_rs::peer::serve::peer_state_path(&peers_path);
    let state = PeerState {
        last_seen: vec![(node(7), now_ms - FIVE_MINUTES_MS)],
        ..PeerState::default()
    };
    teamclaude_rs::peer::state::save(&state_path, &state).expect("the state file writes");

    let block = teamclaude_rs::status::peers_block(&peers_path, now_ms);
    assert_eq!(block.len(), 1, "one pinned row in the file, one row out");
    assert_eq!(block[0].name, "studio-mac");
    assert_eq!(
        block[0].address.as_deref(),
        Some("127.0.0.1:7749"),
        "the address comes off the file's own endpoint list"
    );
    assert_eq!(
        block[0].last_seen_ms,
        Some(now_ms - FIVE_MINUTES_MS),
        "and `lastSeen` comes off the state file beside it, which is the second \
         file this seam exists to open"
    );
}

/// A Mac that has never paired has no peers file at all. That is the ordinary
/// state of a fresh install, so it is an empty block and never a refusal, the
/// accounts half of `tcr status` must not disappear because the mesh is unused.
#[test]
fn a_mac_with_no_peers_file_reports_an_empty_block() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let block = teamclaude_rs::status::peers_block(
        &dir.path().join("never-written.json"),
        1_700_000_000_000,
    );
    assert!(block.is_empty(), "no peers file, no peers: {block:#?}");
}

/// A6-8: a peers file that exists but does not parse must say so, not read as
/// the same "nobody pinned" fact a fresh install reports.
///
/// Watch it fail on the pre-fix `peers_block`, which has no error to hand
/// back at all: swap `peers_block_with_error` for `(peers_block(&peers_path,
/// now_ms), None)` and this goes red, because `error` stays `None` for a file
/// that is one invalid byte.
#[test]
fn an_unparseable_peers_file_names_its_own_parse_error() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("a temp dir");
    let peers_path = dir.path().join("tcr-peers.json");
    std::fs::write(&peers_path, b"\xff").expect("write one invalid byte");
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(&peers_path, perms).expect("set mode 0600");

    let (block, error) =
        teamclaude_rs::status::peers_block_with_error(&peers_path, 1_700_000_000_000);

    assert!(block.is_empty(), "a broken file still reports zero peers");
    let error = error.expect("a file that exists but will not parse must name why");
    assert!(
        error.contains("tcr-peers.json"),
        "the parse error names the file: {error}"
    );
}

// MARK: The `tcr peer ls --json` document, and the secret that used to ride it
//
// The same cross-language pin as the peers block above, for the other payload
// the panel polls. It takes a second fixture because it is a second document:
// `peer ls` reads the two FILES and `status` reads the serving process, they
// carry different blocks, and the panel merges them row by row on `id`.

/// Where the committed `tcr peer ls --json` fixture lives.
///
/// Beside the status one, written by the real serializer, decoded on the Swift
/// side. Two files rather than one because the two commands answer two
/// questions, and a merged sample would pin neither shape.
fn peer_ls_fixture_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/peer-ls.json")
}

/// A hex string of exactly 32 bytes anywhere in `text`: what a leaked
/// `rendezvousSecret` looks like on the wire, whatever key it hides under.
///
/// A key-name check alone would pass the moment the field were renamed, and
/// renaming is precisely how a secret comes back. So this looks for the VALUE's
/// shape: 64 hex characters in a row, with no hex character either side, so a
/// longer blob is still caught and a 40-character digest is not confused for
/// one.
fn holds_a_32_byte_hex_value(text: &str) -> bool {
    let bytes: Vec<char> = text.chars().collect();
    let hex = |c: char| c.is_ascii_hexdigit();
    let mut run = 0_usize;
    for (index, &c) in bytes.iter().enumerate() {
        if hex(c) {
            run += 1;
        } else {
            run = 0;
            continue;
        }
        if run >= 64 {
            let after_is_hex = bytes.get(index + 1).copied().is_some_and(hex);
            if !after_is_hex {
                return true;
            }
        }
    }
    false
}

/// The document, built from the real derivations wherever one exists.
///
/// The pinned row carries a `rendezvous_secret`, which is the point: the
/// fixture is only evidence that the secret does not cross if the row it was
/// rendered from actually held one.
/// An end far enough out that `ended` is `false` whenever this fixture is
/// rendered: the peers store derives that flag against the real wall clock, so
/// a committed sample needs ends the clock cannot walk past.
const LIVE_UNTIL_S: u64 = 4_102_444_800;

/// An end already in the past, for the greyed row. Fixed for the same reason.
const ENDED_UNTIL_S: u64 = 1_600_000_000;

fn peer_ls_fixture_document() -> teamclaude_rs::status::PeerLsJson {
    use teamclaude_rs::peer::config::{LendGrant, LendMode};
    use teamclaude_rs::peer::state::{Ban, BanReason, Knock, Mute};

    let dir = tempfile::tempdir().expect("a temp dir");
    let now_ms = 1_700_000_000_000_i64;

    let mut pinned = row(7, "studio-mac", &["127.0.0.1:7749", "127.0.0.1:7750"]);
    pinned.allow.carry = true;
    pinned.allow.allow_disclose = true;
    pinned.allow.gateway = true;
    // The value this whole fixture exists to keep off the wire. Obviously fake,
    // and never a secret this machine derived.
    pinned.rendezvous_secret = Some([0x5a; 32]);
    pinned.sees_us_at = Some(("203.0.113.9:7749".to_string(), 1_700_000_000_000));
    pinned.lend = vec![
        {
            let mut grant = LendGrant::new(tcr_peer_wire::Window::SevenDay, 0.2, 900, 2);
            grant.id = 0x2a;
            // Far future on purpose: `ended` is derived against the REAL clock
            // as the store hands a row out, so a sample end in the past would
            // make the committed bytes depend on when they were rendered.
            grant.until = Some(LIVE_UNTIL_S);
            grant
        },
        {
            let mut grant = LendGrant::new(tcr_peer_wire::Window::FiveHour, 0.5, 600, 1);
            grant.id = 0x2b;
            grant.mode = LendMode::Hand;
            grant.scope = tcr_peer_wire::LendScope::Group("work".to_string());
            grant
        },
    ];
    // The other shape a row comes in: every lease past its end, which is the
    // greyed row. Its end is in the past and stays there.
    let mut second = row(9, "attic-nuc", &[]);
    second.lend = vec![{
        let mut grant = LendGrant::new(tcr_peer_wire::Window::FiveHour, 0.1, 300, 1);
        grant.id = 0x2c;
        grant.until = Some(ENDED_UNTIL_S);
        grant
    }];

    let peers_path = dir.path().join("tcr-peers.json");
    let rows = write_peers(&peers_path, vec![pinned, second]);
    let store = PeerStore::open(&peers_path).expect("the peers file opens");
    let lent_to = teamclaude_rs::peer::lease::lent_to(
        &store,
        &[("alice@example.com".to_string(), vec!["work".to_string()])],
    );

    // `until`/`ended` are the CLI's own derivation and it lives in the binary,
    // so the fixture states them: what is pinned here is the SHAPE the panel
    // decodes, and the derivation has its own tests.
    let peers = vec![
        teamclaude_rs::status::PeerLsPeer::Trusted(teamclaude_rs::status::PeerLsRow::from_row(
            rows[0].clone(),
            Some(LIVE_UNTIL_S),
            false,
        )),
        teamclaude_rs::status::PeerLsPeer::Trusted(teamclaude_rs::status::PeerLsRow::from_row(
            rows[1].clone(),
            Some(ENDED_UNTIL_S),
            true,
        )),
    ];

    teamclaude_rs::status::PeerLsJson {
        supported: true,
        peers,
        // `addr` carries the port, because that is what the command renders:
        // the row in the state file is keyed on the bare IP and `peer ls`
        // hands out `Knock::dial_address`, which is the string the panel
        // dials to answer. A sample with the bare key in it would pin a shape
        // no invocation emits.
        pending: vec![Knock {
            addr: "192.0.2.14:7766".to_string(),
            instance_id: tcr_peer_wire::InstanceId([0x11; 8]),
            proposed_name: Some("kitchen-mac".to_string()),
            wire_version: 1,
            listen_port: Some(7766),
            first_seen_ms: now_ms - 30_000,
            last_seen_ms: now_ms - 2_000,
        }],
        pending_count: 1,
        blocked: vec![Ban {
            addr: "192.0.2.77".to_string(),
            key: Some(node(3)),
            since_ms: now_ms - 86_400_000,
            reason: BanReason::Blocked,
        }],
        blocked_count: 1,
        muted: vec![Mute {
            addr: "192.0.2.90".to_string(),
            until_ms: now_ms + 600_000,
        }],
        muted_count: 1,
        limited: 0,
        caps: teamclaude_rs::status::PeerCapsJson {
            found_rows: teamclaude_rs::peer::discovery::MAX_FOUND_ROWS,
            found_per_address: teamclaude_rs::peer::discovery::MAX_FOUND_PER_ADDRESS,
            pending: teamclaude_rs::peer::state::MAX_PENDING_KNOCKS,
            knock_interval_ms: teamclaude_rs::peer::listener::KNOCK_INTERVAL_MS,
            knock_burst: teamclaude_rs::peer::listener::KNOCK_BURST,
            unauthenticated_sockets: teamclaude_rs::peer::listener::MAX_UNAUTHENTICATED_SOCKETS,
        },
        lent_to,
        internet: false,
        network: true,
        exits: [(
            "alice".to_string(),
            teamclaude_rs::status::PeerExitJson {
                // The wire id, matching `Egress`'s own `Display` impl
                // (`config.rs`: `write!(f, "{}{}", VIA_PREFIX, peer.to_wire())`).
                // The fixture pinned the display form, which no code path
                // actually emits.
                egress: format!("via {}", node(7).to_wire()),
                egress_strict: true,
                peer_down: false,
                waiting_seconds: None,
            },
        )]
        .into_iter()
        .collect(),
        peers_error: None,
    }
}

/// THE CROSS-LANGUAGE CONTRACT PIN for `tcr peer ls --json`.
#[test]
fn the_committed_peer_ls_fixture_matches_what_the_document_renders() {
    let path = peer_ls_fixture_path();
    let rendered = format!(
        "{}\n",
        serde_json::to_string_pretty(&peer_ls_fixture_document()).expect("the document serializes")
    );

    if std::env::var_os("TCR_UPDATE_FIXTURES").is_some() {
        std::fs::create_dir_all(path.parent().expect("fixture dir")).expect("create fixture dir");
        std::fs::write(&path, &rendered).expect("write fixture");
        eprintln!("regenerated {}", path.display());
        return;
    }

    let committed = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "cannot read the committed fixture at {}: {e}. Regenerate with \
             TCR_UPDATE_FIXTURES=1 cargo test --test peer_status \
             the_committed_peer_ls_fixture_matches_what_the_document_renders",
            path.display()
        )
    });
    assert_eq!(
        committed,
        rendered,
        "`tcr peer ls --json` no longer renders the committed fixture at {}. If the change \
         is intended, regenerate it with TCR_UPDATE_FIXTURES=1 and then run the Swift decode \
         suite (apps/macos: swift test).",
        path.display()
    );
}

/// The rendezvous secret does not cross, and the check is on the VALUE.
///
/// `docs/peers.md` says the pair's rendezvous secret is never sent to anybody.
/// The row used to be `#[serde(flatten)] PeerRow`, so those 64 hex characters
/// rode a payload TcrBar polls every three seconds and an operator pastes into
/// a bug report. Asserted twice over, on the shape of the value and on the key
/// name, because a rename is how such a field comes back.
#[test]
fn the_peer_ls_document_carries_no_rendezvous_secret() {
    let document = peer_ls_fixture_document();
    let wire = serde_json::to_string(&document).expect("the document serializes");

    assert!(
        !holds_a_32_byte_hex_value(&wire),
        "a 32-byte hex value is on the wire, which is what the rendezvous secret looks \
         like: {wire}"
    );
    assert!(
        !wire.to_lowercase().contains("rendezvous"),
        "no key names the rendezvous secret: {wire}"
    );
    // The control: the row really did hold one, so the absence above is the
    // projection's doing and not an empty fixture's.
    let file = serde_json::to_string(&document.peers).expect("rows serialize");
    assert!(
        !file.contains(&"5a".repeat(32)),
        "the fixture row's own secret must not appear: {file}"
    );
}

/// The value check is only evidence if it can fire. Run it against a string
/// that HAS a 32-byte hex value and one that has a shorter digest.
#[test]
fn the_secret_detector_fires_on_a_32_byte_hex_value() {
    let secret = "5a".repeat(32);
    assert!(holds_a_32_byte_hex_value(&format!(
        "{{\"rendezvousSecret\":\"{secret}\"}}"
    )));
    assert!(
        !holds_a_32_byte_hex_value("{\"digest\":\"da39a3ee5e6b4b0d3255bfef95601890afd80709\"}"),
        "a 40-character digest is not a 32-byte value"
    );
    assert!(
        !holds_a_32_byte_hex_value("{\"id\":\"2a\",\"addr\":\"192.0.2.14\"}"),
        "ordinary short hex is not a secret"
    );
}

/// Both golden files name the same Mac the same way, which is the join the
/// panel does. `peer ls`'s `node` and `status`'s `id` are one string; the short
/// form lives on its own key.
#[test]
fn the_two_documents_name_a_mac_the_same_way() {
    let ls = serde_json::to_value(peer_ls_fixture_document()).expect("ls serializes");
    let status = serde_json::to_value(peers_fixture_rows()).expect("status rows serialize");

    let ls_id = ls["peers"][0]["node"]
        .as_str()
        .expect("a row names its node")
        .to_string();
    assert_eq!(ls_id.chars().count(), 52, "the wire form is 52 characters");
    assert!(!ls_id.starts_with("tcr-"), "not the display form: {ls_id}");

    let status_id = status[0]["id"].as_str().expect("a row names its id");
    assert_eq!(
        status_id.chars().count(),
        52,
        "`status --json` carries the same form `peer ls --json` does: {status_id}"
    );
    assert!(
        status[0]["display"]
            .as_str()
            .is_some_and(|short| short.starts_with("tcr-")),
        "the short form is its own key: {}",
        status[0]
    );
    assert_eq!(
        status_id,
        status[0]["display"]
            .as_str()
            .map(|short| short.trim_start_matches("tcr-"))
            .map(|short| format!("{short}{}", &status_id[10..]))
            .expect("a display form")
            .as_str(),
        "the display form is the wire form's first ten characters and nothing else"
    );
}

/// **A lease edge names the lease in the same 32 characters every other
/// surface prints.**
///
/// The graph rendered it with a bare hex format, so any id with a leading zero
/// nibble came out shorter than the handle `tcr peer lend --revoke` takes and
/// than the one `lentTo` and `tcr peer ls --json` carry. A reader lining an
/// edge up against a lease list matched nothing, and a command built from the
/// edge named a lease nobody minted.
///
/// The fixture's lease id is `0x2a`, which is 30 leading zeroes and the reason
/// this is observable at all.
///
/// Watch it fail by putting `format!("{:x}", lease.lease_id)` back in
/// `lease_edge`.
#[test]
fn a_lease_edge_names_the_lease_the_way_every_other_surface_does() {
    let graph = graph_fixture(1_700_000_000_000);
    let wire = serde_json::to_value(&graph).expect("the graph serializes");

    let ids: Vec<&str> = wire["edges"]
        .as_array()
        .expect("the graph carries edges")
        .iter()
        .filter_map(|edge| edge["leaseId"].as_str())
        .collect();
    assert!(
        !ids.is_empty(),
        "the fixture has to carry a lease edge, or this measures nothing: {wire}"
    );
    for id in ids {
        assert_eq!(
            id,
            teamclaude_rs::peer::config::lease_id_string(0x2a),
            "an edge names its lease in the form the CLI prints and reads back"
        );
        assert_eq!(id.len(), 32, "32 characters, padded: {id}");
    }
}
