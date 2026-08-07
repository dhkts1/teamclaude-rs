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

use std::process::Command;
use std::thread::sleep;
use std::time::Duration;

/// Which proxy a recognized incumbent is. The distinction is load-bearing for
/// [`takeover_decision`], not cosmetic: a `tcr` peer is a program that is doing
/// this program's job and whose session pins cost real money to discard, while a
/// leftover JS `teamclaude` is the thing this module was written to displace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyKind {
    /// The Rust proxy — a peer of ours.
    Tcr,
    /// The legacy JS proxy (`node …/teamclaude server`).
    LegacyJs,
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
pub fn replaceable_incumbents(
    holders: &[u32],
    self_pid: u32,
    command: impl Fn(u32) -> String,
) -> Vec<Incumbent> {
    holders
        .iter()
        .copied()
        .filter(|&pid| pid != self_pid)
        .filter_map(|pid| classify_proxy_server(&command(pid)).map(|kind| Incumbent { pid, kind }))
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
    replaceable_incumbents(&holders, std::process::id(), process_command)
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
pub fn takeover_decision(replaceable: &[Incumbent], replace: bool) -> Takeover {
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
fn incumbents_to_signal(replaceable: &[Incumbent], replace: bool) -> Vec<Incumbent> {
    if takeover_decision(replaceable, replace) != Takeover::Proceed {
        return Vec::new();
    }
    replaceable
        .iter()
        .copied()
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
    let replaceable = replaceable_incumbents(&holders, std::process::id(), process_command);

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
        eprintln!("{}", stand_down_message(port, pid));
        return Takeover::IncumbentPresent(pid);
    }

    for Incumbent { pid, kind } in incumbents_to_signal(&replaceable, replace) {
        match kind {
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
