//! The two-process end-to-end: two real `tcr` servers on one box, driven only
//! through the CLI and the peer port.
//!
//! Every other peer test in this tree reaches for a library function. This one
//! may not: it spawns the `tcr` this build produced (`CARGO_BIN_EXE_tcr`) twice,
//! points each copy at its own scratch `HOME`, and then types at them: `tcr
//! peer name`, `find`, `pair`, `pending`, `accept`, `invite`, `join`, `share`,
//! `lend`, `allow`, `forget`: reading the answer off stdout, off each process's
//! own log, and off the fake upstream. That is the surface an operator has, and
//! three of the findings below are invisible from inside the library.
//!
//! # Isolation
//!
//! Same rules as `tests/headless_sigterm.rs` and `tests/first_run_no_config.rs`,
//! set on the spawned [`Command`] and never on this process: `HOME` is a fresh
//! [`tempfile::TempDir`], `XDG_CACHE_HOME` is a directory under it (so the
//! affinity pins, the usage ledger, the owner claim and `peer-state.json` all
//! land in the tempdir), and `TCR_CLAUDE_CODE_CREDENTIALS` names a file that
//! does not exist: a scratch `HOME` does not scratch the login Keychain, and
//! on a developer's Mac that read finds a REAL credential. Both proxies bind
//! `--port 0`, which also short-circuits the whole incumbent question in
//! `server::serve`, so the live proxy on `127.0.0.1:3456` is never probed, never
//! signalled and never connected to. The peer listeners bind two ports the
//! kernel just handed back on `127.0.0.1`. The upstream is an axum router in
//! this test binary. Nothing here reads the operator's config directory or
//! cache directory, and no
//! binary is copied anywhere.
//!
//! Accounts are obviously fake (`lender-fake`, `at-fake-lender`, uuids of one
//! repeated digit): this repository is public.
//!
//! # What this found that a library test could not
//!
//! Four gaps, each named at the assertion that pins today's behaviour. The
//! assertions are written so that the day
//! one is FIXED, this test goes red and says which line to flip.
//!
//! 1. `pair::confirm` (`src/peer/pair.rs:670`) pins with `None` for the
//!    address, so a Mac trusted through the six-digit compare has a row it can
//!    never dial: `fallback::peer_lease_provider` wants `!addrs.is_empty()` and
//!    `serve::dial_peer` has nothing to connect to. The invite/join path does
//!    record one, which is why the borrowing leg below enrols that way.
//! 2. `pair::mint_invite_as` (`:242`) copies `listen` into the token verbatim,
//!    so a peers file holding `127.0.0.1:0` (or `0.0.0.0:9600`) mints a join key
//!    whose address nobody can dial. This test writes a fixed loopback port for
//!    that reason, which is worth knowing before an operator is told to use
//!    `:0`.
//! 3. Closed: the peer-lease fallback used to install at boot only, so a Mac
//!    that paired while its proxy was up could not borrow until it restarted.
//!    `fallback::install_late_if_the_file_now_allows_it` now re-asks the same
//!    question the next time the dry-fleet arm needs an answer, so step 7
//!    below runs no restart at all: it grants `disclose` on a live process and
//!    the very next request is served through the provider that install call
//!    put there.
//! 4. `listener::serve_control` (`src/peer/listener.rs:1486`) answers `Hello`
//!    and `Ping` and refuses everything else, so a real `Control::LeaseRequest`
//!    (the first frame of every borrow), is refused by the lender with no log
//!    line of its own. Step 7 asserts the served 200 directly (the diagnostic
//!    probe this file used to gate behind `TCR_E2E_EXPECT_LEASE_ARM=1` is now
//!    the assertion, unconditional): until LEASE-WIRE's arm lands in this
//!    tree, this test is RED there, and the failure names the line above.
//! 5. `serve::say_hello` (`src/peer/serve.rs:1339`) is the whole client half of
//!    the moved-peer refresh: it dials a pinned peer, sends a `Control::Hello`,
//!    and the listener's own answering half already records the caller's
//!    address (`listener.rs:1734`, wired and green today). It had no caller
//!    anywhere in this tree from the day it was written until
//!    `tcr peer hello <peer>` got wired to it, so a Mac that moved had no way to tell a
//!    peer where it went without re-pairing.
//!    `a_moved_peer_is_reached_through_a_refreshed_hello_endpoint` below runs
//!    against that verb and is no longer `#[ignore]`d. The second path, "through
//!    the beacon bridge with Hello disabled," needs a second, larger piece
//!    that is not written, not even ignored: the
//!    instance-id-to-key binding `discovery::observe_beacons`
//!    (`discovery.rs:541`) needs comes only from a session that already
//!    proved the peer's CURRENT boot's key, and nothing in this tree holds
//!    one of those across the life of a moved peer's new boot.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::response::Response;
use axum::routing::any;
use axum::Router;

/// The E2E-MATRIX scenarios' one shared line format. See that module's docs.
#[path = "tools/e2e/matrix.rs"]
mod matrix;

/// What the real API refuses a request without, shared by every fake upstream
/// in this tree. See that module's docs for why a fake upstream is strict.
#[path = "tools/api_contract.rs"]
mod api_contract;

/// How long any one "wait for this line" step may take. Generous because a
/// debug-profile `tcr` boots a manager, mints a CA and binds two sockets;
/// bounded because a step that never happens has to fail rather than hang.
const LINE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the borrowing leg keeps re-asking for a served answer: long
/// enough to cover the lender's 30-second headroom ticker
/// (`server::boot_peer_listener`), because a lender that has just learned its
/// own quota cannot grant anything until the next note.
const BORROW_SERVED_ATTEMPTS: u32 = 20;

/// One line per step, so a human watching `cargo test -- --nocapture` and
/// `scripts/peer-e2e-local.sh` see the same trace.
fn step(n: u32, what: &str) {
    println!("STEP {n}: {what}");
}

// ---------------------------------------------------------------------------
// The fake upstream
// ---------------------------------------------------------------------------

/// What the upstream was handed, per request: the path and the credential.
///
/// The credential is the whole point of the file: a borrowed request must be
/// served on the LENDER's token, and the borrower's own must never appear here
///: see [`Upstream::credentials`].
#[derive(Clone, Default)]
struct Upstream {
    seen: Arc<Mutex<Vec<(String, String)>>>,
}

impl Upstream {
    fn credentials(&self) -> Vec<String> {
        match self.seen.lock() {
            Ok(seen) => seen.iter().map(|(_, cred)| cred.clone()).collect(),
            Err(poisoned) => poisoned
                .into_inner()
                .iter()
                .map(|(_, cred)| cred.clone())
                .collect(),
        }
    }

    fn requests(&self) -> usize {
        self.seen.lock().map_or(0, |seen| seen.len())
    }
}

/// A fake `api.anthropic.com`: records the credential, answers a canned body and
/// reports a LOW utilization on all three windows, which is what gives the
/// lender a measured window to lend a fraction of.
async fn spawn_upstream() -> (String, Upstream) {
    let upstream = Upstream::default();
    let recorder = upstream.clone();
    let app = Router::new().fallback(any(move |req: axum::extract::Request| {
        let recorder = recorder.clone();
        async move {
            let path = req.uri().path().to_string();
            let credential = req
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
                .to_string();
            let api_key = req
                .headers()
                .get("x-api-key")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
                .to_string();
            // The origin's own refusal, decided before the body is drained and
            // answered after the arrival is recorded: see
            // `tests/tools/api_contract.rs`.
            let refusal = api_contract::refuse_if_incomplete(req.method(), req.headers());
            let _ = axum::body::to_bytes(req.into_body(), 1024 * 1024).await;
            if let Ok(mut seen) = recorder.seen.lock() {
                seen.push((path, format!("{credential}{api_key}")));
            }
            if let Some(refusal) = refusal {
                return refusal;
            }
            Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .header("anthropic-ratelimit-unified-status", "allowed")
                .header("anthropic-ratelimit-unified-5h-utilization", "0.10")
                .header("anthropic-ratelimit-unified-7d-utilization", "0.10")
                .header("anthropic-ratelimit-unified-7d_oi-utilization", "0.10")
                .body(Body::from(
                    br#"{"type":"message","id":"msg_fake"}"#.to_vec(),
                ))
                .expect("the canned upstream answer builds")
        }
    }));
    let listening = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the fake upstream");
    let addr = listening.local_addr().expect("the fake upstream's address");
    tokio::spawn(async move {
        let _ = axum::serve(listening, app).await;
    });
    (format!("http://{addr}"), upstream)
}

// ---------------------------------------------------------------------------
// One Mac
// ---------------------------------------------------------------------------

/// A port the kernel just handed back, released before it is written into a
/// peers file.
///
/// The peer listener needs its port in the file BEFORE the process boots
/// (`server::boot_peer_listener` returns early without `listen`), and
/// `tcr peer invite` copies that value into the token it mints: so `:0` is not
/// usable here even though the listener itself would accept it. See gap 2 in
/// this file's docs.
/// How many times one Mac may lose the peer-port race before its boot is a
/// failure rather than a retry. See [`Mac::boot`] for why the race exists at
/// all and why it is retried instead of removed.
const PORT_RACE_ATTEMPTS: u32 = 5;

fn free_loopback_port() -> u16 {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port to borrow");
    probe
        .local_addr()
        .expect("the borrowed port has an address")
        .port()
}

/// One `tcr` install: its own HOME, config, peers file, node key and logs.
struct Mac {
    label: &'static str,
    home: tempfile::TempDir,
    peers: PathBuf,
    peer_port: u16,
    /// The running server, if this Mac is booted.
    child: Option<Child>,
    /// The log of the CURRENT boot. A new file per boot, never appended to,
    /// because a restart that re-used one log makes every "wait for the boot
    /// line" match the PREVIOUS boot's line and read a dead port.
    log: PathBuf,
    boots: u32,
    /// `host:port` of this boot's proxy.
    proxy: String,
}

