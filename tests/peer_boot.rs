//! **The LAN peer mesh's production caller**: a booted proxy answers a peer.
//!
//! Everything under `src/peer/` was reachable only from `tcr peer` subcommands
//! and from test binaries. The accept loop, the two-phase pairing, the stream
//! gate and the lender's half of a SERVE all existed and no serving process
//! ever called any of them: so a Mac running `tcr` answered nobody on any
//! port, and every gate that proved the mesh worked proved it about a test
//! binary. `server::serve` boots the listener now (`boot_peer_listener`), and
//! this file is what says so from outside the process.
//!
//! # Nothing here touches the live proxy or the operator's own files
//!
//! Every test binds `127.0.0.1:0` on BOTH sockets: the proxy's own and the
//! peer port, so the kernel picks both and neither can be `3456`. The config
//! and the peers file are in a fresh `tempfile::tempdir`, and the peers file's
//! own directory is where the node key and the `peer-state.json` resolve
//! (`serve::peer_state_path`), so no test here reads or writes anything under
//! the operator's config directory or cache directory. No account is
//! configured, so nothing can reach
//! Anthropic.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use teamclaude_rs::config::Config;
use teamclaude_rs::peer::{config as peer_config, serve as peer_serve};
use teamclaude_rs::server::{serve, IncumbentPolicy, ServeOptions, ServeOutcome, TlsSetup};
use teamclaude_rs::singleton::ProxyHost;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// The port `tcr` uses by default. Named so the assertion below says what it is
/// protecting rather than showing a bare number.
const LIVE_PROXY_PORT: u16 = 3456;

/// No accounts, an ephemeral port, and both timer loops off: nothing in this
/// file may reach Anthropic or spend quota.
fn test_config() -> Config {
    serde_json::from_str(
        r#"{
            "proxy": { "port": 0 },
            "quotaProbeSeconds": 0,
            "warmupSeconds": 0,
            "accounts": []
        }"#,
    )
    .expect("the inline test config parses")
}

/// A whole profile in one temp directory: the config this process may write,
/// and the peers file beside it that `serve` resolves off it.
struct Profile {
    dir: tempfile::TempDir,
}

impl Profile {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("a temp profile directory");
        std::fs::write(dir.path().join("teamclaude.json"), "{}")
            .expect("the temp config file writes");
        Self { dir }
    }

    fn config_path(&self) -> std::path::PathBuf {
        self.dir.path().join("teamclaude.json")
    }

    fn peers_path(&self) -> std::path::PathBuf {
        self.dir.path().join("tcr-peers.json")
    }

    /// Write a peers file with `listen` on a kernel port, and whatever else the
    /// caller wants in it.
    fn write_peers(&self, file: peer_config::PeerFile) {
        peer_config::save(&self.peers_path(), &file).expect("the temp peers file writes");
    }
}

fn options(profile: &Profile) -> ServeOptions {
    ServeOptions {
        config: test_config(),
        // The config this process owns, which is also what resolves the peers
        // file beside it: see `server::peers_file_beside_config`.
        persist_path: Some(profile.config_path()),
        port: Some(0),
        // The only policy that signals nothing. A test that could reach
        // `takeover_port` could SIGKILL the developer's live proxy.
        incumbent: IncumbentPolicy::never_signal(),
        affinity_path: None,
        wire_sessions_path: None,
        usage_dir: None,
        // Loading the MITM material mints or reads a CA on disk.
        tls: TlsSetup::Disabled,
        host: ProxyHost::Cli,
        owner_dir: None,
        inherited_listener: None,
    }
}

/// A peers file that opens a peer port on a kernel-assigned port and pins
/// nobody.
fn listening_peers_file() -> peer_config::PeerFile {
    peer_config::PeerFile {
        listen: Some(
            "127.0.0.1:0"
                .parse()
                .expect("a literal loopback address parses"),
        ),
        ..peer_config::PeerFile::default()
    }
}

// ---------------------------------------------------------------------------
// Item 1: `listener::serve` gets its production caller
// ---------------------------------------------------------------------------

