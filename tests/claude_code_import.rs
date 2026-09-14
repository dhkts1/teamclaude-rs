//! Proves the point of the feature: a machine that already has a Claude Code
//! login does not need `tcr login` at all. The first verb that finds an empty
//! fleet imports that login and carries on.
//!
//! End-to-end against the BUILT BINARY, because the defect this guards is not
//! in any one function — it is in whether the verbs are WIRED to the import at
//! all. Every unit test in `src/oauth.rs` stays green with the call site
//! deleted from `cli::list_accounts`.
//!
//! # Isolation
//!
//! Three things are pinned on the spawned [`Command`], never on this test
//! process:
//!
//! * `HOME` → a fresh [`tempfile::TempDir`], so the real
//!   `~/.config/teamclaude.json` (working credentials for real accounts) is
//!   never read or written.
//! * `TCR_CLAUDE_CODE_CREDENTIALS` → a fake credentials file inside that
//!   tempdir. A scratch `HOME` does NOT scratch the login Keychain: without
//!   this, the child would read the developer's real Claude Code login and
//!   import it into the config under test.
//! * `proxy.port` in the config the test writes → a port nothing is listening
//!   on, obtained by binding and releasing one. The import probes that port for
//!   a live account-add route before writing (`oauth::login_route`), and this
//!   machine may well have a real proxy on the default 3456 — which this test
//!   must neither talk to nor add an account to.
//!
//! That last pin is why the config exists at all here rather than being created
//! by the run: the bridge's "no config file" case cannot pin a port, so it
//! would probe whatever is on 3456. The branch under test is the same one
//! either way — `load_or_init` returning a config with zero accounts in it.
use std::path::{Path, PathBuf};
use std::process::Command;

/// A fake Claude Code credentials document — obviously-fake values, and the
/// key names measured on a real install.
const FAKE_CREDENTIALS: &str = r#"{
  "mcpOAuth": {},
  "claudeAiOauth": {
    "accessToken": "at-fake-claude-code-access",
    "refreshToken": "rt-fake-claude-code-refresh",
    "expiresAt": 1789000000000,
    "refreshTokenExpiresAt": 1791000000000,
    "scopes": ["user:inference", "user:profile"],
    "subscriptionType": "max"
  }
}"#;

fn config_path_in(home: &Path) -> PathBuf {
    home.join(".config").join("teamclaude.json")
}

/// A port nothing is listening on: bound to learn the number, then released.
fn dead_port() -> u16 {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").expect("binding an ephemeral port must work");
    let port = listener
        .local_addr()
        .expect("a bound listener has an address")
        .port();
    drop(listener);
    port
}

/// A scratch `HOME` holding a zero-account config on a dead port and a fake
/// Claude Code credentials file. Returns the tempdir and the credentials path.
fn scratch_home() -> (tempfile::TempDir, PathBuf) {
    let home = tempfile::tempdir().expect("a scratch HOME must be creatable");
    let config = config_path_in(home.path());
    std::fs::create_dir_all(config.parent().expect("the config has a parent"))
        .expect("creating the scratch .config dir");
    std::fs::write(
        &config,
        format!(
            r#"{{ "proxy": {{ "port": {} }}, "accounts": [] }}"#,
            dead_port()
        ),
    )
    .expect("writing the scratch config");

    let credentials = home.path().join(".claude").join(".credentials.json");
    std::fs::create_dir_all(credentials.parent().expect("it has a parent"))
        .expect("creating the scratch .claude dir");
    std::fs::write(&credentials, FAKE_CREDENTIALS).expect("writing the fake credentials");
    (home, credentials)
}

