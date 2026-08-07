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

/// Does this command line look like a teamclaude/tcr *server* — a process safe to
/// replace as a proxy incumbent on the port? Recognizes the JS proxy
/// (`node …/teamclaude server`) and the Rust proxy (`…/tcr` at argv0, with the
/// `server` subcommand, no subcommand, or a flag — since bare `tcr` defaults to the
/// server). Never matches a bare `grep`/editor/`tcr run`/`teamclaude run`/etc.
pub fn is_proxy_server(cmd: &str) -> bool {
    let tokens: Vec<&str> = cmd.split_whitespace().collect();
    if tokens.is_empty() {
        return false;
    }
    let is_node = |t: &str| t == "node" || t.ends_with("/node");
    let is_teamclaude = |t: &str| t == "teamclaude" || t.ends_with("/teamclaude");
    let is_tcr = |t: &str| t == "tcr" || t.ends_with("/tcr");

    // JS proxy: the teamclaude bin must be the executed program (argv0, or the arg
    // right after `node`) AND the subcommand must be `server`.
    if let Some(i) = tokens.iter().position(|t| is_teamclaude(t)) {
        let is_program = i == 0 || is_node(tokens[i - 1]);
        if is_program && tokens.get(i + 1) == Some(&"server") {
            return true;
        }
    }

    // Rust proxy: `tcr` at argv0. A server unless the subcommand is a recognized
    // NON-server verb; bare `tcr`, `tcr server`, and `tcr --flag` are all servers.
    if is_tcr(tokens[0]) {
        return match tokens.get(1).copied() {
            None => true,
            Some(t) if t.starts_with('-') || t == "server" => true,
            Some(_) => false, // `tcr run`, `tcr status`, `tcr accounts`, …
        };
    }

    false
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
/// replaceable proxy incumbents? Excludes self and any non-proxy holder. Split out
/// from the side-effecting kill so it is unit-testable with injected inputs.
pub fn pids_to_replace(
    holders: &[u32],
    self_pid: u32,
    command: impl Fn(u32) -> String,
) -> Vec<u32> {
    holders
        .iter()
        .copied()
        .filter(|&pid| pid != self_pid && is_proxy_server(&command(pid)))
        .collect()
}

/// Detection-only: the PID of a live teamclaude/tcr *server* currently holding
/// `port`, if any. Reuses the exact port-scoped, command-verified decision as
/// [`takeover_port`] ([`pids_to_replace`]) but signals NOTHING — `tcr login` uses
/// it to REFUSE to run beside a live server (the server reads config only at boot,
/// and its next `persist_tokens` writes its boot-time TOKENS back over the file,
/// clobbering the login's fresh ones). Returns the first replaceable proxy PID;
/// `None` when the
/// port is free or held only by a non-proxy process.
pub fn live_proxy_server(port: u16) -> Option<u32> {
    let holders = port_listeners(port);
    pids_to_replace(&holders, std::process::id(), process_command)
        .into_iter()
        .next()
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
/// Split out from the side effects for the same reason as [`pids_to_replace`] —
/// and it is the single gate on the kill loop below, so a test that pins this
/// function pins whether a live proxy gets signalled.
pub fn takeover_decision(replaceable: &[u32], replace: bool) -> Takeover {
    match replaceable.first() {
        Some(&pid) if !replace => Takeover::IncumbentPresent(pid),
        _ => Takeover::Proceed,
    }
}

/// Resolve `port` down to one proxy for this server.
///
/// With `replace`, a recognized proxy incumbent is terminated (SIGTERM, then
/// SIGKILL a survivor after a grace) so this instance can bind. Without it — the
/// DEFAULT — the incumbent is reported and left running, and the caller is told
/// to exit rather than bind. A non-proxy holder is reported and LEFT ALONE in
/// both cases (the bind then fails loudly with EADDRINUSE).
#[must_use = "the caller must exit instead of binding on IncumbentPresent"]
pub fn takeover_port(port: u16, replace: bool) -> Takeover {
    let holders = port_listeners(port);
    let replaceable = pids_to_replace(&holders, std::process::id(), process_command);

    // Surface a non-proxy holder (we won't kill it; the bind will fail).
    for &pid in &holders {
        if pid != std::process::id() && !replaceable.contains(&pid) {
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
        eprintln!(
            "[tcr] :{port} is already served by a tcr proxy (pid {pid}) — leaving it alone and exiting. Replacing it would wipe its session→account pin map and cold-start every live session's prompt cache. Pass --replace to take the port over anyway."
        );
        return Takeover::IncumbentPresent(pid);
    }

    for pid in replaceable {
        eprintln!(
            "[tcr] replacing existing proxy on :{port} (pid {pid}) — one proxy per port, or the two mutually invalidate each other's single-use refresh tokens (token war)."
        );
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

    #[test]
    fn pids_to_replace_filters_self_and_non_proxies() {
        let command = |pid: u32| -> String {
            match pid {
                111 => "node /x/teamclaude server".to_string(),
                222 => "/x/tcr server".to_string(),
                333 => "grep teamclaude server".to_string(), // non-proxy on the port
                444 => "/x/tcr run".to_string(),
                _ => String::new(),
            }
        };
        // 999 is self; 333/444 are non-proxies → only 111 and 222 are replaced.
        let replace = pids_to_replace(&[111, 222, 333, 444, 999], 999, command);
        assert_eq!(replace, vec![111, 222]);
    }

    #[test]
    fn pids_to_replace_never_includes_self_even_if_it_looks_like_a_proxy() {
        let command = |_pid: u32| "/x/tcr server".to_string();
        assert!(pids_to_replace(&[42], 42, command).is_empty());
    }

    /// THE DEFAULT, and the whole point of the enum: a healthy proxy incumbent is
    /// left running and the caller is told to stand down. `Proceed` here would
    /// reach the kill loop and cost every live session its prompt cache.
    #[test]
    fn a_healthy_incumbent_is_left_alone_by_default() {
        assert_eq!(
            takeover_decision(&[4242], false),
            Takeover::IncumbentPresent(4242)
        );
    }

    /// `--replace` is the ONLY way to reach the kill loop.
    #[test]
    fn replace_flag_proceeds_to_the_takeover() {
        assert_eq!(takeover_decision(&[4242], true), Takeover::Proceed);
    }

    /// Nothing replaceable on the port — bind, with or without the flag. Covers
    /// both the free port and the non-proxy holder, which `pids_to_replace` has
    /// already filtered out by the time we get here (the bind then fails loudly,
    /// which is the documented behaviour and must not change).
    #[test]
    fn an_empty_port_always_proceeds() {
        assert_eq!(takeover_decision(&[], false), Takeover::Proceed);
        assert_eq!(takeover_decision(&[], true), Takeover::Proceed);
    }

    /// Several incumbents (a leftover JS `teamclaude` beside a `tcr`) report the
    /// first rather than collapsing to `Proceed` — one of them is enough to make
    /// binding wrong.
    #[test]
    fn multiple_incumbents_still_stand_down_by_default() {
        assert_eq!(
            takeover_decision(&[111, 222], false),
            Takeover::IncumbentPresent(111)
        );
    }
}
