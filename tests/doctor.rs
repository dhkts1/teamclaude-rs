//! `tcr doctor` end to end, against a scratch HOME and a fake status endpoint.
//!
//! This tests the BUILT BINARY rather than the functions in `src/doctor.rs`
//! (which carry their own unit tests) because the whole verb is its exit code
//! and its printed lines: a script asks `tcr doctor >/dev/null || open the
//! settings file`, and the panel will shell out to `tcr doctor --json`. Neither
//! contract exists inside a library function.
//!
//! # Isolation
//!
//! Same rules as `tests/first_run_no_config.rs`: `HOME` points at a fresh
//! [`tempfile::TempDir`], the config is written into that temp tree and passed
//! explicitly, and the port under test is a kernel-assigned one bound by this
//! test process. Nothing here reads the operator's real
//! `.config/teamclaude.json`, and no test here names a port literal at all:
//! every port comes from `TcpListener::bind("127.0.0.1:0")`, so the live proxy
//! is never the thing a test connects to.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::Command;

/// A port nothing is listening on: bound from the kernel's ephemeral range and
/// released immediately.
///
/// Never a literal. The live proxy on this machine serves on the configured
/// default port, and a test that wrote that number would have `tcr status` and
/// `tcr doctor` talk to it.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a scratch port");
    listener.local_addr().expect("local addr").port()
}

/// A config with no accounts and one port. `tcr doctor` never rotates, probes
/// or refreshes, so an empty fleet is the honest fixture: the verb's subject is
/// the ROUTE, not the accounts.
fn write_config(home: &Path, port: u16) -> std::path::PathBuf {
    let dir = home.join(".config");
    std::fs::create_dir_all(&dir).expect("create the config directory");
    let path = dir.join("teamclaude.json");
    std::fs::write(
        &path,
        format!("{{\n  \"accounts\": [],\n  \"proxy\": {{ \"port\": {port} }}\n}}\n"),
    )
    .expect("write the config");
    path
}

fn write_settings(home: &Path, base_url: &str) -> std::path::PathBuf {
    let dir = home.join(".claude");
    std::fs::create_dir_all(&dir).expect("create the settings directory");
    let path = dir.join("settings.json");
    std::fs::write(
        &path,
        format!("{{\n  \"env\": {{ \"ANTHROPIC_BASE_URL\": \"{base_url}\" }}\n}}\n"),
    )
    .expect("write settings.json");
    path
}

/// Run `tcr <args…>` with a scratch `HOME`, a scratch working directory of its
/// own, and `ANTHROPIC_BASE_URL` set or cleared explicitly.
///
/// The working directory is a `work` subdirectory rather than the scratch HOME
/// itself, and that is load-bearing twice over. It keeps the REAL checkout's
/// `.claude` directory from being read as this test's project settings; and it
/// keeps `<cwd>/.claude/settings.json` from BEING `<home>/.claude/settings.json`,
/// which made a test written to exercise the user settings silently exercise the
/// project ones instead (and report the kernel's canonical `/private/var` path
/// for them, since `current_dir` resolves symlinks and `HOME` does not).
///
/// Both variables are set on the spawned `Command`, never on this test process:
/// a test that exported one into its own environment would leak it into every
/// other test in this binary.
/// A working directory for the child, inside the scratch HOME and holding no
/// `.claude` of its own.
fn work_dir(home: &Path) -> std::path::PathBuf {
    let path = home.join("work");
    std::fs::create_dir_all(&path).expect("create the scratch working directory");
    path
}