/// **A booted server answers a knock on its peer port, and refuses `/_tcr/` on
/// it.**
///
/// Three claims, and each is a separate way the wiring could be absent:
///
/// 1. the port EXISTS: `serve` bound it and says which one, which a peers file
///    with `listen: 127.0.0.1:0` makes a number only the kernel knew;
/// 2. the production accept loop is behind it: a knock gets the one ack byte a
///    knock earns (`noise::send_knock`), which only `serve_on_with`'s knock arm
///    writes, and only after the rate limit, the mute list and the pending cap
///    have all said yes;
/// 3. **it is not the local control socket**: an HTTP `GET /_tcr/status`, the
///    one route the proxy answers itself, gets NOTHING on this port: the
///    listener reads the first bytes, sees they are not a Noise message 1, and
///    closes having written zero bytes. The same request on the proxy's own
///    port answers 200, which is the positive control that stops claim 3 from
///    passing because the request was malformed or the server was dead.
///
/// Watched red by deleting the `boot_peer_listener(..).await` call from
/// `server::serve`: `peer_addr()` is then `None` and claim 1 fails first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_booted_server_answers_a_knock_and_refuses_the_local_route_on_its_peer_port() {
    let profile = Profile::new();
    profile.write_peers(listening_peers_file());

    let ServeOutcome::Started(mut handle) = serve(options(&profile))
        .await
        .expect("the library boots the proxy")
    else {
        panic!("this test may not run against an incumbent proxy");
    };

    // Claim 1: the peer port exists, and it is not the live proxy's.
    let peer_addr = handle
        .peer_addr()
        .expect("a peers file with `listen` set boots the peer listener");
    assert_ne!(peer_addr.port(), LIVE_PROXY_PORT);
    assert_ne!(handle.addr().port(), LIVE_PROXY_PORT);
    assert_ne!(
        peer_addr.port(),
        handle.addr().port(),
        "the peer listener is a SECOND socket, never the proxy's own"
    );

    // Claim 2: the production accept loop is behind it.
    let mut stream = tokio::net::TcpStream::connect(peer_addr)
        .await
        .expect("the peer port accepts a connection");
    let knock = tcr_peer_wire::Knock {
        instance_id: tcr_peer_wire::InstanceId([7_u8; tcr_peer_wire::INSTANCE_ID_BYTES]),
        proposed_name: Some("studio-mac".to_string()),
        wire_version: tcr_peer_wire::PROTO_VERSION,
        listen_port: Some(7766),
    };
    teamclaude_rs::peer::noise::send_knock(&mut stream, &knock, None)
        .await
        .expect("the booted listener takes a knock and answers the one ack byte");

    // Claim 3: the local control route is not on this socket.
    let mut local_route = tokio::net::TcpStream::connect(peer_addr)
        .await
        .expect("a second connection to the peer port");
    local_route
        .write_all(b"GET /_tcr/status HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        .await
        .expect("the bytes go out");
    let mut answer = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(5), local_route.read_to_end(&mut answer))
        .await
        .expect("the peer listener closes rather than hanging on a non-Noise first frame");
    match read {
        Ok(bytes) => assert_eq!(
            bytes,
            0,
            "the peer listener wrote {bytes} bytes to an HTTP request for the LOCAL control \
             route: {}",
            String::from_utf8_lossy(&answer)
        ),
        // A RESET is the same fact told by the kernel rather than by the
        // socket: this side had already written a whole HTTP request, so a
        // close with those bytes still unread makes macOS answer RST instead
        // of FIN. What matters either way is `answer`, which is what the peer
        // listener WROTE: and a refusal writes nothing.
        Err(err) if matches!(err.kind(), std::io::ErrorKind::ConnectionReset) => assert!(
            answer.is_empty(),
            "the peer listener wrote bytes before resetting: {}",
            String::from_utf8_lossy(&answer)
        ),
        Err(err) => panic!("reading the peer listener's answer: {err}"),
    }

    // The positive control for claim 3: that exact request IS answered, on the
    // proxy's own port. Without it, "zero bytes" could mean the server was
    // never up.
    let status = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("a loopback client")
        .get(format!(
            "http://127.0.0.1:{}{}",
            handle.addr().port(),
            teamclaude_rs::proxy::STATUS_PATH
        ))
        .send()
        .await
        .expect("the proxy's own port answers its own route");
    assert_eq!(
        status.status().as_u16(),
        200,
        "positive control: `/_tcr/status` is a real route, so its silence on the peer port \
         is about the peer port"
    );

    handle.shutdown().await;
}

/// **`find` on with no `listen` opens no port**, and says what to do instead.
///
/// The spec says to boot "when the peers file has `find` on (or a listen
/// address)". `find` alone cannot get there: there is no default peer port
/// anywhere in this tree, so booting on `find` alone would mean inventing one
/// in `server.rs`: a port the peers file does not name, which every other
/// reader of that file (the beacon, a share link, `tcr peer`) would then
/// disagree with. The same requirement holds from the other
/// side: the peers file must hold `listen` before the first connection.
///
/// So this is the documented refusal, asserted rather than described. This
/// report carries it as a disagreement with the brief's parenthetical.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn find_on_with_no_listen_address_opens_no_peer_port() {
    let profile = Profile::new();
    profile.write_peers(peer_config::PeerFile {
        discovery: true,
        listen: None,
        ..peer_config::PeerFile::default()
    });

    let ServeOutcome::Started(mut handle) = serve(options(&profile))
        .await
        .expect("the library boots the proxy")
    else {
        panic!("this test may not run against an incumbent proxy");
    };
    assert_eq!(
        handle.peer_addr(),
        None,
        "`find` on with no `listen` names no port, so none is opened"
    );
    handle.shutdown().await;
}

/// **A peers file with neither `listen` nor `find` opens nothing at all**: the
/// default, and the whole of the feature flag.
///
/// The control for the two tests above: without it, a `peer_addr()` of `None`
/// could mean the boot path never runs rather than that it declined.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_default_peers_file_opens_no_peer_port() {
    let profile = Profile::new();
    // No peers file at all, which is what a fresh install has.
    let ServeOutcome::Started(mut handle) = serve(options(&profile))
        .await
        .expect("the library boots the proxy")
    else {
        panic!("this test may not run against an incumbent proxy");
    };
    assert_eq!(handle.peer_addr(), None);
    handle.shutdown().await;
}

// ---------------------------------------------------------------------------
// The beacon and the listener must name ONE instance id
// ---------------------------------------------------------------------------