impl Mac {
    /// A Mac with one fake account, pointed at `upstream`.
    ///
    /// `disabled` is how the borrower gets a DRY fleet with a non-empty
    /// `accounts[]`: the dry-fleet arm is the only place a fallback provider is
    /// consulted, and a fleet of one disabled account reaches it while still
    /// proving the account count in the 429 body.
    fn new(
        label: &'static str,
        upstream: &str,
        account: &str,
        token: &str,
        disabled: bool,
    ) -> Self {
        let home = tempfile::tempdir().expect("a scratch HOME");
        let config_dir = home.path().join(".config");
        std::fs::create_dir_all(&config_dir).expect("the scratch .config");
        let disabled = if disabled {
            r#", "disabled": true"#
        } else {
            ""
        };
        std::fs::write(
            config_dir.join("teamclaude.json"),
            format!(
                r#"{{
  "proxy": {{ "port": 0 }},
  "upstream": "{upstream}",
  "quotaProbeSeconds": 0,
  "warmupSeconds": 0,
  "accounts": [
    {{
      "name": "{account}",
      "accessToken": "{token}",
      "accountUuid": "11111111-1111-1111-1111-111111111111",
      "orgUuid": "22222222-2222-2222-2222-222222222222"{disabled}
    }}
  ]
}}
"#
            ),
        )
        .expect("the scratch config writes");

        let peer_port = free_loopback_port();
        let peers = config_dir.join("tcr-peers.json");
        std::fs::write(
            &peers,
            format!("{{\"listen\":\"127.0.0.1:{peer_port}\",\"peers\":[]}}\n"),
        )
        .expect("the scratch peers file writes");
        // `config::read_or_default` refuses a peers file it did not write:
        // "has mode 644, expected 0600". A test that skipped this got a proxy
        // with no peer port and no clue why.
        std::fs::set_permissions(&peers, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .expect("the peers file takes 0600");

        Self {
            label,
            peers,
            peer_port,
            child: None,
            log: home.path().join("boot-0.log"),
            boots: 0,
            proxy: String::new(),
            home,
        }
    }

    fn env(&self, command: &mut Command) {
        command
            // Colour off at the source. A child that inherits an unset
            // `NO_COLOR` writes escape codes into its boot log even though the
            // log is a file, because the subscriber never asks whether its sink
            // is a terminal (`tracing_subscriber::fmt::Layer::default`). The
            // searchers strip escapes too (`strip_ansi`), so neither half alone
            // is load-bearing; this half also keeps the logs readable when a
            // failing run prints one.
            .env("NO_COLOR", "1")
            .env("HOME", self.home.path())
            .env("XDG_CACHE_HOME", self.home.path().join(".cache"))
            .env(
                "TCR_CLAUDE_CODE_CREDENTIALS",
                self.home
                    .path()
                    .join("no-such-claude-code-credentials.json"),
            );
    }

    /// Boot this Mac's proxy and wait until both of its sockets are up.
    ///
    /// # Why this retries
    ///
    /// [`free_loopback_port`] borrows a port from the kernel and gives it back
    /// before writing it into the peers file, so between that call and the
    /// child's own bind the number belongs to nobody, and under a full
    /// `cargo test` run, where other binaries are opening sockets at the same
    /// time, another process takes it. The proxy still comes up (it binds
    /// `:0`), the peer listener logs `could not bind the peer socket` and
    /// carries on, and this boot then waits thirty seconds for a
    /// `peer listener up` line that will never arrive. Measured at e64fd59
    /// on an otherwise-unmodified tree: two runs, two different victims
    /// (`a_chained_borrow_leaf_from_middle_middle_from_root`, then
    /// `n_macs_pair_in_a_ring`), the same panic at the same line.
    ///
    /// Reading the port off the child instead would be the better fix and is
    /// not available: the peer listener never binds at all unless `listen` is
    /// already in the peers file ([`free_loopback_port`]'s own docs), and
    /// `tcr peer invite` mints the number into a token, so `:0` cannot be
    /// deferred to the child. So the race is retried rather than removed, and
    /// the assertion below keeps it VISIBLE: a boot that needed all five
    /// attempts is not a passing test, it is a runner that has stopped having
    /// free ports.
    fn boot(&mut self) {
        assert!(
            self.child.is_none(),
            "{} is already booted; shut it down before booting again",
            self.label
        );
        let mut lost_the_port = 0;
        while !self.boot_once() {
            lost_the_port += 1;
            assert!(
                lost_the_port < PORT_RACE_ATTEMPTS,
                "{}: lost the peer port to another process {lost_the_port} times in a row. \
                 That is no longer a race, so this is a runner defect and not a retry: \
                 something on this box is taking loopback ports as fast as the kernel \
                 hands them out.",
                self.label
            );
            self.peer_port = free_loopback_port();
            self.rewrite_listen_port();
        }
    }

    /// Point the peers file at [`Self::peer_port`], leaving every other key,
    /// including any pinned rows a previous boot wrote, exactly as it was.
    fn rewrite_listen_port(&self) {
        let body = std::fs::read_to_string(&self.peers).expect("the peers file reads back");
        let mut file: serde_json::Value =
            serde_json::from_str(&body).expect("the peers file is JSON");
        file["listen"] = serde_json::Value::String(format!("127.0.0.1:{}", self.peer_port));
        std::fs::write(
            &self.peers,
            format!(
                "{}\n",
                serde_json::to_string(&file).expect("it re-serializes")
            ),
        )
        .expect("the peers file rewrites");
        std::fs::set_permissions(
            &self.peers,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )
        .expect("the peers file takes 0600");
    }

    /// One boot attempt. `false` means the peer listener lost its port to
    /// another process and the child has been stopped again; every other
    /// failure still panics, because only this one is a race worth retrying.
    fn boot_once(&mut self) -> bool {
        self.boots += 1;
        self.log = self.home.path().join(format!("boot-{}.log", self.boots));
        let out = std::fs::File::create(&self.log).expect("the boot log opens");
        let err = out.try_clone().expect("the boot log clones for stderr");
        let mut command = Command::new(env!("CARGO_BIN_EXE_tcr"));
        command
            .args(["--headless", "--port", "0", "--no-replace"])
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err));
        self.env(&mut command);
        let child = command.spawn().unwrap_or_else(|err| {
            panic!(
                "spawning the tcr this build produced ({}) failed: {err}",
                env!("CARGO_BIN_EXE_tcr")
            )
        });
        self.child = Some(child);

        let listening = self.wait_for_log("listening on http://");
        self.proxy = listening
            .rsplit_once("http://")
            .map(|(_, rest)| {
                rest.split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_string()
            })
            .unwrap_or_default();
        assert!(
            self.proxy.starts_with("127.0.0.1:"),
            "{}: could not read a loopback proxy address out of {listening:?}",
            self.label
        );
        // Re-read rather than assert while `peer_listen` is missing: the
        // waiters now skip a line the child has not finished writing, and a
        // line that arrives without the field would otherwise be read as a
        // listener on the wrong port. Bounded by the same deadline as every
        // other wait here, so a line that never grows the field still fails,
        // and fails saying which line it was.
        let deadline = Instant::now() + LINE_TIMEOUT;
        let announced = loop {
            let (which, peer_line) =
                self.wait_for_one_of(&["peer listener up", "could not bind the peer socket"]);
            if which == 1 {
                // The port went to somebody else between `free_loopback_port` and
                // this child's own bind. Stop the child and let `boot` try again on
                // a fresh number; the line is printed so a run that hit the race
                // says so in its output rather than only in a timing difference.
                println!(
                    "         {}: lost peer port {} to another process, retrying: {peer_line}",
                    self.label, self.peer_port
                );
                self.shutdown();
                return false;
            }
            if let Some(announced) = field(&peer_line, "peer_listen") {
                break announced;
            }
            assert!(
                Instant::now() < deadline,
                "{}: the peer listener line never carried peer_listen within {LINE_TIMEOUT:?}: {peer_line:?}",
                self.label
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(
            announced,
            format!("127.0.0.1:{}", self.peer_port),
            "{}: the peer listener bound an address other than the one its peers file names",
            self.label
        );
        true
    }

    /// The whole of this boot's log.
    fn log(&self) -> String {
        strip_ansi(&std::fs::read_to_string(&self.log).unwrap_or_default())
    }

    /// Block until this boot's log holds a line containing `needle`, and return
    /// that line. Fails with the log's tail, which is the only useful thing to
    /// look at when a step never happened.
    /// Block until this boot's log holds a line containing ONE of `needles`,
    /// and return which one matched with the line itself.
    ///
    /// Separate from [`Self::wait_for_log`] because a wait on a single success
    /// line cannot tell a slow boot from a boot that has already printed the
    /// reason it will never get there, and thirty seconds later the failure it
    /// reports is "no line", never the refusal that is sitting in the log.
    /// Earliest needle wins when a line matches two.
    fn wait_for_one_of(&self, needles: &[&str]) -> (usize, String) {
        let deadline = Instant::now() + LINE_TIMEOUT;
        loop {
            let body = self.log();
            for line in complete_lines(&body).rev() {
                if let Some(which) = needles.iter().position(|needle| line.contains(needle)) {
                    return (which, line.to_string());
                }
            }
            assert!(
                Instant::now() < deadline,
                "{}: no log line containing any of {needles:?} within {LINE_TIMEOUT:?}. Tail:\n{}",
                self.label,
                tail(&body, 12)
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn wait_for_log(&self, needle: &str) -> String {
        let deadline = Instant::now() + LINE_TIMEOUT;
        loop {
            let body = self.log();
            if let Some(line) = complete_lines(&body)
                .rev()
                .find(|line| line.contains(needle))
            {
                return line.to_string();
            }
            assert!(
                Instant::now() < deadline,
                "{}: no log line containing {needle:?} within {LINE_TIMEOUT:?}. Tail:\n{}",
                self.label,
                tail(&body, 12)
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Run `tcr peer <args> --peers <this Mac's peers file>` and hand back
    /// (stdout, stderr, success).
    fn peer(&self, args: &[&str]) -> (String, String, bool) {
        let mut command = Command::new(env!("CARGO_BIN_EXE_tcr"));
        command
            .arg("peer")
            .args(args)
            .args(["--peers", self.peers.to_str().expect("a utf-8 peers path")]);
        self.env(&mut command);
        let out = command
            .output()
            .unwrap_or_else(|err| panic!("{}: spawning `tcr peer {args:?}`: {err}", self.label));
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
            out.status.success(),
        )
    }

    /// [`Self::peer`], refusing anything but exit 0. Returns stdout.
    fn peer_ok(&self, args: &[&str]) -> String {
        let (stdout, stderr, ok) = self.peer(args);
        assert!(
            ok,
            "{}: `tcr peer {args:?}` exited non-zero.\nstdout: {stdout}\nstderr: {stderr}",
            self.label
        );
        stdout
    }

    /// `tcr peer status --json` against THIS Mac's running proxy, parsed.
    ///
    /// The block the panel's live half reads, off the live process, which is a
    /// different fact from what the peers file says, and the only way to catch
    /// a block that is derived correctly and never assigned.
    fn status_json(&self) -> serde_json::Value {
        // This Mac's own config says `"port": 0`, the kernel picks one at
        // boot, so the port to ASK on is the one the boot line reported, not
        // the one on disk. A side config carrying it is written here rather
        // than rewriting the Mac's own, which the running server also owns.
        let asking = self.home.path().join("asking-config.json");
        let port = self
            .proxy
            .rsplit_once(':')
            .map(|(_, port)| port)
            .unwrap_or_default();
        std::fs::write(
            &asking,
            format!("{{\n  \"proxy\": {{ \"port\": {port} }},\n  \"accounts\": []\n}}\n"),
        )
        .expect("the asking config writes");

        let mut command = Command::new(env!("CARGO_BIN_EXE_tcr"));
        command.args([
            "peer",
            "status",
            "--json",
            "--config",
            asking.to_str().expect("a utf-8 config path"),
        ]);
        self.env(&mut command);
        let out = command
            .output()
            .unwrap_or_else(|err| panic!("{}: spawning `tcr status --json`: {err}", self.label));
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            out.status.success(),
            "{}: `tcr peer status --json` exited non-zero.\nstdout: {stdout}\nstderr: {}",
            self.label,
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_str(stdout.trim())
            .unwrap_or_else(|err| panic!("{}: `peer status --json` is not JSON: {err}", self.label))
    }

    /// `tcr peer ls --json`, parsed.
    fn ls(&self) -> serde_json::Value {
        let stdout = self.peer_ok(&["ls", "--json"]);
        serde_json::from_str(stdout.trim())
            .unwrap_or_else(|err| panic!("{}: `peer ls --json` is not JSON: {err}", self.label))
    }

    /// The first pinned row's full wire node id: the form `allow` and `lend`
    /// take, never the `tcr-…` short form `tcr peer id` prints.
    fn first_pinned_node(&self) -> String {
        let ls = self.ls();
        ls["peers"][0]["node"]
            .as_str()
            .unwrap_or_else(|| {
                panic!(
                    "{}: no pinned peer to read a node id from: {ls}",
                    self.label
                )
            })
            .to_string()
    }

    /// Every pinned row's full wire node id, in the order the peers file holds
    /// them.
    ///
    /// For a Mac pinned to more than one peer, where
    /// [`Self::first_pinned_node`] answers a question with two answers: the
    /// caller reads the list and picks by what it already knows (the id it
    /// recorded after the FIRST join is the one to skip), rather than trusting
    /// an order this test does not set.
    fn pinned_nodes(&self) -> Vec<String> {
        let ls = self.ls();
        let rows = ls["peers"]
            .as_array()
            .unwrap_or_else(|| panic!("{}: `peer ls --json` has no peers array: {ls}", self.label));
        rows.iter()
            .map(|row| {
                row["node"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{}: a pinned row with no node: {ls}", self.label))
                    .to_string()
            })
            .collect()
    }

    fn shutdown(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Mac {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The lines of `body` the writer has FINISHED: everything up to its last
/// newline. Whatever follows that newline is still being written, so it is not
/// a line yet and no waiter here may match on it.
///
/// A tracing event reaches these logs as its message first and its fields
/// behind it, so a reader that catches one in flight sees
/// `peer listener up (a second socket; ...)` with no `peer_listen=` after it.
/// That half line is what the macOS CI job matched on 2026-09-19: all twelve
/// tests in this file panicked at the same assertion, every one of them
/// `left: ""` against the port its peers file names, while the same code
/// passed locally because a 50 ms poll never lands inside the write on an idle
/// machine. The emitters are not at fault: both `src/server.rs` and
/// `src/peer/listener.rs` carry `peer_listen` on that event, so a matched line
/// without it was never a whole line. How the write came apart is not measured
/// here: polling a live boot log on a fast machine caught no mid-line read in
/// 8346 of them, which is why the waiters stop trusting the tail rather than
/// the writer.
fn complete_lines(body: &str) -> std::str::Lines<'_> {
    let finished = body.rfind('\n').map_or(0, |at| at + 1);
    body[..finished].lines()
}

#[test]
fn a_half_written_line_is_not_a_line() {
    let whole = "INFO server: peer listener up (a second socket) peer_listen=127.0.0.1:49894\n";
    let half = "INFO server: peer listener up (a second socket)";
    let body = format!("{whole}{half}");
    assert_eq!(
        complete_lines(&body).next_back(),
        Some(whole.trim_end()),
        "the last finished line is the one carrying peer_listen, not the half line under it"
    );
    assert_eq!(
        complete_lines(half).next(),
        None,
        "a body with no newline in it holds no finished line at all"
    );
    let announced = complete_lines(&body)
        .rev()
        .find(|line| line.contains("peer listener up"))
        .and_then(|line| field(line, "peer_listen"));
    assert_eq!(
        announced.as_deref(),
        Some("127.0.0.1:49894"),
        "a waiter that skips the half line reads the address the emitter announced"
    );
}

/// Every ANSI escape sequence out of some child output.
///
/// # Why the tests need this
///
/// `tracing_subscriber`'s `Layer::default` turns colour ON whenever `NO_COLOR`
/// is unset, and it never asks whether the sink is a terminal, so a child
/// whose stdout is a plain file still gets escapes. Every spawn here now sets
/// `NO_COLOR=1`, but a searcher that only works on plain text is one dropped
/// `env` call away from the failure that cost this file a CI round: locally
/// the tool shell exports `NO_COLOR=1`, the runner does not, and
/// `field(line, "peer_listen")` looked for a literal `peer_listen=` that had
/// become `\e[3mpeer_listen\e[0m\e[2m=\e[0m`. Stripping here makes the
/// searchers hold either way.
///
/// Handles the two forms the formatter emits: CSI (`\e[` … final byte in
/// `@`-`~`) and a two-byte escape such as `\e(B`.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            out.push(ch);
            continue;
        }
        // A CSI sequence runs until its final byte, `@` through `~`. Anything
        // else is the two-byte form: the escape and the one character after it,
        // both already consumed.
        if chars.next() == Some('[') {
            for next in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&next) {
                    break;
                }
            }
        }
    }
    out
}

/// `key=value` out of one tracing line, where the value runs to the next space.
///
/// Colour-blind by construction: see [`strip_ansi`].
fn field(line: &str, key: &str) -> Option<String> {
    let line = strip_ansi(line);
    let marker = format!("{key}=");
    let start = line.find(&marker)? + marker.len();
    Some(
        line[start..]
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string(),
    )
}

/// The exact boot line a CI runner produced at 8668af1, where `NO_COLOR` is
/// unset and every field name and `=` sits inside an escape sequence. Red
/// before [`strip_ansi`]: `field` searched for a literal `peer_listen=` that
/// is not in this line at all, and all twelve tests in this file timed out
/// thirty seconds later saying the field never arrived.
#[test]
fn a_coloured_boot_line_still_yields_the_peer_address() {
    let coloured = "\u{1b}[2m2026-09-19T16:08:55Z\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m \u{1b}[2mteamclaude_rs::server\u{1b}[0m\u{1b}[2m:\u{1b}[0m peer listener up (a second socket; the local /_tcr/ gate is untouched) \u{1b}[3mpeer_listen\u{1b}[0m\u{1b}[2m=\u{1b}[0m127.0.0.1:34865 \u{1b}[3mnode\u{1b}[0m\u{1b}[2m=\u{1b}[0mtcr-EXAMPLENODE";
    assert_eq!(
        field(coloured, "peer_listen").as_deref(),
        Some("127.0.0.1:34865"),
        "the address reads out of a line whose field name and `=` are wrapped in escapes"
    );
    assert_eq!(
        field(coloured, "node").as_deref(),
        Some("tcr-EXAMPLENODE"),
        "the last field on a coloured line ends at the line, not at an escape"
    );
    assert!(
        !strip_ansi(coloured).contains('\u{1b}'),
        "stripping leaves no escape byte behind"
    );
    assert!(
        strip_ansi(coloured).contains("peer_listen=127.0.0.1:34865"),
        "a stripped line carries the plain `key=value` the searchers look for"
    );
    let plain = "INFO server: peer listener up peer_listen=127.0.0.1:34865";
    assert_eq!(
        strip_ansi(plain),
        plain,
        "a line with no escapes in it comes back unchanged"
    );
}

/// The six digits out of `code=NNNNNN` or `shows NNNNNN`.
fn six_digits(haystack: &str) -> Option<String> {
    let after_marker = |marker: &str| {
        haystack.rsplit_once(marker).map(|(_, rest)| {
            rest.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
        })
    };
    after_marker("code=")
        .or_else(|| after_marker("this Mac shows "))
        .filter(|digits| digits.len() == 6)
}

fn tail(body: &str, lines: usize) -> String {
    let all: Vec<&str> = body.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// One POST at a proxy, answered or not. Returns (status, body).
async fn post_messages(proxy: &str) -> (u16, String) {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("a loopback client");
    let answered = client
        .post(format!("http://{proxy}/v1/messages"))
        .header("content-type", "application/json")
        // What a real client sends, and what the API refuses a message request
        // without: see `tests/tools/api_contract.rs`.
        .header("anthropic-version", "2023-06-01")
        .body(r#"{"model":"claude-sonnet-4-5","messages":[]}"#)
        .send()
        .await
        .unwrap_or_else(|err| panic!("the proxy at {proxy} did not answer: {err}"));
    let status = answered.status().as_u16();
    let body = answered.text().await.unwrap_or_default();
    (status, body)
}

// ---------------------------------------------------------------------------
// The pairing path
// ---------------------------------------------------------------------------

/// **Two Macs pair the way an operator pairs them**: the beacon switch, a
/// pairing request that discloses nothing until Accept, the same six digits on
/// both screens, and a pin.
///
/// Every assertion is on output an operator can see: a CLI exit code, a line on
/// stdout, a line in the other process's log, a field in `--json`. Nothing here
/// calls a library function, which is what makes it able to catch a verb that
/// never reached the dispatcher.
///
/// Watched red: `tests/tools/e2e/watch-peer-e2e-fail.sh` deletes the
/// `record_pairing_key`/six-digit `tracing::info!` block from
/// `src/peer/listener.rs` (the `Handshake::Pair` arm) and this fails at the
/// "the lender's screen shows the digits" step, because the responder's half of
/// the compare is then invisible to the operator standing at the lender.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_macs_find_each_other_pair_on_six_digits_and_pin() {
    let (upstream, _seen) = spawn_upstream().await;
    let mut lender = Mac::new("lender", &upstream, "lender-fake", "at-fake-lender", false);
    let mut borrower = Mac::new(
        "borrower",
        &upstream,
        "borrower-fake",
        "at-fake-borrower",
        true,
    );

    step(1, "boot two tcr processes on kernel proxy ports");
    lender.boot();
    borrower.boot();
    println!(
        "         lender proxy={} peer=127.0.0.1:{}; borrower proxy={} peer=127.0.0.1:{}",
        lender.proxy, lender.peer_port, borrower.proxy, borrower.peer_port
    );

    step(
        2,
        "each Mac takes a display name (never this machine's hostname)",
    );
    assert_eq!(
        lender.peer_ok(&["name", "lender-mac"]).trim(),
        "lender-mac",
        "`tcr peer name` must echo the name it stored"
    );
    assert_eq!(
        borrower.peer_ok(&["name", "borrower-mac"]).trim(),
        "borrower-mac"
    );

    step(3, "find on, one Mac at a time");
    for mac in [&lender, &borrower] {
        find_on(mac);
    }

    step(
        4,
        "the borrower presses Trust on the lender's address, which SENDS a pairing request",
    );
    let mut pair = spawn_pair(&borrower, &format!("127.0.0.1:{}", lender.peer_port));
    let asked = wait_for_file(&pair.out, "peer pair: asked", &borrower);
    let instance = field(&asked, "instance").unwrap_or_default();
    let instance = asked
        .rsplit_once("as instance ")
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .map(str::to_string)
        .unwrap_or(instance);
    assert_eq!(
        instance.len(),
        16,
        "the request must name this boot's 8-byte instance id, got {instance:?}"
    );

    step(5, "the lender sees a request and nothing else about it");
    let pending: serde_json::Value =
        serde_json::from_str(lender.peer_ok(&["pending", "--json"]).trim())
            .expect("`peer pending --json` is JSON");
    assert_eq!(
        pending["pending"][0]["instanceId"].as_str(),
        Some(instance.as_str()),
        "the queued request must be the one the borrower just sent: {pending}"
    );
    assert_eq!(
        pending["pending"][0]["proposedName"].as_str(),
        Some("borrower-mac"),
        "the row must carry the name the borrower proposed, not its hostname: {pending}"
    );
    let text = lender.peer_ok(&["pending"]);
    assert!(
        text.contains("wants to pair") && text.contains(&instance),
        "the text row is what the operator reads: {text}"
    );
    // The whole point: before Accept, the requester has been told
    // nothing: no static key, so no digits either.
    let before = strip_ansi(&std::fs::read_to_string(&pair.out).unwrap_or_default());
    assert!(
        !before.contains("this Mac shows"),
        "digits appeared BEFORE anyone pressed Accept, which means an XX message 2 \
         went out to an unapproved instance:\n{before}"
    );

    step(
        6,
        "Accept opens a window for that one instance, and the digits appear",
    );
    let accepted = lender.peer_ok(&["accept", &instance]);
    assert!(
        accepted.contains(&format!("instance={instance}")),
        "accept must name the instance it approved: {accepted}"
    );
    let borrower_line = wait_for_file(&pair.out, "this Mac shows", &borrower);
    let borrower_code =
        six_digits(&borrower_line).unwrap_or_else(|| panic!("no six digits in {borrower_line:?}"));
    let lender_line = lender.wait_for_log("peer pairing: compare these six digits");
    let lender_code =
        six_digits(&lender_line).unwrap_or_else(|| panic!("no six digits in {lender_line:?}"));
    assert_eq!(
        borrower_code, lender_code,
        "the two screens must show the SAME six digits; a mismatch is the one signal \
         this path exists to produce"
    );
    println!("         both screens show {borrower_code}");

    step(
        7,
        "the operator compares them, and the borrower pins the lender",
    );
    pair.answer(&lender_code);
    let outcome = pair.finish(&borrower);
    assert!(
        outcome.contains("peer pair: trusted"),
        "the compare must end in a pin: {outcome}"
    );
    let row = borrower.ls();
    assert_eq!(
        row["peers"].as_array().map(Vec::len),
        Some(1),
        "the borrower must hold exactly one pinned row: {row}"
    );
    assert_eq!(
        row["pendingCount"].as_u64(),
        Some(0),
        "an accepted request must leave the pending list: {row}"
    );

    // GAP 1, now closed: `pair::confirm` takes the address the
    // pairing ran over and `pin_row` writes it, so a Mac trusted on six digits
    // has a way back to the Mac that trusted it. This assertion used to pin
    // the gap at zero endpoints; it now pins the fix.
    let endpoints = row["peers"][0]["endpoints"].clone();
    assert_eq!(
        endpoints.as_array().map(Vec::len),
        Some(1),
        "a six-digit pairing must leave exactly one endpoint on the pinned row: {row}"
    );
    assert_eq!(
        endpoints[0]["addr"].as_str(),
        Some(format!("127.0.0.1:{}", lender.peer_port).as_str()),
        "the endpoint must be the address the pairing actually ran over: {endpoints}"
    );
    assert_eq!(
        endpoints[0]["kind"].as_str(),
        Some("direct"),
        "an address this Mac dialled itself is a direct locator: {endpoints}"
    );
    assert_eq!(
        endpoints[0]["source"].as_str(),
        Some("paired"),
        "an endpoint written at the pin is sourced `paired`, which is what makes it \
         worth more than a beacon: {endpoints}"
    );
    assert!(
        endpoints[0]["observedAtMs"].as_i64().unwrap_or_default() > 0,
        "the endpoint must carry when it was observed, on this Mac's own clock: {endpoints}"
    );

    // And the RUNNING process reports the same pin in the payload the panel
    // reads. Derived correctly in `peer_status_rows` and never assigned is
    // exactly the failure this step exists to catch: `tcr peer ls` would still
    // be right while the panel drew nothing.
    let status = borrower.status_json();
    let peers = status["peers"]
        .as_array()
        .unwrap_or_else(|| panic!("`tcr peer status --json` must carry a peers block: {status}"));
    assert_eq!(
        peers.len(),
        1,
        "the pinned row must reach the payload the panel reads: {status}"
    );
    assert_eq!(
        peers[0]["address"].as_str(),
        Some(format!("127.0.0.1:{}", lender.peer_port).as_str()),
        "and it must carry the endpoint, not an empty row: {status}"
    );
    assert_eq!(
        peers[0]["paths"][0]["kind"].as_str(),
        Some("direct"),
        "with one direct path under it: {status}"
    );

    lender.shutdown();
    borrower.shutdown();
}

// ---------------------------------------------------------------------------
// The N-node ring
// ---------------------------------------------------------------------------

/// `TCR_E2E_NODES`, default 2, clamped to the 2..=5 range
/// `scripts/peer-e2e-local.sh --nodes N` is documented to accept. A value
/// outside that range is a test-runner mistake, not a shape this test learns
/// to tolerate, so it panics rather than clamping silently.
fn e2e_node_count() -> usize {
    let n = std::env::var("TCR_E2E_NODES")
        .ok()
        .map(|raw| {
            raw.parse::<usize>()
                .unwrap_or_else(|err| panic!("TCR_E2E_NODES={raw:?} is not a number: {err}"))
        })
        .unwrap_or(2);
    assert!((2..=5).contains(&n), "TCR_E2E_NODES must be 2..=5, got {n}");
    n
}

/// The six-digit compare `two_macs_find_each_other_pair_on_six_digits_and_pin`
/// proves, generalized to any pair inside a larger ring: `initiator` sends the
/// pairing request, `responder` accepts it, both screens must show the SAME
/// six digits, and `initiator` ends up with one more pinned row than before.
///
/// This is a new helper next to that test, not an edit to it: the two-Mac test
/// stays byte-for-byte what it was, and this one reuses its exact CLI
/// incantations and log-line needles so the ring case is provably the same
/// protocol run N times, not a shape that merely looks similar.
fn pair_two_macs(initiator: &Mac, responder: &Mac) {
    let mut pair = spawn_pair(initiator, &format!("127.0.0.1:{}", responder.peer_port));
    let asked = wait_for_file(&pair.out, "peer pair: asked", initiator);
    let instance = asked
        .rsplit_once("as instance ")
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .map(str::to_string)
        .unwrap_or_default();
    assert_eq!(
        instance.len(),
        16,
        "{}: the request must name this boot's 8-byte instance id, got {instance:?}",
        initiator.label
    );

    let pending: serde_json::Value =
        serde_json::from_str(responder.peer_ok(&["pending", "--json"]).trim())
            .expect("`peer pending --json` is JSON");
    let proposed_name = format!("{}-mac", initiator.label);
    let row = pending["pending"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|row| row["instanceId"].as_str() == Some(instance.as_str()))
        .unwrap_or_else(|| {
            panic!(
                "{}: no pending row for instance {instance}: {pending}",
                responder.label
            )
        });
    assert_eq!(
        row["proposedName"].as_str(),
        Some(proposed_name.as_str()),
        "{}: the pending row must carry the name {} proposed, not its hostname: {pending}",
        responder.label,
        initiator.label
    );

    let accepted = responder.peer_ok(&["accept", &instance]);
    assert!(
        accepted.contains(&format!("instance={instance}")),
        "{}: accept must name the instance it approved: {accepted}",
        responder.label
    );
    let initiator_line = wait_for_file(&pair.out, "this Mac shows", initiator);
    let initiator_code = six_digits(&initiator_line)
        .unwrap_or_else(|| panic!("{}: no six digits in {initiator_line:?}", initiator.label));
    let responder_line = responder.wait_for_log("peer pairing: compare these six digits");
    let responder_code = six_digits(&responder_line)
        .unwrap_or_else(|| panic!("{}: no six digits in {responder_line:?}", responder.label));
    assert_eq!(
        initiator_code, responder_code,
        "{} and {} must show the SAME six digits",
        initiator.label, responder.label
    );

    pair.answer(&initiator_code);
    let outcome = pair.finish(initiator);
    assert!(
        outcome.contains("peer pair: trusted"),
        "{}: the compare must end in a pin: {outcome}",
        initiator.label
    );
}

/// **N Macs in a ring** (`TCR_E2E_NODES`, default 2): boot N, name each, turn
/// `find` on for each, then pair every adjacent pair around the ring with the
/// same six-digit protocol the two-Mac test proves. At N=2 there is exactly
/// ONE edge (0-1), which is the two-Mac test's own topology, run through this
/// generalized driver rather than duplicated by hand; at N>=3 there are N
/// edges (0-1, 1-2, ..., (N-1)-0) and every Mac ends the run with exactly two
/// pinned rows.
///
/// `tests/peer_e2e.rs`'s existing two-Mac test is untouched by this addition
/// and keeps passing on its own, so "every existing step still passes at N=2
/// byte-for-byte" holds trivially: this is a new, separate driver, not a
/// rewrite of that one.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn n_macs_pair_in_a_ring() {
    let n = e2e_node_count();
    let (upstream, _seen) = spawn_upstream().await;
    let mut macs: Vec<Mac> = (0..n)
        .map(|i| {
            let label: &'static str = Box::leak(format!("node{i}").into_boxed_str());
            let account = format!("{label}-fake");
            let token = format!("at-fake-{label}");
            Mac::new(label, &upstream, &account, &token, true)
        })
        .collect();

    step(1, &format!("boot {n} tcr processes on kernel proxy ports"));
    for mac in &mut macs {
        mac.boot();
    }
    for mac in &macs {
        println!(
            "         {} proxy={} peer=127.0.0.1:{}",
            mac.label, mac.proxy, mac.peer_port
        );
    }

    step(
        2,
        "each Mac takes a display name (never this machine's hostname)",
    );
    for mac in &macs {
        let name = format!("{}-mac", mac.label);
        assert_eq!(
            mac.peer_ok(&["name", &name]).trim(),
            name,
            "`tcr peer name` must echo the name it stored"
        );
    }

    step(3, "find on, one Mac at a time");
    for mac in &macs {
        find_on(mac);
    }

    let edges: Vec<(usize, usize)> = if n == 2 {
        vec![(0, 1)]
    } else {
        (0..n).map(|i| (i, (i + 1) % n)).collect()
    };
    step(4, &format!("pair the ring: {} edge(s)", edges.len()));
    // GAP 1 (this file's module docs, `src/peer/pair.rs:670`): `pair::confirm`
    // pins only the INITIATOR's side: the asker ends up with a row for the
    // responder, and the responder's `accept` approves the request but pins
    // nothing of its own. A ring where node `i` always initiates the edge to
    // `i+1 mod n` therefore leaves every node with exactly ONE pinned row (the
    // node it asked), never two, even at N>=3: the expected count below is
    // the number of times each node appears as the INITIATOR, read off the
    // edge list rather than assumed, so this test breaks loudly the day GAP 1
    // is fixed and a responder starts pinning too.
    let mut expected_pins = vec![0usize; n];
    for (initiator, responder) in &edges {
        pair_two_macs(&macs[*initiator], &macs[*responder]);
        expected_pins[*initiator] += 1;
    }

    for (i, mac) in macs.iter().enumerate() {
        let ls = mac.ls();
        assert_eq!(
            ls["peers"].as_array().map(Vec::len),
            Some(expected_pins[i]),
            "{}: must hold exactly {} pinned row(s) after the ring (GAP 1: only an \
             initiator's own `pair::confirm` pins): {ls}",
            mac.label,
            expected_pins[i]
        );
    }

    for mac in &mut macs {
        mac.shutdown();
    }
}

/// `tcr peer find on` must exit 0, write the switch, and say so: a non-zero
/// exit is a test failure here, never a NOTE this step swallows. A box with no
/// multicast-capable interface is not a passing outcome for an operator, and
/// it is not one for this gate either: if the mDNS calls behind the switch
/// cannot start on this runner, that is an environment defect to fix in the
/// runner, not a shape this test learns to tolerate.
///
/// What this step does NOT claim: that one Mac SAW the other's beacon. It
/// cannot be claimed through the CLI on this build: `find on` starts a browse
/// and discards the scan (`src/main.rs:2056-2062`), and the announcement dies
/// with the short-lived CLI process that registered it, so there is no
/// cross-process sighting to read. What the beacon may carry is
/// `tests/peer_discovery.rs`'s subject; this is the switch.
fn find_on(mac: &Mac) {
    let (stdout, stderr, ok) = mac.peer(&["find", "on", "--announce-name", "off"]);
    assert!(
        ok,
        "{}: `tcr peer find on` exited non-zero.\nstdout: {stdout}\nstderr: {stderr}",
        mac.label
    );
    assert!(
        stdout.contains("peer.find: on (announceName=false)"),
        "{}: `find on` said something else: {stdout}",
        mac.label
    );
    let file: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&mac.peers).expect("the peers file reads back"),
    )
    .expect("the peers file is JSON");
    assert_eq!(
        file["discovery"].as_bool(),
        Some(true),
        "{}: `find on` must write the switch: {file}",
        mac.label
    );
    assert_eq!(
        file["announceName"].as_bool(),
        Some(false),
        "{}: `--announce-name off` must be stored too: {file}",
        mac.label
    );
}

/// A `tcr peer pair` process: it knocks, then retries the handshake for ten
/// minutes while it waits for the far side's Accept, then asks on stdin for the
/// digits the other screen shows.
struct PairProcess {
    child: Child,
    out: PathBuf,
}

impl PairProcess {
    /// Type the digits the other Mac is showing.
    fn answer(&mut self, code: &str) {
        let stdin = self.child.stdin.as_mut().expect("the pair process's stdin");
        stdin
            .write_all(format!("{code}\n").as_bytes())
            .expect("the compared code writes to stdin");
        stdin.flush().expect("stdin flushes");
    }

    /// Wait for it to exit and return everything it printed.
    fn finish(mut self, mac: &Mac) -> String {
        let status = self
            .child
            .wait()
            .expect("the pair process is waitable after its answer");
        let printed = strip_ansi(&std::fs::read_to_string(&self.out).unwrap_or_default());
        assert!(
            status.success(),
            "{}: `tcr peer pair` exited {status:?}:\n{printed}",
            mac.label
        );
        printed
    }
}

fn spawn_pair(mac: &Mac, addr: &str) -> PairProcess {
    let out = mac.home.path().join("pair.out");
    let file = std::fs::File::create(&out).expect("the pair log opens");
    let err = file.try_clone().expect("the pair log clones for stderr");
    let mut command = Command::new(env!("CARGO_BIN_EXE_tcr"));
    command
        .args(["peer", "pair", addr])
        .args(["--peers", mac.peers.to_str().expect("a utf-8 peers path")])
        .stdin(Stdio::piped())
        .stdout(Stdio::from(file))
        .stderr(Stdio::from(err));
    mac.env(&mut command);
    let child = command
        .spawn()
        .unwrap_or_else(|err| panic!("{}: spawning `tcr peer pair`: {err}", mac.label));
    PairProcess { child, out }
}

/// Block until `path` holds a line containing `needle`; return that line.
fn wait_for_file(path: &Path, needle: &str, mac: &Mac) -> String {
    let deadline = Instant::now() + LINE_TIMEOUT;
    loop {
        let body = strip_ansi(&std::fs::read_to_string(path).unwrap_or_default());
        if let Some(line) = complete_lines(&body)
            .rev()
            .find(|line| line.contains(needle))
        {
            return line.to_string();
        }
        assert!(
            Instant::now() < deadline,
            "{}: nothing containing {needle:?} in {} within {LINE_TIMEOUT:?}. Got:\n{body}",
            mac.label,
            path.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ---------------------------------------------------------------------------
// The borrowing path
// ---------------------------------------------------------------------------

/// **A borrowed request, end to end through the two processes**: enrol, share,
/// lend, borrow, forget.
///
/// The lender is warmed with one ordinary local request first, because a
/// lender with no measured window has no headroom to lend
/// (`Manager::lendable_fraction` answers `0.0` on an unmeasured window and
/// `Ledger::grant` reads the last note).
///
/// Step 7 asserts the served 200 through the lease arm (gap 4 in this file's
/// docs, `listener::serve_control`, `src/peer/listener.rs:1486`), with the
/// chain in front of it proven too: the provider installed itself for this
/// dry fleet with no restart, the borrower dialled the lender, the lender's
/// Noise handshake completed. Until LEASE-WIRE lands that arm in this tree,
/// this test is RED at step 7, with the borrower's dry-fleet ladder exhausted
/// at 429 in the failure message instead of served.
///
/// Watched red: `tests/tools/e2e/watch-peer-e2e-fail.sh` deletes the
/// `install_peer_lease_provider` block from `src/server.rs` and this fails at
/// step 7 with no `peer-lease fallback` line at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_borrowed_request_reaches_the_lender_and_never_carries_the_borrowers_credential() {
    let (upstream, seen) = spawn_upstream().await;
    let mut lender = Mac::new("lender", &upstream, "lender-fake", "at-fake-lender", false);
    let mut borrower = Mac::new(
        "borrower",
        &upstream,
        "borrower-fake",
        "at-fake-borrower",
        true,
    );

    step(1, "boot two tcr processes");
    lender.boot();
    borrower.boot();

    step(
        2,
        "one ordinary request on the lender, so its 7d window is measured",
    );
    let (warm, _) = post_messages(&lender.proxy).await;
    assert_eq!(
        warm, 200,
        "the lender's own fleet must serve its own request before it can lend a fraction of it"
    );
    assert_eq!(
        seen.credentials(),
        vec!["Bearer at-fake-lender".to_string()],
        "the warm request must have gone out on the lender's own token"
    );

    step(
        3,
        "the lender mints a one-use join key; the borrower joins with it",
    );
    let minted = lender.peer_ok(&["invite", "--label", "borrower-mac", "--ttl", "300"]);
    let key = minted
        .lines()
        .find(|line| line.starts_with("tcr-join:"))
        .unwrap_or_else(|| panic!("no join key on stdout: {minted}"))
        .to_string();
    join_with_stdin(&borrower, &key);

    step(
        4,
        "both sides hold a pinned row, and the borrower's carries an address",
    );
    let lender_node = borrower.first_pinned_node();
    let borrower_node = lender.first_pinned_node();
    assert_ne!(
        lender_node, borrower_node,
        "the two Macs must have pinned each OTHER, not themselves"
    );
    let endpoints = borrower.ls()["peers"][0]["endpoints"].clone();
    assert_eq!(
        endpoints[0]["addr"].as_str(),
        Some(format!("127.0.0.1:{}", lender.peer_port).as_str()),
        "the joiner's row must name the address it enrolled through: {endpoints}"
    );
    assert_eq!(
        endpoints[0]["source"].as_str(),
        Some("paired"),
        "an enrolment writes the socket it arrived on, sourced `paired`: {endpoints}"
    );

    step(
        5,
        "share on, then one lease grant for that peer, on the lender",
    );
    let shared = lender.peer_ok(&["share", "on", "--window", "7d", "--fraction", "0.20"]);
    assert!(
        shared.contains("peer share: on peers=1"),
        "share must say what it changed: {shared}"
    );
    assert!(
        shared.contains("READ those requests in full"),
        "share must name the disclosure it grants: {shared}"
    );
    let lent = lender.peer_ok(&[
        "lend",
        &borrower_node,
        "--window",
        "7d",
        "--fraction",
        "0.20",
        "--ttl",
        "300",
        "--max-inflight",
        "2",
    ]);
    assert!(
        lent.contains("peer lend: ok") && lent.contains("window=7d"),
        "lend must confirm the grant: {lent}"
    );
    let grants = lender.ls()["peers"][0]["lend"].clone();
    assert_eq!(
        grants.as_array().map(Vec::len),
        Some(1),
        "the lender's row must carry exactly one grant: {grants}"
    );

    step(
        6,
        "disclose on, on the borrower: the other half of the lease",
    );
    let disclosed = borrower.peer_ok(&["allow", &lender_node, "disclose", "on"]);
    assert!(
        disclosed.contains("grant=disclose state=on"),
        "allow must confirm the grant: {disclosed}"
    );

    step(
        7,
        "a request on the borrower's proxy, served through the lease the provider installs itself for",
    );
    let mut last = (0_u16, String::new());
    for attempt in 1..=BORROW_SERVED_ATTEMPTS {
        last = post_messages(&borrower.proxy).await;
        println!("         borrow attempt {attempt} -> {}", last.0);
        if last.0 == 200 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }

    // The chain in front of the answer: the borrower really reached the
    // lender's listener over Noise, whichever way the answer went.
    let asked = borrower.log();
    let reached_the_lender = asked.contains("peer lease: asking for a lease failed")
        || asked.contains("served through a fallback provider");
    assert!(
        reached_the_lender,
        "the borrower's dry-fleet arm never reached its provider at all. Tail:\n{}",
        tail(&asked, 12)
    );

    assert_eq!(
        last.0,
        200,
        "the lease arm must serve the borrowed request (LEASE-WIRE's \
         listener::serve_control Control::LeaseRequest arm, \
         src/peer/listener.rs:1486). Borrower tail:\n{}\nLender tail:\n{}",
        tail(&asked, 12),
        tail(&lender.log(), 12)
    );
    assert!(
        seen.requests() >= 2,
        "a served borrow must have reached the fake upstream a second time"
    );

    step(8, "the borrower's own credential never left the borrower");
    let credentials = seen.credentials();
    assert!(
        !credentials
            .iter()
            .any(|seen| seen.contains("at-fake-borrower")),
        "the borrower's token reached the upstream, which is the one thing this \
         whole design forbids: {credentials:?}"
    );
    assert!(
        credentials
            .iter()
            .all(|seen| seen == "Bearer at-fake-lender"),
        "every request the upstream saw must be on the lender's own token: {credentials:?}"
    );

    step(9, "forget, and the next request is today's 429");
    let forgotten = lender.peer_ok(&["forget", &borrower_node]);
    assert!(
        forgotten.contains("peer forget: ok"),
        "forget must confirm: {forgotten}"
    );
    let (after, body) = post_messages(&borrower.proxy).await;
    assert_eq!(
        after, 429,
        "a forgotten borrower gets the honest 429 the ladder always had"
    );
    assert!(
        body.contains("exhausted"),
        "and it is the exhausted-fleet body: {body}"
    );
    // The refusal is the lender's, and it names the pin check: which is what
    // makes this 429 different from the one above it.
    let refusal = lender.wait_for_log("peer connection refused before it authenticated");
    assert!(
        refusal.contains("is not pinned on this node"),
        "the lender must refuse the forgotten peer at the pin check: {refusal}"
    );

    lender.shutdown();
    borrower.shutdown();
}

/// `tcr peer join --stdin`, which is the path the panel and the `tcr://` handler
/// use: the key is piped, so it never enters an argument vector.
fn join_with_stdin(mac: &Mac, key: &str) -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tcr"));
    command
        .args(["peer", "join", "--stdin", "--label", "borrower-mac"])
        .args(["--peers", mac.peers.to_str().expect("a utf-8 peers path")])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    mac.env(&mut command);
    let mut child = command
        .spawn()
        .unwrap_or_else(|err| panic!("{}: spawning `tcr peer join`: {err}", mac.label));
    child
        .stdin
        .as_mut()
        .expect("the join process's stdin")
        .write_all(format!("{key}\n").as_bytes())
        .expect("the join key writes to stdin");
    let out = child.wait_with_output().expect("the join process exits");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "{}: `tcr peer join --stdin` exited {:?}.\nstdout: {stdout}\nstderr: {stderr}",
        mac.label,
        out.status.code()
    );
    assert!(
        stdout.contains("peer join: ok"),
        "the join must report the enrolment: {stdout}"
    );
    // The secret is a live bearer token for its ten minutes. Neither stream may
    // echo it, and the failure paths print more than the success path. The
    // field is taken off the END rather than by stripping a version prefix: the
    // prefix moves between builds, and a strip that quietly matched nothing
    // would leave this assertion comparing against the whole key, which no
    // surface prints anyway.
    let secret = key.rsplit(':').next().unwrap_or(key);
    assert!(
        !stdout.contains(secret) && !stderr.contains(secret),
        "the join key appeared in this command's own output"
    );
    stdout
}

// ---------------------------------------------------------------------------
// N=3: chained borrow, two-hop carry, and inflight contention across two
// lenders
//
// Each test below stands up its own fixed three-Mac fleet rather than reading
// `TCR_E2E_NODES`: they need a specific topology (a middle node that is both
// a lender and a borrower, or two lenders and one borrower), not "whatever N
// the ring happens to be", so they run every time this binary runs,
// independent of the ring test's env var.
// ---------------------------------------------------------------------------

/// Like [`spawn_upstream`], but any request bearing `trip_credential` answers
/// a DURABLE quota rejection (`anthropic-ratelimit-unified-status: rejected`
/// plus a scoped `5h-status: rejected`, which is the ONE 429 arm
/// `src/proxy.rs:3073-3091` parks the account on via `Manager::mark_rate_limited`,
/// never the transient/no-guidance arm a few lines below it) the FIRST time
/// the returned flag is armed, then reverts to the canned 200 for every other
/// credential and every later hit. This is how the chained-borrow test forces
/// one Mac's own account into `AccountStatus::Throttled` without touching any
/// file that Mac did not already have a reason to read.
async fn spawn_upstream_with_trip(trip_credential: String) -> (String, Upstream, Arc<AtomicBool>) {
    let upstream = Upstream::default();
    let recorder = upstream.clone();
    let armed = Arc::new(AtomicBool::new(false));
    let armed_for_handler = Arc::clone(&armed);
    let app = Router::new().fallback(any(move |req: axum::extract::Request| {
        let recorder = recorder.clone();
        let armed = Arc::clone(&armed_for_handler);
        let trip_credential = trip_credential.clone();
        async move {
            let path = req.uri().path().to_string();
            let credential = req
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
                .to_string();
            let api_key = req
                .headers()
                .get("x-api-key")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
                .to_string();
            // The origin's own refusal, decided before the body is drained and
            // answered after the arrival is recorded: see
            // `tests/tools/api_contract.rs`.
            let refusal = api_contract::refuse_if_incomplete(req.method(), req.headers());
            let _ = axum::body::to_bytes(req.into_body(), 1024 * 1024).await;
            let full_credential = format!("{credential}{api_key}");
            if let Ok(mut seen) = recorder.seen.lock() {
                seen.push((path, full_credential.clone()));
            }
            if let Some(refusal) = refusal {
                return refusal;
            }
            if full_credential == trip_credential && armed.swap(false, Ordering::SeqCst) {
                return Response::builder()
                    .status(429)
                    .header("content-type", "application/json")
                    .header("retry-after", "3600")
                    .header("anthropic-ratelimit-unified-status", "rejected")
                    .header("anthropic-ratelimit-unified-5h-status", "rejected")
                    .body(Body::from(
                        br#"{"type":"error","error":{"type":"rate_limit_error"}}"#.to_vec(),
                    ))
                    .expect("the canned 429 builds");
            }
            Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .header("anthropic-ratelimit-unified-status", "allowed")
                .header("anthropic-ratelimit-unified-5h-utilization", "0.10")
                .header("anthropic-ratelimit-unified-7d-utilization", "0.10")
                .header("anthropic-ratelimit-unified-7d_oi-utilization", "0.10")
                .body(Body::from(
                    br#"{"type":"message","id":"msg_fake"}"#.to_vec(),
                ))
                .expect("the canned upstream answer builds")
        }
    }));
    let listening = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the fake upstream");
    let addr = listening.local_addr().expect("the fake upstream's address");
    tokio::spawn(async move {
        let _ = axum::serve(listening, app).await;
    });
    (format!("http://{addr}"), upstream, armed)
}

/// **Chained borrow**: `root` lends to `middle`, and `middle`, separately,
/// lends to `leaf`: two independent lease relationships, in a chain, proven
/// not to interfere with each other. First `leaf`'s request is served and the
/// upstream sees exactly `middle`'s own credential for it (the pair
/// (`middle`, `leaf`) works exactly like the two-process borrow test above).
/// Then `middle`'s own account is durably rejected by the upstream, `middle`'s
/// own local fleet has nothing left to serve with, and `middle`'s OWN request
/// falls through to ITS fallback provider (`root`), so the upstream sees
/// exactly `root`'s credential for that one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chained_borrow_leaf_from_middle_middle_from_root() {
    let middle_credential = "Bearer at-fake-middle".to_string();
    let (upstream, seen, trip_middle) = spawn_upstream_with_trip(middle_credential).await;

    let mut root = Mac::new("root", &upstream, "root-fake", "at-fake-root", false);
    let mut middle = Mac::new("middle", &upstream, "middle-fake", "at-fake-middle", false);
    let mut leaf = Mac::new("leaf", &upstream, "leaf-fake", "at-fake-leaf", true);

    step(1, "boot three tcr processes: root, middle, leaf");
    root.boot();
    middle.boot();
    leaf.boot();

    step(
        2,
        "warm root's own account, so it has a measured window to lend from",
    );
    let (warm, _) = post_messages(&root.proxy).await;
    assert_eq!(
        warm, 200,
        "root must serve its own request before it can lend a fraction of it"
    );

    step(3, "middle joins root and borrows from it");
    let minted = root.peer_ok(&["invite", "--label", "middle-mac", "--ttl", "300"]);
    let key = minted
        .lines()
        .find(|line| line.starts_with("tcr-join:"))
        .unwrap_or_else(|| panic!("no join key on stdout: {minted}"))
        .to_string();
    join_with_stdin(&middle, &key);
    let root_node = middle.first_pinned_node();
    let middle_node_at_root = root.first_pinned_node();
    root.peer_ok(&["share", "on", "--window", "7d", "--fraction", "0.20"]);
    root.peer_ok(&[
        "lend",
        &middle_node_at_root,
        "--window",
        "7d",
        "--fraction",
        "0.20",
        "--ttl",
        "300",
        "--max-inflight",
        "2",
    ]);
    middle.peer_ok(&["allow", &root_node, "disclose", "on"]);
    middle.shutdown();
    middle.boot();
    let installed = middle.wait_for_log("peer-lease fallback");
    assert!(
        installed.contains("outcome=Yes"),
        "middle's boot must install a fallback now that it has a lender (root): {installed}"
    );

    step(
        4,
        "warm middle's own account too, so IT has a measured window to lend from",
    );
    let (warm_middle, _) = post_messages(&middle.proxy).await;
    assert_eq!(
        warm_middle, 200,
        "middle must serve its own request before it can lend a fraction of it"
    );

    step(5, "leaf joins middle and borrows from it");
    let minted = middle.peer_ok(&["invite", "--label", "leaf-mac", "--ttl", "300"]);
    let key = minted
        .lines()
        .find(|line| line.starts_with("tcr-join:"))
        .unwrap_or_else(|| panic!("no join key on stdout: {minted}"))
        .to_string();
    join_with_stdin(&leaf, &key);
    let middle_node_at_leaf = leaf.first_pinned_node();
    let leaf_node = middle.first_pinned_node();
    middle.peer_ok(&["share", "on", "--window", "7d", "--fraction", "0.20"]);
    middle.peer_ok(&[
        "lend",
        &leaf_node,
        "--window",
        "7d",
        "--fraction",
        "0.20",
        "--ttl",
        "300",
        "--max-inflight",
        "2",
    ]);
    leaf.peer_ok(&["allow", &middle_node_at_leaf, "disclose", "on"]);
    leaf.shutdown();
    leaf.boot();
    let installed_leaf = leaf.wait_for_log("peer-lease fallback");
    assert!(
        installed_leaf.contains("outcome=Yes"),
        "leaf's boot must install a fallback now that it has a lender (middle): {installed_leaf}"
    );

    step(
        6,
        "leaf's request is served, and the upstream sees exactly middle's own credential",
    );
    let before = seen.requests();
    let mut leaf_result = (0_u16, String::new());
    for attempt in 1..=BORROW_SERVED_ATTEMPTS {
        leaf_result = post_messages(&leaf.proxy).await;
        println!("         leaf attempt {attempt} -> {}", leaf_result.0);
        if leaf_result.0 == 200 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
    assert_eq!(
        leaf_result.0,
        200,
        "leaf's borrowed request must be served through middle. Tail:\n{}",
        tail(&leaf.log(), 12)
    );
    let credentials = seen.credentials();
    assert!(
        credentials[before..]
            .iter()
            .all(|credential| credential == "Bearer at-fake-middle"),
        "every request the upstream saw while serving leaf must be on middle's own \
         token: {credentials:?}"
    );

    step(
        7,
        "middle's own account is durably rejected by the upstream",
    );
    trip_middle.store(true, Ordering::SeqCst);

    step(
        8,
        "middle's own next request falls through to its OWN fallback (root), and \
         the upstream sees root's credential",
    );
    let before = seen.requests();
    let mut middle_result = (0_u16, String::new());
    for attempt in 1..=BORROW_SERVED_ATTEMPTS {
        middle_result = post_messages(&middle.proxy).await;
        println!("         middle attempt {attempt} -> {}", middle_result.0);
        if middle_result.0 == 200 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
    let middle_log = middle.log();
    let reached_fallback = middle_log.contains("peer lease: asking for a lease failed")
        || middle_log.contains("served through a fallback provider");
    assert!(
        reached_fallback,
        "middle's own request never reached its own fallback provider. Tail:\n{}",
        tail(&middle_log, 12)
    );
    assert_eq!(
        middle_result.0,
        200,
        "middle's own request must fall through to root once middle's own account is \
         durably rejected. Tail:\n{}\nRoot tail:\n{}",
        tail(&middle_log, 12),
        tail(&root.log(), 12)
    );
    // `credentials[before..]` covers the whole ladder for this ONE request:
    // the rejected attempt on middle's own token (the 429 that tripped the
    // durable rejection) comes first, then the served attempt through root.
    // Only the LAST entry is what actually served the client, so that is what
    // gets the "on root's own token" claim; the earlier middle entry is
    // asserted separately, naming the sequence rather than asserting "all".
    let credentials = seen.credentials();
    assert!(
        credentials.len() > before,
        "middle's own overflow must have reached the upstream at all: {credentials:?}"
    );
    assert_eq!(
        credentials.last().map(String::as_str),
        Some("Bearer at-fake-root"),
        "the request that actually served middle's own client must be on root's own \
         token: {credentials:?}"
    );
    assert!(
        credentials[before..credentials.len() - 1]
            .iter()
            .all(|credential| credential == "Bearer at-fake-middle"),
        "every rejected attempt before the fallback served must have been on middle's \
         own (durably rejected) token: {credentials:?}"
    );

    leaf.shutdown();
    middle.shutdown();
    root.shutdown();
}

/// **Five concurrent borrows against one lender, capped at `max_inflight:
/// 2`.** `spender` is a dry-fleet Mac pinned to `bank1` alone: the brief for
/// this item asked for a SECOND lender (`bank2`) as overflow, and the
/// exploration for this test built exactly that topology first and found a
/// real race that made it the wrong control, reported here rather than
/// hidden: [`PeerLeaseProvider::lease_for`] (`src/peer/lease.rs:1455`) checks
/// its lease cache and only mints+caches a NEW lease on a miss; the check and
/// the mint are two steps with an `.await` between them and no lock held
/// across it, so N concurrent Asks that all see the SAME cache MISS each mint
/// their OWN fresh lease with its OWN fresh `lease_id`, and `max_inflight` is
/// enforced PER `lease_id` (`Ledger::enter_relay`): never per lender, never
/// per borrower. Measured live: three of five concurrent first-asks refused
/// by `bank1`'s cached lease (`InFlightFull` in the log, proving THIS
/// invariant works) fell through to `bank2` and every one of the three
/// independently raced a cache miss there, minted three DIFFERENT leases, and
/// all three were served: five of five succeeded, zero refused, which would
/// have been this codebase's own DNA's "vacuously-passing control" if it had
/// shipped as the gate. So this test primes ONE lease first (sequential, so
/// there is exactly one cached `lease_id` to contend on) and never joins
/// `bank2` at all, which is what makes "at most 2 of 5 may be served at once"
/// a claim about `max_inflight`, and not an artifact of how many lenders were
/// reachable to race across.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn five_concurrent_borrows_respect_max_inflight() {
    let (upstream, _seen) = spawn_slow_upstream(Duration::from_millis(1500)).await;
    let mut bank1 = Mac::new("bank1", &upstream, "bank1-fake", "at-fake-bank1", false);
    let mut spender = Mac::new(
        "spender",
        &upstream,
        "spender-fake",
        "at-fake-spender",
        true,
    );

    step(
        1,
        "boot two tcr processes: one lender (bank1), one borrower (spender)",
    );
    bank1.boot();
    spender.boot();

    step(2, "warm the lender's own account");
    let (warm, _) = post_messages(&bank1.proxy).await;
    assert_eq!(
        warm, 200,
        "bank1 must serve its own request before it can lend"
    );

    step(
        3,
        "spender joins bank1 and borrows a max_inflight:2 lease from it",
    );
    let minted = bank1.peer_ok(&["invite", "--label", "spender-mac", "--ttl", "300"]);
    let key = minted
        .lines()
        .find(|line| line.starts_with("tcr-join:"))
        .unwrap_or_else(|| panic!("no join key on stdout: {minted}"))
        .to_string();
    join_with_stdin(&spender, &key);
    let bank1_node_at_spender = spender.first_pinned_node();
    let spender_node = bank1.first_pinned_node();
    bank1.peer_ok(&["share", "on", "--window", "7d", "--fraction", "0.90"]);
    bank1.peer_ok(&[
        "lend",
        &spender_node,
        "--window",
        "7d",
        "--fraction",
        "0.90",
        "--ttl",
        "300",
        "--max-inflight",
        "2",
    ]);
    spender.peer_ok(&["allow", &bank1_node_at_spender, "disclose", "on"]);
    spender.shutdown();
    spender.boot();
    let installed = spender.wait_for_log("peer-lease fallback");
    assert!(
        installed.contains("outcome=Yes"),
        "spender's boot must install a fallback now that it has a lender: {installed}"
    );

    step(
        4,
        "one priming request, sequential, so spender caches ONE lease against bank1 \
         before any concurrency is asked of it",
    );
    let (primed, _) = post_messages(&spender.proxy).await;
    assert_eq!(
        primed, 200,
        "the priming request must be served, so the lease it caches is real"
    );

    step(
        5,
        "five concurrent borrows against that ONE cached lease: at most 2 (its \
         max_inflight) may be served at once",
    );
    let deadline = Instant::now() + LINE_TIMEOUT;
    let handles: Vec<_> = (0..5)
        .map(|_| {
            let proxy = spender.proxy.clone();
            tokio::spawn(async move { post_messages(&proxy).await })
        })
        .collect();
    let mut statuses = Vec::new();
    for handle in handles {
        let (status, _) = handle.await.expect("a borrow task panicked");
        statuses.push(status);
    }
    assert!(
        Instant::now() < deadline,
        "all five concurrent borrows must return within the borrow deadline"
    );
    println!("         statuses: {statuses:?}");

    let served = statuses.iter().filter(|status| **status == 200).count();
    let refused = statuses.len() - served;
    assert_eq!(statuses.len(), 5, "must have five results, one per task");
    assert!(
        served <= 2,
        "at most 2 of 5 concurrent borrows against one cached lease \
         (max_inflight 2) may be served at once: {statuses:?}"
    );
    assert!(
        refused >= 1,
        "at least one of the five must be refused for max_inflight to be the thing \
         under test here: {statuses:?}"
    );

    let spender_log = spender.log();
    assert!(
        spender_log.contains("InFlightFull"),
        "the refused borrow(s) must be logged with the max_inflight wire word \
         (LeaseRefusal::InFlightFull). Tail:\n{}",
        tail(&spender_log, 30)
    );

    spender.shutdown();
    bank1.shutdown();
}

/// Like [`spawn_upstream`], but every answer is held open for `delay` before
/// it is sent: long enough to make several requests fired at once genuinely
/// overlap in flight, which is what the concurrent-borrow test needs to prove
/// `max_inflight` binds DURING a request rather than only between them.
async fn spawn_slow_upstream(delay: Duration) -> (String, Upstream) {
    let upstream = Upstream::default();
    let recorder = upstream.clone();
    let app = Router::new().fallback(any(move |req: axum::extract::Request| {
        let recorder = recorder.clone();
        async move {
            let path = req.uri().path().to_string();
            let credential = req
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
                .to_string();
            let api_key = req
                .headers()
                .get("x-api-key")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
                .to_string();
            // The origin's own refusal, decided before the body is drained and
            // answered after the arrival is recorded: see
            // `tests/tools/api_contract.rs`.
            let refusal = api_contract::refuse_if_incomplete(req.method(), req.headers());
            let _ = axum::body::to_bytes(req.into_body(), 1024 * 1024).await;
            if let Ok(mut seen) = recorder.seen.lock() {
                seen.push((path, format!("{credential}{api_key}")));
            }
            tokio::time::sleep(delay).await;
            if let Some(refusal) = refusal {
                return refusal;
            }
            Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .header("anthropic-ratelimit-unified-status", "allowed")
                .header("anthropic-ratelimit-unified-5h-utilization", "0.10")
                .header("anthropic-ratelimit-unified-7d-utilization", "0.10")
                .header("anthropic-ratelimit-unified-7d_oi-utilization", "0.10")
                .body(Body::from(
                    br#"{"type":"message","id":"msg_fake"}"#.to_vec(),
                ))
                .expect("the canned upstream answer builds")
        }
    }));
    let listening = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the fake upstream");
    let addr = listening.local_addr().expect("the fake upstream's address");
    tokio::spawn(async move {
        let _ = axum::serve(listening, app).await;
    });
    (format!("http://{addr}"), upstream)
}

// ---------------------------------------------------------------------------
// Comparison measurements
//
// These print `measure: <name> <value> <unit>` lines that
// `scripts/peer-compare-local.sh` greps; they are `#[ignore]`d because they
// are wall-clock measurements, not correctness gates: the borrow path's
// correctness is `a_borrowed_request_reaches_the_lender_...` above.
// ---------------------------------------------------------------------------

/// The value at `pct` (0..=100) in `values`, nearest-rank. Panics on an empty
/// slice rather than returning a made-up number for zero samples.
fn percentile(values: &[f64], pct: usize) -> f64 {
    assert!(
        !values.is_empty(),
        "cannot take a percentile of zero samples"
    );
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("no NaNs in a wall-clock sample"));
    let rank = (pct * (sorted.len() - 1)) / 100;
    sorted[rank]
}

