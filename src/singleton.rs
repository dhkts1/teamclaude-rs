//! Port-takeover singleton: only one proxy may own the configured port.
//!
//! The recurring failure is two proxies — a leftover JS `teamclaude` and/or a
//! `tcr` — both refreshing the same accounts. Each holds its own in-memory copy of
//! the SINGLE-USE OAuth refresh tokens, so the first refresh by either revokes the
//! other's copy and accounts flap to `error` (the "token war"). On startup we
//! resolve the configured port down to exactly ONE proxy.
//!
//! **Resolving it does not have to mean killing the incumbent, and by default it
//! does not.** Both outcomes — replace, or attach-and-exit — leave one proxy
//! running, so the token war is averted either way; the difference is only WHICH
//! process survives. Killing the incumbent is by far the more expensive of the
//! two: it wipes the in-memory session→account pin map, so every live session
//! pays a full cold prompt prefix. Measured over 79.6h: 50 boots, 31 of the 49
//! gaps between them under 120s — a bare `tcr` typed to "check on things" would
//! silently take the port. So the default is now [`Takeover::IncumbentPresent`],
//! which reports and exits without binding, and the kill lives behind an explicit
//! `--replace`.
//!
//! Two safety properties, both load-bearing:
//! - **Port-scoped, not address-scoped.** A candidate is any process listening on
//!   `<configured port>` on ANY address — `127.0.0.1`, `0.0.0.0`, `[::1]`, or a LAN
//!   IP all match, exactly as the old `lsof -iTCP:<port>` did — so a proxy (or
//!   anything else) on another PORT is never touched, and a test server on an
//!   ephemeral port is safe.
//! - **Command-verified.** A candidate is signalled only if its command line is an
//!   actual `teamclaude`/`tcr` *server*. A non-proxy holder is left alone rather
//!   than killed — the bind then fails loudly with EADDRINUSE if that holder
//!   shares our bind address (in practice, loopback), but not necessarily
//!   otherwise; see [`port_listeners`]'s doc for the measured gap. This is what
//!   makes the takeover safe against a reused PID or an unrelated process on
//!   the port, whether or not the bind itself ends up refusing.
//!
//! # Identity: the owner file, then the name matcher
//!
//! Recognition used to be *only* the name matcher — `argv[0]` ending in `/tcr`.
//! That reads the HOST process, not the proxy: a proxy running inside another
//! program (the menu-bar app) reports that program's path and matches nothing, so
//! [`live_proxy_server`] returns `None`, `tcr login` stops refusing to run beside
//! a live server, and the server's next `persist_tokens` writes its boot-time
//! SINGLE-USE refresh tokens back over the fresh ones. That failure is silent
//! until every account fails to refresh at the next boot.
//!
//! So a bound proxy now ADVERTISES itself: [`write_owner_file`] drops
//! `proxy-owner-<port>.json` beside the affinity pin cache, and
//! [`classify_port_owner`] is consulted BEFORE the name matcher. Identity becomes
//! the proxy's own claim instead of an inference from someone else's `argv`.
//!
//! Three properties keep that from being a new way to get it wrong:
//! - **A stale file proves nothing.** The pid it names must ALSO appear in
//!   `port_listeners(port)` (and the port must match) or the claim is ignored
//!   entirely — a crashed proxy, or anyone at all, can leave a file behind.
//! - **A claim SUPPLEMENTS the command check, it never replaces it.** A `cli`
//!   claim is believed only when the command line ALSO says its pid is a proxy,
//!   so "command-verified" above still holds on the claim path; a pid the OS
//!   recycled onto an unrelated listener is not signalled. See [`verified_owner`].
//! - **The name matcher is RETAINED as the fallback**, so a `tcr` that predates
//!   the owner file (including one serving right now) and a `LegacyJs` proxy —
//!   which will never write one — are recognized exactly as before. Nothing has
//!   to restart for this change to be safe.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, Signal, System, UpdateKind};

/// Which proxy a recognized incumbent is. The distinction is load-bearing for
/// [`takeover_decision`], not cosmetic: a `tcr` peer is a program that is doing
/// this program's job and whose session pins cost real money to discard, while a
/// leftover JS `teamclaude` is the thing this module was written to displace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyKind {
    /// The Rust proxy running as its own process — a peer of ours.
    Tcr,
    /// The legacy JS proxy (`node …/teamclaude server`).
    LegacyJs,
    /// The Rust proxy running **inside another program** (the menu-bar app),
    /// which said so in its owner file ([`ProxyHost::Embedded`]).
    ///
    /// Never signalled, not even under `--replace`
    /// ([`incumbents_to_signal`], [`takeover_decision`]). The pid on the port is
    /// the HOST's, and `takeover_port` would SIGTERM it: AppKit installs no
    /// SIGTERM handler, so the app dies without running
    /// `applicationWillTerminate` — no graceful shutdown, no final session pin
    /// write, and the operator's windows go with it. A CLI flag must not be able
    /// to do that.
    ///
    /// This is not a new policy. `classify_proxy_server` already returns `None`
    /// for `tcr run` (below), which is exactly why a proxy hosted inside a
    /// `tcr run` process is never replaced either. Same shape, stronger reason.
    /// Stopping an embedded proxy is the host application's job.
    TcrEmbedded,
}

/// Which kind of process a proxy is running inside, as the proxy's **own claim**
/// in its owner file — never sniffed from `argv[0]`.
///
/// [`crate::server::ServeOptions::host`] carries it, and each caller states it:
/// the binary passes [`Self::Cli`], an in-process embedder passes
/// [`Self::Embedded`]. The library does not guess, because guessing is the bug
/// this whole mechanism replaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyHost {
    /// A standalone `tcr` process.
    Cli,
    /// A proxy served from inside a host application.
    Embedded,
    /// A host this build does not know about — a claim written by a NEWER `tcr`.
    ///
    /// The file is a cross-process, cross-BUILD contract: the `tcr` that reads it
    /// may be older than the one that wrote it. Without this catch-all an unknown
    /// `host` value fails the whole `ProxyOwner` parse, so a live proxy reads as
    /// NO CLAIM AT ALL — the name matcher then sees the host program's `argv[0]`,
    /// recognizes nothing, `tcr login` runs beside the live server and its fresh
    /// single-use refresh tokens are overwritten, and `tcr server --replace`
    /// SIGTERMs the process the claim existed to protect. Degrading to "a proxy is
    /// there, do not signal it" is strictly safer than degrading to "nothing is
    /// there", so this maps to [`ProxyKind::TcrEmbedded`].
    ///
    /// Only ever produced by DEserialization. Nothing in this crate writes it.
    #[serde(other)]
    Unknown,
}

impl ProxyHost {
    /// The incumbent kind a *verified* claim from this host means.
    fn kind(self) -> ProxyKind {
        match self {
            ProxyHost::Cli => ProxyKind::Tcr,
            // An unrecognized host is treated as embedded: of the two, it is the
            // kind that is never signalled. See [`ProxyHost::Unknown`].
            ProxyHost::Embedded | ProxyHost::Unknown => ProxyKind::TcrEmbedded,
        }
    }
}

/// What a bound proxy writes to claim the port: `proxy-owner-<port>.json`.
///
/// Every field is load-bearing to a reader:
/// - `pid` is what makes a stale file harmless — it must also be listening on
///   `port` ([`verified_owner`]).
/// - `port` guards against a file being read for the wrong port (a copied file,
///   a caller passing a path that does not match its own port).
/// - `sha` is which build is serving, so an operator reading the file gets the
///   same fact the `server started` log line carries.
/// - `host` is the whole point: it says whether SIGTERM is survivable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyOwner {
    /// The pid holding the listener — the HOST process's pid when embedded.
    pub pid: u32,
    /// The port that pid bound.
    pub port: u16,
    /// The build the proxy is executing (`build_info::SHA`).
    pub sha: String,
    /// What is hosting the proxy.
    pub host: ProxyHost,
}

/// `proxy-owner-<port>.json` inside `dir` — the file name is port-scoped so two
/// proxies on two ports never overwrite each other's claim.
pub fn owner_path_in(dir: &Path, port: u16) -> PathBuf {
    dir.join(format!("proxy-owner-{port}.json"))
}

/// The directory the binary keeps the claim in: beside the session-affinity pin
/// cache, so the proxy's two pieces of cross-boot state live in one place.
pub fn default_owner_dir() -> PathBuf {
    let cache = crate::affinity::default_path();
    cache
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

/// Where the binary keeps the claim for `port`. Every reader resolves the claim
/// through this function, which is why a writer must never be handed a free-form
/// path: see [`crate::server::ServeOptions::owner_dir`].
pub fn default_owner_path(port: u16) -> PathBuf {
    owner_path_in(&default_owner_dir(), port)
}

/// Write the claim atomically (0600, temp + rename — the same
/// [`crate::config::write_atomic`] every other state file uses, so a reader can
/// never see a half-written claim).
pub fn write_owner_file(path: &Path, owner: &ProxyOwner) -> Result<(), crate::config::ConfigError> {
    crate::config::write_atomic(path, &serde_json::to_string_pretty(owner)?)
}

/// Withdraw the claim at `path` — but ONLY if it still names `pid` on `port`.
///
/// The check is the whole function. `proxy-owner-<port>.json` is shared state
/// named after the PORT, not after the process, so a shutting-down proxy and its
/// successor address the same file. A shutdown frees the listener first and then
/// keeps joining background tasks (the affinity flusher does a blocking write, so
/// hundreds of milliseconds); in that window a successor can bind the port and
/// write its own claim. An unconditional unlink here would delete the SUCCESSOR's
/// live claim, and for an embedded successor — whose command line identifies
/// nothing — that means `live_proxy_server` returns `None`, `tcr login` stops
/// refusing, and the boot-time single-use refresh tokens clobber the fresh ones.
/// That is the exact failure the claim file exists to prevent, reintroduced by
/// its own cleanup path.
///
/// Best-effort past the check: a claim left behind by a crash cannot produce a
/// false positive ([`verified_owner`] re-checks the pid against the live
/// listeners and the command line), so failing to remove one costs nothing.
///
/// A read-then-unlink still has a microsecond-wide TOCTOU window, which is not
/// closable portably. It replaces a hundreds-of-milliseconds window with one
/// bounded by two syscalls; the alternative — not checking — loses the file every
/// time the race is entered at all.
pub fn remove_owner_file_if_owned(path: &Path, pid: u32, port: u16) {
    match read_owner_file(path) {
        Some(owner) if owner.pid == pid && owner.port == port => {}
        Some(owner) => {
            tracing::info!(
                path = %path.display(),
                claim_pid = owner.pid,
                claim_port = owner.port,
                our_pid = pid,
                our_port = port,
                "not withdrawing the port claim: it names another proxy, so a successor \
                 already took the port over"
            );
            return;
        }
        None => {
            // Absent (already withdrawn) or unparseable — either way it is not
            // this process's claim to delete, and a successor's write will
            // replace it.
            return;
        }
    }
    if let Err(err) = std::fs::remove_file(path) {
        if err.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "could not remove the proxy owner file; a stale claim is harmless (its pid is re-checked against the live listeners)"
            );
        }
    }
}