/// **The instance id the boot path's listener holds is the one the beacon
/// announces**: one value per process, minted once.
///
/// The ask was to "pass the SAME boot instance id
/// (`SessionContext::instance_id()`) to discovery if the boot path starts it".
/// It is satisfied by construction rather than by an argument: both sides read
/// `peer::id::boot_instance_id()`, which is a `OnceLock`, so there is no
/// parameter through which they could differ: `SessionContext::new` reads it
/// (`src/peer/listener.rs`) and `discovery::build_beacon_info` reads it on the
/// beacon's side. `discovery::advertise` therefore takes no instance argument
/// at all, which is why this is asserted rather than wired.
///
/// What is asserted is exactly that: the context a booted listener is built
/// with names the process's boot instance id, and the beacon TXT built for that
/// same id carries it. A second read minting a second id would fail the first
/// assertion; a beacon that named something else would fail the second.
///
/// Watched red by replacing `boot_instance_id()` in `SessionContext::new` with
/// a freshly minted id: the two stop agreeing.
#[test]
fn the_listener_and_the_beacon_name_one_boot_instance_id() {
    let dir = tempfile::tempdir().expect("a temp profile directory");
    let peers = dir.path().join("tcr-peers.json");
    let key = teamclaude_rs::peer::id::NodeKey::load_or_mint(dir.path()).expect("a node key");
    let context = teamclaude_rs::peer::listener::SessionContext::new(
        &key,
        &peers,
        &peer_serve::peer_state_path(&peers),
    );

    let boot = teamclaude_rs::peer::id::boot_instance_id();
    assert_eq!(
        context.instance_id(),
        boot,
        "a listener built at boot must knock and answer under the process's own instance id"
    );
    // And a second context is the same id: one value per process, so a boot
    // path that builds the listener and the beacon in either order cannot
    // announce one id and answer under another.
    let second = teamclaude_rs::peer::listener::SessionContext::new(
        &key,
        &peers,
        &peer_serve::peer_state_path(&peers),
    );
    assert_eq!(second.instance_id(), boot);

    // The beacon's side: the TXT record for that id carries it, so the two
    // surfaces really are naming one value and not two that happen to match.
    let txt = teamclaude_rs::peer::discovery::beacon_txt(&boot, 9_600, None, None, 0);
    assert!(
        txt.iter().any(|(_, value)| value == &boot.to_wire()),
        "the beacon TXT names the boot instance id: {txt:?}"
    );
}

/// The state file a booted listener writes is the one beside its peers file,
/// never the operator's own cache file at `teamclaude/peer-state.json`
/// under their cache directory.
///
/// The gate for the resolution `boot_peer_listener` uses. Without it a
/// `--config` pointing at a temp profile would have its knocks, mutes and bans
/// land in the real machine's runtime state: which is also how a test suite
/// comes to depend on whether the developer has a pairing window open.
#[test]
fn a_profiles_peer_state_file_is_the_one_beside_its_peers_file() {
    let beside =
        peer_serve::peer_state_path(std::path::Path::new("/tmp/tcr-unit-profile/tcr-peers.json"));
    assert_eq!(
        beside,
        std::path::PathBuf::from("/tmp/tcr-unit-profile/peer-state.json")
    );

    // And the DEFAULT peers file keeps the config-directory-to-cache-directory split that
    // `state::default_path` owns, rather than putting runtime state in the
    // config directory.
    assert_eq!(
        peer_serve::peer_state_path(&peer_config::default_path()),
        teamclaude_rs::peer::state::default_path()
    );
}

// ---------------------------------------------------------------------------
// Item 4: the lender's log line names four things and nothing else
// ---------------------------------------------------------------------------

/// One captured tracing event: its message and its field set.
#[derive(Debug, Clone)]
struct CapturedEvent {
    message: String,
    fields: BTreeMap<String, String>,
}

#[derive(Clone, Default)]
struct CaptureLayer {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

struct FieldVisitor(BTreeMap<String, String>);

impl tracing::field::Visit for FieldVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_string(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CaptureLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = FieldVisitor(BTreeMap::new());
        event.record(&mut visitor);
        let message = visitor
            .0
            .get("message")
            .cloned()
            .unwrap_or_else(|| "<no message field>".to_string());
        self.events
            .lock()
            .expect("capture lock")
            .push(CapturedEvent {
                message,
                fields: visitor.0,
            });
    }
}

impl CaptureLayer {
    fn matching(&self, needle: &str) -> Vec<CapturedEvent> {
        self.events
            .lock()
            .expect("capture lock")
            .iter()
            .filter(|event| event.message == needle)
            .cloned()
            .collect()
    }
}

