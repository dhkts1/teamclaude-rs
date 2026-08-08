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
//! - **Port-scoped.** Only the process listening on `127.0.0.1:<configured port>`
//!   is a candidate — a proxy (or anything else) on another port is never touched,
//!   so a test server on an ephemeral port is safe.
//! - **Command-verified.** A candidate is signalled only if its command line is an
//!   actual `teamclaude`/`tcr` *server*. A non-proxy holder is left alone (the bind
//!   then fails loudly with EADDRINUSE) rather than killed. This is what makes the
//!   takeover safe against a reused PID or an unrelated process on the port.
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

use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::sleep;
use std::time::Duration;

use serde::{Deserialize, Serialize};

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
}

impl ProxyHost {
    /// The incumbent kind a *verified* claim from this host means.
    fn kind(self) -> ProxyKind {
        match self {
            ProxyHost::Cli => ProxyKind::Tcr,
            ProxyHost::Embedded => ProxyKind::TcrEmbedded,
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

/// Where the binary keeps the claim: beside the session-affinity pin cache, so
/// the proxy's two pieces of cross-boot state live in one directory.
pub fn default_owner_path(port: u16) -> PathBuf {
    let cache = crate::affinity::default_path();
    let dir = cache
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    owner_path_in(&dir, port)
}

/// Write the claim atomically (0600, temp + rename — the same
/// [`crate::config::write_atomic`] every other state file uses, so a reader can
/// never see a half-written claim).
pub fn write_owner_file(path: &Path, owner: &ProxyOwner) -> Result<(), crate::config::ConfigError> {
    crate::config::write_atomic(path, &serde_json::to_string_pretty(owner)?)
}

/// Drop the claim. Best-effort by design: a file left behind by a crash cannot
/// produce a false positive ([`verified_owner`] re-checks the pid against the
/// live listeners), so failing to remove one costs nothing.
pub fn remove_owner_file(path: &Path) {
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
fn read_owner_file(path: &Path) -> Option<ProxyOwner> {
    let data = std::fs::read_to_string(path).ok()?;
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
    command: impl Fn(u32) -> String,
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
    command: impl Fn(u32) -> String,
) -> Option<Incumbent> {
    verified_owner(read_owner_file(owner_path).as_ref(), port, holders, command)
}

/// A recognized, replaceable proxy holding the port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Incumbent {
    pub pid: u32,
    pub kind: ProxyKind,
}

/// Does this command line look like a teamclaude/tcr *server* — a process safe to
/// replace as a proxy incumbent on the port? Recognizes the JS proxy
/// (`node …/teamclaude server`) and the Rust proxy (`…/tcr` at argv0, with the
/// `server` subcommand, no subcommand, or a flag — since bare `tcr` defaults to the
/// server). Never matches a bare `grep`/editor/`tcr run`/`teamclaude run`/etc.
pub fn is_proxy_server(cmd: &str) -> bool {
    classify_proxy_server(cmd).is_some()
}

/// [`is_proxy_server`] plus WHICH proxy it is. Same recognition rules — the bool
/// version delegates here, so the two can never drift apart.
pub fn classify_proxy_server(cmd: &str) -> Option<ProxyKind> {
    let tokens: Vec<&str> = cmd.split_whitespace().collect();
    if tokens.is_empty() {
        return None;
    }
    let is_node = |t: &str| t == "node" || t.ends_with("/node");
    let is_teamclaude = |t: &str| t == "teamclaude" || t.ends_with("/teamclaude");
    let is_tcr = |t: &str| t == "tcr" || t.ends_with("/tcr");

    // JS proxy: the teamclaude bin must be the executed program (argv0, or the arg
    // right after `node`) AND the subcommand must be `server`.
    if let Some(i) = tokens.iter().position(|t| is_teamclaude(t)) {
        let is_program = i == 0 || is_node(tokens[i - 1]);
        if is_program && tokens.get(i + 1) == Some(&"server") {
            return Some(ProxyKind::LegacyJs);
        }
    }

    // Rust proxy: `tcr` at argv0. A server unless the subcommand is a recognized
    // NON-server verb; bare `tcr`, `tcr server`, and `tcr --flag` are all servers.
    if is_tcr(tokens[0]) {
        return match tokens.get(1).copied() {
            None => Some(ProxyKind::Tcr),
            Some(t) if t.starts_with('-') || t == "server" => Some(ProxyKind::Tcr),
            Some(_) => None, // `tcr run`, `tcr status`, `tcr accounts`, …
        };
    }

    None
}

/// The command line of `pid` (via `ps`); empty if the process is gone.
fn process_command(pid: u32) -> String {
    match Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => String::new(),
    }
}

/// PIDs listening on `127.0.0.1:port` (via `lsof`). Empty on any failure — a
/// missing/erroring `lsof` degrades to "no takeover", never a false kill.
fn port_listeners(port: u16) -> Vec<u32> {
    match Command::new("lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-t"])
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter_map(|l| l.trim().parse::<u32>().ok())
            .collect(),
        _ => Vec::new(),
    }
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
    command: impl Fn(u32) -> String,
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
    command: impl Fn(u32) -> String,
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
/// clobbering the login's fresh ones). Returns the first replaceable proxy PID;
/// `None` when the
/// port is free or held only by a non-proxy process.
pub fn live_proxy_server(port: u16) -> Option<u32> {
    let holders = port_listeners(port);
    let owner = classify_port_owner(port, &holders, &default_owner_path(port), process_command);
    replaceable_incumbents_with_owner(&holders, std::process::id(), process_command, owner)
        .into_iter()
        .next()
        .map(|incumbent| incumbent.pid)
}

/// Is `pid` still alive? (`kill -0`.)
fn is_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .is_ok_and(|s| s.success())
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
    IncumbentPresent(u32),
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
pub fn takeover_decision(replaceable: &[Incumbent], replace: bool) -> Takeover {
    if let Some(embedded) = replaceable
        .iter()
        .find(|incumbent| incumbent.kind == ProxyKind::TcrEmbedded)
    {
        return Takeover::IncumbentPresent(embedded.pid);
    }
    if replace {
        return Takeover::Proceed;
    }
    match replaceable
        .iter()
        .find(|incumbent| incumbent.kind == ProxyKind::Tcr)
    {
        Some(peer) => Takeover::IncumbentPresent(peer.pid),
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
fn incumbents_to_signal(replaceable: &[Incumbent], replace: bool) -> Vec<Incumbent> {
    if takeover_decision(replaceable, replace) != Takeover::Proceed {
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
/// LEFT ALONE in every case (the bind then fails loudly with EADDRINUSE).
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
                    "[tcr] :{port} is held by a non-proxy process (pid {pid}): {cmd} — not replacing it; the bind will fail if it stays."
                );
            }
        }
    }