/// Read a claim, or `None` for absent/unreadable/unparseable. A malformed file is
/// simply not a claim — the name matcher still gets its turn.
///
/// Every failure that is not "the file is absent" is LOGGED, the read errors as
/// well as the parse errors. The claim is written 0600, so a proxy started under
/// another uid (launchd, `sudo`) leaves one a client cannot read: the read fails
/// EACCES, identity silently falls back to the matcher that by construction
/// cannot see an embedded proxy, and `tcr login` proceeds beside a live server
/// whose next persist overwrites the fresh single-use refresh tokens. An
/// unreadable claim and an absent one lead to the same fallback but they are not
/// the same fact, and the operator needs the difference on the record.
fn read_owner_file(path: &Path) -> Option<ProxyOwner> {
    let data = match std::fs::read_to_string(path) {
        Ok(data) => data,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "the proxy owner file exists but could not be READ (permissions? it is written \
                 0600, so a proxy running under another uid leaves one a client cannot open); \
                 falling back to command-line recognition, which does NOT recognize an embedded \
                 proxy"
            );
            return None;
        }
    };
    match serde_json::from_str::<ProxyOwner>(&data) {
        Ok(owner) => Some(owner),
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "proxy owner file is unreadable; falling back to command-line recognition"
            );
            None
        }
    }
}

/// The claim's VERIFICATION, as a pure function: a claim counts only when it is
/// for this `port`, its pid is one of the processes actually listening on it,
/// AND — for a claim that says it is CLI-hosted — that pid's command line is
/// still a proxy server.
///
/// This is the check that makes the owner file safe to trust. Without it the file
/// is a lie anyone can plant: any process could write `{"pid":<live proxy>,…}`
/// and change what `tcr` does about the port, and every crashed proxy would leave
/// behind a claim that outlives it and protects a pid the OS has since recycled
/// onto something unrelated.
///
/// # A claim SUPPLEMENTS the command check; it never replaces it
///
/// The pid check alone is not enough, and the difference is a killed process. A
/// crashed `tcr` leaves `{"pid":5150,"host":"cli"}` behind; months later pid 5150
/// is someone's dev server listening on the same port. `pid ∈ holders` holds, so
/// a claim-trusting reader calls it a `tcr` peer and `tcr --replace` SIGTERMs it,
/// then SIGKILLs it 800ms later. The module's "command-verified" property (see the
/// module doc) has to hold on the claim path too, so a `cli` claim is believed
/// only when [`classify_proxy_server`] ALSO recognizes the pid. The claim's job is
/// to say WHICH proxy it is and that it is ours; only the command line can say
/// that the pid is still a proxy at all.
///
/// # The one asymmetry, and why it is safe
///
/// An `embedded` claim cannot be command-verified — by construction: the pid is
/// the HOST application's and its `argv[0]` matches nothing, which is the entire
/// reason this file exists. So an embedded claim is believed on the pid check
/// alone, and that is deliberate: a false positive there can only ever make this
/// process STAND DOWN (an embedded incumbent is never signalled, on any path —
/// [`incumbents_to_signal`]), while a false positive on a CLI claim is a kill. The
/// unverifiable case is the one whose worst outcome is refusing to act.
fn verified_owner(
    owner: Option<&ProxyOwner>,
    port: u16,
    holders: &[u32],
    command: impl Fn(u32) -> Vec<String>,
) -> Option<Incumbent> {
    let owner = owner?;
    if owner.port != port || !holders.contains(&owner.pid) {
        return None;
    }
    let kind = owner.host.kind();
    if owner.host == ProxyHost::Cli && classify_proxy_server(&command(owner.pid)).is_none() {
        tracing::warn!(
            pid = owner.pid,
            port,
            "ignoring a cli-hosted port claim whose pid is not running a proxy: the claim is \
             stale and the OS has recycled that pid onto something else"
        );
        return None;
    }
    Some(Incumbent {
        pid: owner.pid,
        kind,
    })
}

/// The owner file at `owner_path`, verified against the processes actually
/// holding `port` and against the claimed pid's command line. `None` when there is
/// no claim, the claim is for another port, its pid is not listening, or a
/// CLI-hosted claim names a pid that is not a proxy — in every one of those cases
/// the caller falls back to the name matcher.
///
/// `holders`, `owner_path` and `command` are parameters rather than read in here
/// so this is testable against a real file without a real proxy.
pub fn classify_port_owner(
    port: u16,
    holders: &[u32],
    owner_path: &Path,
    command: impl Fn(u32) -> Vec<String>,
) -> Option<Incumbent> {
    verified_owner(read_owner_file(owner_path).as_ref(), port, holders, command)
}

/// A recognized, replaceable proxy holding the port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Incumbent {
    pub pid: u32,
    pub kind: ProxyKind,
}

/// Does this argv look like a teamclaude/tcr *server* — a process safe to
/// replace as a proxy incumbent on the port? Recognizes the JS proxy
/// (`node …/teamclaude server`) and the Rust proxy (`…/tcr` at argv0, with the
/// `server` subcommand, no subcommand, or a flag — since bare `tcr` defaults to the
/// server). Never matches a bare `grep`/editor/`tcr run`/`teamclaude run`/etc.
///
/// Takes pre-split argv (`&[String]`, as [`sysinfo::Process::cmd`] returns it —
/// kernel-tokenized from `/proc/<pid>/cmdline` on Linux, `libproc` argv on macOS),
/// never a joined command LINE. A joined line has to be re-split by whitespace to
/// be examined, and that re-split mis-tokenizes any executable path containing a
/// space: `"/Applications/My App/tcr server"` used to split into
/// `["/Applications/My", "App/tcr", "server"]`, so `tokens[0]` never ended with
/// `/tcr` and a REAL tcr server went unrecognized. Taking the kernel's own argv
/// removes that defect class instead of patching the symptom — there is
/// deliberately no `&str`/whitespace-splitting overload here, because a
/// convenience wrapper that re-introduces the split is the same bug behind a
/// fixed test case.
pub fn is_proxy_server(argv: &[String]) -> bool {
    classify_proxy_server(argv).is_some()
}

/// [`is_proxy_server`] plus WHICH proxy it is. Same recognition rules — the bool
/// version delegates here, so the two can never drift apart.
pub fn classify_proxy_server(argv: &[String]) -> Option<ProxyKind> {
    if argv.is_empty() {
        return None;
    }
    let is_node = |t: &str| t == "node" || t.ends_with("/node");
    let is_teamclaude = |t: &str| t == "teamclaude" || t.ends_with("/teamclaude");
    let is_tcr = |t: &str| t == "tcr" || t.ends_with("/tcr");

    // JS proxy: the teamclaude bin must be the executed program (argv0, or the arg
    // right after `node`) AND the subcommand must be `server`.
    if let Some(i) = argv.iter().position(|t| is_teamclaude(t)) {
        let is_program = i == 0 || is_node(&argv[i - 1]);
        if is_program && argv.get(i + 1).map(String::as_str) == Some("server") {
            return Some(ProxyKind::LegacyJs);
        }
    }

    // Rust proxy: `tcr` at argv0. A server unless the subcommand is a recognized
    // NON-server verb; bare `tcr`, `tcr server`, and `tcr --flag` are all servers.
    if is_tcr(&argv[0]) {
        return match argv.get(1).map(String::as_str) {
            None => Some(ProxyKind::Tcr),
            Some(t) if t.starts_with('-') || t == "server" => Some(ProxyKind::Tcr),
            Some(_) => None, // `tcr run`, `tcr status`, `tcr accounts`, …
        };
    }

    None
}