fn run_tcr(
    home: &Path,
    base_url_env: Option<&str>,
    args: &[&str],
) -> (Option<i32>, String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tcr"));
    command
        .args(args)
        .env("HOME", home)
        .env_remove("XDG_CACHE_HOME")
        // A scratch HOME does not scratch the login Keychain, see
        // `tests/first_run_no_config.rs` for why this override is set to a path
        // that does not exist.
        .env(
            "TCR_CLAUDE_CODE_CREDENTIALS",
            home.join("no-credentials.json"),
        )
        .current_dir(work_dir(home));
    match base_url_env {
        Some(url) => command.env("ANTHROPIC_BASE_URL", url),
        None => command.env_remove("ANTHROPIC_BASE_URL"),
    };
    let out = command.output().expect("run tcr");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A `/_tcr/status` endpoint on a kernel-assigned port, answering every
/// connection with a payload carrying `served` requests in the newest minute of
/// one session's sparkline. Returns the port it bound.
///
/// Hand-written HTTP rather than the real server: standing up
/// `server::serve` would need accounts, a manager and a bind of its own, and
/// what is under test is the CLIENT's reading of a payload. The bytes are the
/// contract (`crate::status::STATUS_KIND`), so they are written out here
/// literally, so a change to that contract must fail this test loudly.
///
/// The accept loop never ends and the handle is dropped on purpose. An earlier
/// version served exactly one connection and then returned, which dropped the
/// listener the instant `tcr doctor` finished reading it: the verb then
/// enumerated the port, found nobody holding it, and printed `portHolder:
/// none` about a socket that had been open a millisecond earlier. The thread
/// dies with the test binary.
fn fake_status_endpoint(served: u16) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a scratch port");
    let port = listener.local_addr().expect("local addr").port();
    let mut sparkline = vec![0u16; 30];
    sparkline[29] = served;
    let body = format!(
        "{{\"kind\":\"tcr.status.v1\",\"accounts\":[],\"sessions\":[{{\"sessionId\":\"11111111-1111-1111-1111-111111111111\",\"firstSeenMs\":0,\"lastSeenMs\":0,\"reqPerMinute\":{sparkline:?}}}]}}"
    );
    std::thread::spawn(move || loop {
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        serve_one(stream, &body);
    });
    port
}

fn serve_one(mut stream: TcpStream, body: &str) {
    // Drain the request head so the client's write completes before we answer.
    let peek = stream.try_clone().expect("clone the stream");
    let mut reader = BufReader::new(peek);
    let mut line = String::new();
    while reader.read_line(&mut line).is_ok_and(|read| read > 0) {
        if line == "\r\n" || line == "\n" {
            break;
        }
        line.clear();
    }
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _written = stream.write_all(response.as_bytes());
    let _flushed = stream.flush();
}

/// A prior failure, reproduced: a settings file points Claude Code at
/// another gateway, so the proxy is healthy and serving nobody. `doctor` must
/// exit 2 and name the FILE: the operator's next action is to open it.
#[test]
fn a_settings_file_pointing_elsewhere_exits_two_and_names_the_file() {
    let home = tempfile::tempdir().expect("temp home");
    let settings = write_settings(home.path(), "https://gateway.example.com");
    let port = free_port();
    let config = write_config(home.path(), port);

    let (code, stdout, stderr) = run_tcr(
        home.path(),
        Some(&format!("http://127.0.0.1:{port}")),
        &["doctor", "--config", &config.display().to_string()],
    );

    assert_eq!(code, Some(2), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout.contains(&format!("baseUrlSource: {}", settings.display())),
        "the source line must name the settings file:\n{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "verdict: this proxy is not on Claude's route: {} sets https://gateway.example.com",
            settings.display()
        )),
        "the verdict must name the file and the URL it sets:\n{stdout}"
    );
    assert!(
        stdout.contains("baseUrl: https://gateway.example.com"),
        "{stdout}"
    );
}

/// The healthy case: nothing in the scratch HOME overrides the route, the
/// environment points at a proxy on this machine, and that proxy answers.
#[test]
fn a_route_to_an_answering_proxy_exits_zero_and_reports_the_requests() {
    let home = tempfile::tempdir().expect("temp home");
    let port = fake_status_endpoint(3);
    let config = write_config(home.path(), port);

    let (code, stdout, stderr) = run_tcr(
        home.path(),
        Some(&format!("http://127.0.0.1:{port}")),
        &["doctor", "--config", &config.display().to_string()],
    );

    assert_eq!(code, Some(0), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("requests10m: 3"), "{stdout}");
    assert!(
        stdout.contains(&format!(
            "verdict: Claude is routed to http://127.0.0.1:{port} (ANTHROPIC_BASE_URL); this proxy has served 3 requests"
        )),
        "{stdout}"
    );
    // The port holder is this test process, which is not a tcr, and doctor must
    // say so rather than assume the listener it found is ours.
    assert!(stdout.contains("portHolderIsTcr: no"), "{stdout}");
    assert!(
        stdout.contains(&format!("portHolder: pid {}", std::process::id())),
        "the holder must be named by pid, and it is this test process:\n{stdout}"
    );
}