/// **The lender writes ONE line per served request and it names four things:
/// the peer, the window, the bytes and the milliseconds.**
///
/// No path, no model, no body, no header. The prompts of a borrowed request are
/// in the lender's process because that is what borrowing an account means: and
/// a log file is a copy of them that outlives the request, on a machine whose
/// operator agreed to serve traffic, not to keep a transcript of it.
///
/// The field set is asserted EXACTLY, in both directions: a field added to that
/// line fails this test, and a field removed fails it too. That is the half a
/// grep for a few names cannot do: the failure this guards against is a field
/// nobody wrote down, and every one of them passes a test that only checks the
/// four it knows.
///
/// The lease accounting (`observed_rise`, `debited`) is asserted to be on a
/// DIFFERENT line, which is where `handle_serve_on` puts it: both figures are
/// honest and neither is a prompt, but the four fields above are the line an
/// operator reads to answer "who used my account and for how long", and a line
/// whose field set grows is a line whose next field nobody argues about.
///
/// This test owns the process-wide tracing subscriber, so it is the only
/// log-capture test in this binary.
///
/// Watched red by putting `observed_rise` and `debited` back on the `info!`:
/// the exact-set assertion then names both as unexpected.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_lenders_served_line_names_the_peer_the_window_the_bytes_and_the_ms() {
    let capture = CaptureLayer::default();
    {
        use tracing_subscriber::layer::SubscriberExt as _;
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        tracing::subscriber::set_global_default(subscriber)
            .expect("no other default is set in this test binary");
    }

    let served = lender::serve_one_relayed_request().await;
    assert_eq!(
        served.status, 200,
        "the relayed request was served, so there is a line to read"
    );

    // Every `SERVED_LINE` this borrower caused. Filtered by peer and not
    // counted raw: the scope gate above serves a request in this same process,
    // and a raw count of the message would be a count of both tests.
    let lines: Vec<_> = capture
        .matching(peer_serve::SERVED_LINE)
        .into_iter()
        .filter(|event| event.fields.get("peer") == Some(&served.peer))
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "one served request, one line for this borrower ({}): {:?}",
        served.peer,
        capture.events.lock().expect("capture lock").len()
    );

    let fields: Vec<&str> = lines[0]
        .fields
        .keys()
        .map(String::as_str)
        .filter(|name| *name != "message")
        .collect();
    assert_eq!(
        fields,
        vec!["bytes", "ms", "peer", "window"],
        "the lender's served line carries exactly four fields (BTreeMap order). A field this \
         test has never heard of is how a path, a model or a header ends up in a log file on \
         somebody else's machine; a field that vanished is an operator who can no longer \
         answer who used their account."
    );

    // The accounting is on its own line, and it is not this one.
    let accounting = lines[0].fields.keys().any(|name| {
        name == "observed_rise" || name == "debited" || name == "path" || name == "model"
    });
    assert!(
        !accounting,
        "the served line carries accounting or request detail: {:?}",
        lines[0].fields
    );
}

/// **A lease scoped to group `work` is never served on an account outside it**
///: the picker restriction, measured at the upstream.
///
/// Two accounts on the lender: `spare-fake` first (so an unrestricted pick
/// lands on it) and `work-fake` in group `work`. The lease draws from
/// `group:work`. The fake upstream records the `authorization` of every request
/// that reaches it, and the lender's own proxy substitutes the SERVING
/// account's pooled Bearer on the way out: so the credential that arrived is
/// which account actually paid.
///
/// **This is measured upstream and not asserted about the lender's intent.** A
/// test that read the header the lender attached would pass for a picker that
/// ignored it, which is exactly the failure here: the group header is
/// PREFER-shaped for a spill group and strict otherwise, and nothing in the
/// scope itself says which.
///
/// Watched red by replacing `utilization.scope_restriction(&scope)` in
/// `handle_serve_on` with `ScopeRestriction::Unrestricted`: the request is then
/// served on `at-fake-spare` and the assertion names the credential that
/// arrived.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_group_scoped_lease_never_serves_on_an_account_outside_it() {
    let (served, credentials) = lender::serve_one_request_scoped_to_work().await;
    assert_eq!(served.status, 200, "the relayed request was served");

    assert!(
        !credentials.is_empty(),
        "the positive control: the upstream really did see the relayed request, so an \
         absence below would mean something"
    );
    for credential in &credentials {
        assert!(
            credential.contains("at-fake-work"),
            "a lease scoped to group `work` was served on an account outside it: the \
             credential that reached the upstream was {credential:?} (every one seen: \
             {credentials:?})"
        );
    }
}

/// **A session already pinned to an out-of-scope account does not drag a
/// borrowed request onto it**: the affinity fast-path honours a strict group.
///
/// The hole this closes was proven, not suspected. `Manager::select_with_group`
/// honours an existing pin through four eligibility calls that all pass
/// `group: None` (a warm pin is never re-litigated against a per-request
/// PREFERENCE), and `Manager::reserved_blocks` answers `false` for any
/// `Some(g)`: so nothing in that path looked at group MEMBERSHIP at all. The
/// lender's serving leg expresses the lease scope as exactly that header,
/// so a borrowed request whose session key collided with a local session's pin
/// was served on an account OUTSIDE the lease's scope.
///
/// The collision is the ordinary case rather than a contrivance: the pin is
/// keyed on the client's stable identity (`proxy::stable_session_key`), and
/// this harness gives the pre-pinning local request and the borrowed request
/// the same request body (the same `metadata.user_id`, which is tier 2), so
/// both derive one key, which is what two Macs running one operator's harness
/// look like.
///
/// Two assertions, and the first is the instrument: the local request really
/// was served on `at-fake-spare` (so a pin on the out-of-scope account exists
/// when the relay arrives), and every credential after it is `at-fake-work`.
///
/// Watched red by reverting the `out_of_strict_group` branch in
/// `Manager::select_with_group`'s affinity fast-path: the relayed request is
/// then served on `at-fake-spare`, the account the lease does not name.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pinned_out_of_scope_account_never_serves_a_group_scoped_lease() {
    let (served, pinned_on, credentials) =
        lender::serve_one_request_scoped_to_work_after_a_pin().await;
    assert_eq!(served.status, 200, "the relayed request was served");

    // THE INSTRUMENT, checked before the claim: the local request that came
    // first was served on the account the lease does NOT name, so a pin on it
    // exists. Without this, a fixture whose pin never landed would pass every
    // assertion below while testing nothing.
    assert!(
        pinned_on.contains("at-fake-spare"),
        "the pre-pinning local request was meant to land on the out-of-scope account and \
         landed on {pinned_on:?} instead, so this test never had a pin to defeat"
    );
    assert!(
        !credentials.is_empty(),
        "the positive control: the upstream saw the relayed request"
    );
    for credential in &credentials {
        assert!(
            credential.contains("at-fake-work"),
            "a lease scoped to group `work` was served on the session's pinned account \
             outside it: the credential that reached the upstream was {credential:?} (every \
             one seen after the pin: {credentials:?})"
        );
    }
}