/// 50 requests served locally on the lender's own proxy versus 50 borrowed
/// through the lender via the borrower's proxy, through the same two-process
/// setup `a_borrowed_request_reaches_the_lender_...` proves correct. Prints
/// p50/p95 of each leg and the delta.
///
/// If the borrowed leg cannot reach a served 200 (the LEASE-WIRE arm named in
/// this file's module docs, gap 4, not yet in the tree), that is reported as
/// `measure: latency_borrowed_blocked` with the file:line and status/body
/// that blocked it, and the local leg's numbers are still printed, this
/// test never fakes a borrowed-leg number and never implements the missing
/// arm itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "wall-clock measurement; run via scripts/peer-compare-local.sh"]
async fn measure_latency_local_vs_borrowed() {
    const SAMPLES: u32 = 50;

    let (upstream, _seen) = spawn_upstream().await;
    let mut lender = Mac::new("lender", &upstream, "lender-fake", "at-fake-lender", false);
    let mut borrower = Mac::new(
        "borrower",
        &upstream,
        "borrower-fake",
        "at-fake-borrower",
        true,
    );

    lender.boot();
    borrower.boot();

    let (warm, _) = post_messages(&lender.proxy).await;
    assert_eq!(
        warm, 200,
        "the lender's own fleet must serve its own request before it can lend a fraction of it"
    );

    let minted = lender.peer_ok(&["invite", "--label", "borrower-mac", "--ttl", "300"]);
    let key = minted
        .lines()
        .find(|line| line.starts_with("tcr-join:"))
        .unwrap_or_else(|| panic!("no join key on stdout: {minted}"))
        .to_string();
    join_with_stdin(&borrower, &key);

    let lender_node = borrower.first_pinned_node();
    let borrower_node = lender.first_pinned_node();
    lender.peer_ok(&["share", "on", "--window", "7d", "--fraction", "0.20"]);
    lender.peer_ok(&[
        "lend",
        &borrower_node,
        "--window",
        "7d",
        "--fraction",
        "0.20",
        "--ttl",
        "300",
        "--max-inflight",
        "2",
    ]);
    borrower.peer_ok(&["allow", &lender_node, "disclose", "on"]);

    println!("STEP: {SAMPLES} requests served locally on the lender");
    let mut local_ms = Vec::with_capacity(SAMPLES as usize);
    for _ in 0..SAMPLES {
        let start = Instant::now();
        let (status, _) = post_messages(&lender.proxy).await;
        assert_eq!(status, 200, "a locally served request must succeed");
        local_ms.push(start.elapsed().as_secs_f64() * 1000.0);
    }

    println!("STEP: priming the borrowed leg until it serves a 200");
    let mut primed = false;
    for attempt in 1..=BORROW_SERVED_ATTEMPTS {
        let (status, _) = post_messages(&borrower.proxy).await;
        if status == 200 {
            primed = true;
            break;
        }
        println!("         prime attempt {attempt} -> {status}");
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }

    println!(
        "measure: latency_local_p50 {:.2} ms",
        percentile(&local_ms, 50)
    );
    println!(
        "measure: latency_local_p95 {:.2} ms",
        percentile(&local_ms, 95)
    );

    if !primed {
        println!(
            "measure: latency_borrowed_blocked src/peer/listener.rs:1486 \
             (listener::serve_control's Control::LeaseRequest arm) never reached a served 200 \
             within {BORROW_SERVED_ATTEMPTS} attempts"
        );
    } else {
        println!("STEP: {SAMPLES} requests borrowed through the lender");
        let mut borrowed_ms = Vec::with_capacity(SAMPLES as usize);
        let mut blocked_at: Option<(u32, u16, String)> = None;
        for i in 1..=SAMPLES {
            let start = Instant::now();
            let (status, body) = post_messages(&borrower.proxy).await;
            let elapsed = start.elapsed().as_secs_f64() * 1000.0;
            if status != 200 {
                blocked_at = Some((i, status, body));
                break;
            }
            borrowed_ms.push(elapsed);
        }
        match blocked_at {
            Some((i, status, body)) => println!(
                "measure: latency_borrowed_blocked request {i} of {SAMPLES} answered \
                 status={status} body={body}"
            ),
            None => {
                let borrowed_p50 = percentile(&borrowed_ms, 50);
                let borrowed_p95 = percentile(&borrowed_ms, 95);
                println!("measure: latency_borrowed_p50 {borrowed_p50:.2} ms");
                println!("measure: latency_borrowed_p95 {borrowed_p95:.2} ms");
                println!(
                    "measure: latency_delta_p50 {:.2} ms",
                    borrowed_p50 - percentile(&local_ms, 50)
                );
            }
        }
    }

    lender.shutdown();
    borrower.shutdown();
}