/// Routed here, nothing listening: exit 3, and the request count reads `none`
/// rather than a fabricated zero.
#[test]
fn a_route_here_with_no_proxy_exits_three() {
    let home = tempfile::tempdir().expect("temp home");
    let port = free_port();
    let config = write_config(home.path(), port);

    let (code, stdout, stderr) = run_tcr(
        home.path(),
        Some(&format!("http://127.0.0.1:{port}")),
        &["doctor", "--config", &config.display().to_string()],
    );

    assert_eq!(code, Some(3), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("requests10m: none"), "{stdout}");
    assert!(stdout.contains("portHolder: none"), "{stdout}");
    assert!(
        stdout.contains("no proxy answered on 127.0.0.1:"),
        "{stdout}"
    );
}

/// `--json` carries the same decision as one object, including the exit code,
/// so the panel reads one document rather than parsing lines.
#[test]
fn the_json_form_carries_the_verdict_and_the_exit_code() {
    let home = tempfile::tempdir().expect("temp home");
    write_settings(home.path(), "https://gateway.example.com");
    let config = write_config(home.path(), free_port());

    let (code, stdout, stderr) = run_tcr(
        home.path(),
        None,
        &[
            "doctor",
            "--json",
            "--config",
            &config.display().to_string(),
        ],
    );

    assert_eq!(code, Some(2), "stdout:\n{stdout}\nstderr:\n{stderr}");
    let document: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("--json must emit one JSON object ({e}):\n{stdout}"));
    assert_eq!(document["exitCode"], 2);
    assert_eq!(document["routedHere"], false);
    assert_eq!(document["baseUrl"], "https://gateway.example.com");
    assert_eq!(document["baseUrlSource"]["kind"], "userSettings");
    assert_eq!(document["requestsInWindow"], serde_json::Value::Null);
    assert!(
        document["verdict"].as_str().is_some_and(
            |verdict| verdict.starts_with("verdict: this proxy is not on Claude's route")
        )
    );
}

/// A `tcr status` glance shows the route problem, first
/// line, so nobody has to know `doctor` exists to be told the proxy they are
/// reading is not the one their Claude talks to.
#[test]
fn tcr_status_leads_with_the_verdict_when_the_route_is_elsewhere() {
    let home = tempfile::tempdir().expect("temp home");
    let settings = write_settings(home.path(), "https://gateway.example.com");
    let config = write_config(home.path(), free_port());

    let (code, stdout, stderr) = run_tcr(
        home.path(),
        None,
        &["status", "--config", &config.display().to_string()],
    );

    assert_eq!(code, Some(0), "status still exits 0:\n{stdout}\n{stderr}");
    let first = stdout.lines().next().unwrap_or_default();
    assert_eq!(
        first,
        format!(
            "verdict: this proxy is not on Claude's route: {} sets https://gateway.example.com",
            settings.display()
        ),
        "the verdict must be the FIRST line of `tcr status`:\n{stdout}"
    );
    assert!(
        stdout.contains("status source="),
        "the fleet view still follows it:\n{stdout}"
    );
}

/// The `--json` fleet contract is a bare array, and the verdict line must never
/// appear in it: `tcr status --json | jq` is what the panel runs.
#[test]
fn tcr_status_json_stays_a_bare_array() {
    let home = tempfile::tempdir().expect("temp home");
    write_settings(home.path(), "https://gateway.example.com");
    let config = write_config(home.path(), free_port());

    let (code, stdout, stderr) = run_tcr(
        home.path(),
        None,
        &[
            "status",
            "--json",
            "--config",
            &config.display().to_string(),
        ],
    );

    assert_eq!(code, Some(0), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        !stdout.contains("verdict:"),
        "the json form carries no verdict line:\n{stdout}"
    );
    let document: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("bare array ({e}):\n{stdout}"));
    assert!(document.is_array(), "{stdout}");
}
