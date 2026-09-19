//! Guards the invariant a Sparkle auto-updater imposes on this binary: a
//! config shape that shipped in any past release must still `config::load`
//! successfully in the CURRENT binary.
//!
//! This is the class of bug behind a real incident, not a hypothetical: a
//! 0.2.18 → 0.2.28 auto-update split the `throttle` config key into
//! `accountThrottle`/`fleetThrottle` with no migration step of its own, and a
//! stale `throttle` key was a hard `load` error. Every affected install broke
//! on the update — the CLI exited non-zero on every verb, and the server
//! silently ran a zero-account fleet that answered every request with 429
//! while looking alive. `src/config.rs`'s `load` now migrates that key
//! instead of rejecting it (see its doc-comment and the `legacy_throttle_*`
//! unit tests there), but that fix only proves THIS rename is safe. The next
//! one needs the same proof, and nothing else in this repo asserts it.
//!
//! `tests/fixtures/legacy_configs/` holds one file per historical config
//! shape this binary must keep loading. Whoever renames or removes a config
//! key next must either keep every fixture here loading, or consciously
//! delete the fixture that shape belongs to — and a deletion is a visible,
//! reviewable line in that PR's diff, which is the actual point: silence is
//! no longer an option for breaking a shipped config shape.

use std::path::Path;

use teamclaude_rs::config;

const FIXTURE_DIR: &str = "tests/fixtures/legacy_configs";

#[test]
fn every_historical_config_shape_still_loads() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_DIR);
    let entries: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|err| panic!("fixture directory {} must exist: {err}", dir.display()))
        .map(|entry| entry.expect("readable dir entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();

    // The control that keeps this test from being a for-loop over nothing: an
    // empty or missing directory would let every future rename through
    // silently, which is exactly the failure mode this test exists to catch.
    assert!(
        entries.len() >= 3,
        "expected at least 3 legacy config fixtures in {}, found {} — a for loop \
         over an empty directory passes green forever and asserts nothing",
        dir.display(),
        entries.len()
    );

    for path in &entries {
        config::load(path).unwrap_or_else(|err| {
            panic!(
                "a config shape that shipped in a past release must still load: \
                 {} failed with: {err}",
                path.display()
            )
        });
    }
}

/// **A peers file written before the dead drop existed reads as off, and is
/// written back with no new key.**
///
/// The same invariant as the loop above, for the other file an update carries
/// forward. Three claims:
///
/// 1. the file still reads, and the absent `deadDrop` key reads as the value
///    `DeadDropConfig::default()` is, which is the whole rule a serde default
///    exists to hold;
/// 2. `is_live` is false, so nothing is published and nothing is fetched: a
///    default that read as on would turn a surface nobody configured into a
///    boot-time network write on every install that took the update;
/// 3. saving it back writes no `deadDrop` key. An `is_unset` predicate that
///    stopped matching would add a key to every peers file on this machine the
///    first time any `tcr peer` verb wrote one, silently, on an update.
///
/// Watched red: delete the `skip_serializing_if` on `PeerFile::dead_drop` and
/// claim 3 fails with the added key in the diff; make `is_live` read
/// `self.enabled || self.store.is_some()` and claim 2 still passes, which is
/// why claim 2 carries the second half below, a file whose switch is on with no
/// surface behind it.
#[test]
fn a_peers_file_with_no_dead_drop_key_reads_as_off() {
    use teamclaude_rs::peer::config::{self as peers, DeadDropConfig};

    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("tcr-peers.json");

    // The shape a build before this feature wrote: no `deadDrop` key anywhere.
    let before = "{\n  \"discovery\": true,\n  \"maxHops\": 1,\n  \"peers\": []\n}\n";
    std::fs::write(&path, before).expect("the legacy peers file writes");
    // The mode the reader checks: a peers file this program did not write is
    // refused before any key in it is read.
    std::fs::set_permissions(
        &path,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600),
    )
    .expect("the legacy peers file takes its mode");

    let file = peers::read_or_default(&path).expect("a peers file with no deadDrop key reads");
    assert_eq!(
        file.dead_drop,
        DeadDropConfig::default(),
        "an absent key is the default value and not a second shape: {:?}",
        file.dead_drop
    );
    assert!(
        !file.dead_drop.is_live(),
        "a file that never asked for a dead drop publishes nothing and fetches nothing"
    );
    assert!(
        !DeadDropConfig {
            enabled: true,
            ..DeadDropConfig::default()
        }
        .is_live(),
        "and the switch alone is not enough: on with no surface behind it is still off, \
         so the publisher and the fetcher cannot read that state two ways"
    );

    peers::save(&path, &file).expect("the peers file writes back");
    let after = std::fs::read_to_string(&path).expect("the peers file's bytes");
    assert!(
        !after.contains("deadDrop"),
        "a save adds no key to a file that never turned the dead drop on: {after}"
    );
}