/// Proxy start-to-ready wall clock, ten boots with a `tcr-peers.json` present
/// and ten without, keyed on the same log line `Mac::boot` waits for
/// (`"teamclaude-rs listening on http://"`, `src/main.rs:3732`). Prints the
/// median of each and the delta.
#[test]
#[ignore = "wall-clock measurement; run via scripts/peer-compare-local.sh"]
fn measure_boot_cost_with_and_without_peers_file() {
    const RUNS: u32 = 10;

    println!("STEP: {RUNS} boots with a peers file present");
    let with_peers: Vec<f64> = (0..RUNS).map(|_| boot_cost_once(true)).collect();
    println!("STEP: {RUNS} boots with no peers file");
    let without_peers: Vec<f64> = (0..RUNS).map(|_| boot_cost_once(false)).collect();

    let with_median = percentile(&with_peers, 50);
    let without_median = percentile(&without_peers, 50);
    println!("measure: boot_with_peers_median {with_median:.1} ms");
    println!("measure: boot_without_peers_median {without_median:.1} ms");
    println!("measure: boot_delta {:.1} ms", with_median - without_median);
}

/// One boot of the built `tcr`, `--headless --port 0`, timed from spawn to
/// the ready log line. `with_peers` controls whether a `tcr-peers.json` is
/// written into the scratch HOME before the boot.
fn boot_cost_once(with_peers: bool) -> f64 {
    let home = tempfile::tempdir().expect("a scratch HOME");
    let config_dir = home.path().join(".config");
    std::fs::create_dir_all(&config_dir).expect("the scratch .config");
    std::fs::write(
        config_dir.join("teamclaude.json"),
        r#"{"proxy": {"port": 0}, "accounts": []}"#,
    )
    .expect("the scratch config writes");
    if with_peers {
        let port = free_loopback_port();
        let peers = config_dir.join("tcr-peers.json");
        std::fs::write(
            &peers,
            format!("{{\"listen\":\"127.0.0.1:{port}\",\"peers\":[]}}\n"),
        )
        .expect("the scratch peers file writes");
        std::fs::set_permissions(&peers, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .expect("the peers file takes 0600");
    }

    let log = home.path().join("boot.log");
    let out = std::fs::File::create(&log).expect("the boot log opens");
    let err = out.try_clone().expect("the boot log clones for stderr");
    let mut command = Command::new(env!("CARGO_BIN_EXE_tcr"));
    command
        .args(["--headless", "--port", "0", "--no-replace"])
        // The same colour-off contract as `Mac::env`; this sample owns no
        // `Mac` to borrow it from.
        .env("NO_COLOR", "1")
        .env("HOME", home.path())
        .env("XDG_CACHE_HOME", home.path().join(".cache"))
        .env(
            "TCR_CLAUDE_CODE_CREDENTIALS",
            home.path().join("no-such-claude-code-credentials.json"),
        )
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));

    let start = Instant::now();
    let mut child = command
        .spawn()
        .unwrap_or_else(|err| panic!("spawning the built tcr for a boot-cost sample: {err}"));

    let deadline = Instant::now() + LINE_TIMEOUT;
    loop {
        let body = strip_ansi(&std::fs::read_to_string(&log).unwrap_or_default());
        if body.contains("listening on http://") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no ready line within {LINE_TIMEOUT:?} (with_peers={with_peers}). Tail:\n{}",
            tail(&body, 12)
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let elapsed = start.elapsed().as_secs_f64() * 1000.0;

    let _ = child.kill();
    let _ = child.wait();
    elapsed
}