/// **A lease scoped to `account:work-fake` is served only on that account** :
/// the third lease scope, which this build used to refuse outright.
///
/// `--scope account:<label>` parsed, was written into the peers file and then
/// answered `ScopeRestriction::Unenforceable` at serve time, because the only
/// account-selection header the proxy read was the GROUP one. The lender now
/// writes the account set on `proxy::ACCOUNTS_HEADER_NAME` and the picker
/// benches every account outside it before the rotation loop starts.
///
/// Measured at the upstream, for the same reason the group gate is: the
/// credential that arrives names the account that actually paid, and a test
/// that read the header the lender attached would pass for a picker that
/// ignored it.
///
/// `spare-fake` is first in the fixture, so an unrestricted pick lands on it :
/// the scope is doing the work, not the account order.
///
/// Watched red by dropping the `ACCOUNTS_HEADER_NAME` arm from
/// `serve_on_own_account`: the relayed request is then served on
/// `at-fake-spare`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_account_scoped_lease_serves_only_the_account_it_names() {
    let (served, credentials) = lender::serve_one_request_scoped_to_one_account().await;
    assert_eq!(served.status, 200, "the relayed request was served");

    assert!(
        !credentials.is_empty(),
        "the positive control: the upstream really did see the relayed request"
    );
    for credential in &credentials {
        assert!(
            credential.contains("at-fake-work"),
            "a lease scoped to `account:work-fake` was served on an account outside it: the \
             credential that reached the upstream was {credential:?} (every one seen: \
             {credentials:?})"
        );
    }
}

/// One lender, one borrower, one relayed request: the smallest harness that
/// makes the lender's log line exist.
///
/// A module rather than inline so the test above reads as the assertion it is.
/// Everything here is on kernel ports in temp directories; see this file's
/// module docs.
mod lender {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::HeaderMap;
    use axum::response::Response;
    use axum::routing::any;
    use axum::Router;
    use tcr_peer_wire::{Lease, LeaseUnit, PeerId, Window};
    use teamclaude_rs::fallback::Ask;
    use teamclaude_rs::peer::config::{
        Allow, ControlGrants, Endpoint, EndpointSource, LendGrant, PeerFile, PeerRow, PeerStore,
    };
    use teamclaude_rs::peer::id::NodeKey;
    use teamclaude_rs::peer::lease::Ledger;
    use teamclaude_rs::peer::listener::{self, LeaseServing, SessionContext};
    use teamclaude_rs::peer::serve;

    /// The lease id every test in this file spends.
    const LEASE: u128 = 0x1eaf_0000_0000_0001;

    /// What one relayed request came back as.
    pub struct Served {
        pub status: u16,
        /// The BORROWER's own id, as the lender's log line spells it.
        ///
        /// Here because two tests in this binary each serve one request and
        /// the tracing capture is process-wide: a count of every `SERVED_LINE`
        /// is a count of both, and which one lands first depends on the
        /// scheduler. The borrower's key is fresh per harness run, so this is
        /// the field that tells one served request from the other.
        pub peer: String,
    }

    /// Every `authorization` header the fake upstream saw, in arrival order.
    ///
    /// This is what makes the scope gate a MEASUREMENT rather than an
    /// inspection of the lender's intent: the proxy substitutes the serving
    /// account's own pooled Bearer on the way out, so the credential that
    /// arrived upstream names which account actually paid for the request.
    pub type Credentials = Arc<std::sync::Mutex<Vec<String>>>;

    /// A fake upstream that answers a canned body, so the lender's own proxy
    /// has something to serve from without reaching Anthropic.
    async fn spawn_upstream() -> String {
        spawn_upstream_recording(Arc::new(std::sync::Mutex::new(Vec::new()))).await
    }

    /// The same, recording the credential of every request that reaches it.
    async fn spawn_upstream_recording(seen: Credentials) -> String {
        let app = Router::new().fallback(any(move |req: axum::extract::Request| {
            let seen = Arc::clone(&seen);
            async move {
                if let Some(credential) = req
                    .headers()
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                {
                    seen.lock()
                        .expect("the credential log is never poisoned")
                        .push(credential.to_string());
                }
                let _ = axum::body::to_bytes(req.into_body(), 1024 * 1024).await;
                Response::builder()
                    .status(200)
                    .header("content-type", "application/json")
                    .body(Body::from(br#"{"type":"message"}"#.to_vec()))
                    .expect("build the canned answer")
            }
        }));
        let listening = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the fake upstream");
        let addr = listening.local_addr().expect("upstream addr");
        tokio::spawn(async move {
            let _ = axum::serve(listening, app).await;
        });
        format!("http://{addr}")
    }

    /// A manager with ONE fake account, pointed at `upstream`. No real email,
    /// no org uuid, no account uuid: this repository is public.
    fn lending_manager(upstream: &str) -> Arc<teamclaude_rs::manager::Manager> {
        let config: teamclaude_rs::config::Config = serde_json::from_str(&format!(
            r#"{{
                "proxy": {{ "port": 0 }},
                "upstream": "{upstream}",
                "quotaProbeSeconds": 0,
                "warmupSeconds": 0,
                "accounts": [
                    {{
                        "name": "lender-fake",
                        "accessToken": "at-fake-lender",
                        "accountUuid": "11111111-1111-1111-1111-111111111111",
                        "orgUuid": "22222222-2222-2222-2222-222222222222"
                    }}
                ]
            }}"#
        ))
        .expect("the inline lender config parses");
        teamclaude_rs::manager::Manager::with_live_refresher(config, None)
    }

