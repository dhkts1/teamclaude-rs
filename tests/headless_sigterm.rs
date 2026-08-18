//! Proves the SIGTERM regression `src/main.rs`'s headless `select!` fixes: a
//! supervised stop (TcrBar's `process.terminate()`) must fall through to
//! `handle.shutdown()`, not hard-kill the process.
//!
//! This is deliberately an end-to-end test of the BUILT BINARY, not a unit
//! test of a hand-written data structure. A literal list of trigger names
//! living beside the `select!` can drift from the `select!` itself with
//! nothing to notice — this test instead spawns the real `tcr` process,
//! sends it a real `SIGTERM`, and checks the two externally-observable
//! effects a graceful shutdown (and only a graceful shutdown) produces: the
//! "SIGTERM received; shutting down" log line, and the port-owner claim file
//! being withdrawn. Deleting the SIGTERM arm from the production `select!`
//! makes both assertions fail, because nothing in the running process would
//! react to the signal at all — see the mutation note on the test itself.
//!
//! # Isolation
//!
//! The spawned process gets `HOME` pointed at a fresh [`tempfile::TempDir`]
//! and `XDG_CACHE_HOME` explicitly removed, via [`std::process::Command::env`]
//! / [`std::process::Command::env_remove`] — set on the `Command` itself, not
//! the test harness's own environment, so this process's real
//! `~/.config`/`~/.cache/teamclaude` (and the live proxy that may be reading
//! them) are never touched. `--port 0` binds an OS-assigned ephemeral port,
//! never the live proxy's `3456`; the bound port is recovered by parsing the
//! process's own "listening on http://…" log line, never guessed.
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, Signal, System};

use teamclaude_rs::singleton::owner_path_in;

/// `handle.shutdown()` is `shutdown_within(DEFAULT_SHUTDOWN_GRACE)` = 5s
/// (`src/server.rs`). Give the process comfortably longer than that to exit
/// on its own before this test gives up and force-kills it — a timeout here
/// must mean "it hung", not "5s was too tight a margin".
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);
const BOOT_TIMEOUT: Duration = Duration::from_secs(20);

/// Reads a child's stdout on a background thread and hands lines back over a
/// channel, so the test can wait for a specific line without blocking
/// forever on a `read` the child may never satisfy (e.g. if it never prints
/// the line this test is looking for).
fn stream_stdout(child: &mut Child) -> mpsc::Receiver<String> {
    let stdout = child.stdout.take().expect("stdout was piped");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

/// Pulls the bound port out of `tracing::info!("teamclaude-rs listening on
/// http://{bound} (headless)")` (`src/main.rs`). Matches the literal marker
/// text rather than a general "any 5-digit number" regex, so a change to the
/// log's wording fails this test loudly (wrong port) instead of silently
/// reading a different number out of the line.
fn parse_listening_port(line: &str) -> Option<u16> {
    let marker = "listening on http://";
    let after = &line[line.find(marker)? + marker.len()..];
    let host_port = after.split_whitespace().next()?;
    host_port.rsplit(':').next()?.parse().ok()
}

/// The MITM cert files under the REAL `~/.config` this test process itself
/// inherited — named so isolation can be proven, not assumed. `.env("HOME",
/// …)` on the spawned `Command` is a claim about the CHILD's environment;
/// it says nothing about whether the child actually honoured it. A test
/// that only ever asserted against the tempdir would pass identically
/// whether or not `HOME` isolation worked, which is the same defect class
/// the fixture-only version of this test had.
fn real_home_cert_paths() -> Option<[PathBuf; 2]> {
    let real_home = std::env::var_os("HOME")?;
    let config_dir = PathBuf::from(real_home).join(".config");
    Some([
        config_dir.join("tcr-ca.pem"),
        config_dir.join("tcr-leaf.pem"),
    ])
}

fn mtimes(paths: &[PathBuf]) -> Vec<Option<SystemTime>> {
    paths
        .iter()
        .map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .collect()
}

/// Waits up to `timeout` for a line matching `predicate` to arrive on `rx`,
/// draining (and discarding) everything else. Returns the matching line.
fn wait_for_line(
    rx: &mpsc::Receiver<String>,
    timeout: Duration,
    predicate: impl Fn(&str) -> bool,
) -> Option<String> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
            Ok(line) => {
                if predicate(&line) {
                    return Some(line);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return None,
        }
    }
}

