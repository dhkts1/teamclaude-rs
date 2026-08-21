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
