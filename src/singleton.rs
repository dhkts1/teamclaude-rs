//! Port-takeover singleton: only one proxy may own the configured port.
//!
//! The recurring failure is two proxies — a leftover JS `teamclaude` and/or a
//! `tcr` — both refreshing the same accounts. Each holds its own in-memory copy of
//! the SINGLE-USE OAuth refresh tokens, so the first refresh by either revokes the
//! other's copy and accounts flap to `error` (the "token war"). On startup we take
//! over the configured port: if a recognized proxy already holds it, replace it,
//! then bind.
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

/// Is `pid` still alive? (`kill -0`.)
fn is_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Take over `port` for this server. If a recognized proxy already holds it,
/// terminate it (SIGTERM, then SIGKILL a survivor after a grace) so this instance
/// can bind. A non-proxy holder is reported and LEFT ALONE. When `no_replace` is
/// set, an incumbent proxy is only warned about, not killed.
pub fn takeover_port(port: u16, no_replace: bool) {
    let holders = port_listeners(port);
    let replace = pids_to_replace(&holders, std::process::id(), process_command);

    // Surface a non-proxy holder (we won't kill it; the bind will fail).
    for &pid in &holders {
        if pid != std::process::id() && !replace.contains(&pid) {
            let cmd = process_command(pid);
            if !cmd.is_empty() {
                eprintln!(
                    "[tcr] :{port} is held by a non-proxy process (pid {pid}): {cmd} — not replacing it; the bind will fail if it stays."
                );
            }
        }
    }

    for pid in replace {
        if no_replace {
            eprintln!(
                "[tcr] another proxy holds :{port} (pid {pid}) and --no-replace was set; two proxies refreshing the same single-use tokens will token-war. Not replacing."
            );
            continue;
        }
        eprintln!(
            "[tcr] replacing existing proxy on :{port} (pid {pid}) — one proxy per port, or the two mutually invalidate each other's single-use refresh tokens (token war)."
        );
        let _ = Command::new("kill").arg(pid.to_string()).status();
        sleep(Duration::from_millis(800));
        if is_alive(pid) {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
            sleep(Duration::from_millis(300));
        }
    }
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
}