/// A supervised SIGTERM (TcrBar's `process.terminate()`) must drain: the
/// graceful-shutdown log line appears, the port-owner claim is withdrawn,
/// and the process exits on its own — never needing this test to escalate
/// to SIGKILL.
///
/// # Watched this fail against the production defect
///
/// Reverting the SIGTERM arm out of the headless `select!` in
/// `src/main.rs::run_server` (i.e. back to racing only `ctrl_c()` and
/// `serving_stopped()`) makes this test fail: the spawned process receives
/// SIGTERM with no handler installed, the default disposition kills it
/// immediately, and `wait_for_line` times out waiting for "SIGTERM received;
/// shutting down" — which never gets printed because `handle.shutdown()`
/// never runs. The failure text this test produces in that case is quoted in
/// the PR description; it is not a hypothesis, it was observed.
#[test]
fn a_supervised_sigterm_drains_before_the_process_exits() {
    let home = tempfile::tempdir().expect("a scratch HOME must be creatable");
    let bin = env!("CARGO_BIN_EXE_tcr");

    // Isolation must be PROVEN, not assumed from having written `.env()`
    // correctly: snapshot the real HOME's cert mtimes now, and diff them
    // again after the child has minted its own (into the tempdir) and
    // exited. If the child were ever spawned against the real HOME, this
    // is what would have caught it — see the incident note in the PR body.
    let real_certs = real_home_cert_paths();
    let real_certs_before = real_certs.as_ref().map(|p| mtimes(p));

    let mut child = Command::new(bin)
        .args(["--headless", "--port", "0", "--no-replace"])
        // Isolates config, the MITM cert dir, and the affinity/owner-file
        // cache dir all at once — every one of them resolves off `HOME`
        // (`config::default_path`, `mitm::config_dir`, `affinity::default_path`).
        .env("HOME", home.path())
        .env_remove("XDG_CACHE_HOME")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|err| panic!("spawning the built tcr binary ({bin}) failed: {err}"));

    let stdout = stream_stdout(&mut child);

    let listening_line = wait_for_line(&stdout, BOOT_TIMEOUT, |line| {
        line.contains("listening on http://")
    })
    .unwrap_or_else(|| {
        let _ = child.kill();
        panic!(
            "tcr never printed a 'listening on http://' line within {BOOT_TIMEOUT:?} \
             — cannot recover the bound port to check the owner-file claim"
        );
    });
    let port = parse_listening_port(&listening_line).unwrap_or_else(|| {
        let _ = child.kill();
        panic!("could not parse a port out of the listening line: {listening_line:?}")
    });

    let owner_dir = home.path().join(".cache").join("teamclaude");
    let owner_path = owner_path_in(&owner_dir, port);
    assert!(
        owner_path.exists(),
        "the port-owner claim must exist once the process reports listening: {}",
        owner_path.display()
    );

    // Isolation proven POSITIVELY, not just "the real files didn't move":
    // the child must have actually minted its MITM cert pair UNDER THE
    // TEMPDIR, which only happens if it honoured the `HOME` this test set.
    let tempdir_ca = home.path().join(".config").join("tcr-ca.pem");
    assert!(
        tempdir_ca.is_file(),
        "the child never minted a cert under the scratch HOME ({}) — either it \
         did not honour HOME, or it read/wrote the real ~/.config instead",
        tempdir_ca.display()
    );

    let pid = child.id();
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
        true,
        ProcessRefreshKind::nothing(),
    );
    let process = sys
        .process(Pid::from_u32(pid))
        .unwrap_or_else(|| panic!("the just-spawned child (pid {pid}) is not visible to sysinfo"));
    assert_eq!(
        process.kill_with(Signal::Term),
        Some(true),
        "SIGTERM must be deliverable to the freshly spawned child (pid {pid})"
    );

    let graceful_line = wait_for_line(&stdout, SHUTDOWN_TIMEOUT, |line| {
        line.contains("SIGTERM received; shutting down")
    });

    // Wait for the process to exit on its own. `Child` has no `wait_timeout`
    // in std, so poll `try_wait` — but never send SIGKILL ourselves: an
    // exit-code assertion below is only meaningful if nothing here forced it.
    let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().expect("polling child exit status") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!(
                "process did not exit within {SHUTDOWN_TIMEOUT:?} of SIGTERM — the shutdown \
                 grace is 5s, so this means it hung rather than merely needing a kill"
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    assert!(
        graceful_line.is_some(),
        "never saw \"SIGTERM received; shutting down\" — the headless select! either has no \
         SIGTERM arm, or it fired without falling through to the shared shutdown log"
    );
    assert!(
        status.success(),
        "a graceful SIGTERM shutdown must exit 0, got {status:?}"
    );
    assert!(
        !owner_path.exists(),
        "the port-owner claim must be withdrawn by handle.shutdown() on a graceful SIGTERM \
         (still present at {})",
        owner_path.display()
    );

    // Isolation proven NEGATIVELY too: whatever this test process's real
    // `~/.config` cert files looked like before the child ran, they must
    // look identical now — same files present/absent, same mtimes. This is
    // the assertion that would have caught the manual-probe incident this
    // PR's history includes: a `Command::env()` mistake big enough to spawn
    // against the real HOME regenerates these files, and a passing test
    // that never checked would have hidden it.
    if let (Some(paths), Some(before)) = (real_certs.as_ref(), real_certs_before.as_ref()) {
        let after = mtimes(paths);
        assert_eq!(
            &after, before,
            "the real ~/.config MITM cert files changed mtime during this test — the \
             spawned child was NOT isolated to the scratch HOME: {paths:?}"
        );
    }
}