    /// The lender's own proxy, which is where a relayed request is sent: the
    /// whole of "own picker, own Bearer, own bucket".
    async fn spawn_proxy(manager: Arc<teamclaude_rs::manager::Manager>) -> String {
        let listening = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the lender's proxy");
        let addr = listening.local_addr().expect("proxy addr");
        tokio::spawn(async move {
            teamclaude_rs::mitm::serve(listening, manager, None).await;
        });
        format!("http://{addr}")
    }

    /// The same row, with the lease drawing from `scope`.
    fn lender_row_scoped(borrower: PeerId, scope: tcr_peer_wire::LendScope) -> PeerRow {
        let mut grant = LendGrant::new(Window::SevenDay, 0.20, 300, 2);
        grant.scope = scope;
        PeerRow {
            node: borrower,
            label: "borrower-mac".to_string(),
            endpoints: Vec::new(),
            added_at: teamclaude_rs::now_ms(),
            rendezvous_secret: None,
            sees_us_at: None,
            allow: Allow {
                inspect: true,
                control: ControlGrants::default(),
                ..Allow::default()
            },
            lend: vec![grant],
        }
    }

    fn borrower_row(lender: PeerId, addr: std::net::SocketAddr) -> PeerRow {
        PeerRow {
            node: lender,
            label: "lender-mac".to_string(),
            endpoints: vec![Endpoint::direct(addr, 0, EndpointSource::Paired)],
            added_at: teamclaude_rs::now_ms(),
            rendezvous_secret: None,
            sees_us_at: None,
            allow: Allow {
                allow_disclose: true,
                control: ControlGrants::default(),
                ..Allow::default()
            },
            lend: Vec::new(),
        }
    }

    fn write_peers(path: &std::path::Path, peers: Vec<PeerRow>) {
        teamclaude_rs::peer::config::save(
            path,
            &PeerFile {
                peers,
                ..PeerFile::default()
            },
        )
        .expect("the peers file writes");
    }

