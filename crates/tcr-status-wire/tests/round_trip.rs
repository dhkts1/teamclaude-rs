//! Fixture -> struct -> JSON -> equals fixture, for the shared contract type.
//!
//! This is this crate's half of the byte-identical contract; the other half
//! (`cli::tests::status_contract_fixture_matches_committed` in the main
//! crate) proves `render_accounts_json` itself still emits the committed
//! bytes. Reads the SAME committed fixture both sides already read
//! (`tests/fixtures/status-contract.json`, the "only arrangement in which a
//! silent rename is impossible" per its doc-comment) — never a private copy.

use tcr_status_wire::AccountStatusRow;

fn fixture_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/status-contract.json")
}

#[test]
fn fixture_round_trips_through_account_status_row() {
    let committed =
        std::fs::read_to_string(fixture_path()).expect("committed contract fixture is readable");
    let rows: Vec<serde_json::Value> =
        serde_json::from_str(&committed).expect("fixture is a bare JSON array");
    assert!(!rows.is_empty(), "fixture must carry at least one row");

    // Row-at-a-time, per this crate's decode contract — never
    // `serde_json::from_str::<Vec<AccountStatusRow>>` against the whole file.
    let decoded: Vec<AccountStatusRow> = rows
        .iter()
        .cloned()
        .map(|v| AccountStatusRow::from_value(v).expect("fixture row decodes"))
        .collect();

    let reencoded: Vec<serde_json::Value> = decoded
        .into_iter()
        .map(|row| serde_json::to_value(row).expect("row re-encodes"))
        .collect();
    let rendered =
        serde_json::to_string_pretty(&reencoded).expect("re-encoded rows serialize") + "\n";

    assert_eq!(
        committed, rendered,
        "AccountStatusRow does not round-trip the committed fixture byte-for-byte"
    );
}

/// The fixture must carry BOTH shapes of the `usage` key — present and absent —
/// or it only ever proves one of them.
///
/// Absent is not a hypothetical: a client built after usage shipped routinely
/// reads a server built before it, because the binary on disk is rebuilt on
/// merge while the live process keeps serving until someone restarts it. A row
/// with no `usage` key must decode to `None` ("not measured"), never fail the
/// row and never read as a zero-filled day.
#[test]
fn the_fixture_pins_usage_present_and_usage_absent() {
    let committed =
        std::fs::read_to_string(fixture_path()).expect("committed contract fixture is readable");
    let rows: Vec<serde_json::Value> =
        serde_json::from_str(&committed).expect("fixture is a bare JSON array");

    let with_key = rows.iter().filter(|r| r.get("usage").is_some()).count();
    let without_key = rows.len() - with_key;
    assert!(with_key > 0, "no fixture row exercises a present `usage`");
    assert!(
        without_key > 0,
        "no fixture row exercises an absent `usage`"
    );

    for row in &rows {
        let present = row.get("usage").is_some();
        let decoded = AccountStatusRow::from_value(row.clone()).expect("fixture row decodes");
        assert_eq!(
            decoded.usage.is_some(),
            present,
            "a row's decoded usage must match whether the key was there"
        );
    }

    // And the partial-cost case: somewhere in the sample, a day with unpriced
    // requests reports a cost that is a PARTIAL sum, with the count that says
    // so — the reading a client is most likely to present as a whole spend.
    let partial = rows
        .iter()
        .filter_map(|r| AccountStatusRow::from_value(r.clone()).ok())
        .filter_map(|r| r.usage)
        .find(|u| u.today.unpriced_requests > 0)
        .expect("one fixture row must carry unpriced requests");
    assert!(
        partial.today.unpriced_requests < partial.today.requests,
        "the sample's partial day must be partial, not wholly unpriced"
    );
    assert!(
        partial.today.cost_usd.is_some(),
        "a day with SOME priced requests still reports what could be priced"
    );
}