/// Run `tcr <args…>` against a scratch `HOME`, with the credential store
/// pinned to `credentials`. Returns (exit code, stdout, stderr).
fn run_tcr(home: &Path, credentials: &Path, args: &[&str]) -> (Option<i32>, String, String) {
    let bin = env!("CARGO_BIN_EXE_tcr");
    let out = Command::new(bin)
        .args(args)
        .env("HOME", home)
        .env("TCR_CLAUDE_CODE_CREDENTIALS", credentials)
        .env_remove("XDG_CACHE_HOME")
        .output()
        .unwrap_or_else(|err| panic!("spawning the built tcr binary ({bin}) failed: {err}"));
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn accounts_of(config: &Path) -> Vec<serde_json::Value> {
    let written = std::fs::read_to_string(config)
        .unwrap_or_else(|err| panic!("no config at {}: {err}", config.display()));
    let parsed: serde_json::Value =
        serde_json::from_str(&written).unwrap_or_else(|err| panic!("unparseable config: {err}"));
    parsed
        .get("accounts")
        .and_then(|a| a.as_array())
        .cloned()
        .unwrap_or_default()
}

/// Mutation note: delete the `import_claude_code_login_if_empty` call from
/// `cli::list_accounts` and this fails on the account count — the config keeps
/// the empty `accounts` array it started with.
#[test]
fn a_first_run_imports_the_machines_claude_code_login() {
    let (home, credentials) = scratch_home();
    let config = config_path_in(home.path());
    assert!(
        accounts_of(&config).is_empty(),
        "the scratch config must start empty — this test proves the FIRST run"
    );

    let (code, stdout, stderr) = run_tcr(home.path(), &credentials, &["accounts"]);
    assert_eq!(
        code,
        Some(0),
        "`tcr accounts` must still exit 0 while importing. stdout={stdout} stderr={stderr}"
    );

    let accounts = accounts_of(&config);
    assert_eq!(
        accounts.len(),
        1,
        "the import must leave exactly one account behind, got {}",
        accounts.len()
    );
    assert_eq!(
        accounts[0].get("refreshToken"),
        Some(&serde_json::json!("rt-fake-claude-code-refresh")),
        "the imported row must carry the credential's refresh token"
    );
    assert_eq!(
        accounts[0].get("expiresAt"),
        Some(&serde_json::json!(1_789_000_000_000i64)),
        "the imported row's expiry must be the credential's own `expiresAt`, in ms"
    );

    assert!(
        stderr.contains("imported") && stderr.contains("Claude Code login"),
        "the import must say so, on stderr: {stderr}"
    );
    assert!(
        stderr.contains("log in again"),
        "the single-use refresh-token consequence must be stated: {stderr}"
    );
    assert!(
        !stderr.contains("no accounts configured"),
        "a run that imported an account must not also print the empty-fleet hint: {stderr}"
    );
    assert!(
        !stdout.contains("rt-fake-claude-code-refresh")
            && !stderr.contains("rt-fake-claude-code-refresh"),
        "no output may ever carry token material"
    );
}

/// Idempotence, which is what keeps a deliberate `tcr remove` from being undone
/// behind the user's back: the second run has accounts, so it imports nothing.
#[test]
fn a_second_run_imports_nothing() {
    let (home, credentials) = scratch_home();
    let config = config_path_in(home.path());

    let (first, _, _) = run_tcr(home.path(), &credentials, &["accounts"]);
    assert_eq!(first, Some(0), "the first run must exit 0");
    let after_first = std::fs::read(&config).expect("the config is there after the first run");

    let (second, _, stderr) = run_tcr(home.path(), &credentials, &["accounts"]);
    assert_eq!(second, Some(0), "the second run must exit 0");
    assert!(
        !stderr.contains("imported"),
        "a config that already has an account must not be imported into again: {stderr}"
    );
    assert_eq!(
        std::fs::read(&config).expect("the config is still there"),
        after_first,
        "a non-importing run must leave the file byte-for-byte alone"
    );
    assert_eq!(accounts_of(&config).len(), 1, "still exactly one account");
}

/// No login on the machine: the ordinary first-run hint, and nothing written.
#[test]
fn no_claude_code_login_leaves_the_first_run_hint_alone() {
    let (home, _) = scratch_home();
    let config = config_path_in(home.path());
    let absent = home.path().join("no-such-credentials.json");

    let (code, _, stderr) = run_tcr(home.path(), &absent, &["accounts"]);
    assert_eq!(code, Some(0), "`tcr accounts` must exit 0. stderr={stderr}");
    assert!(
        accounts_of(&config).is_empty(),
        "nothing may be written when there is no login to import"
    );
    assert!(
        stderr.contains("no accounts configured — run `tcr login` to add one"),
        "the empty-fleet hint must print when no login was found: {stderr}"
    );
}

/// A credentials document that exists and cannot be read is ONE warning line,
/// not a non-zero exit — TcrBar renders any non-zero `tcr status` as a failed
/// poll.
#[test]
fn an_unreadable_credentials_document_warns_and_carries_on() {
    let (home, credentials) = scratch_home();
    std::fs::write(&credentials, "{not json at all").expect("writing a corrupt document");

    let (code, _, stderr) = run_tcr(home.path(), &credentials, &["accounts"]);
    assert_eq!(
        code,
        Some(0),
        "an unreadable credential store must not fail the verb. stderr={stderr}"
    );
    assert!(
        stderr.contains("warning") && stderr.contains("Claude Code login"),
        "it must warn once, naming what it could not read: {stderr}"
    );
    assert!(
        stderr.contains("no accounts configured — run `tcr login` to add one"),
        "and still print the ordinary empty-fleet hint: {stderr}"
    );
}