    /// A manager with TWO fake accounts, one of them in group `work`, pointed
    /// at `upstream`.
    ///
    /// The out-of-scope account is FIRST, so an unrestricted pick lands on it:
    /// a fixture whose in-scope account is also the one the picker would have
    /// chosen anyway proves nothing about the restriction. No real email, no
    /// org uuid.
    fn two_group_manager(upstream: &str) -> Arc<teamclaude_rs::manager::Manager> {
        let config: teamclaude_rs::config::Config = serde_json::from_str(&format!(
            r#"{{
                "proxy": {{ "port": 0 }},
                "upstream": "{upstream}",
                "quotaProbeSeconds": 0,
                "warmupSeconds": 0,
                "accounts": [
                    {{
                        "name": "spare-fake",
                        "accessToken": "at-fake-spare",
                        "accountUuid": "33333333-3333-3333-3333-333333333333",
                        "orgUuid": "44444444-4444-4444-4444-444444444444"
                    }},
                    {{
                        "name": "work-fake",
                        "accessToken": "at-fake-work",
                        "groups": ["work"],
                        "accountUuid": "11111111-1111-1111-1111-111111111111",
                        "orgUuid": "22222222-2222-2222-2222-222222222222"
                    }}
                ]
            }}"#
        ))
        .expect("the inline two-account config parses");
        teamclaude_rs::manager::Manager::with_live_refresher(config, None)
    }

    /// Borrow once against a lease scoped to group `work`, and return every
    /// credential the upstream saw.
    pub async fn serve_one_request_scoped_to_work() -> (Served, Vec<String>) {
        let seen: Credentials = Arc::new(std::sync::Mutex::new(Vec::new()));
        let upstream = spawn_upstream_recording(Arc::clone(&seen)).await;
        let manager = two_group_manager(&upstream);
        let lender_proxy = spawn_proxy(Arc::clone(&manager)).await;
        let served = borrow_once(
            lender_proxy,
            manager,
            tcr_peer_wire::LendScope::Group("work".to_string()),
            SESSION_BODY,
        )
        .await;
        let credentials = seen
            .lock()
            .expect("the credential log is never poisoned")
            .clone();
        (served, credentials)
    }

    /// Borrow once against a lease scoped to `account:work-fake`.
    pub async fn serve_one_request_scoped_to_one_account() -> (Served, Vec<String>) {
        let seen: Credentials = Arc::new(std::sync::Mutex::new(Vec::new()));
        let upstream = spawn_upstream_recording(Arc::clone(&seen)).await;
        let manager = two_group_manager(&upstream);
        let lender_proxy = spawn_proxy(Arc::clone(&manager)).await;
        let served = borrow_once(
            lender_proxy,
            manager,
            tcr_peer_wire::LendScope::Accounts(vec!["work-fake".to_string()]),
            SESSION_BODY,
        )
        .await;
        let credentials = seen
            .lock()
            .expect("the credential log is never poisoned")
            .clone();
        (served, credentials)
    }

    /// The request body both legs of the pinning harness send.
    ///
    /// `metadata.user_id` is tier 2 of `proxy::stable_session_key`, so two
    /// requests carrying this body derive the SAME affinity key however they
    /// arrived: which is what makes the local request below a pin the
    /// borrowed request then has to get past. Obviously fake, because this
    /// repository is public.
    const SESSION_BODY: &[u8] = br#"{"metadata":{"user_id":"fake-session-id"}}"#;

    /// Pin a session onto the OUT-OF-SCOPE account with one ordinary local
    /// request, then borrow once against a lease scoped to group `work`.
    ///
    /// Returns what the borrower got, the credential the PRE-PINNING request
    /// was served on (the instrument: no pin, no test) and every credential
    /// the upstream saw after it.
    pub async fn serve_one_request_scoped_to_work_after_a_pin() -> (Served, String, Vec<String>) {
        let seen: Credentials = Arc::new(std::sync::Mutex::new(Vec::new()));
        let upstream = spawn_upstream_recording(Arc::clone(&seen)).await;
        let manager = two_group_manager(&upstream);
        let lender_proxy = spawn_proxy(Arc::clone(&manager)).await;

        // The lender's OWN client, before any borrowing: an unrestricted pick,
        // which lands on the first account (`spare-fake`) and pins this session
        // to it. `no_proxy` because this is a loopback call to the lender's own
        // proxy and an operator's `HTTPS_PROXY` must not redirect it.
        let local = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("the local client builds");
        let pinned = local
            .post(format!("{lender_proxy}/v1/messages"))
            .header("content-type", "application/json")
            .body(SESSION_BODY)
            .send()
            .await
            .expect("the lender's own proxy answered its own client");
        assert_eq!(
            pinned.status().as_u16(),
            200,
            "the pre-pinning local request has to succeed for a pin to exist"
        );
        let pinned_on = seen
            .lock()
            .expect("the credential log is never poisoned")
            .last()
            .cloned()
            .expect("the pre-pinning request reached the upstream");
        // Only what comes AFTER the pin is the claim; the pin itself is the
        // instrument and is returned separately.
        seen.lock()
            .expect("the credential log is never poisoned")
            .clear();

        let served = borrow_once(
            lender_proxy,
            manager,
            tcr_peer_wire::LendScope::Group("work".to_string()),
            SESSION_BODY,
        )
        .await;
        let credentials = seen
            .lock()
            .expect("the credential log is never poisoned")
            .clone();
        (served, pinned_on, credentials)
    }

    /// Stand up a lender on the real accept loop, borrow once, and return what
    /// the borrower's client got.
    pub async fn serve_one_relayed_request() -> Served {
        let upstream = spawn_upstream().await;
        let manager = lending_manager(&upstream);
        let lender_proxy = spawn_proxy(Arc::clone(&manager)).await;
        borrow_once(lender_proxy, manager, tcr_peer_wire::LendScope::All, b"{}").await
    }

    /// The borrowing half, shared by both harnesses above: a real listener, a
    /// real `open_serve`, one relayed request.
    ///
    /// `utilization` is the lender's own `Manager` rather than
    /// `NoFleetUtilization`, because the picker restriction is
    /// answered off it (`Manager::scope_restriction`): a harness that handed
    /// in the no-fleet reader would be testing the fail-closed default instead
    /// of the production answer.
    ///
    /// `body` is the borrowed request's own body, because one harness now has
    /// to be able to send the SAME bytes an earlier local request sent: the
    /// affinity key is derived from the body (`proxy::stable_session_key`), so
    /// that is what makes two requests one session.
    async fn borrow_once(
        lender_proxy: String,
        manager: Arc<teamclaude_rs::manager::Manager>,
        scope: tcr_peer_wire::LendScope,
        body: &'static [u8],
    ) -> Served {
        let lender_home = tempfile::tempdir().expect("the lender's temp home");
        let borrower_home = tempfile::tempdir().expect("the borrower's temp home");
        let lender_peers = lender_home.path().join("tcr-peers.json");
        let borrower_peers = borrower_home.path().join("tcr-peers.json");
        let lender_key = NodeKey::load_or_mint(lender_home.path()).expect("the lender's key");
        let borrower_id = NodeKey::load_or_mint(borrower_home.path())
            .expect("the borrower's key")
            .id();

        write_peers(
            &lender_peers,
            vec![lender_row_scoped(borrower_id, scope.clone())],
        );

        let now = teamclaude_rs::now_ms();
        let lease = Lease {
            lease_id: LEASE,
            window: Window::SevenDay,
            unit: LeaseUnit::Fraction(0.20),
            granted_at_ms: now,
            expires_at_ms: now + 300_000,
            spent: 0.0,
            max_inflight: 2,
            until: None,
        };
        let ledger = Arc::new(std::sync::Mutex::new(Ledger::new()));
        {
            let mut held = ledger.lock().expect("ledger lock");
            // `record_scoped`, because the scope is what the serving leg reads
            // to restrict the picker. A `record` here would leave every lease
            // reading `All` and the restriction untested.
            held.record_scoped(lease, borrower_id, scope);
            held.note_owner_headroom(Window::SevenDay, 0.30);
        }

        let listening = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the lender's peer listener");
        let peer_addr = listening.local_addr().expect("lender peer addr");
        let store = PeerStore::open(&lender_peers).expect("the lender's peers file");
        let context = SessionContext::new(
            &lender_key,
            store.path(),
            &serve::peer_state_path(store.path()),
        )
        .with_lease_serving(Some(LeaseServing {
            ledger,
            upstream: lender_proxy,
            utilization: manager.clone(),
            manager,
        }));
        tokio::spawn(async move {
            let _ = listener::serve_on_with(listening, context).await;
        });

        write_peers(
            &borrower_peers,
            vec![borrower_row(lender_key.id(), peer_addr)],
        );
        let borrower_store = PeerStore::open(&borrower_peers).expect("the borrower's peers file");

        let ask = Ask {
            path: "/v1/messages",
            query: None,
            method: "POST",
            model: None,
            group: None,
            affinity: None,
            tried_local: 0,
            body: bytes::Bytes::from_static(body),
            headers: HeaderMap::new(),
        };
        let response = serve::open_serve(
            &lender_key.id(),
            &lease,
            &ask,
            &HeaderMap::new(),
            &borrower_store,
        )
        .await
        .expect("the SERVE stream ran")
        .served()
        .expect("the lender served it");
        Served {
            status: response.status().as_u16(),
            peer: borrower_id.display(),
        }
    }
}