/// The argv of `pid` (via [`sysinfo`], no subprocess); empty if the process is
/// gone or its cmdline could not be read.
///
/// A fresh [`System`] is built per call rather than kept and re-refreshed,
/// deliberately: `pid` is reused by the OS, and a stale snapshot could hand back
/// a RECYCLED process's cmdline. `ProcessRefreshKind::nothing().with_cmd(Always)`
/// is required — the crate's own `refresh_processes` convenience method does not
/// populate `cmd` at all (its default `UpdateKind` for `cmd` is `Never`), so a
/// caller reaching for that shortcut here would get an empty argv on every call
/// and every proxy would silently stop being recognized.
fn process_command(pid: u32) -> Vec<String> {
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
        false,
        ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always),
    );
    sys.process(Pid::from_u32(pid))
        .map(|p| {
            p.cmd()
                .iter()
                .map(|s| s.to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// PIDs listening on `:port` on any address (via [`listeners`], no subprocess),
/// each pid appearing at most once. Empty on any failure — an erroring
/// [`listeners::get_all`] degrades to no signal, and to binding anyway; it is
/// not "no takeover" either: [`takeover_decision`]`(&[], _)` is
/// `Takeover::Proceed`. That is safe for two of the three callers and not for
/// the third, and only PARTIALLY safe even there. [`takeover_port`] and
/// `server.rs`'s boot path both then actually BIND, and `EADDRINUSE` is a
/// property of the `(address, port)` PAIR, not the port — and the platforms
/// this crate ships to disagree on what that licenses (measured via
/// `std::net::TcpListener::bind`, the same constructor as `server.rs:784`;
/// see `scripts/FINDINGS-bind-overlap.md`): on Linux the backstop fires for
/// every pairing measured, but on macOS it fires only against a SAME-address
/// (loopback) incumbent — in practice another `tcr`, which hard-codes
/// `127.0.0.1` — and coexists silently with a WILDCARD-bound one (`0.0.0.0`
/// or `::`), which this module's own doc (`:22`) already lists as a
/// candidate. Both platforms set `SO_REUSEADDR` (Rust std does so
/// unconditionally on Unix); they disagree on whether it licenses overlap
/// between a live wildcard and a live specific-address listener. On macOS,
/// against a wildcard incumbent, an enumeration failure means we could end up
/// bound ALONGSIDE it, silently, risking the two proxies token-war — the
/// platform where this matters most, since `TcrBar.app` and the live proxy
/// run there. This gap is inherited, not introduced by the dedup or logging
/// above: neither the old `lsof` scan nor the new one filters by address, so
/// a working scan already sees and handles a wildcard holder — only the
/// failure-collapse path lets one slip past unbound-against. Empty on
/// enumeration failure is unchanged by any of this — "never a false kill" is
/// still true and load-bearing throughout — but "no takeover" is not a
/// consequence of it, and where a bind fails to actually refuse depends on
/// the incumbent's address AND the platform, not merely on the port being
/// held. Whether the legacy JS proxy actually binds wildcard is a separate,
/// unmeasured claim about node's defaults. `oauth::login`'s
/// guard binds nothing at all, so no backstop of any strength applies to it:
/// on an enumeration failure it proceeds beside a live server it failed to
/// detect, and the server's next `persist_tokens` writes its boot-time tokens
/// back over the ones the login just wrote, clobbering the single-use refresh
/// tokens (observed live 2026-07-19, see `oauth.rs:788-793`).
///
/// Empty-on-failure is parity with the old `lsof`-based implementation only on
/// the `Err` path — `lsof` exiting 1 on a real failure and on an ordinary free
/// port were indistinguishable, so the old code degraded to empty either way,
/// exactly as the `Err` arm below does, and it is now LOGGED rather than
/// silently discarded (see the `Err` arm). But `get_all()` has a second failure
/// shape with no `lsof` analogue at all, because it never reaches this
/// function's `Err` arm: on macOS, a listener whose process name cannot be read
/// or is not valid UTF-8 is silently DROPPED from an otherwise-`Ok` result
/// (`listeners-0.6.1/src/platform/macos/mod.rs:35`, gated on
/// `proc_names_cache.get(pid)` returning `None`; the read itself can fail two
/// ways in `proc_name.rs:27-29` and `:35-37`). `lsof -t` never looks at a
/// process name, so it has no equivalent gap. This is upstream's to fix, not
/// ours to work around here — documented so a future "why did this holder go
/// unseen" investigation does not restart from zero. The axis where the new
/// code is strictly better: the old path failed OPEN wherever `lsof` itself was
/// absent, which is routine on Linux and in containers — that external-binary
/// dependency is gone entirely.
///
/// Deliberately `listeners::get_all()` filtered to [`listeners::SocketState::Listen`]
/// ourselves, NEVER [`listeners::get_process_by_port`]. That shortcut applies no
/// socket-state filter at all (upstream `listeners` issue #36, open,
/// maintainer-confirmed) and returns the FIRST socket matching the port —
/// `sshd` LISTENing on 22 and `sshd-session` ESTABLISHED on the same local port
/// both key to port 22, and the shortcut can hand back the child. Our old
/// `lsof -sTCP:LISTEN` excluded non-listening sockets specifically so that a
/// pid merely ESTABLISHED as a *client* of our port could never reach
/// `replaceable_incumbents` and, under `--replace`, the SIGKILL loop. The
/// explicit state filter below is what reproduces that guarantee. It also sides
/// around `get_process_by_port`'s separate `port == 0` error, which we would
/// otherwise have to special-case for the ephemeral-port test harness.
///
/// The protocol filter is the same kind of parity restoration, made explicit
/// rather than left to rest on an upstream implementation detail: the old
/// `lsof -iTCP:{port}` excluded UDP outright. Today, both `listeners` backends
/// hard-code every UDP entry's state to `SocketState::Unknown` (never `Listen`)
/// (`platform/linux/proto_listener.rs:128,165`, `platform/macos/c_socket_fd_info.rs:32`),
/// so the state filter above already excludes UDP as a side effect — but
/// `SocketState` is a public enum upstream is still evolving (see issue #36),
/// and a future release giving UDP a real state would silently start admitting
/// UDP holders here with no line of code to blame. Filtering on
/// `Protocol::TCP` directly keeps that guarantee independent of an
/// undocumented invariant in a dependency.
///
/// **Dedup, order-preserving.** One process can hold several LISTEN sockets on
/// the same port at once — the common case is a dual-stack bind,
/// `127.0.0.1:<port>` plus `[::1]:<port>` from one `bind(0.0.0.0)`/`bind(::)`
/// call, which is how a Node HTTP server (including the legacy JS proxy this
/// module exists to migrate off of) listens by default. [`listeners::Listener`]
/// derives `Hash`/`Eq` over `{process, socket, protocol, state}`, and `socket` is
/// part of that key, so `get_all()`'s internal `HashSet` keeps both sockets as
/// separate entries — the same pid then appears twice in this function's output.
/// The old `lsof -t` collapsed that to one line per process; below reproduces
/// that collapse explicitly, in FIRST-SEEN order, because callers
/// ([`live_proxy_server`] via `.next()`, [`takeover_decision`] via `.find()`)
/// read the first element as *the* incumbent to report, and reordering holders
/// would change which one that is.
///
/// Opposite divergence on Linux, needing no fix here: that backend's
/// inode→process map uses last-writer-wins `HashMap::insert`
/// (`platform/linux/helpers.rs:53-54`), so a listening fd shared across a fork
/// family collapses to one pid rather than macOS's several — untested by CI
/// despite running on `ubuntu-latest`, since nothing here forks a listener.
fn port_listeners(port: u16) -> Vec<u32> {
    let mut seen = HashSet::new();
    let all = match listeners::get_all() {
        Ok(all) => all,
        Err(error) => {
            // No silent fallbacks: an enumeration failure still degrades to "no
            // holders" (empty-on-failure is load-bearing, see the doc above),
            // but that degradation must be visible, not invisible — this is the
            // one signal `lsof`'s `Ok`/exit-1 conflation could never produce.
            tracing::warn!(port, %error, "listeners::get_all failed; treating the port as unheld");
            Default::default()
        }
    };
    all.into_iter()
        .filter(|listener| listener.protocol == listeners::Protocol::TCP)
        .filter(|listener| listener.state == listeners::SocketState::Listen)
        .filter(|listener| listener.socket.port() == port)
        .map(|listener| listener.process.pid)
        .filter(|&pid| seen.insert(pid))
        .collect()
}

/// The pure takeover DECISION: of the processes holding the port, which are
/// replaceable proxy incumbents, and which proxy is each? Excludes self and any
/// non-proxy holder. Split out from the side-effecting kill so it is unit-testable
/// with injected inputs.
///
/// Name matching only — [`replaceable_incumbents_with_owner`] is the version that
/// takes a verified owner-file claim into account.
pub fn replaceable_incumbents(
    holders: &[u32],
    self_pid: u32,
    command: impl Fn(u32) -> Vec<String>,
) -> Vec<Incumbent> {
    replaceable_incumbents_with_owner(holders, self_pid, command, None)
}

/// [`replaceable_incumbents`] with the port's **verified** owner-file claim
/// (`classify_port_owner`) consulted FIRST.
///
/// Order matters: the claim is the proxy's own statement about what is hosting it,
/// while the command line is an inference from the host process's `argv[0]` — and
/// for an embedded proxy that inference is wrong in the dangerous direction (it
/// recognizes nothing at all). Where both speak, the claim wins.
///
/// `owner` must already be verified against the live listeners AND (for a `cli`
/// claim) against the pid's command line; pass the result of
/// [`classify_port_owner`], never a raw file read. That is what keeps this
/// short-circuit from being a way around the command check — by the time a claim
/// arrives here, the command check has already had its say.
pub fn replaceable_incumbents_with_owner(
    holders: &[u32],
    self_pid: u32,
    command: impl Fn(u32) -> Vec<String>,
    owner: Option<Incumbent>,
) -> Vec<Incumbent> {
    holders
        .iter()
        .copied()
        .filter(|&pid| pid != self_pid)
        .filter_map(|pid| match owner.filter(|owner| owner.pid == pid) {
            Some(claimed) => Some(claimed),
            None => classify_proxy_server(&command(pid)).map(|kind| Incumbent { pid, kind }),
        })
        .collect()
}

/// Detection-only: the PID of a live teamclaude/tcr *server* currently holding
/// `port`, if any. Reuses the exact port-scoped, command-verified decision as
/// [`takeover_port`] ([`replaceable_incumbents`]) but signals NOTHING — `tcr login` uses
/// it to REFUSE to run beside a live server (the server reads config only at boot,
/// and its next `persist_tokens` writes its boot-time TOKENS back over the file,
/// clobbering the login's fresh ones). Returns the first replaceable
/// [`Incumbent`]; `None` when the port is free or held only by a non-proxy
/// process.
///
/// The KIND travels with the pid, and it has to. Since the owner file, this can
/// return the pid of a HOST APPLICATION serving the proxy in-process, and a
/// caller that renders "kill {pid}" would be telling the operator to SIGTERM a
/// GUI that installs no handler for it — no `applicationWillTerminate`, no final
/// session→account pin write. A bare `u32` cannot carry that distinction, so it
/// is not returned as one; see `oauth::login_guard_refusal`.
pub fn live_proxy_server(port: u16) -> Option<Incumbent> {
    let holders = port_listeners(port);
    let owner = classify_port_owner(port, &holders, &default_owner_path(port), process_command);
    replaceable_incumbents_with_owner(&holders, std::process::id(), process_command, owner)
        .into_iter()
        .next()
}

/// Is `pid` still alive? Uses the same `refresh_processes_specifics` /
/// `ProcessesToUpdate::Some` shape as [`signal_pid`] (both pass
/// `ProcessRefreshKind::nothing()`, no `cmd`), minus the signal — a fresh
/// snapshot per call so a reused pid cannot read as the process we just
/// signalled. [`process_command`] is the odd one out: it refreshes the same way
/// but additionally requests `.with_cmd(UpdateKind::Always)`, since it is the
/// only one of the three that needs the command line.
fn is_alive(pid: u32) -> bool {
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
        true,
        ProcessRefreshKind::nothing(),
    );
    sys.process(Pid::from_u32(pid)).is_some()
}

/// Send `signal` to `pid` via [`sysinfo::Process::kill_with`] — never
/// [`sysinfo::Process::kill`], which always sends `SIGKILL` regardless of what
/// the caller asked for and would silently delete the graceful-shutdown window
/// between the `Signal::Term` and `Signal::Kill` calls in [`takeover_port`].
///
/// `kill_with` returns `None` when `signal` is unsupported on the current
/// platform — that case is logged, not swallowed, because a caller reading
/// "we tried to signal it" from a `None` it never checked is exactly how a
/// process that was never actually asked to stop ends up presumed dead.
fn signal_pid(pid: u32, signal: Signal) {
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
        true,
        ProcessRefreshKind::nothing(),
    );
    let Some(process) = sys.process(Pid::from_u32(pid)) else {
        // Already gone; nothing to signal.
        return;
    };
    match process.kill_with(signal) {
        Some(true) => {}
        Some(false) => {
            tracing::warn!(pid, ?signal, "failed to signal process");
        }
        None => {
            tracing::warn!(
                pid,
                ?signal,
                "this signal is not supported on the current platform; the process was NOT signalled"
            );
        }
    }
}

/// What the caller must do after [`takeover_port`] has looked at the port.
///
/// The enum exists so the decision stays HERE — with the port-scoped,
/// command-verified knowledge of who holds it — while the exit belongs to
/// `main`, which owns the process's exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Takeover {
    /// Nothing replaceable is in the way (the port is free, held only by a
    /// non-proxy, or the incumbent was just replaced). Bind.
    Proceed,
    /// A recognized proxy incumbent holds the port and was deliberately left
    /// running. The caller must NOT bind; it should exit 0.
    ///
    /// Carries the whole [`Incumbent`], not just its pid. The KIND decides what a
    /// caller may tell the operator to do about it — a `tcr` peer can be signalled
    /// and an embedded one must only be quit — and a caller handed a bare `u32`
    /// has to re-scan the incumbent list to recover a fact this function already
    /// looked up, which is exactly the kind of parallel predicate that drifts.
    IncumbentPresent(Incumbent),
}