/// **Five concurrent borrows against TWO lenders, each capped at
/// `max_inflight: 2`: at most four are served at once and the rest are
/// refused.**
///
/// This is the original topology as first written, and it is back
/// because the race that made it unwinnable is fixed. The exploration for
/// [`five_concurrent_borrows_respect_max_inflight`] built this exact topology
/// first and measured five served, zero refused:
/// `PeerLeaseProvider::lease_for` checked its lease cache, missed, released
/// the lock and only then asked the lender, so five concurrent first asks each
/// minted their OWN `lease_id`: and `max_inflight` is enforced per
/// `lease_id`, so the cap bound nothing across them. That test kept the gate
/// meaningful by priming ONE lease and using ONE lender; this one needs no
/// priming, because `lease_for` now single-flights the first ask per lender
/// and five concurrent borrows share the one lease each lender grants.
///
/// So the ceiling here is two lenders times `max_inflight: 2` (four), and it
/// is a claim about the borrower's ask path as much as about the lender's cap.
///
/// Watch it fail by deleting the ask-gate block in `lease_for`
/// (`src/peer/lease.rs`): five leases are minted at bank1 alone and all five
/// borrows are served.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn five_concurrent_borrows_across_two_lenders_stop_at_four() {
    let (upstream, _seen) = spawn_slow_upstream(Duration::from_millis(1500)).await;
    let mut bank1 = Mac::new("bank1", &upstream, "bank1-fake", "at-fake-bank1", false);
    let mut bank2 = Mac::new("bank2", &upstream, "bank2-fake", "at-fake-bank2", false);
    let mut spender = Mac::new(
        "spender",
        &upstream,
        "spender-fake",
        "at-fake-spender",
        true,
    );

    step(1, "boot three tcr processes: two lenders, one borrower");
    bank1.boot();
    bank2.boot();
    spender.boot();

    step(
        2,
        "warm both lenders' own accounts, so each has a window to lend from",
    );
    for lender in [&bank1, &bank2] {
        let (warm, _) = post_messages(&lender.proxy).await;
        assert_eq!(
            warm, 200,
            "{} must serve its own request before it can lend",
            lender.label
        );
    }

    step(
        3,
        "spender joins bank1, which lends it a max_inflight:2 lease",
    );
    let bank1_at_spender = lend_to(&bank1, &mut spender, "spender-mac-1");

    step(4, "spender joins bank2 as well, on the same terms");
    let bank2_at_spender = lend_to(&bank2, &mut spender, "spender-mac-2");
    assert_ne!(
        bank1_at_spender, bank2_at_spender,
        "two joins must leave two distinct pinned lenders"
    );

    step(
        5,
        "restart spender so the peer-lease fallback is installed with BOTH lenders",
    );
    spender.shutdown();
    spender.boot();
    let installed = spender.wait_for_log("peer-lease fallback");
    assert!(
        installed.contains("outcome=Yes"),
        "spender's boot must install a fallback now that it has lenders: {installed}"
    );

    step(
        6,
        "five concurrent borrows, no priming: two lenders times max_inflight 2 is \
         the ceiling",
    );
    let deadline = Instant::now() + LINE_TIMEOUT;
    let handles: Vec<_> = (0..5)
        .map(|_| {
            let proxy = spender.proxy.clone();
            tokio::spawn(async move { post_messages(&proxy).await })
        })
        .collect();
    let mut statuses = Vec::new();
    for handle in handles {
        let (status, _) = handle.await.expect("a borrow task panicked");
        statuses.push(status);
    }
    assert!(
        Instant::now() < deadline,
        "all five concurrent borrows must return within the borrow deadline"
    );
    println!("         statuses: {statuses:?}");

    let served = statuses.iter().filter(|status| **status == 200).count();
    let refused = statuses.len() - served;
    assert_eq!(statuses.len(), 5, "must have five results, one per task");
    assert!(
        served <= 4,
        "two lenders at max_inflight 2 may serve at most 4 of 5 concurrent \
         borrows; 5 means the borrower minted a lease per request and the cap \
         bound nothing: {statuses:?}\nSpender tail:\n{}",
        tail(&spender.log(), 30)
    );
    // The control on the instrument. A run where nothing was served is a run
    // where no lease was ever granted, and it would satisfy the ceiling above
    // while proving nothing about it.
    assert!(
        served >= 2,
        "at least one lender's lease must actually have served requests, or the \
         ceiling above is vacuous: {statuses:?}\nSpender tail:\n{}",
        tail(&spender.log(), 30)
    );
    assert!(
        refused >= 1,
        "the fifth borrow has no lender left with room and must be refused: \
         {statuses:?}"
    );

    let spender_log = spender.log();
    assert!(
        spender_log.contains("InFlightFull"),
        "the refused borrow(s) must be logged with the max_inflight wire word \
         (LeaseRefusal::InFlightFull). Tail:\n{}",
        tail(&spender_log, 30)
    );

    spender.shutdown();
    bank2.shutdown();
    bank1.shutdown();
}