// ---------------------------------------------------------------------------
// The serving process does what a serving process has to do
// ---------------------------------------------------------------------------

/// **A booted server with `internet` on reaches the mapping decision**, in the
/// process that holds the listener.
///
/// `boot_peer_listener` calls `bind` + `serve_on_with`, never
/// `listener::serve`, and `listener::serve` was the only function that read
/// `peer.internet` and started a keeper. So the shipped proxy asked its router
/// for nothing on any setting: nobody off the LAN could dial this Mac, and
/// `reach::external_socket()` was `None` on every Mac, which is the input
/// `tunnel::reverse_carry_is_wanted` reads, so every Mac also parked a reverse
/// carrier at every friend it had.
///
/// The DECISION is what is asserted, not a mapping: this test binds loopback
/// (every test here does), and a keeper that ran would talk to whatever router
/// the machine running the suite sits behind. `reach::mapping_boot` is
/// asserted on all three inputs in `tests/peer_reach.rs`.
///
/// Watch it fail: delete the `start_peer_mapping` call from
/// `boot_peer_listener` and the count never moves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_booted_server_with_internet_on_starts_the_mapping_keeper() {
    use teamclaude_rs::peer::reach;

    let before = reach::mapping_boot_counts();

    let profile = Profile::new();
    profile.write_peers(peer_config::PeerFile {
        internet: true,
        ..listening_peers_file()
    });

    let ServeOutcome::Started(mut handle) = serve(options(&profile))
        .await
        .expect("the library boots the proxy")
    else {
        panic!("this test may not run against an incumbent proxy");
    };
    handle
        .peer_addr()
        .expect("a peers file with `listen` set boots the peer listener");

    let after = reach::mapping_boot_counts();
    assert!(
        after.loopback_only > before.loopback_only,
        "a serving process with `internet` on has to reach the mapping decision; it took \
         none ({before:?} -> {after:?})"
    );
    assert_eq!(
        after.wanted, before.wanted,
        "and it must not have asked the machine's real router for anything: this listener is \
         bound to loopback ({before:?} -> {after:?})"
    );

    handle.shutdown().await;
}

/// **A booted server with `find` on announces from its own process.**
///
/// `tcr peer find on` used to register the beacon in the CLI process and exit,
/// which took the mdns-sd daemon with it: `find off` in a new process found
/// the shared daemon empty, and no serving process ever announced anything. A
/// beacon also carries the announcing process's per-boot instance id, and the
/// knock a neighbour sends after seeing a beacon names the id it saw, so a
/// beacon announced by the CLI named a process that had already exited and
/// could never match the server's own
/// (`the_listener_and_the_beacon_name_one_boot_instance_id`, above, is the
/// pure half of that).
///
/// Watch it fail: delete the `spawn_beacon` call from `boot_peer_listener` and
/// the count never moves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_booted_server_with_find_on_announces_from_its_own_process() {
    use teamclaude_rs::peer::discovery;

    let before = discovery::announcements();

    let profile = Profile::new();
    profile.write_peers(peer_config::PeerFile {
        discovery: true,
        ..listening_peers_file()
    });

    let ServeOutcome::Started(mut handle) = serve(options(&profile))
        .await
        .expect("the library boots the proxy")
    else {
        panic!("this test may not run against an incumbent proxy");
    };
    handle
        .peer_addr()
        .expect("a peers file with `listen` set boots the peer listener");

    // The announcer's first wake is immediate; the daemon it starts runs on
    // its own thread, so this waits for the registration rather than assuming
    // it landed inside the boot call.
    let mut announced = false;
    for _ in 0..50 {
        if discovery::announcements() > before {
            announced = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        announced,
        "a serving process with `find` on has to be the one announcing: it registered no \
         beacon (announcements stayed at {before})"
    );
    // What it is HOLDING is not asserted here: `discovery`'s registers are
    // process-global (one announcer per Mac in production), and every other
    // server this binary boots calls `stop_all` on its way out, which clears
    // them. The count is monotonic and is the claim: a beacon was registered
    // by this process.

    handle.shutdown().await;
}