/// The pure decision: given the replaceable incumbents on the port and whether
/// `--replace` was passed, do we bind or stand down?
///
/// Split out from the side effects for the same reason as
/// [`replaceable_incumbents`] — and it is the single gate on the kill loop below,
/// so a test that pins this function pins whether a live proxy gets signalled.
///
/// A [`ProxyKind::Tcr`] incumbent is the one that is protected by default: it is
/// doing this program's job, and replacing it wipes its session→account pin map.
/// A [`ProxyKind::LegacyJs`] incumbent is NOT protected, because displacing it is
/// the reason this module exists. Standing down for it would leave the JS proxy
/// serving forever and make `tcr` unable to complete the migration it was
/// installed for — and the two would token-war the moment tcr did bind elsewhere.
///
/// A [`ProxyKind::TcrEmbedded`] incumbent is checked BEFORE `replace`, and is the
/// one kind `--replace` cannot override: the pid on the port belongs to the host
/// application, so replacing it means SIGTERMing a GUI that installs no handler
/// for it. See [`ProxyKind::TcrEmbedded`].
///
/// That stand-down decides only whether WE bind. It says nothing about the OTHER
/// incumbents on the port — see [`incumbents_to_signal`], which partitions them by
/// kind rather than reading this one verdict as "touch nothing".
pub fn takeover_decision(replaceable: &[Incumbent], replace: bool) -> Takeover {
    if let Some(embedded) = replaceable
        .iter()
        .find(|incumbent| incumbent.kind == ProxyKind::TcrEmbedded)
    {
        return Takeover::IncumbentPresent(*embedded);
    }
    if replace {
        return Takeover::Proceed;
    }
    match replaceable
        .iter()
        .find(|incumbent| incumbent.kind == ProxyKind::Tcr)
    {
        Some(peer) => Takeover::IncumbentPresent(*peer),
        None => Takeover::Proceed,
    }
}

/// Which of the recognized incumbents this run will actually signal.
///
/// Gated on [`takeover_decision`] rather than reimplementing its reasoning, so
/// "we proceeded" and "we signalled exactly these" cannot drift apart: a
/// stand-down signals NOTHING, including a legacy JS proxy sharing the port with
/// the `tcr` peer we are standing down for. Half-killing there would leave the
/// port held by the survivor and this process refusing to bind anyway — all cost,
/// no benefit.
///
/// Past that gate: with `--replace`, every incumbent. Without it, only the legacy
/// JS proxy, which is the one case that reaches `Proceed` unasked.
///
/// A [`ProxyKind::TcrEmbedded`] incumbent is filtered out unconditionally, on top
/// of the stand-down gate that already covers it. Both, deliberately: signalling
/// one kills a host application without its `applicationWillTerminate`, so the
/// guarantee is worth stating twice rather than resting on a `!= Proceed` that a
/// future edit to [`takeover_decision`] could relax by accident.
///
/// # The one stand-down that is not "touch nothing": `--replace` past an embedded
///
/// `port_listeners` filters by port, not by address, so a legacy JS
/// `teamclaude server` on `[::1]:3456` and an embedded tcr on `127.0.0.1:3456`
/// are both holders. Reading the embedded stand-down as "signal nothing" made
/// that JS proxy undisplaceable on EVERY path — `--replace` included, since the
/// embedded check precedes it — and the two proxies then token-war over the same
/// single-use refresh tokens forever, which is the failure this module exists to
/// end. So the kinds are partitioned rather than collapsed: the embedded
/// incumbent is never signalled and still stands us down, while an explicit
/// `--replace` still reaches the NON-embedded incumbents beside it. That is the
/// escape hatch `takeover_decision`'s doc-comment promises for a `LegacyJs`
/// incumbent, and it is the only way to honour both rules at once.
///
/// Without `--replace` a stand-down still signals nothing at all, embedded or
/// not: the operator did not ask for a kill, and killing half the port's holders
/// while refusing to bind is all cost and no benefit.
fn incumbents_to_signal(replaceable: &[Incumbent], replace: bool) -> Vec<Incumbent> {
    let embedded_present = replaceable
        .iter()
        .any(|incumbent| incumbent.kind == ProxyKind::TcrEmbedded);
    let stood_down = takeover_decision(replaceable, replace) != Takeover::Proceed;
    if stood_down && !(embedded_present && replace) {
        return Vec::new();
    }
    replaceable
        .iter()
        .copied()
        .filter(|incumbent| incumbent.kind != ProxyKind::TcrEmbedded)
        .filter(|incumbent| replace || incumbent.kind == ProxyKind::LegacyJs)
        .collect()
}

/// A marker phrase in [`stand_down_message`] that a SECOND codebase parses.
///
/// TcrBar (`apps/macos`) supervises a spawned `tcr server` by scanning its
/// stderr: `ServerController.incumbentMarkers` turns a match into
/// `.incumbentHoldsPort` (a benign report) or `.takeoverRefused` (the takeover
/// did not happen), and a miss into a bare `.exited(0)` — which for a stand-down
/// would render "the server exited cleanly" for a server that never bound.
/// The phrase is load-bearing across a language boundary that no compiler
/// checks, so it is a named constant with a test, not an inline literal.
pub const INCUMBENT_MARKER: &str = "another proxy holds";

/// What we print before standing down, as a pure function so the cross-language
/// marker contract above is testable.
///
/// Says "is listening", NOT "is healthy". All we established is that a
/// command-verified proxy holds the port (`replaceable_incumbents` ->
/// `classify_proxy_server`);
/// nothing here probes whether it still serves. A wedged proxy holds its port just
/// fine, and the build line printed right after this one can say the incumbent never
/// answered — so claiming health here would both overstate the check and contradict
/// the next line. Assert only what the code checked.
fn stand_down_message(port: u16, pid: u32) -> String {
    format!(
        "[tcr] {INCUMBENT_MARKER} :{port} (pid {pid}) and it is still listening — leaving it \
         alone and exiting without binding. Replacing it would wipe its session→account pin map \
         and cold-start every live session's prompt cache, the most expensive event in this \
         system. Pass --replace to take the port over anyway."
    )
}

/// The stand-down message for an EMBEDDED incumbent — the one case `--replace`
/// does not override, so the message must not offer it.
///
/// Carries the same [`INCUMBENT_MARKER`] the CLI message does, because TcrBar's
/// `incumbentMarkers` scan is what turns this into a reported incumbent instead of
/// a bare `exited(0)`; only the instruction differs, and it names the host
/// application, which is the one place that can actually stop this proxy.
fn embedded_stand_down_message(port: u16, pid: u32) -> String {
    format!(
        "[tcr] {INCUMBENT_MARKER} :{port} (pid {pid}) and it is served from inside its host \
         application — leaving it alone and exiting without binding. --replace cannot take this \
         one over: the pid is the app's, and signalling it would kill the app without its normal \
         shutdown, losing the session→account pin map. Quit the host application to stop this \
         proxy."
    )
}