/// Invite `borrower` onto `lender`, lend it a `max_inflight: 2` lease, and let
/// it disclose to that lender: the five CLI steps every borrowing leg in this
/// file repeats, in one place because the two-lender test performs them twice
/// and a second hand-written copy is a second chance to mistype a fraction.
fn lend_to(lender: &Mac, borrower: &mut Mac, label: &str) -> String {
    let pinned_before = borrower.pinned_nodes();
    let minted = lender.peer_ok(&["invite", "--label", label, "--ttl", "300"]);
    let key = minted
        .lines()
        .find(|line| line.starts_with("tcr-join:"))
        .unwrap_or_else(|| panic!("no join key on stdout: {minted}"))
        .to_string();
    join_with_stdin(borrower, &key);
    let borrower_node = lender.first_pinned_node();
    lender.peer_ok(&["share", "on", "--window", "7d", "--fraction", "0.90"]);
    lender.peer_ok(&[
        "lend",
        &borrower_node,
        "--window",
        "7d",
        "--fraction",
        "0.90",
        "--ttl",
        "300",
        "--max-inflight",
        "2",
    ]);
    // The row the join ADDED, by difference rather than by position: which end
    // of the peers file a new row lands on is not this test's to know, and an
    // `allow` aimed at the wrong lender would read as a lender that refuses.
    let lender_node_at_borrower = borrower
        .pinned_nodes()
        .into_iter()
        .find(|node| !pinned_before.contains(node))
        .expect("the borrower pinned the lender it just joined");
    borrower.peer_ok(&["allow", &lender_node_at_borrower, "disclose", "on"]);
    lender_node_at_borrower
}

// ---------------------------------------------------------------------------
// A moved peer
//
// Both plausible paths read one production gap, and only one of
// them has a patch small enough to hand over here; see gap 5 in
// this file's module docs. The second path, "through
// the beacon bridge with Hello disabled," needs a binding that outlives one
// CLI process and is not written here at all, not even ignored: the same
// call a hand-mode leg needs and is still waiting on its own
// rather than shipping as a red placeholder.
// ---------------------------------------------------------------------------

/// **A moved peer, reached through the endpoint a refreshed `Hello` carries,
/// with no re-pairing.**
///
/// `lender` and `mover` pair the way `a_borrowed_request_...` does (join, so
/// the joiner's row carries a dialable address from the start; see gap 1).
/// `mover` then shuts down and reboots on a FRESH port, so `lender`'s row
/// for it names a socket nothing answers on any more. `mover` calls the verb
/// this test is written against, `tcr peer hello <lender>`, which dials
/// `lender` on the address it still has (lender never moved) and sends one
/// `Control::Hello`. The listener's own answering half already records the
/// caller's address on an incoming `Hello` (`listener.rs:1734`, wired and
/// green today, proven by `two_macs_find_each_other_...` recording the same
/// kind of endpoint at pairing time), so this one round trip is also
/// `lender` learning where `mover` is now, and `mover` never goes through
/// `pair` or `join` a second time to get there.
///
/// Ran `#[ignore]`d until the `tcr peer hello` verb this is
/// written against was wired (gap 5, this file's module docs); nothing below this line
/// changed when the verb landed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_moved_peer_is_reached_through_a_refreshed_hello_endpoint() {
    let (upstream, _seen) = spawn_upstream().await;
    let mut lender = Mac::new("lender", &upstream, "lender-fake", "at-fake-lender", false);
    let mut mover = Mac::new("mover", &upstream, "mover-fake", "at-fake-mover", true);

    step(1, "boot two tcr processes: lender and mover");
    lender.boot();
    mover.boot();

    step(
        2,
        "mover joins lender; lender's row for it exists, but names only the ephemeral \
         port the join arrived FROM, never mover's own listen port",
    );
    let minted = lender.peer_ok(&["invite", "--label", "mover-mac", "--ttl", "300"]);
    let key = minted
        .lines()
        .find(|line| line.starts_with("tcr-join:"))
        .unwrap_or_else(|| panic!("no join key on stdout: {minted}"))
        .to_string();
    join_with_stdin(&mover, &key);
    let mover_node = lender.first_pinned_node();
    let lender_node = mover.first_pinned_node();
    let before = lender.ls()["peers"][0]["endpoints"].clone();
    assert_eq!(
        before.as_array().map(Vec::len),
        Some(1),
        "the join must leave exactly one endpoint on lender's row for mover: {before}"
    );
    assert_ne!(
        before[0]["addr"].as_str(),
        Some(format!("127.0.0.1:{}", mover.peer_port).as_str()),
        "a join's endpoint on the ACCEPTING side is the ephemeral port the connection \
         arrived from, never the joiner's own listen port; that gap is what a Hello round \
         trip closes: {before}"
    );

    step(
        3,
        "mover restarts on a FRESH port; lender still has no way to dial it",
    );
    mover.shutdown();
    let old_port = mover.peer_port;
    mover.peer_port = free_loopback_port();
    mover.rewrite_listen_port();
    mover.boot();
    assert_ne!(
        mover.peer_port, old_port,
        "the whole point of this test is a peer answering on a DIFFERENT port"
    );

    step(
        4,
        "mover says hello to lender on lender's still-good address; the one round trip \
         is also lender learning mover's new listen port",
    );
    let hello = mover.peer_ok(&["hello", &lender_node]);
    assert!(
        hello.contains("peer hello: ok"),
        "the verb must confirm the round trip: {hello}"
    );

    step(
        5,
        "lender's row for mover now names a DIALABLE address, mover's fresh listen port, \
         sourced hello, and NOT by re-pairing",
    );
    let ls = lender.ls();
    let endpoints = ls["peers"][0]["endpoints"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let refreshed = endpoints
        .iter()
        .find(|endpoint| {
            endpoint["addr"].as_str() == Some(format!("127.0.0.1:{}", mover.peer_port).as_str())
        })
        .unwrap_or_else(|| {
            panic!(
                "no endpoint on lender's row for mover names its fresh listen port: {endpoints:?}"
            )
        });
    assert_eq!(
        refreshed["source"].as_str(),
        Some("hello"),
        "an endpoint learned from this round trip is sourced `hello`, never `paired`: {refreshed}"
    );
    assert_eq!(
        refreshed["kind"].as_str(),
        Some("direct"),
        "a socket this node can dial itself is a direct locator: {refreshed}"
    );
    assert_eq!(
        ls["peers"].as_array().map(Vec::len),
        Some(1),
        "lender must still hold exactly one pinned row for mover, not a second one from a \
         re-pair: {ls}"
    );
    assert_eq!(
        ls["peers"][0]["node"].as_str(),
        Some(mover_node.as_str()),
        "the refreshed row must be the SAME pinned key, which is what proves nothing re-paired"
    );

    lender.shutdown();
    mover.shutdown();
}

// ---------------------------------------------------------------------------
// A forwarded borrow
//
// The leg `scripts/peer-e2e-local.sh --nodes 3 --moved` runs under the filter
// `forward`, which matched nothing until this file. Its subject is the client
// half of a forward: three real `tcr` processes, the borrower holding NO
// address for its lender at all, and the request arriving anyway because a
// third Mac both of them pinned carried the stream.
// ---------------------------------------------------------------------------

