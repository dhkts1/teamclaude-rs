//! Proves a first run works: `tcr status` on a box with no
//! `~/.config/teamclaude.json` must exit 0 and leave a real config file behind.
//!
//! This is an end-to-end test of the BUILT BINARY rather than a unit test of
//! `config::load_or_init`, because the defect it guards was never in the load
//! function — it was in which load function each verb reached for. `tcr status`
//! called `config::load`, got `Io(NotFound)`, and exited 1 with `config i/o
//! error: No such file or directory`; TcrBar's panel shells to `tcr status
//! --json` and renders a non-zero exit as a failed poll, so a fresh install off
//! the dmg showed an error in the menu bar while the proxy behind it was up. A
//! unit test on the config module would have stayed green through all of that.
//!
//! # Isolation
//!
//! Same rules as `tests/headless_sigterm.rs`: `HOME` points at a fresh
//! [`tempfile::TempDir`] and `XDG_CACHE_HOME` is removed, both set on the
//! spawned [`Command`] rather than on this test process, so the real
//! `~/.config/teamclaude.json` — which holds working credentials — and the live
//! proxy reading it are never touched. Nothing here binds a port or signals a
//! process: `tcr status` with no server running falls back to its offline
//! snapshot, which is exactly the first-run case under test.
use std::path::PathBuf;
use std::process::Command;

/// The config path the child will resolve from its own `HOME`
/// (`config::default_path`).
fn config_path_in(home: &std::path::Path) -> PathBuf {
    home.join(".config").join("teamclaude.json")
}

/// Run `tcr <args…>` against a scratch `HOME`, returning (exit code, stdout, stderr).
fn run_tcr(home: &std::path::Path, args: &[&str]) -> (Option<i32>, String, String) {
    let bin = env!("CARGO_BIN_EXE_tcr");
    let out = Command::new(bin)
        .args(args)
        .env("HOME", home)
        .env_remove("XDG_CACHE_HOME")
        .output()
        .unwrap_or_else(|err| panic!("spawning the built tcr binary ({bin}) failed: {err}"));
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Mutation note: point any verb's load back at `config::load` (i.e. undo
/// `cli::load_config`) and this fails on its first assertion with `exit=1` and
/// the `config i/o error: No such file or directory` text in stderr — observed
/// on `346ea60` before the fix, not hypothesised.
#[test]
fn status_with_no_config_exits_zero_and_creates_the_file() {
    let home = tempfile::tempdir().expect("a scratch HOME must be creatable");
    let config = config_path_in(home.path());
    assert!(
        !config.exists(),
        "the scratch HOME must start with no config — this test proves the FIRST run"
    );

    let (code, stdout, stderr) = run_tcr(home.path(), &["status", "--json"]);
    assert_eq!(
        code,
        Some(0),
        "`tcr status --json` on a fresh HOME must exit 0 (TcrBar renders any other \
         exit as a failed poll). stderr={stderr}"
    );

    // The file is on disk afterwards, and it is a config a later run can read —
    // asserted by parsing it, not by `exists()`, so a zero-byte or half-written
    // file cannot pass.
    let written = std::fs::read_to_string(&config)
        .unwrap_or_else(|err| panic!("no config at {} after the run: {err}", config.display()));
    let parsed: serde_json::Value =
        serde_json::from_str(&written).unwrap_or_else(|err| panic!("unparseable config: {err}"));
    assert_eq!(
        parsed.get("accounts").and_then(|a| a.as_array()),
        Some(&vec![]),
        "a created config must hold an empty accounts array, got {written}"
    );

    // stdout stays a decodable fleet array for TcrBar and for `jq` — never an
    // error string. Its LENGTH is deliberately not asserted here: `status`
    // probes the configured port, and a developer box may have a real proxy
    // answering on it, whose fleet is not this tempdir's. `tcr accounts` below
    // is the offline verb and carries the empty-fleet assertion instead.
    // Nothing from that stdout is echoed into a panic message: it can hold real
    // account names.
    let fleet: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|_| panic!("`status --json` stdout is not a JSON document"));
    assert!(
        fleet.is_array(),
        "`status --json` must emit a JSON array, not an object or a string"
    );
    assert!(
        stderr.contains("no accounts configured — run `tcr login` to add one"),
        "the empty-fleet hint must be on stderr, got: {stderr}"
    );
    assert!(
        stderr.contains("created") && stderr.contains("teamclaude.json"),
        "creating the config must say so once, got: {stderr}"
    );
}

/// `tcr accounts` is the offline half of the same first run: no port probe, so
/// its empty table and its exit code are this tempdir's alone.
#[test]
fn accounts_with_no_config_exits_zero_with_an_empty_table() {
    let home = tempfile::tempdir().expect("a scratch HOME must be creatable");
    let (code, stdout, stderr) = run_tcr(home.path(), &["accounts"]);
    assert_eq!(
        code,
        Some(0),
        "`tcr accounts` on a fresh HOME must exit 0. stdout={stdout} stderr={stderr}"
    );
    assert!(
        !stdout.contains("Error"),
        "stdout must stay the ordinary (empty) table: {stdout}"
    );
    assert!(
        stderr.contains("no accounts configured — run `tcr login` to add one"),
        "the empty-fleet hint must be on stderr, got: {stderr}"
    );
    assert!(
        config_path_in(home.path()).exists(),
        "`tcr accounts` must create the config it could not find"
    );
}

/// The second run must be an ordinary run: no second "created" line, still
/// exit 0, and the file it wrote the first time is what it reads back.
#[test]
fn the_second_run_reuses_the_file_it_created() {
    let home = tempfile::tempdir().expect("a scratch HOME must be creatable");
    let (first, _, _) = run_tcr(home.path(), &["accounts"]);
    assert_eq!(first, Some(0), "first `tcr accounts` must exit 0");
    let after_first = std::fs::read(config_path_in(home.path())).expect("the config was created");

    let (second, _, stderr) = run_tcr(home.path(), &["accounts"]);
    assert_eq!(second, Some(0), "second `tcr accounts` must exit 0");
    assert!(
        !stderr.contains("created"),
        "an existing config must not be reported as created: {stderr}"
    );
    assert_eq!(
        std::fs::read(config_path_in(home.path())).expect("the config is still there"),
        after_first,
        "a read-only verb must leave the file byte-for-byte alone"
    );
}