/// Resolve `port` down to one proxy for this server.
///
/// With `replace`, a recognized proxy incumbent is terminated (SIGTERM, then
/// SIGKILL a survivor after a grace) so this instance can bind. Without it — the
/// DEFAULT — a `tcr` incumbent is reported and left running, and the caller is
/// told to exit rather than bind; a LEGACY JS incumbent is still replaced, since
/// displacing it is what this module is for. A non-proxy holder is reported and
/// LEFT ALONE in every case — the bind then fails loudly with EADDRINUSE if
/// that holder shares our bind address (in practice, loopback); against a
/// wildcard-bound holder the bind is not guaranteed to refuse, see
/// [`port_listeners`]'s doc for the measured gap.
#[must_use = "the caller must exit instead of binding on IncumbentPresent"]
pub fn takeover_port(port: u16, replace: bool) -> Takeover {
    let holders = port_listeners(port);
    let owner = classify_port_owner(port, &holders, &default_owner_path(port), process_command);
    let replaceable =
        replaceable_incumbents_with_owner(&holders, std::process::id(), process_command, owner);

    // Surface a non-proxy holder (we won't kill it; the bind will fail).
    for &pid in &holders {
        if pid != std::process::id() && !replaceable.iter().any(|i| i.pid == pid) {
            let cmd = process_command(pid);
            if !cmd.is_empty() {
                eprintln!(
                    "[tcr] :{port} is held by a non-proxy process (pid {pid}): {} — not replacing it; the bind will fail if it stays.",
                    cmd.join(" ")
                );
            }
        }
    }

    let decision = takeover_decision(&replaceable, replace);
    if let Takeover::IncumbentPresent(incumbent) = decision {
        // Only the pid and the instruction live here; `main` prints the build
        // comparison, because THAT is the part a user typing `tcr` after a rebuild
        // actually needs and it requires an async status read we must not do here.
        //
        // The kind comes from the decision itself rather than a second scan of
        // `replaceable` for the same predicate: two copies of "is this one
        // embedded?" is how the generic message — which offers `--replace` — ends
        // up printed for the one kind `--replace` cannot take over.
        if incumbent.kind == ProxyKind::TcrEmbedded {
            eprintln!("{}", embedded_stand_down_message(port, incumbent.pid));
        } else {
            eprintln!("{}", stand_down_message(port, incumbent.pid));
        }
    }

    // Not `else`: standing down for an EMBEDDED incumbent still lets an explicit
    // `--replace` displace the non-embedded proxies sharing the port. See
    // `incumbents_to_signal`; without it a legacy JS proxy co-resident with an
    // embedded tcr is undisplaceable on every path.
    for Incumbent { pid, kind } in incumbents_to_signal(&replaceable, replace) {
        match kind {
            // Unreachable by construction — `incumbents_to_signal` filters this
            // kind out twice. It is handled rather than `unreachable!()`d because
            // the cost of being wrong is a SIGTERM to the operator's menu-bar app:
            // skip and say so, never panic and never signal.
            ProxyKind::TcrEmbedded => {
                tracing::warn!(
                    port,
                    pid,
                    "refusing to signal an embedded proxy: the pid belongs to its host application"
                );
                continue;
            }
            ProxyKind::LegacyJs => eprintln!(
                "[tcr] replacing the legacy JS teamclaude proxy on :{port} (pid {pid}) — migrating the port to tcr; leaving it running would token-war over the same single-use refresh tokens."
            ),
            ProxyKind::Tcr => eprintln!(
                "[tcr] replacing existing proxy on :{port} (pid {pid}) — one proxy per port, or the two mutually invalidate each other's single-use refresh tokens (token war)."
            ),
        }
        // The other half of the boot marker: in TUI mode this eprintln lands on a
        // terminal that the alternate screen is about to cover, so the durable log
        // is the only place a takeover is recoverable. Pairs with "server started"
        // to turn "the server bounced" into "pid N was killed by pid M".
        tracing::info!(port, replaced_pid = pid, "replacing incumbent proxy");
        // Graceful first: SIGTERM, then a grace window, then SIGKILL only if it
        // survived. `signal_pid` calls `kill_with`, never `kill` (which always
        // sends SIGKILL) — collapsing these two calls onto `kill` would delete
        // this exact grace window and cost every live session its prompt cache.
        signal_pid(pid, Signal::Term);
        sleep(Duration::from_millis(800));
        if is_alive(pid) {
            signal_pid(pid, Signal::Kill);
            sleep(Duration::from_millis(300));
        }
    }
    decision
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `listeners` integration itself, not just the pure logic around it:
    /// bind an ephemeral port IN-PROCESS (port 0 — never a literal port, and
    /// never the live proxy's `127.0.0.1:3456`) and assert `port_listeners`
    /// reports exactly THIS process's pid on the port the OS actually assigned.
    /// `local_addr()` is required rather than querying port 0 directly:
    /// `listeners::get_process_by_port` (which we deliberately do not call) even
    /// errors outright on `port == 0`, and port 0 is not a real listener's port
    /// in the socket table either way.
    #[test]
    fn port_listeners_finds_this_process_on_an_ephemeral_port() {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("binding an ephemeral port");
        let port = listener
            .local_addr()
            .expect("a bound listener has a local address")
            .port();

        let holders = port_listeners(port);
        assert!(
            holders.contains(&std::process::id()),
            "port {port} should be reported as held by this process (pid {}); got {holders:?}",
            std::process::id()
        );

        drop(listener);
    }

    /// The real defect, end to end, not just the pure dedup helper in isolation:
    /// bind `0.0.0.0:0` to get an OS-assigned port, then bind `[::]:<that same
    /// port>` in the SAME process — the dual-stack pattern a default-configured
    /// Node HTTP server (including the legacy JS proxy this module migrates off
    /// of) produces from one `listen()` call. Both are genuine LISTEN sockets on
    /// the same pid and port, so without the dedup this reports `[pid, pid]` and
    /// callers that read the first element still get the right pid — but the
    /// `takeover_port` signal loop iterates the WHOLE list, so the SIGTERM ->
    /// sleep -> is_alive -> SIGKILL sequence would run twice against one process
    /// with no fresh verification on the second pass. Port 0, in-process,
    /// nothing signalled.
    #[test]
    fn port_listeners_collapses_a_dual_stack_bind_to_one_pid() {
        let v4 = std::net::TcpListener::bind("0.0.0.0:0").expect("binding an ephemeral IPv4 port");
        let port = v4
            .local_addr()
            .expect("a bound listener has a local address")
            .port();

        let Ok(v6) = std::net::TcpListener::bind(format!("[::]:{port}")) else {
            // Some stacks refuse the v6 bind once v4 already holds the port (no
            // dual-stack support, or `IPV6_V6ONLY` defaults differ locally).
            // Falling back to the pure-helper check rather than failing the run
            // — this asserts the same dedup logic `port_listeners` uses, just
            // without a real second socket to prove the end-to-end case.
            drop(v4);
            let mut seen = HashSet::new();
            let deduped: Vec<u32> = [4242_u32, 4242_u32, 7_u32]
                .into_iter()
                .filter(|&pid| seen.insert(pid))
                .collect();
            assert_eq!(
                deduped,
                vec![4242, 7],
                "fallback: the dedup pattern itself should be order-preserving \
                 (could not verify the real dual-stack bind on this machine)"
            );
            return;
        };

        let holders = port_listeners(port);
        assert_eq!(
            holders,
            vec![std::process::id()],
            "one process holding the port on two dual-stack sockets should report ONE pid \
             (order-preserving dedup), got {holders:?}"
        );

        drop(v4);
        drop(v6);
    }

    /// The negative control the `SocketState::Listen` filter never had: a test
    /// proving a LISTEN socket IS found existed, nothing proved a non-LISTEN
    /// socket is NOT. That filter is the issue-#36 guard on the kill path
    /// (`replaceable_incumbents` -> `takeover_decision` -> the SIGTERM/SIGKILL
    /// loop), so positive-only coverage means a regression that dropped the
    /// filter would ship silently. Single process, port 0, no second proxy, no
    /// root, nothing signalled.
    #[test]
    fn port_listeners_excludes_a_non_listening_socket_on_the_same_port() {
        let server = std::net::TcpListener::bind("127.0.0.1:0").expect("binding an ephemeral port");
        let port = server
            .local_addr()
            .expect("a bound listener has a local address")
            .port();

        // Positive control: the listener itself is still up.
        let holders = port_listeners(port);
        assert!(
            holders.contains(&std::process::id()),
            "the LISTEN socket on port {port} should be reported; got {holders:?}"
        );

        let _client = std::net::TcpStream::connect(("127.0.0.1", port))
            .expect("connecting to our own listener");
        let (_accepted, _) = server.accept().expect("accepting the connection");
        // `_accepted`'s LOCAL port is also `port`, but its state is ESTABLISHED,
        // not LISTEN — it must never appear here.
        drop(server);
        // The LISTEN socket is gone; only the ESTABLISHED accepted socket still
        // has local port `port`. Without the `SocketState::Listen` filter this
        // would report this process's pid again and the assertion below fails.
        // Same narrowing as the UDP case below, for the same reason: once `server`
        // is dropped the port is released, so an unrelated process can take it and
        // be reported here legitimately. The claim under test is about OUR
        // ESTABLISHED socket, and the inline positive control above already proved
        // `port_listeners` reports this pid when a real LISTEN socket exists.
        let holders_after = port_listeners(port);
        assert!(
            !holders_after.contains(&std::process::id()),
            "an ESTABLISHED socket on port {port} must not be reported as a LISTEN holder; \
             got {holders_after:?}"
        );
    }

    /// UDP control: `listeners`' backends hard-code every UDP socket's state to
    /// `SocketState::Unknown` (never `Listen`), so this passes today for that
    /// reason rather than because of an explicit UDP exclusion — which is
    /// exactly why it is worth having as a canary. The day upstream gives UDP a
    /// real state, `Protocol::TCP` (not the state filter) is what keeps this
    /// green; if this test ever goes red, that upstream change is why.
    #[test]
    fn port_listeners_excludes_a_udp_socket_on_the_same_port() {
        let udp = std::net::UdpSocket::bind("127.0.0.1:0").expect("binding an ephemeral UDP port");
        let port = udp
            .local_addr()
            .expect("a bound socket has a local address")
            .port();

        // Assert on OUR pid, not on global emptiness. A UDP bind reserves a port
        // in the UDP space only — the TCP space is independent, so an unrelated
        // process may legitimately be TCP-LISTENing on this same number. Asserting
        // `is_empty()` made this test depend on the whole machine's TCP usage, a
        // precondition it never established, and it went red on a CI runner where
        // pid 9460 held TCP on the port the kernel handed us for UDP.
        //
        // This cannot pass vacuously through a broken `port_listeners` that always
        // returns empty: `port_listeners_finds_this_process_on_an_ephemeral_port`
        // is the positive control for that, and would fail first.
        let holders = port_listeners(port);
        assert!(
            !holders.contains(&std::process::id()),
            "a UDP socket on port {port} must never be reported as a TCP LISTEN holder; \
             got {holders:?}"
        );

        drop(udp);
    }

    /// Proves `signal_pid` sends the SIGNAL ITSELF, not merely "some kill" — the
    /// single most dangerous mistake available in this module
    /// (`sysinfo::Process::kill()` always sends SIGKILL regardless of what a
    /// caller asks for; `kill()` is `kill_with(Signal::Kill)` under the hood).
    /// A green test suite that only checks the code path taken would pass
    /// whether this sends SIGTERM or SIGKILL — so this reads the SIGNAL NUMBER
    /// the OS actually delivered off the child's exit status.
    ///
    /// Spawns our OWN throwaway child (`sleep 30`) — this never signals any pid
    /// the test did not itself spawn, and never comes near a real proxy or the
    /// live pid 50152.
    #[cfg(unix)]
    #[test]
    fn signal_pid_term_delivers_sigterm_not_sigkill() {
        use std::os::unix::process::ExitStatusExt;
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawning our own throwaway child");
        let pid = child.id();

        signal_pid(pid, Signal::Term);

        let status = child.wait().expect("waiting on our own child");
        assert_eq!(
            status.signal(),
            Some(15), // SIGTERM
            "signal_pid(.., Signal::Term) must deliver SIGTERM (15), not SIGKILL (9) \
             or any other signal -- this is the graceful-shutdown window the module \
             exists to protect"
        );
    }

    /// The other half of the same proof, for the survivor path: `Signal::Kill`
    /// must deliver SIGKILL (9), not the graceful signal.
    #[cfg(unix)]
    #[test]
    fn signal_pid_kill_delivers_sigkill_not_sigterm() {
        use std::os::unix::process::ExitStatusExt;
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawning our own throwaway child");
        let pid = child.id();

        signal_pid(pid, Signal::Kill);

        let status = child.wait().expect("waiting on our own child");
        assert_eq!(
            status.signal(),
            Some(9), // SIGKILL
            "signal_pid(.., Signal::Kill) must deliver SIGKILL (9)"
        );
    }

    /// Builds an argv `Vec<String>` from string-literal parts — matching what
    /// [`sysinfo::Process::cmd`] returns (pre-tokenized, no shell/whitespace
    /// splitting involved). Test-only stand-in for a real process's argv, kept
    /// tight so no test has to hand-expand every literal into a `Vec`.
    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn recognizes_js_teamclaude_server() {
        assert!(is_proxy_server(&argv(&[
            "node",
            "/opt/nvm/bin/teamclaude",
            "server"
        ])));
        assert!(is_proxy_server(&argv(&[
            "node",
            "/path/teamclaude",
            "server",
            "-r"
        ])));
        assert!(is_proxy_server(&argv(&["/path/teamclaude", "server"]))); // shebang exec
    }

    #[test]
    fn recognizes_tcr_server() {
        assert!(is_proxy_server(&argv(&[
            "/opt/teamclaude-rs/target/release/tcr",
            "server"
        ])));
        assert!(is_proxy_server(&argv(&["tcr"]))); // bare = default server
        assert!(is_proxy_server(&argv(&["/x/tcr", "--headless"]))); // default server + flag
        assert!(is_proxy_server(&argv(&["tcr", "server", "--port", "3456"])));
    }

    #[test]
    fn rejects_non_servers() {
        assert!(!is_proxy_server(&argv(&[
            "node",
            "/path/teamclaude",
            "run"
        ])));
        assert!(!is_proxy_server(&argv(&[
            "node",
            "/path/teamclaude",
            "run",
            "-r"
        ])));
        assert!(!is_proxy_server(&argv(&["/x/tcr", "run"])));
        assert!(!is_proxy_server(&argv(&["/x/tcr", "status"])));
        assert!(!is_proxy_server(&argv(&["/x/tcr", "accounts"])));
        assert!(!is_proxy_server(&argv(&["grep", "teamclaude", "server"]))); // teamclaude is a search term
        assert!(!is_proxy_server(&argv(&["rg", "tcr", "server"])));
        assert!(!is_proxy_server(&argv(&["vim", "teamclaude-server.md"])));
        assert!(!is_proxy_server(&argv(&[])));
        assert!(!is_proxy_server(&argv(&["node", "/path/other", "server"])));
    }

    fn tcr(pid: u32) -> Incumbent {
        Incumbent {
            pid,
            kind: ProxyKind::Tcr,
        }
    }

    fn legacy_js(pid: u32) -> Incumbent {
        Incumbent {
            pid,
            kind: ProxyKind::LegacyJs,
        }
    }

    fn embedded(pid: u32) -> Incumbent {
        Incumbent {
            pid,
            kind: ProxyKind::TcrEmbedded,
        }
    }

    /// A host application's argv: nothing in it matches the `tcr` name matcher,
    /// which is the entire reason the owner file exists.
    const HOST_APP_ARGV: &[&str] = &["/Applications/TcrBar.app/Contents/MacOS/TcrBar"];

    /// An unrelated program listening on the port — what a recycled pid actually
    /// runs. Not a proxy by any reading of its argv.
    const DEV_SERVER_ARGV: &[&str] = &["/usr/local/bin/node", "/srv/app/dev-server.js"];

    /// The argv of a real CLI-hosted proxy, for the tests that need the claim's
    /// pid to survive the command check.
    fn tcr_command(_pid: u32) -> Vec<String> {
        argv(&["/x/tcr", "server"])
    }

    /// A unique scratch dir for a claim file. Never [`default_owner_path`] — that
    /// one is the live proxy's, and `tcr login` reads it.
    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tcr-owner-test-{}-{tag}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("a scratch dir under the temp dir is creatable");
        dir
    }

    fn write_claim(dir: &Path, port: u16, pid: u32, host: ProxyHost) -> PathBuf {
        let path = owner_path_in(dir, port);
        write_owner_file(
            &path,
            &ProxyOwner {
                pid,
                port,
                sha: "0000000".to_string(),
                host,
            },
        )
        .expect("the scratch claim is writable");
        path
    }

    /// (a) THE FALSE-POSITIVE CONTROL, and the one that makes the whole mechanism
    /// safe to trust: a claim whose pid is **not** listening on the port is
    /// IGNORED.
    ///
    /// Without this check the file is a lie anyone can plant — any process could
    /// write `{"pid":<some pid>,"host":"embedded"}` and make `tcr` treat that pid
    /// as an unreplaceable proxy, and every crashed proxy would leave behind a
    /// claim protecting a pid the OS has since recycled onto something unrelated.
    /// The file is a *claim*; `port_listeners` is what settles it.
    #[test]
    fn a_claim_whose_pid_is_not_listening_is_ignored() {
        let dir = scratch_dir("stale");
        let path = write_claim(&dir, 4444, 4242, ProxyHost::Embedded);

        // The file is real, parses, and is for the right port — the ONLY defect is
        // that its pid holds nothing.
        assert!(path.exists());
        assert_eq!(
            classify_port_owner(4444, &[], &path, tcr_command),
            None,
            "a claim with no live listener at all must be ignored"
        );
        assert_eq!(
            classify_port_owner(4444, &[9999, 8888], &path, tcr_command),
            None,
            "a claim whose pid is not among the port's listeners must be ignored"
        );
        // And the positive control, so the assertions above cannot be passing for
        // the boring reason that this function always returns None.
        assert_eq!(
            classify_port_owner(4444, &[9999, 4242], &path, tcr_command),
            Some(embedded(4242)),
            "a claim whose pid IS listening is the one case that counts"
        );
        // A claim for a different port is not this port's claim either.
        assert_eq!(
            classify_port_owner(4445, &[4242], &path, tcr_command),
            None,
            "the port in the file must match the port being resolved"
        );

        // A stale claim does not even suppress the fallback: the name matcher
        // still gets its turn, so a `tcr` predating the owner file is recognized.
        let command = |_pid: u32| argv(&["/x/tcr", "server"]);
        let owner = classify_port_owner(4444, &[7777], &path, command);
        assert_eq!(
            replaceable_incumbents_with_owner(&[7777], 1, command, owner),
            vec![tcr(7777)],
            "ignoring a stale claim must fall through to command-line recognition"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The case the whole phase exists for: a proxy inside a host application is
    /// invisible to the name matcher, and the claim is what makes it visible.
    ///
    /// The `assert!` on the matcher is not decoration — it is the bug being fixed,
    /// stated as an executable fact. If it ever starts matching, `live_proxy_server`
    /// stopped depending on the claim and this file's reason to exist changed.
    #[test]
    fn an_embedded_proxy_is_recognized_only_through_its_claim() {
        assert_eq!(
            classify_proxy_server(&argv(HOST_APP_ARGV)),
            None,
            "the name matcher cannot see a proxy hosted inside another program — \
             that is the failure the claim file replaces"
        );

        let dir = scratch_dir("embedded");
        let path = write_claim(&dir, 3456, 5150, ProxyHost::Embedded);
        let command = |_pid: u32| argv(HOST_APP_ARGV);
        let owner = classify_port_owner(3456, &[5150], &path, command);

        assert_eq!(
            replaceable_incumbents_with_owner(&[5150], 1, command, owner),
            vec![embedded(5150)],
            "the claim identifies the proxy the process name hides"
        );
        // Without the claim, the same holder is nothing at all — which is exactly
        // what made `tcr login` stop refusing to run beside a live server.
        assert!(replaceable_incumbents(&[5150], 1, command).is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `host: "cli"` is a plain [`ProxyKind::Tcr`] — the claim adds identity, not a
    /// new policy, for a proxy the matcher could already see.
    #[test]
    fn a_cli_claim_is_an_ordinary_tcr_peer() {
        let dir = scratch_dir("cli");
        let path = write_claim(&dir, 3456, 1234, ProxyHost::Cli);
        assert_eq!(
            classify_port_owner(3456, &[1234], &path, tcr_command),
            Some(tcr(1234)),
            "a CLI-hosted claim is the same kind the name matcher would produce"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// THE SECOND FALSE-POSITIVE CONTROL, and the one that stands between a stale
    /// claim and a SIGKILL to an innocent process: the claimed pid IS listening on
    /// the port — it is simply not a proxy.
    ///
    /// The stale-file test above only covers "the pid is not listening", which a
    /// recycled pid passes trivially. The scenario that costs someone their unsaved
    /// state is the other one: `tcr` is SIGKILLed leaving a `cli` claim behind,
    /// months later that pid is a dev server on the same port, and a reader that
    /// treats the claim as sufficient hands it to the kill loop. `verified_owner`
    /// must ask `ps` before believing a `cli` claim, and the pid must then survive
    /// to the very end of the chain unrecognized — `replaceable_incumbents_with_owner`
    /// is what `takeover_port` iterates to choose whom to signal.
    #[test]
    fn a_cli_claim_whose_listening_pid_is_not_a_proxy_is_ignored() {
        let dir = scratch_dir("recycled-pid");
        let path = write_claim(&dir, 3456, 5150, ProxyHost::Cli);
        let dev_server = |_pid: u32| argv(DEV_SERVER_ARGV);

        assert_eq!(
            classify_proxy_server(&argv(DEV_SERVER_ARGV)),
            None,
            "the fixture is only meaningful if the command line is genuinely not a proxy"
        );
        assert_eq!(
            classify_port_owner(3456, &[5150], &path, dev_server),
            None,
            "a cli claim naming a pid that is not running a proxy is a stale claim on a \
             recycled pid, not an incumbent"
        );
        // The positive control: the SAME claim, same pid, same holders — only the
        // command line differs. So the None above is the command check, not the
        // function refusing everything.
        assert_eq!(
            classify_port_owner(3456, &[5150], &path, tcr_command),
            Some(tcr(5150)),
            "the same claim IS believed when the pid is still running a proxy"
        );

        // End to end: the pid reaches the takeover chain as nothing at all, so
        // there is no incumbent for --replace to SIGTERM.
        let owner = classify_port_owner(3456, &[5150], &path, dev_server);
        assert_eq!(
            replaceable_incumbents_with_owner(&[5150], 1, dev_server, owner),
            vec![],
            "an unrelated listener on the port must never become a replaceable incumbent"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// BACKWARD COMPATIBILITY, and the reason this phase needs no restart: the
    /// proxy serving right now wrote no claim, and it must still be recognized.
    ///
    /// A missing file, an empty one and a corrupt one all mean "no claim", never
    /// "not a proxy" — a claim that cannot be read must not be able to *hide* an
    /// incumbent the matcher would have found.
    #[test]
    fn a_proxy_that_wrote_no_claim_is_still_recognized() {
        let dir = scratch_dir("absent");
        let command = |_pid: u32| argv(&["/x/tcr", "server"]);

        for (label, path) in [
            ("absent", owner_path_in(&dir, 3456)),
            ("empty", {
                let p = dir.join("empty.json");
                std::fs::write(&p, b"").expect("scratch write");
                p
            }),
            ("corrupt", {
                let p = dir.join("corrupt.json");
                std::fs::write(&p, b"{not json").expect("scratch write");
                p
            }),
            ("wrong-shape", {
                let p = dir.join("wrong-shape.json");
                std::fs::write(&p, br#"{"pid":222}"#).expect("scratch write");
                p
            }),
        ] {
            let owner = classify_port_owner(3456, &[222], &path, command);
            assert_eq!(owner, None, "{label} is not a claim");
            assert_eq!(
                replaceable_incumbents_with_owner(&[222], 1, command, owner),
                vec![tcr(222)],
                "{label}: the retained name matcher must still recognize a running \
                 proxy that never wrote a claim"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (b) An EMBEDDED incumbent stands the caller down even under `--replace`.
    ///
    /// The pid on the port belongs to the host application, and `takeover_port`
    /// SIGTERMs it (see [`ProxyKind::TcrEmbedded`]): AppKit installs no SIGTERM
    /// handler, so `--replace` would kill the app without its shutdown path — no
    /// final session→account pin write, and the operator's app simply vanishes. A
    /// CLI flag must not be able to do that.
    #[test]
    fn an_embedded_incumbent_stands_us_down_even_under_replace() {
        for replace in [false, true] {
            assert_eq!(
                takeover_decision(&[embedded(777)], replace),
                Takeover::IncumbentPresent(embedded(777)),
                "replace={replace} must not be able to take over an embedded proxy"
            );
        }
        // And it wins over a legacy JS proxy sharing the port, in either order:
        // proceeding for the JS proxy means SIGNALLING it, and this process would
        // still refuse to bind afterwards.
        for holders in [
            vec![legacy_js(111), embedded(777)],
            vec![embedded(777), legacy_js(111)],
        ] {
            assert_eq!(
                takeover_decision(&holders, true),
                Takeover::IncumbentPresent(embedded(777)),
                "{holders:?}"
            );
        }
    }

    /// (c) The embedded incumbent ITSELF is never signalled — the assertion that
    /// stands between `--replace` and SIGTERM to a GUI process.
    #[test]
    fn an_embedded_incumbent_is_never_signalled() {
        for replace in [false, true] {
            assert_eq!(
                incumbents_to_signal(&[embedded(777)], replace),
                vec![],
                "replace={replace}: an embedded proxy must never be signalled"
            );
            assert!(
                !incumbents_to_signal(&[embedded(777), legacy_js(111)], replace)
                    .iter()
                    .any(|incumbent| incumbent.kind == ProxyKind::TcrEmbedded),
                "replace={replace}: not even beside another incumbent on the port"
            );
        }
        // Without --replace nothing at all is signalled: the operator asked for no
        // kill, and half-killing while refusing to bind is all cost, no benefit.
        assert_eq!(
            incumbents_to_signal(&[embedded(777), legacy_js(111)], false),
            vec![]
        );
    }

    /// THE ESCAPE HATCH, and the reason the embedded stand-down is not read as
    /// "touch nothing": a legacy JS proxy sharing the port with an embedded tcr
    /// must still be displaceable with `--replace`.
    ///
    /// `port_listeners` filters by port and not by address, so `node …/teamclaude
    /// server` on `[::1]:3456` and an embedded tcr on `127.0.0.1:3456` are both
    /// holders. Collapsing that to one kind-blind stand-down left the JS proxy
    /// serving forever on every path — and `takeover_decision`'s own doc-comment
    /// says that outcome is unacceptable, because the two then token-war over the
    /// same single-use refresh tokens with no way out.
    #[test]
    fn replace_still_displaces_a_legacy_js_proxy_beside_an_embedded_incumbent() {
        for holders in [
            vec![legacy_js(111), embedded(777)],
            vec![embedded(777), legacy_js(111)],
        ] {
            assert_eq!(
                incumbents_to_signal(&holders, true),
                vec![legacy_js(111)],
                "--replace must still reach the JS proxy, and ONLY it: {holders:?}"
            );
            // We still do not bind: the embedded proxy legitimately holds the port
            // and the stand-down is unchanged. Ending the token war and taking the
            // port over are two different questions.
            assert_eq!(
                takeover_decision(&holders, true),
                Takeover::IncumbentPresent(embedded(777)),
                "{holders:?}"
            );
        }
        // A `tcr` peer beside an embedded proxy is likewise reachable by an
        // explicit --replace, and the embedded one still is not.
        assert_eq!(
            incumbents_to_signal(&[embedded(777), tcr(222)], true),
            vec![tcr(222)]
        );
    }

    /// The embedded stand-down still carries the marker TcrBar greps for, and does
    /// NOT offer `--replace` — the one instruction that cannot work here.
    #[test]
    fn the_embedded_stand_down_message_reports_without_offering_replace() {
        let message = embedded_stand_down_message(3456, 777);
        assert!(
            message.contains("another proxy holds"),
            "apps/macos ServerController.incumbentMarkers greps for this: {message}"
        );
        assert!(message.contains("777"), "names the incumbent: {message}");
        assert!(
            !message.contains("--replace to"),
            "must not offer an override that is refused for this kind: {message}"
        );
    }

    /// THE SUCCESSOR'S CLAIM SURVIVES OUR SHUTDOWN. The claim path is named after
    /// the port, so a proxy shutting down and the proxy that just replaced it
    /// address the same file — and the shutdown frees the listener BEFORE it gets
    /// here (`server::ServerHandle::shutdown_within` joins background tasks in
    /// between, including a blocking affinity write). An unconditional unlink
    /// deletes the live successor's claim: for an embedded successor the name
    /// matcher then recognizes nothing, `tcr login` stops refusing, and the
    /// boot-time single-use refresh tokens overwrite the fresh ones.
    #[test]
    fn a_withdrawal_deletes_only_a_claim_this_process_still_owns() {
        let dir = scratch_dir("withdraw");

        // (1) The successor's claim — different pid, same port, same path.
        let path = write_claim(&dir, 3456, 777, ProxyHost::Embedded);
        remove_owner_file_if_owned(&path, 5150, 3456);
        assert!(
            path.exists(),
            "withdrawing must not delete a claim written by another pid: {}",
            path.display()
        );
        assert_eq!(
            read_owner_file(&path).map(|owner| owner.pid),
            Some(777),
            "and it must be left byte-for-byte the successor's"
        );

        // (2) A claim for another port at this path is not ours either.
        remove_owner_file_if_owned(&path, 777, 3457);
        assert!(path.exists(), "the port must match too: {}", path.display());

        // (3) The positive control: our OWN claim is withdrawn, so a shutdown
        // still stops advertising a port it no longer listens on.
        let ours = write_claim(&dir, 3457, 5150, ProxyHost::Cli);
        remove_owner_file_if_owned(&ours, 5150, 3457);
        assert!(
            !ours.exists(),
            "a proxy must withdraw its own claim on shutdown: {}",
            ours.display()
        );

        // (4) Withdrawing twice (a re-issued shutdown) is quiet and harmless.
        remove_owner_file_if_owned(&ours, 5150, 3457);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The claim file is named after the PORT, so two proxies on two ports never
    /// overwrite each other's identity, and it lives beside the pin cache rather
    /// than anywhere a caller has to remember.
    #[test]
    fn the_claim_path_is_port_scoped_and_beside_the_pin_cache() {
        assert_eq!(
            owner_path_in(Path::new("/tmp/x"), 3456),
            PathBuf::from("/tmp/x/proxy-owner-3456.json")
        );
        assert_ne!(
            owner_path_in(Path::new("/tmp/x"), 3456),
            owner_path_in(Path::new("/tmp/x"), 3457)
        );
        assert_eq!(
            default_owner_path(3456).parent(),
            crate::affinity::default_path().parent(),
            "the claim belongs in the same directory as the pin cache"
        );
    }

    /// FORWARD COMPATIBILITY. The claim is read by a different BUILD of `tcr` than
    /// the one that wrote it — an older one, on any machine where a newer proxy is
    /// serving. A host value this build does not know must degrade to "a proxy is
    /// there, do not signal it", never to "no claim at all": the latter drops the
    /// reader back to the name matcher, which cannot see an embedded proxy, so
    /// `tcr login` proceeds beside a live server and `--replace` SIGTERMs the
    /// process the file existed to protect.
    #[test]
    fn a_host_this_build_does_not_know_is_still_a_proxy_worth_protecting() {
        let dir = scratch_dir("future-host");
        let path = owner_path_in(&dir, 3456);
        std::fs::write(
            &path,
            br#"{"pid":900,"port":3456,"sha":"0000000","host":"sidecar"}"#,
        )
        .expect("scratch write");

        let parsed: ProxyOwner =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("scratch read"))
                .expect("an unknown host must not fail the whole parse");
        assert_eq!(parsed.host, ProxyHost::Unknown);
        assert_eq!(parsed.pid, 900, "the rest of the claim still parses");

        // And it lands on the kind that is never signalled, whatever the host
        // program's command line says.
        let command = |_pid: u32| argv(HOST_APP_ARGV);
        assert_eq!(
            classify_port_owner(3456, &[900], &path, command),
            Some(embedded(900)),
            "an unrecognized host degrades to the protected kind, not to nothing"
        );
        assert_eq!(
            incumbents_to_signal(&[embedded(900)], true),
            vec![],
            "and --replace still cannot signal it"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The on-disk shape is a cross-process contract: the file a proxy writes is
    /// read by a DIFFERENT `tcr` build, possibly older or newer. The key names are
    /// spelled out here so a rename cannot happen silently.
    #[test]
    fn the_claim_is_the_documented_json_shape() {
        let json = serde_json::to_string(&ProxyOwner {
            pid: 42,
            port: 3456,
            sha: "abc1234".to_string(),
            host: ProxyHost::Embedded,
        })
        .expect("a claim serializes");
        assert_eq!(
            json, r#"{"pid":42,"port":3456,"sha":"abc1234","host":"embedded"}"#,
            "the claim's field names and the lowercase host values are read by \
             other builds of tcr"
        );
        let parsed: ProxyOwner =
            serde_json::from_str(r#"{"pid":7,"port":1,"sha":"s","host":"cli"}"#)
                .expect("the documented shape parses");
        assert_eq!(parsed.host, ProxyHost::Cli);
    }

    /// THE INDEPENDENT ORACLE for this lane. This defect predates the argv
    /// refactor: the OLD `&str` implementation did `cmd.split_whitespace()`,
    /// which mis-tokenises any executable path containing a space —
    /// `"/Applications/My App/tcr server"` split into
    /// `["/Applications/My", "App/tcr", "server"]`, so `tokens[0]` never ended
    /// with `/tcr` and a REAL tcr server went unrecognized (treated as a
    /// non-proxy holder, or worse left un-killable under `--replace`, or
    /// spuriously blocking `tcr login`). Verified failing against the `&str` API
    /// before this refactor landed. Now expressed against the argv API this
    /// module actually exposes: `sysinfo::Process::cmd()` hands back the
    /// kernel's own tokenization, so the path's internal space never has to be
    /// re-split at all — the defect class is gone by construction, not patched.
    #[test]
    fn recognizes_a_tcr_server_at_a_path_containing_a_space() {
        assert_eq!(
            classify_proxy_server(&argv(&["/Applications/My App/tcr", "server"])),
            Some(ProxyKind::Tcr),
            "a space in the executable's path must not hide a real tcr server"
        );
    }

    #[test]
    fn classify_names_which_proxy_it_found() {
        assert_eq!(
            classify_proxy_server(&argv(&["node", "/opt/nvm/bin/teamclaude", "server"])),
            Some(ProxyKind::LegacyJs)
        );
        assert_eq!(
            classify_proxy_server(&argv(&["/opt/teamclaude-rs/target/release/tcr", "server"])),
            Some(ProxyKind::Tcr)
        );
        assert_eq!(classify_proxy_server(&argv(&["tcr"])), Some(ProxyKind::Tcr));
        assert_eq!(classify_proxy_server(&argv(&["/x/tcr", "status"])), None);
    }

    #[test]
    fn replaceable_incumbents_filters_self_and_non_proxies() {
        let command = |pid: u32| -> Vec<String> {
            match pid {
                111 => argv(&["node", "/x/teamclaude", "server"]),
                222 => argv(&["/x/tcr", "server"]),
                333 => argv(&["grep", "teamclaude", "server"]), // non-proxy on the port
                444 => argv(&["/x/tcr", "run"]),
                _ => Vec::new(),
            }
        };
        // 999 is self; 333/444 are non-proxies → only 111 and 222 survive, each
        // carrying WHICH proxy it is, because the default treats them differently.
        let replace = replaceable_incumbents(&[111, 222, 333, 444, 999], 999, command);
        assert_eq!(replace, vec![legacy_js(111), tcr(222)]);
    }

    #[test]
    fn replaceable_incumbents_never_includes_self_even_if_it_looks_like_a_proxy() {
        let command = |_pid: u32| argv(&["/x/tcr", "server"]);
        assert!(replaceable_incumbents(&[42], 42, command).is_empty());
    }

    /// THE DEFAULT, and the whole point of the enum: a healthy `tcr` PEER is left
    /// running and the caller is told to stand down. `Proceed` here would reach
    /// the kill loop and cost every live session its prompt cache.
    #[test]
    fn a_healthy_tcr_peer_is_left_alone_by_default() {
        assert_eq!(
            takeover_decision(&[tcr(4242)], false),
            Takeover::IncumbentPresent(tcr(4242))
        );
        assert_eq!(
            incumbents_to_signal(&[tcr(4242)], false),
            vec![],
            "a protected peer must not be signalled"
        );
    }

    /// `--replace` is the only way to reach the kill loop for a `tcr` peer.
    #[test]
    fn replace_flag_proceeds_to_the_takeover() {
        assert_eq!(takeover_decision(&[tcr(4242)], true), Takeover::Proceed);
        assert_eq!(
            incumbents_to_signal(&[tcr(4242)], true),
            vec![tcr(4242)],
            "and it is the incumbent that gets signalled"
        );
    }

    /// THE MIGRATION CASE. `classify_proxy_server` recognizes the JS proxy
    /// precisely so the port can be RECLAIMED from it — the failure this module's
    /// doc-comment names. Standing down for it would leave `node …/teamclaude
    /// server` serving every request forever, and a user who installed tcr to
    /// replace it would never get the port without a flag nothing tells them
    /// about. It is not a peer whose prompt cache we are protecting; it is the
    /// other half of the token war.
    #[test]
    fn a_legacy_js_incumbent_is_replaced_by_default() {
        assert_eq!(
            takeover_decision(&[legacy_js(111)], false),
            Takeover::Proceed,
            "the JS proxy is the incumbent this module exists to displace"
        );
        assert_eq!(
            incumbents_to_signal(&[legacy_js(111)], false),
            vec![legacy_js(111)],
            "proceeding past a JS incumbent means SIGNALLING it — leaving it \
             running risks the two proxies token-warring instead, not \
             necessarily a loud EADDRINUSE; see `port_listeners`' doc on the \
             wildcard gap"
        );
    }

    /// Nothing replaceable on the port — bind, with or without the flag. Covers
    /// both the free port and the non-proxy holder, which `replaceable_incumbents`
    /// has already filtered out by the time we get here (the bind then fails
    /// loudly, which is the documented behaviour and must not change).
    #[test]
    fn an_empty_port_always_proceeds() {
        assert_eq!(takeover_decision(&[], false), Takeover::Proceed);
        assert_eq!(takeover_decision(&[], true), Takeover::Proceed);
        assert_eq!(incumbents_to_signal(&[], false), vec![]);
    }

    /// The stand-down message is read by TcrBar's `incumbentMarkers`, in another
    /// language, with no compiler between the two. Dropping the phrase would make
    /// a stand-down render in the menu-bar app as a clean `exited(0)` — "the
    /// server started and stopped" for a server that never bound — and would turn
    /// its takeover button into a silent no-op instead of a reported refusal.
    #[test]
    fn the_stand_down_message_carries_the_marker_tcrbar_greps_for() {
        // The literal is SPELLED OUT rather than referenced through
        // `INCUMBENT_MARKER`, deliberately. Asserting `message.contains(MARKER)`
        // compares the constant with itself and passes for ANY value of it — the
        // constant is exactly the thing that must not drift, so the test has to
        // hold the other copy. This literal must equal the entry in
        // `apps/macos/Sources/TcrBarCore/ServerController.swift`'s
        // `incumbentMarkers`; nothing but this assertion couples them.
        let tcrbar_greps_for = "another proxy holds";
        assert_eq!(
            INCUMBENT_MARKER, tcrbar_greps_for,
            "the marker must stay the string TcrBar's incumbentMarkers list carries"
        );
        let message = stand_down_message(3456, 4242);
        assert!(
            message.contains(tcrbar_greps_for),
            "apps/macos ServerController.incumbentMarkers greps for \
             {tcrbar_greps_for:?}: {message}"
        );
        assert!(message.contains("4242"), "names the incumbent: {message}");
        assert!(
            message.contains("--replace"),
            "names the override: {message}"
        );
    }

    /// Several incumbents at once (a leftover JS `teamclaude` beside a `tcr`):
    /// the `tcr` peer wins the protection whatever order `lsof` returned them in,
    /// and standing down signals NOTHING — a half-kill would leave the port held
    /// by the survivor and this process refusing to bind anyway.
    #[test]
    fn a_tcr_peer_protects_the_port_even_beside_a_legacy_js_incumbent() {
        for holders in [
            vec![legacy_js(111), tcr(222)],
            vec![tcr(222), legacy_js(111)],
        ] {
            assert_eq!(
                takeover_decision(&holders, false),
                Takeover::IncumbentPresent(tcr(222)),
                "the protected peer is the one named: {holders:?}"
            );
            assert_eq!(
                incumbents_to_signal(&holders, false),
                vec![],
                "standing down kills nothing: {holders:?}"
            );
        }
    }
}