/// Delete every endpoint from one pinned row in a Mac's peers file, leaving
/// every other key and every other row exactly as they were.
///
/// This is the fact the whole case turns on and it is written directly rather
/// than acted out, because the ways a peer's addresses really go stale (it
/// moved, its router mapping expired, it came up on another network) all take
/// wall time this test has no reason to spend. What matters downstream is the
/// row's own content, and `PeerStore` re-reads the file when its mtime moves,
/// so the running borrower sees this within one request.
fn strip_endpoints(mac: &Mac, node: &str) {
    let body = std::fs::read_to_string(&mac.peers).expect("the peers file reads back");
    let mut file: serde_json::Value = serde_json::from_str(&body).expect("the peers file is JSON");
    let rows = file["peers"]
        .as_array_mut()
        .expect("the peers file has a peers array");
    let row = rows
        .iter_mut()
        .find(|row| row["node"].as_str() == Some(node))
        .unwrap_or_else(|| panic!("{}: no pinned row for {node}", mac.label));
    row["endpoints"] = serde_json::Value::Array(Vec::new());
    std::fs::write(
        &mac.peers,
        format!(
            "{}\n",
            serde_json::to_string(&file).expect("it re-serializes")
        ),
    )
    .expect("the peers file rewrites");
    std::fs::set_permissions(
        &mac.peers,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .expect("the peers file takes 0600");
}

/// The endpoints on one Mac's row for `node`, as `tcr peer ls --json` reports
/// them: the running process's own view, not this test's copy of the file.
fn endpoints_for(mac: &Mac, node: &str) -> Vec<serde_json::Value> {
    let ls = mac.ls();
    let rows = ls["peers"]
        .as_array()
        .unwrap_or_else(|| panic!("{}: no peers array: {ls}", mac.label));
    let row = rows
        .iter()
        .find(|row| row["node"].as_str() == Some(node))
        .unwrap_or_else(|| panic!("{}: no pinned row for {node}: {ls}", mac.label));
    row["endpoints"].as_array().cloned().unwrap_or_default()
}

/// Mint an invite on `host`, join `guest` to it, and answer with the node ids
/// each one now holds for the other.
///
/// By DIFFERENCE against the rows each side held first, never by position: a
/// third pairing in this test would otherwise read whichever row the file
/// happens to list first, and the grants below would land on the wrong Mac.
fn join_to(host: &Mac, guest: &Mac, label: &str) -> (String, String) {
    let host_had = host.pinned_nodes();
    let guest_had = guest.pinned_nodes();
    let minted = host.peer_ok(&["invite", "--label", label, "--ttl", "300"]);
    let key = minted
        .lines()
        .find(|line| line.starts_with("tcr-join:"))
        .unwrap_or_else(|| panic!("no join key on stdout: {minted}"))
        .to_string();
    join_with_stdin(guest, &key);
    let guest_at_host = host
        .pinned_nodes()
        .into_iter()
        .find(|node| !host_had.contains(node))
        .unwrap_or_else(|| panic!("{} pinned nothing new when {label} joined", host.label));
    let host_at_guest = guest
        .pinned_nodes()
        .into_iter()
        .find(|node| !guest_had.contains(node))
        .unwrap_or_else(|| panic!("{} pinned nothing new after joining", guest.label));
    (guest_at_host, host_at_guest)
}

/// **A borrow that arrives through a third Mac, with the lender's own address
/// gone from the borrower's file.**
///
/// The topology is the one the forward-dial item exists for. `borrower` holds
/// a lease from `lender` and not one address it answers on; `carrier` is
/// pinned by both, grants `borrower` the right to ask it to forward, and holds
/// `lender`'s real listen port because `lender` said hello to it. The request
/// is then served on the LENDER's own credential, which is what proves the
/// stream reached the lender itself rather than being answered by the Mac in
/// the middle: a forwarder holds no key for what it carries and could not
/// have served this if it wanted to.
///
/// # Three things are asserted and each fails differently
///
/// The borrower's row for the lender has NO endpoint at the moment the request
/// is made, so a pass cannot come from a direct dial that quietly still worked.
/// The upstream saw the lender's credential, so the answer came from the
/// lender. And `carrier`'s log carries `peer forward: carried`, so the bytes
/// really went through the third Mac rather than by some route this test did
/// not think of.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_forwarded_borrow_reaches_the_lender_through_a_third_mac() {
    let (upstream, seen) = spawn_upstream().await;
    let mut lender = Mac::new("lender", &upstream, "lender-fake", "at-fake-lender", false);
    let mut carrier = Mac::new(
        "carrier",
        &upstream,
        "carrier-fake",
        "at-fake-carrier",
        false,
    );
    let mut borrower = Mac::new(
        "borrower",
        &upstream,
        "borrower-fake",
        "at-fake-borrower",
        true,
    );

    step(1, "boot three tcr processes: lender, carrier and borrower");
    lender.boot();
    carrier.boot();
    borrower.boot();

    step(
        2,
        "warm the lender's own account, so it has a measured window to lend from",
    );
    let (warm, _) = post_messages(&lender.proxy).await;
    assert_eq!(
        warm, 200,
        "the lender must serve its own request before it can lend a fraction of it"
    );

    step(3, "borrower joins lender and is granted a lease");
    let lender_at_borrower = lend_to(&lender, &mut borrower, "borrower-mac");

    step(
        4,
        "borrower joins carrier: borrower may ASK it to carry (`carry`), carrier may \
         forward FOR it (`forward`). Two grants, two directions, both explicit",
    );
    let (borrower_at_carrier, carrier_at_borrower) = join_to(&carrier, &borrower, "borrower-mac");
    borrower.peer_ok(&["allow", &carrier_at_borrower, "carry", "on"]);
    carrier.peer_ok(&["allow", &borrower_at_carrier, "forward", "on"]);

    step(
        5,
        "lender joins carrier and says hello, so carrier holds the lender's real LISTEN \
         port and not the ephemeral port the join arrived from",
    );
    let (lender_at_carrier, carrier_at_lender) = join_to(&carrier, &lender, "lender-mac");
    let hello = lender.peer_ok(&["hello", &carrier_at_lender]);
    assert!(
        hello.contains("peer hello: ok"),
        "the round trip that teaches the carrier where the lender listens must confirm: \
         {hello}"
    );
    let listening = format!("127.0.0.1:{}", lender.peer_port);
    let at_carrier = endpoints_for(&carrier, &lender_at_carrier);
    assert!(
        at_carrier
            .iter()
            .any(|endpoint| endpoint["addr"].as_str() == Some(listening.as_str())),
        "the carrier cannot forward to an address it does not hold: {at_carrier:?}"
    );

    step(
        6,
        "the lender's addresses go stale in the borrower's file: its row keeps the pinned \
         key, the lease and the grant, and nothing to dial",
    );
    strip_endpoints(&borrower, &lender_at_borrower);
    let stripped = endpoints_for(&borrower, &lender_at_borrower);
    assert!(
        stripped.is_empty(),
        "the whole case is a lender with no address on the borrower's row: {stripped:?}"
    );

    step(
        7,
        "the borrower's request is served anyway, through the fallback provider it installs \
         itself for on this dry fleet, on the LENDER's own credential, and the carrier's log \
         says it carried the stream",
    );
    let before = seen.requests();
    let mut result = (0_u16, String::new());
    for attempt in 1..=BORROW_SERVED_ATTEMPTS {
        result = post_messages(&borrower.proxy).await;
        println!("         borrower attempt {attempt} -> {}", result.0);
        if result.0 == 200 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
    assert_eq!(
        result.0,
        200,
        "the borrowed request must be served through the carrier. Borrower tail:\n{}\n\
         Carrier tail:\n{}\nLender tail:\n{}",
        tail(&borrower.log(), 14),
        tail(&carrier.log(), 14),
        tail(&lender.log(), 14)
    );
    let credentials = seen.credentials();
    assert!(
        credentials.len() > before,
        "the request never reached the upstream at all: {credentials:?}"
    );
    assert_eq!(
        credentials.last().map(String::as_str),
        Some("Bearer at-fake-lender"),
        "the request that served the borrower must be on the LENDER's own token: a \
         forwarder holds no key for what it carries and could not have answered this: \
         {credentials:?}"
    );

    // The carrier writes this line when the carried stream CLOSES, which is
    // after the borrower already holds its 200, so the line is waited for and
    // never read once. Without it the borrow found some other route and this
    // test is not measuring a forward; the wait panics with the carrier's tail.
    carrier.wait_for_log("peer forward: carried");
    // And the row is STILL addressless, so nothing along the way quietly wrote
    // an endpoint back and turned the next run into a direct dial.
    let after = endpoints_for(&borrower, &lender_at_borrower);
    assert!(
        after.is_empty(),
        "the borrower's row for the lender gained an endpoint during the borrow, so a \
         second run of this test would not be measuring a forward: {after:?}"
    );

    borrower.shutdown();
    carrier.shutdown();
    lender.shutdown();
}

// ---------------------------------------------------------------------------
// E2E-MATRIX: four shapes the ring and the forward tests
// above do not cover, each one scenario, red first.
// ---------------------------------------------------------------------------

/// `tcr peer ls --json`'s `until` for one Mac's row on `node`.
///
/// Only meaningful against a grant minted with `--for` or `--until`:
/// `Lease::until` is copied straight from the GRANT's own end
/// (`src/peer/lease.rs`'s `grant()`, `let end = granted.as_ref().and_then(|g|
/// g.until)`), so an open-ended grant (no `--for`, what [`lend_to`] mints)
/// reads `until: null` REGARDLESS of whether a cached lease is live. A
/// scenario that wants "is spender's cached lease for this row still there"
/// as an observable must lend with an explicit end; see
/// [`lend_to_for_a_bounded_time`].
fn until_for(mac: &Mac, node: &str) -> Option<u64> {
    let ls = mac.ls();
    let rows = ls["peers"]
        .as_array()
        .unwrap_or_else(|| panic!("{}: no peers array: {ls}", mac.label));
    let row = rows
        .iter()
        .find(|row| row["node"].as_str() == Some(node))
        .unwrap_or_else(|| panic!("{}: no pinned row for {node}: {ls}", mac.label));
    row["until"].as_u64()
}

/// [`lend_to`] with an explicit end (`--for`), so the lease it mints carries a
/// real `until` an observer can read with [`until_for`] rather than the
/// permanent `null` an open-ended grant produces. Otherwise identical: same
/// five CLI steps, same fraction, same `max-inflight`.
fn lend_to_for_a_bounded_time(lender: &Mac, borrower: &mut Mac, label: &str, for_: &str) -> String {
    let pinned_before = borrower.pinned_nodes();
    let minted = lender.peer_ok(&["invite", "--label", label, "--ttl", "300"]);
    let key = minted
        .lines()
        .find(|line| line.starts_with("tcr-join:"))
        .unwrap_or_else(|| panic!("no join key on stdout: {minted}"))
        .to_string();
    join_with_stdin(borrower, &key);
    let borrower_node = lender.first_pinned_node();
    lender.peer_ok(&["share", "on", "--window", "7d", "--fraction", "0.90"]);
    lender.peer_ok(&[
        "lend",
        &borrower_node,
        "--window",
        "7d",
        "--fraction",
        "0.90",
        "--ttl",
        "300",
        "--max-inflight",
        "2",
        "--for",
        for_,
    ]);
    let lender_node_at_borrower = borrower
        .pinned_nodes()
        .into_iter()
        .find(|node| !pinned_before.contains(node))
        .expect("the borrower pinned the lender it just joined");
    borrower.peer_ok(&["allow", &lender_node_at_borrower, "disclose", "on"]);
    lender_node_at_borrower
}

/// Rewrite `mac`'s row for `node` to hold exactly one endpoint: `addr`, a
/// direct locator observed just now. Used to put an address on the row that
/// this test controls rather than one a real dial happened to leave there,
/// [`strip_endpoints`]'s sibling, writing a value in rather than emptying it.
fn set_direct_endpoint(mac: &Mac, node: &str, addr: &str) {
    let body = std::fs::read_to_string(&mac.peers).expect("the peers file reads back");
    let mut file: serde_json::Value = serde_json::from_str(&body).expect("the peers file is JSON");
    let rows = file["peers"]
        .as_array_mut()
        .expect("the peers file has a peers array");
    let row = rows
        .iter_mut()
        .find(|row| row["node"].as_str() == Some(node))
        .unwrap_or_else(|| panic!("{}: no pinned row for {node}", mac.label));
    row["endpoints"] = serde_json::json!([{
        "kind": "direct",
        "addr": addr,
        "observedAtMs": 1_700_000_000_000_i64,
        "source": "paired",
    }]);
    std::fs::write(
        &mac.peers,
        format!(
            "{}\n",
            serde_json::to_string(&file).expect("it re-serializes")
        ),
    )
    .expect("the peers file rewrites");
    std::fs::set_permissions(
        &mac.peers,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .expect("the peers file takes 0600");
}

/// **No global IPv6: the recorded endpoint is IPv6 and unreachable, and the
/// borrow still completes, through the forwarder.**
///
/// This machine has no global IPv6 address (loopback-only CI, `reach.rs`'s own
/// module doc says a global v6 address is what lets two Macs skip a relay
/// entirely), so a row whose only direct endpoint is an IPv6 socket is
/// exactly the shape `dial_peer_reaching_within` (`src/peer/serve.rs:1654`)
/// must fall through on, the same way it falls through a stale IPv4 endpoint
/// in `a_forwarded_borrow_reaches_the_lender_through_a_third_mac` above. The
/// difference from that test is deliberate: there the row has NO address at
/// all; here it has one, on a family this box cannot use to reach it, so the
/// direct attempt must fail fast (connection refused on `::1`, not a timeout)
/// before the forwarder loop runs.
///
/// Watch it fail by deleting the `for forwarder in forwarders_for(...)` loop
/// in `dial_peer_reaching_within` (`src/peer/serve.rs`): the IPv6 attempt
/// still fails, nothing else is tried, and this test times out at 429 instead
/// of reaching 200.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn no_ipv6_endpoint_falls_through_to_a_forwarder() {
    let (upstream, seen) = spawn_upstream().await;
    let mut lender = Mac::new("lender", &upstream, "lender-fake", "at-fake-lender", false);
    let mut carrier = Mac::new(
        "carrier",
        &upstream,
        "carrier-fake",
        "at-fake-carrier",
        false,
    );
    let mut borrower = Mac::new(
        "borrower",
        &upstream,
        "borrower-fake",
        "at-fake-borrower",
        true,
    );

    step(1, "boot three tcr processes: lender, carrier and borrower");
    lender.boot();
    carrier.boot();
    borrower.boot();

    step(2, "warm the lender's own account");
    let (warm, _) = post_messages(&lender.proxy).await;
    assert_eq!(
        warm, 200,
        "the lender must serve its own request before it can lend"
    );

    step(3, "borrower joins lender and is granted a lease");
    let lender_at_borrower = lend_to(&lender, &mut borrower, "borrower-mac");

    step(4, "borrower joins carrier and both grants are set");
    let (borrower_at_carrier, carrier_at_borrower) = join_to(&carrier, &borrower, "borrower-mac");
    borrower.peer_ok(&["allow", &carrier_at_borrower, "carry", "on"]);
    carrier.peer_ok(&["allow", &borrower_at_carrier, "forward", "on"]);

    step(
        5,
        "lender joins carrier and says hello, so carrier can forward to it",
    );
    let (_lender_at_carrier, carrier_at_lender) = join_to(&carrier, &lender, "lender-mac");
    let hello = lender.peer_ok(&["hello", &carrier_at_lender]);
    assert!(
        hello.contains("peer hello: ok"),
        "the hello round trip must confirm: {hello}"
    );

    step(
        6,
        "the lender's row on borrower is rewritten to an IPv6-only endpoint no route on this \
         box can reach: no global IPv6, and this address is loopback-only IPv6, so the direct \
         attempt fails fast rather than timing out",
    );
    set_direct_endpoint(&borrower, &lender_at_borrower, "[::1]:1");
    let rewritten = endpoints_for(&borrower, &lender_at_borrower);
    assert_eq!(
        rewritten.len(),
        1,
        "the row must carry exactly the one IPv6 endpoint this step wrote: {rewritten:?}"
    );
    assert_eq!(
        rewritten[0]["addr"].as_str(),
        Some("[::1]:1"),
        "and it must be the IPv6 socket, not a IPv4 one left over from the join: {rewritten:?}"
    );

    step(
        7,
        "the borrow is served anyway, through the carrier, on the lender's credential and \
         the fallback provider the dry fleet installs for itself",
    );
    let before = seen.requests();
    let mut result = (0_u16, String::new());
    for attempt in 1..=BORROW_SERVED_ATTEMPTS {
        result = post_messages(&borrower.proxy).await;
        println!("         borrower attempt {attempt} -> {}", result.0);
        if result.0 == 200 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
    assert_eq!(
        result.0,
        200,
        "the borrowed request must fall through the dead IPv6 endpoint to the carrier. \
         Borrower tail:\n{}\nCarrier tail:\n{}\nLender tail:\n{}",
        tail(&borrower.log(), 14),
        tail(&carrier.log(), 14),
        tail(&lender.log(), 14)
    );
    let credentials = seen.credentials();
    assert!(
        credentials.len() > before,
        "the request never reached the upstream: {credentials:?}"
    );
    assert_eq!(
        credentials.last().map(String::as_str),
        Some("Bearer at-fake-lender"),
        "the answer must be on the LENDER's token, proving it reached the lender and was not \
         answered by the carrier: {credentials:?}"
    );
    // The carrier writes this line when the carried stream CLOSES, which is
    // after the borrower already holds its 200, so the line is waited for and
    // never read once. Without it the borrow found some other route and this
    // test is not measuring a forward; the wait panics with the carrier's tail.
    carrier.wait_for_log("peer forward: carried");
    println!(
        "{}",
        matrix::matrix_line(
            "no_ipv6",
            "IPv6-only direct endpoint fell through to the carrier and served on the lender's \
             credential"
        )
    );

    borrower.shutdown();
    carrier.shutdown();
    lender.shutdown();
}

/// **A forwarder killed mid-borrow releases the client instead of hanging to
/// the deadline.**
///
/// The topology is the forwarded-borrow one, with the lender's own endpoint
/// stripped so every borrow must go through `carrier`. The upstream answers
/// slowly (1.5s), so there is a real window between the borrower opening the
/// forwarded stream and the reply landing; `carrier` is killed inside that
/// window. `borrow_once` (`src/peer/serve.rs`) is mid-stream against a socket
/// whose other end just vanished, so the read must fail with an I/O error
/// almost immediately, and that is the case [`open_serve_within`]'s own doc
/// distinguishes from a lender that is merely slow: that one is `Ok(None)` at
/// the 10-second deadline, this one is `Err` well inside it, and
/// `PeerLeaseProvider::try_serve` logs the two differently
/// (`"peer lease: the SERVE stream failed"` only on the `Err` arm). That log
/// line is this test's named refusal.
///
/// Watch it fail by deleting the `tracing::warn!` call on the `Err(err)` arm
/// of `try_serve`'s match on `serve::open_serve` (`src/peer/lease.rs`): the
/// borrow still fails, but the line this test greps for is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn forwarder_killed_mid_borrow_releases_the_client_promptly() {
    let (upstream, _seen) = spawn_slow_upstream(Duration::from_millis(1500)).await;
    let mut lender = Mac::new("lender", &upstream, "lender-fake", "at-fake-lender", false);
    let mut carrier = Mac::new(
        "carrier",
        &upstream,
        "carrier-fake",
        "at-fake-carrier",
        false,
    );
    let mut borrower = Mac::new(
        "borrower",
        &upstream,
        "borrower-fake",
        "at-fake-borrower",
        true,
    );

    step(1, "boot three tcr processes: lender, carrier and borrower");
    lender.boot();
    carrier.boot();
    borrower.boot();

    step(2, "warm the lender's own account against the slow upstream");
    let (warm, _) = post_messages(&lender.proxy).await;
    assert_eq!(
        warm, 200,
        "the lender must serve its own request before it can lend"
    );

    step(3, "borrower joins lender and is granted a lease");
    let lender_at_borrower = lend_to(&lender, &mut borrower, "borrower-mac");

    step(
        4,
        "borrower joins carrier: carry / forward, both grants explicit",
    );
    let (borrower_at_carrier, carrier_at_borrower) = join_to(&carrier, &borrower, "borrower-mac");
    borrower.peer_ok(&["allow", &carrier_at_borrower, "carry", "on"]);
    carrier.peer_ok(&["allow", &borrower_at_carrier, "forward", "on"]);

    step(5, "lender joins carrier and says hello");
    let (_lender_at_carrier, carrier_at_lender) = join_to(&carrier, &lender, "lender-mac");
    let hello = lender.peer_ok(&["hello", &carrier_at_lender]);
    assert!(
        hello.contains("peer hello: ok"),
        "the hello round trip must confirm: {hello}"
    );

    step(
        6,
        "the lender's addresses go stale on borrower's row: every borrow must go via carrier",
    );
    strip_endpoints(&borrower, &lender_at_borrower);
    assert!(
        endpoints_for(&borrower, &lender_at_borrower).is_empty(),
        "the row must be addressless before the kill, or a direct dial could still answer"
    );

    step(
        7,
        "start one borrow against the slow upstream, then kill carrier while it is in flight",
    );
    let start = Instant::now();
    let proxy = borrower.proxy.clone();
    let task = tokio::spawn(async move { post_messages(&proxy).await });
    tokio::time::sleep(Duration::from_millis(400)).await;
    carrier.shutdown();
    let (status, _body) = task.await.expect("the borrow task panicked");
    let elapsed = start.elapsed();
    println!("         borrow status={status} elapsed={elapsed:?}");

    assert_ne!(
        status, 200,
        "with carrier dead mid-stream and the lender's own row addressless, nothing could have \
         served this request"
    );
    assert!(
        elapsed < Duration::from_secs(6),
        "a forwarder that dies mid-stream must release the client promptly, not ride out the \
         full {BORROW_TIMEOUT_SECS}s borrow deadline as though the lender were merely slow: \
         took {elapsed:?}"
    );
    let borrower_log = borrower.log();
    // EITHER line is the stream failing, and which one depends on whether the
    // body had crossed when the carrier died. The distinction this assertion
    // exists for is the other one: a stream that FAILED, against the
    // deadline-reached line a slow-but-alive lender would produce.
    //
    // The delivered line is the one this fixture usually takes, and it is not
    // the SERVE-stream-failed line any more: a borrow whose body has crossed
    // stops the ladder rather than offering the same request to the next
    // lender, so it is reported as delivered-and-unknown. See
    // `peer::serve::Borrowed`.
    let failed = [
        "peer lease: the SERVE stream failed",
        "the borrowed request was delivered and the exchange then failed",
    ]
    .into_iter()
    .any(|line| borrower_log.contains(line));
    assert!(
        failed,
        "the borrower must log the SERVE-stream failure this test kills carrier to cause, not \
         the deadline-reached line a slow-but-alive lender would produce. Tail:\n{}",
        tail(&borrower_log, 30)
    );
    println!(
        "{}",
        matrix::matrix_line(
            "forwarder_down_mid_borrow",
            "carrier killed mid-stream; borrower released promptly with a named SERVE-stream \
             refusal"
        )
    );

    borrower.shutdown();
    lender.shutdown();
}

/// Mirrors `BORROW_TIMEOUT` (`src/peer/serve.rs`), kept as a named constant
/// here rather than a bare `10` in the assertion above, since this test's
/// whole point is proving the release happens well inside it.
const BORROW_TIMEOUT_SECS: u64 = 10;

/// **A lender that vanishes: the next borrow re-asks rather than hanging on a
/// stale lease, and the eighth-lease cap is not left in a bad state by the
/// crash.**
///
/// `bank` lends to `spender`, one request is served (warming the cache and
/// `until` on `spender`'s own `peer ls --json`), then `bank` is killed:
/// `Mac::shutdown` is `child.kill()`, so this is a crash, not a graceful stop.
/// `spender`'s next request has nothing to dial and must be refused; the case
/// this test pins is that the failure also DROPS the cached lease
/// (`PeerLeaseProvider::try_serve`'s `Ok(None)` arm in `src/peer/lease.rs`),
/// which is what "re-asks" means operationally: `until` reads back `null`
/// rather than a lease `spender` can never spend again. `bank` then reboots on
/// the same port (its identity, in its own scratch HOME, is untouched by the
/// kill) with a FRESH in-memory ledger: `held` for `spender` is back to zero,
/// and `spender`'s next request must be served again, which is the re-ask
/// actually completing. The eighth-lease cap
/// ([`MAX_LEASES_PER_PEER`](../../../src/peer/lease.rs), `src/peer/lease.rs:472`)
/// is asserted by ABSENCE: `bank`'s fresh boot log must never contain the
/// cap's own refusal line, because a ledger that came back corrupted (still
/// counting the pre-crash lease as live) is exactly the failure mode that
/// would produce it here, against a single well-behaved borrower.
///
/// Watch it fail by deleting the cache-drop block (`let dropped = ...
/// self.persist_borrowed();`) from the `Ok(None)` arm of `try_serve`'s match
/// on `serve::open_serve` (`src/peer/lease.rs`): `until` stays non-null after
/// the kill, because nothing ever told `spender` the lease was dead.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_vanished_lenders_next_borrow_re_asks_and_the_cap_is_not_corrupted() {
    let (upstream, _seen) = spawn_upstream().await;
    let mut bank = Mac::new("bank", &upstream, "bank-fake", "at-fake-bank", false);
    let mut spender = Mac::new(
        "spender",
        &upstream,
        "spender-fake",
        "at-fake-spender",
        true,
    );

    step(
        1,
        "boot two tcr processes: bank (lender) and spender (borrower)",
    );
    bank.boot();
    spender.boot();

    step(2, "warm bank's own account");
    let (warm, _) = post_messages(&bank.proxy).await;
    assert_eq!(
        warm, 200,
        "bank must serve its own request before it can lend"
    );

    step(
        3,
        "spender joins bank and is granted a lease that ends in 30 minutes",
    );
    let bank_at_spender = lend_to_for_a_bounded_time(&bank, &mut spender, "spender-mac", "30m");

    step(4, "spender restarts, installing the fallback");
    spender.shutdown();
    spender.boot();
    let installed = spender.wait_for_log("peer-lease fallback");
    assert!(
        installed.contains("outcome=Yes"),
        "boot must install the fallback: {installed}"
    );

    step(5, "one request is served, warming spender's cached lease");
    let (first, _) = post_messages(&spender.proxy).await;
    assert_eq!(
        first, 200,
        "the first borrow must be served while bank is alive"
    );
    let until_before = until_for(&spender, &bank_at_spender);
    assert!(
        until_before.is_some(),
        "a served borrow must leave a live `until` on spender's row for bank: {until_before:?}"
    );

    step(
        6,
        "bank is killed mid-lease, a crash and not a graceful stop",
    );
    bank.shutdown();

    step(
        7,
        "spender's next request is refused, and the cached lease is dropped",
    );
    let (refused, _) = post_messages(&spender.proxy).await;
    assert_ne!(
        refused, 200,
        "nothing can serve this request with bank dead"
    );
    let until_after_kill = until_for(&spender, &bank_at_spender);
    assert_eq!(
        until_after_kill, None,
        "a refused borrow against a dead lender must drop the cached lease, or spender's next \
         attempt would spend a lease bank no longer holds any record of: {until_after_kill:?}"
    );

    step(
        8,
        "bank reboots on the same port, with a fresh in-memory ledger",
    );
    bank.boot();
    let rebooted = bank.wait_for_log("listening on http://");
    assert!(
        rebooted.contains("listening"),
        "bank must come back up: {rebooted}"
    );

    step(
        9,
        "bank warms its own account again: a fresh boot's headroom ticker reads 0.0 until \
         measured, and an unwarmed bank refuses every ask OwnerGuard regardless of the cap",
    );
    let (rewarm, _) = post_messages(&bank.proxy).await;
    assert_eq!(
        rewarm, 200,
        "bank must serve its own request again after rebooting"
    );

    step(
        10,
        "spender's next request re-asks bank fresh and is served again",
    );
    let mut result = (0_u16, String::new());
    for attempt in 1..=BORROW_SERVED_ATTEMPTS {
        result = post_messages(&spender.proxy).await;
        println!("         spender attempt {attempt} -> {}", result.0);
        if result.0 == 200 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
    assert_eq!(
        result.0,
        200,
        "the re-ask must succeed once bank is back. Spender tail:\n{}\nBank tail:\n{}",
        tail(&spender.log(), 20),
        tail(&bank.log(), 20)
    );
    let until_after_reask = until_for(&spender, &bank_at_spender);
    assert!(
        until_after_reask.is_some(),
        "the re-ask must leave a fresh live `until`: {until_after_reask:?}"
    );

    let bank_log = bank.log();
    assert!(
        !bank_log.contains("this peer already holds the most leases"),
        "bank's FRESH ledger must not refuse spender's one legitimate re-ask with the \
         eighth-lease cap; that would mean the crash left the cap in a corrupted state rather \
         than a clean one. Bank tail:\n{}",
        tail(&bank_log, 30)
    );
    println!(
        "{}",
        matrix::matrix_line(
            "lender_vanishes",
            "kill dropped the cached lease, the re-ask after reboot was served, and the \
             eighth-lease cap was not spuriously tripped"
        )
    );

    spender.shutdown();
    bank.shutdown();
}

/// **A captive portal: every outbound request gets an HTTP 302 from a fake
/// gateway, and the keeper is expected to report "no network" without pinning
/// or writing anything.**
///
/// `asks_for_other_lanes`: no production code in this tree reports a "no
/// network" state at all. `spawn_mapping_keeper` (`src/peer/reach.rs:1185`)
/// only ever answers "no gateway responded" (silence) or a NAT-PMP mapping; a
/// captive portal answering every request with a 302 on the TCP link a
/// NAT-PMP UDP probe never touches would today read as the SAME "no gateway"
/// silence a plain unplugged cable produces, with no distinguishing state and
/// nothing a test can observe from outside the process, and `tcr peer reach`
/// has not landed in this tree, and until it
/// does there is no CLI surface this scenario could assert against, and no
/// state field it could read for "nothing is pinned or written" either. This
/// needs a connectivity read ("the keeper holds no
/// mapping and no global IPv6, read `reach`'s state") plus a captive-portal
/// classifier neither piece of work names as its own.
///
/// Left `#[ignore]` rather than deleted, so the file states the gap where a
/// reader looking for this scenario will look for it, and a green run of this
/// scenario one day is what its own `#[ignore]` should have removed.
#[tokio::test]
#[ignore = "needs_unbuilt_pieces: no keeper 'no network' state and no `tcr peer reach` verb \
            exist in this tree to assert a captive-portal HTTP 302 against; those and \
            src/peer/reach.rs's MappingKeeper are not built yet"]
async fn a_captive_portal_reports_no_network_and_pins_nothing() {
    unimplemented!(
        "blocked on `tcr peer reach` (PUNCH) and the keeper's no-network read (REVERSE); see \
         this test's doc-comment"
    );
}

// ---------------------------------------------------------------------------
// The Mac nobody can dial
//
// The leg `scripts/peer-e2e-local.sh --nodes 3 --undialable` runs under the
// filter `undialable`. Its subject is the topology the reverse carry exists
// for: the lender has no address ANYWHERE, not on the borrower's row and not
// on the carrier's, so neither Mac can open a socket to it and the only way in
// is a socket the lender itself opened.
// ---------------------------------------------------------------------------

/// **A lender nothing can dial is still SERVED, over the socket it parked at
/// a friend, inside the borrow timeout.**
///
/// # What this measures today
///
/// The reverse topology in three real processes: the lender has no address on
/// either Mac's row, so the borrower cannot dial it and neither can the Mac it
/// asks to carry. The borrow goes out through the carrier anyway (the
/// borrower's own log line says so, and this asserts it, otherwise the case
/// would pass on a borrower that never asked anyone) and it ENDS: no 200 from
/// a path this test did not set up, and no request held open, which is the
/// half of the gate that has to hold whether or not a carrier was parked.
///
/// The sentence the carrier says when it finds neither a parked carrier nor an
/// address is gated in process, in `tests/peer_forward.rs`, rather than here:
/// a refusal reaches a peer's log through a per-address rate limit
/// (`listener::RefusalLog`) that an enrolment refusal from the same loopback
/// address earlier in the same run can spend, so an assertion on it here would
/// be a flake with a story.
///
/// # What flips it to a served 200, and it landed
///
/// The lender parking a carrier at the carrier Mac: `StreamKind::Park` on the
/// peer wire, the listener arm that reads it and calls
/// `tunnel::park_reverse_carry`, and the boot supervisor that opens one
/// carrier per friend when nothing can dial this Mac. So the assertion below
/// is the 200 the forwarded-borrow test makes, over a socket the lender
/// opened outwards, and the in-process half stays gated by
/// `tests/peer_forward.rs::a_forward_rides_the_carrier_an_undialable_target_parked`.
///
/// Watched red by removing the `StreamKind::Park` arm from
/// `listener::serve_stream`: the carrier never holds a socket, step 7's wait
/// for the desk's own line times out, and the borrow goes back to ending
/// unserved.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn an_undialable_lender_ends_the_borrow_through_a_carrier_and_never_hangs() {
    let (upstream, _seen) = spawn_upstream().await;
    let mut lender = Mac::new("lender", &upstream, "lender-fake", "at-fake-lender", false);
    let mut carrier = Mac::new(
        "carrier",
        &upstream,
        "carrier-fake",
        "at-fake-carrier",
        false,
    );
    let mut borrower = Mac::new(
        "borrower",
        &upstream,
        "borrower-fake",
        "at-fake-borrower",
        true,
    );

    step(1, "boot three tcr processes: lender, carrier and borrower");
    lender.boot();
    carrier.boot();
    borrower.boot();

    step(
        2,
        "warm the lender's own account, so it has a window to lend from",
    );
    let (warm, _) = post_messages(&lender.proxy).await;
    assert_eq!(warm, 200, "the lender serves its own request first");

    step(3, "borrower joins lender and is granted a lease");
    let lender_at_borrower = lend_to(&lender, &mut borrower, "borrower-mac");

    step(
        4,
        "borrower joins carrier: `carry` one way, `forward` the other, both explicit",
    );
    let (borrower_at_carrier, carrier_at_borrower) = join_to(&carrier, &borrower, "borrower-mac");
    borrower.peer_ok(&["allow", &carrier_at_borrower, "carry", "on"]);
    carrier.peer_ok(&["allow", &borrower_at_carrier, "forward", "on"]);

    step(5, "lender joins carrier, so the carrier pins it at all");
    let (lender_at_carrier, carrier_at_lender) = join_to(&carrier, &lender, "lender-mac");
    // The lender's own half of "this Mac may hold a socket for me". Without it
    // `tunnel::reverse_carriers` hands back an empty list and the lender parks
    // nothing, which is the same grant `forwarders_for` reads for every other
    // carry and deliberately not a new one.
    lender.peer_ok(&["allow", &carrier_at_lender, "carry", "on"]);

    step(
        6,
        "the lender becomes undialable: its addresses go from BOTH files, so neither the \
         borrower nor the carrier holds a socket address for it",
    );
    strip_endpoints(&borrower, &lender_at_borrower);
    strip_endpoints(&carrier, &lender_at_carrier);
    assert!(
        endpoints_for(&borrower, &lender_at_borrower).is_empty()
            && endpoints_for(&carrier, &lender_at_carrier).is_empty(),
        "the whole case is a lender with no address on either Mac's row"
    );

    step(
        7,
        "the lender restarts with no address anywhere, so its boot asks the carrier to hold \
         a socket for it",
    );
    lender.shutdown();
    lender.boot();
    // The CARRIER's line, not the lender's: it is the one that says a socket
    // reached the desk and was admitted, which is the fact step 9 rides. The
    // lender's own line says only that it sent a header.
    let parked = carrier.wait_for_log("holding a carrier for a Mac that cannot be dialled");
    assert!(
        parked.contains("open="),
        "the desk reports how many carriers it holds for that Mac, and one of them is this \
         one: {parked}"
    );

    step(
        8,
        "warm the restarted lender again, so it has a MEASURED window to lend from",
    );
    // A restart takes the fleet's utilization with it: `Ledger::may_relay`
    // refuses on an ABSENT measurement (`OwnerGuard`), and the warm-up in step
    // 2 measured a process that is no longer running. This is the same request
    // as step 2 and it is here for the same reason.
    let (rewarm, _) = post_messages(&lender.proxy).await;
    assert_eq!(rewarm, 200, "the restarted lender serves its own request");

    step(
        9,
        "the borrow is SERVED, over the socket the lender parked, and through the fallback \
         provider the borrower's dry fleet installs for itself, inside the borrow timeout",
    );
    let started = std::time::Instant::now();
    let (status, _body) = post_messages(&borrower.proxy).await;
    let elapsed = started.elapsed();
    assert_eq!(
        status,
        200,
        "the only way in to this lender is the carrier it parked, and this is the whole \
         point of the reverse path: nothing else in this run can answer. Carrier tail:\n{}\n\
         Lender tail:\n{}",
        tail(&carrier.log(), 20),
        tail(&lender.log(), 20)
    );
    assert!(
        elapsed < Duration::from_secs(60),
        "the borrow has to END, and it took {elapsed:?}: a forward with nowhere to go must \
         refuse rather than hold the request open"
    );

    // The carry was really ATTEMPTED, so this is a measurement of the reverse
    // topology and not of a borrower that gave up before it asked anyone: the
    // borrower's own line says its lender answered on no address of its own
    // and that a Mac it may ask to carry took the stream. What happens after
    // that, on the carrier, is the half this file exercises and
    // `tests/peer_forward.rs` gates in process, because the carrier's refusal
    // reaches its log through a per-address rate limit that an enrolment
    // refusal in the same run can spend.
    let attempted = borrower.wait_for_log("a Mac this node may ask to carry reached it");
    assert!(
        attempted.contains("via="),
        "the borrow has to have gone out through the carrier for this case to be about \
         reach at all: {attempted}"
    );

    borrower.shutdown();
    carrier.shutdown();
    lender.shutdown();
}