    if let Takeover::IncumbentPresent(pid) = takeover_decision(&replaceable, replace) {
        // Only the pid and the instruction live here; `main` prints the build
        // comparison, because THAT is the part a user typing `tcr` after a rebuild
        // actually needs and it requires an async status read we must not do here.
        let embedded = replaceable
            .iter()
            .any(|incumbent| incumbent.pid == pid && incumbent.kind == ProxyKind::TcrEmbedded);
        if embedded {
            eprintln!("{}", embedded_stand_down_message(port, pid));
        } else {
            eprintln!("{}", stand_down_message(port, pid));
        }
        return Takeover::IncumbentPresent(pid);
    }

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
        let _ = Command::new("kill").arg(pid.to_string()).status();
        sleep(Duration::from_millis(800));
        if is_alive(pid) {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
            sleep(Duration::from_millis(300));
        }
    }
    Takeover::Proceed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_js_teamclaude_server() {
        assert!(is_proxy_server("node /opt/nvm/bin/teamclaude server"));
        assert!(is_proxy_server("node /path/teamclaude server -r"));
        assert!(is_proxy_server("/path/teamclaude server")); // shebang exec
    }

    #[test]
    fn recognizes_tcr_server() {
        assert!(is_proxy_server(
            "/opt/teamclaude-rs/target/release/tcr server"
        ));
        assert!(is_proxy_server("tcr")); // bare = default server
        assert!(is_proxy_server("/x/tcr --headless")); // default server + flag
        assert!(is_proxy_server("tcr server --port 3456"));
    }

    #[test]
    fn rejects_non_servers() {
        assert!(!is_proxy_server("node /path/teamclaude run"));
        assert!(!is_proxy_server("node /path/teamclaude run -r"));
        assert!(!is_proxy_server("/x/tcr run"));
        assert!(!is_proxy_server("/x/tcr status"));
        assert!(!is_proxy_server("/x/tcr accounts"));
        assert!(!is_proxy_server("grep teamclaude server")); // teamclaude is a search term
        assert!(!is_proxy_server("rg tcr server"));
        assert!(!is_proxy_server("vim teamclaude-server.md"));
        assert!(!is_proxy_server(""));
        assert!(!is_proxy_server("node /path/other server"));
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

    /// A host application's command line: nothing in it matches the `tcr` name
    /// matcher, which is the entire reason the owner file exists.
    const HOST_APP_COMMAND: &str = "/Applications/TcrBar.app/Contents/MacOS/TcrBar";

    /// An unrelated program listening on the port — what a recycled pid actually
    /// runs. Not a proxy by any reading of its command line.
    const DEV_SERVER_COMMAND: &str = "/usr/local/bin/node /srv/app/dev-server.js";

    /// The command line of a real CLI-hosted proxy, for the tests that need the
    /// claim's pid to survive the command check.
    fn tcr_command(_pid: u32) -> String {
        "/x/tcr server".to_string()
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
        let command = |_pid: u32| "/x/tcr server".to_string();
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
            classify_proxy_server(HOST_APP_COMMAND),
            None,
            "the name matcher cannot see a proxy hosted inside another program — \
             that is the failure the claim file replaces"
        );

        let dir = scratch_dir("embedded");
        let path = write_claim(&dir, 3456, 5150, ProxyHost::Embedded);
        let command = |_pid: u32| HOST_APP_COMMAND.to_string();
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
        let dev_server = |_pid: u32| DEV_SERVER_COMMAND.to_string();

        assert_eq!(
            classify_proxy_server(DEV_SERVER_COMMAND),
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
        let command = |_pid: u32| "/x/tcr server".to_string();

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
                Takeover::IncumbentPresent(777),
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
                Takeover::IncumbentPresent(777),
                "{holders:?}"
            );
        }
    }

    /// (c) Nothing is ever signalled for an embedded incumbent — the assertion
    /// that stands between `--replace` and SIGTERM to a GUI process.
    #[test]
    fn an_embedded_incumbent_is_never_signalled() {
        for replace in [false, true] {
            assert_eq!(
                incumbents_to_signal(&[embedded(777)], replace),
                vec![],
                "replace={replace}: an embedded proxy must never be signalled"
            );
            assert_eq!(
                incumbents_to_signal(&[embedded(777), legacy_js(111)], replace),
                vec![],
                "replace={replace}: nor its port-mates, since we stand down anyway"
            );
        }
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

    #[test]
    fn classify_names_which_proxy_it_found() {
        assert_eq!(
            classify_proxy_server("node /opt/nvm/bin/teamclaude server"),
            Some(ProxyKind::LegacyJs)
        );
        assert_eq!(
            classify_proxy_server("/opt/teamclaude-rs/target/release/tcr server"),
            Some(ProxyKind::Tcr)
        );
        assert_eq!(classify_proxy_server("tcr"), Some(ProxyKind::Tcr));
        assert_eq!(classify_proxy_server("/x/tcr status"), None);
    }

    #[test]
    fn replaceable_incumbents_filters_self_and_non_proxies() {
        let command = |pid: u32| -> String {
            match pid {
                111 => "node /x/teamclaude server".to_string(),
                222 => "/x/tcr server".to_string(),
                333 => "grep teamclaude server".to_string(), // non-proxy on the port
                444 => "/x/tcr run".to_string(),
                _ => String::new(),
            }
        };
        // 999 is self; 333/444 are non-proxies → only 111 and 222 survive, each
        // carrying WHICH proxy it is, because the default treats them differently.
        let replace = replaceable_incumbents(&[111, 222, 333, 444, 999], 999, command);
        assert_eq!(replace, vec![legacy_js(111), tcr(222)]);
    }

    #[test]
    fn replaceable_incumbents_never_includes_self_even_if_it_looks_like_a_proxy() {
        let command = |_pid: u32| "/x/tcr server".to_string();
        assert!(replaceable_incumbents(&[42], 42, command).is_empty());
    }

    /// THE DEFAULT, and the whole point of the enum: a healthy `tcr` PEER is left
    /// running and the caller is told to stand down. `Proceed` here would reach
    /// the kill loop and cost every live session its prompt cache.
    #[test]
    fn a_healthy_tcr_peer_is_left_alone_by_default() {
        assert_eq!(
            takeover_decision(&[tcr(4242)], false),
            Takeover::IncumbentPresent(4242)
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
             running would land the bind on EADDRINUSE instead"
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
                Takeover::IncumbentPresent(222),
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
