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