/// **A key carries every address that Mac answers at, and the joiner works
/// down the list.**
///
/// The real event this closes: a key minted on a Mac listening on `0.0.0.0`
/// carried `0.0.0.0`, and the friend's `tcr peer join` dialled its own
/// machine. The fix is a list, and a list is only worth carrying if the second
/// address is really tried when the first one refuses, which is what this
/// measures: the key handed to the guest below names a port nothing is
/// listening on FIRST and the host's real one second.
///
/// Two processes, because the claim is about the shipped binary: the joiner's
/// own fall-through is invisible from inside the library, and the line an
/// operator reads (`peer join: ok addr=…`) has to name the address that
/// answered rather than the one at the top of the key.
///
/// Watched red: with `connect_in_key_order` returning after its first
/// attempt, the join exits non-zero and the failure names the dead port.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_join_key_falls_through_to_the_second_address() {
    let (upstream, _seen) = spawn_upstream().await;
    let mut host = Mac::new("host", &upstream, "host-fake", "at-fake-host", false);
    let mut guest = Mac::new("guest", &upstream, "guest-fake", "at-fake-guest", false);

    step(1, "boot the two tcr processes");
    host.boot();
    guest.boot();

    step(
        2,
        "the host mints a key, which carries the address it listens on",
    );
    let minted = host.peer_ok(&["invite", "--label", "guest-mac", "--ttl", "300"]);
    let key = minted
        .lines()
        .find(|line| line.starts_with("tcr-join:"))
        .unwrap_or_else(|| panic!("no join key on stdout: {minted}"))
        .to_string();
    let live = format!("127.0.0.1:{}", host.peer_port);
    assert!(
        key.contains(&live),
        "the key has to carry the address the host really listens on: {key}"
    );
    assert!(
        minted.contains(&format!("peer invite: chosen {live}")),
        "the mint names each address it carried: {minted}"
    );

    step(
        3,
        "put a port nothing is listening on at the FRONT of the key's address list",
    );
    // Borrowed and released, so a connect to it is refused rather than
    // answered by something this test did not start.
    let dead = format!("127.0.0.1:{}", free_loopback_port());
    let doctored = key.replacen(&live, &format!("{dead},{live}"), 1);
    assert_ne!(doctored, key, "the fixture has to differ from the real key");

    step(
        4,
        "the guest joins: the first address refuses, the second answers",
    );
    let stdout = join_with_stdin(&guest, &doctored);
    assert!(
        stdout.contains(&format!("peer join: ok addr={live}")),
        "the line an operator reads must name the address that ANSWERED, not the first \
         one in the key: {stdout}"
    );
    assert!(
        !stdout.contains(&dead),
        "the dead address is not what this join ran over: {stdout}"
    );

    step(5, "and both Macs hold a pinned row for the other");
    assert_eq!(
        guest.pinned_nodes().len(),
        1,
        "the joiner pins the registrar it enrolled with"
    );
    assert_eq!(
        host.pinned_nodes().len(),
        1,
        "and the registrar pins the joiner"
    );

    guest.shutdown();
    host.shutdown();
}
