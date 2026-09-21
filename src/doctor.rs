//! `tcr doctor`: whether Claude Code is actually reaching this proxy, and
//! which file decides that.
//!
//! # The failure this exists to end
//!
//! A colleague installed tcr, added a shared account, and every answer kept
//! coming from their own keychain account: the added account stayed at zero
//! requests with the proxy up and healthy. Nothing in `tcr status` could say
//! why, because the fleet view reports the accounts this proxy holds and never
//! the question underneath it: is any client pointed at this proxy at all.
//!
//! `tcr run` exports `ANTHROPIC_BASE_URL` onto the child it launches
//! (`src/main.rs`, `apply_base_url_env`). Claude Code then applies its
//! settings files' `env` block on top of the process environment it inherited
//! (measured, and the reason `apply_capability_defaults` sets its variables at
//! EXEC time rather than leaving them to settings, see its doc-comment in
//! `src/main.rs`). So a base URL written in a settings file WINS over the one
//! the launcher exported, silently, and the proxy sits there serving nobody.
//!
//! # What it reports
//!
//! One greppable `key: value` line each: the base URL Claude Code will use,
//! where that value comes from, who holds the proxy port and whether that
//! process is a `tcr`, how many requests this proxy served in the last
//! [`REQUEST_WINDOW_MINUTES`] minutes, and a `verdict:` line that answers the
//! question in one sentence. Exit code 0 when Claude is routed here, 2 when it
//! is routed somewhere else, 3 when it is routed here and no proxy answers.
//!
//! # Boundaries
//!
//! Everything in this module below [`inspect`] is pure: resolution takes the
//! environment value and two directories as arguments, so a test drives it
//! with a temp HOME and never reads the operator's own files. The I/O half
//! (loading the config, reading the live status endpoint, printing) lives in
//! `crate::cli::doctor`.

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::status::StatusPayload;

/// The base URL Claude Code talks to when nothing sets one.
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// The environment variable Claude Code reads its base URL from, and the one
/// `tcr run` exports onto its child.
pub const BASE_URL_VAR: &str = "ANTHROPIC_BASE_URL";

/// How many minutes of serving history the `requests10m:` line covers. Ten
/// minutes because the question it answers is "is anything reaching this proxy
/// right now", not "has it ever served anything": a cumulative counter stays
/// non-zero forever after a single request and would call a dead route healthy.
pub const REQUEST_WINDOW_MINUTES: usize = 10;

/// Where the base URL Claude Code will use comes from, in the order Claude Code
/// itself resolves them: the project's settings files first, then the user's,
/// then the process environment, then the built-in default.
///
/// The settings files outrank the process environment, which is the whole point
/// of this verb: `tcr run` sets the variable on its child and a settings file
/// overwrites it afterwards. See the module doc-comment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum BaseUrlSource {
    /// `<cwd>/.claude/settings.local.json`, the project's un-shared settings.
    ProjectLocalSettings { path: PathBuf },
    /// `<cwd>/.claude/settings.json`, the project's shared settings.
    ProjectSettings { path: PathBuf },
    /// `<home>/.claude/settings.json`, the user's settings.
    UserSettings { path: PathBuf },
    /// The process environment this `tcr` inherited.
    ProcessEnv,
    /// Nothing set one, so Anthropic's own endpoint.
    Default,
}

impl BaseUrlSource {
    /// The settings file this source names, when it is a file at all.
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::ProjectLocalSettings { path }
            | Self::ProjectSettings { path }
            | Self::UserSettings { path } => Some(path),
            Self::ProcessEnv | Self::Default => None,
        }
    }

    /// How the source is named in the operator-facing lines: the file's path
    /// when it is a file, the variable's name when it is the environment. A
    /// path rather than a label like "user settings" because the operator's
    /// next action is to open the file, and the verdict line should hand them
    /// the argument for that.
    pub fn label(&self) -> String {
        match self {
            Self::ProjectLocalSettings { path }
            | Self::ProjectSettings { path }
            | Self::UserSettings { path } => path.display().to_string(),
            Self::ProcessEnv => BASE_URL_VAR.to_string(),
            Self::Default => "the default".to_string(),
        }
    }
}

/// What one settings file had to say about the base URL.
///
/// `Unreadable` is its own variant rather than being folded into "no value
/// here": a settings file this build cannot parse is the single most likely
/// place for the answer to be hiding, and swallowing that would make `doctor`
/// confidently name the WRONG source. It is reported and resolution continues,
/// which is the only honest degradation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsRead {
    /// No such file.
    Missing,
    /// The file exists and could not be read or parsed; the string says why.
    Unreadable(String),
    /// Parsed, with no `env.ANTHROPIC_BASE_URL` in it.
    NoBaseUrl,
    /// Parsed, and it sets this base URL.
    BaseUrl(String),
}

/// Read `env.ANTHROPIC_BASE_URL` out of one Claude Code settings file.
pub fn read_settings_base_url(path: &Path) -> SettingsRead {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return SettingsRead::Missing,
        Err(error) => return SettingsRead::Unreadable(error.to_string()),
    };
    let document: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(error) => return SettingsRead::Unreadable(format!("not readable as JSON: {error}")),
    };
    match document.get("env").and_then(|env| env.get(BASE_URL_VAR)) {
        None => SettingsRead::NoBaseUrl,
        Some(serde_json::Value::String(url)) => SettingsRead::BaseUrl(url.clone()),
        Some(other) => SettingsRead::Unreadable(format!(
            "env.{BASE_URL_VAR} is {other}, which is not a string"
        )),
    }
}

/// The base URL Claude Code will use, and which of the four sources set it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Route {
    pub url: String,
    pub source: BaseUrlSource,
}

/// [`resolve_base_url`]'s answer plus anything that got in the way of reaching
/// it. `problems` is never silently dropped: `doctor` prints each one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    pub route: Route,
    pub problems: Vec<String>,
}

/// Resolve the base URL the way Claude Code does, from an environment value and
/// the two directories whose `.claude` holds a settings file.
///
/// Takes `env_value`, `home` and `cwd` as parameters rather than reading the
/// process's own: every test here runs against a temp HOME, and a function that
/// read `$HOME` itself could not be tested without touching the operator's real
/// settings.
pub fn resolve_base_url(env_value: Option<&str>, home: &Path, cwd: &Path) -> Resolution {
    let mut problems = Vec::new();
    let candidates = [
        BaseUrlSource::ProjectLocalSettings {
            path: cwd.join(".claude").join("settings.local.json"),
        },
        BaseUrlSource::ProjectSettings {
            path: cwd.join(".claude").join("settings.json"),
        },
        BaseUrlSource::UserSettings {
            path: home.join(".claude").join("settings.json"),
        },
    ];
    for source in candidates {
        let Some(path) = source.path() else {
            continue;
        };
        match read_settings_base_url(path) {
            SettingsRead::BaseUrl(url) => {
                let source = source.clone();
                return Resolution {
                    route: Route { url, source },
                    problems,
                };
            }
            SettingsRead::Missing | SettingsRead::NoBaseUrl => {}
            SettingsRead::Unreadable(why) => {
                problems.push(format!("{}: {why}", path.display()));
            }
        }
    }
    let route = match env_value.filter(|value| !value.trim().is_empty()) {
        Some(url) => Route {
            url: url.to_string(),
            source: BaseUrlSource::ProcessEnv,
        },
        None => Route {
            url: DEFAULT_BASE_URL.to_string(),
            source: BaseUrlSource::Default,
        },
    };
    Resolution { route, problems }
}

/// The host and port of an `http[s]://…` URL, without a URL parser.
///
/// Hand-rolled deliberately: this crate has no `url` dependency and adding one
/// for two fields would be a new dependency for nothing. Scope is exactly what
/// the caller needs: scheme, optional userinfo, host (bracketed IPv6
/// included), optional port. Anything else returns `None` rather than a
/// half-parsed guess.
pub fn authority(url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    let default_port = match scheme.to_ascii_lowercase().as_str() {
        "http" => 80,
        "https" => 443,
        _ => return None,
    };
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .filter(|part| !part.is_empty())?;
    // Userinfo, if any, is everything before the LAST `@`.
    let authority = match authority.rsplit_once('@') {
        Some((_, after)) => after,
        None => authority,
    };
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        let port = match after.strip_prefix(':') {
            Some(digits) => digits.parse().ok()?,
            None => default_port,
        };
        return Some((host.to_string(), port));
    }
    match authority.split_once(':') {
        Some((host, digits)) => Some((host.to_string(), digits.parse().ok()?)),
        None => Some((authority.to_string(), default_port)),
    }
}

/// Is this URL one that reaches a proxy on THIS machine's `port`?
///
/// Loopback by address, not by spelling: `127.0.0.1`, `127.0.0.2`, `localhost`
/// and `[::1]` all reach the same listener, and an operator who wrote any of
/// them has a working route. A non-loopback host is another machine's proxy
/// (or Anthropic), whatever port it names.
pub fn routes_to_port(url: &str, port: u16) -> bool {
    let Some((host, url_port)) = authority(url) else {
        return false;
    };
    if url_port != port {
        return false;
    }
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

/// The process holding the proxy port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PortHolder {
    pub pid: u32,
    /// The process's own name, as the OS reports it.
    pub name: String,
    /// Whether that process is a `tcr` (or the JS `teamclaude` it replaced).
    pub is_tcr: bool,
}

/// Build a holder row from a pid and what the OS says about it. Split from
/// [`port_holder`] so the recognition rule is testable without a live process.
///
/// A holder counts as a `tcr` on its argv0 or its process name, not on
/// [`crate::singleton::classify_proxy_server`]: that function answers a
/// different question (is this safe to REPLACE on the port) and deliberately
/// returns `None` for `tcr run`, which is a tcr by any reading an operator
/// cares about here.
pub fn holder_from(pid: u32, name: &str, argv: &[String]) -> PortHolder {
    let is_proxy_program = |token: &str| {
        token == "tcr"
            || token.ends_with("/tcr")
            || token == "teamclaude"
            || token.ends_with("/teamclaude")
    };
    let is_tcr =
        argv.first().is_some_and(|argv0| is_proxy_program(argv0)) || is_proxy_program(name);
    PortHolder {
        pid,
        name: name.to_string(),
        is_tcr,
    }
}

/// Who is listening on `port`, on any address.
///
/// Same enumeration `src/singleton.rs` uses for the takeover decision (the
/// `listeners` crate, no subprocess, so no `lsof` to be missing or to conflate
/// "no holder" with "the tool failed"), and the same failure posture: an
/// enumeration error is warned about, never turned into a confident "nobody
/// holds the port".
pub fn port_holder(port: u16) -> Option<PortHolder> {
    let all = match listeners::get_all() {
        Ok(all) => all,
        Err(error) => {
            tracing::warn!(port, %error, "listeners::get_all failed; doctor cannot name the port holder");
            return None;
        }
    };
    let pid = all
        .into_iter()
        .filter(|listener| listener.protocol == listeners::Protocol::TCP)
        .filter(|listener| listener.state == listeners::SocketState::Listen)
        .find(|listener| listener.socket.port() == port)
        .map(|listener| listener.process.pid)?;

    let mut system = sysinfo::System::new();
    system.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(pid)]),
        false,
        sysinfo::ProcessRefreshKind::nothing().with_cmd(sysinfo::UpdateKind::Always),
    );
    let process = system.process(sysinfo::Pid::from_u32(pid));
    let name = process.map_or_else(String::new, |p| p.name().to_string_lossy().into_owned());
    let argv = process.map_or_else(Vec::new, |p| {
        p.cmd()
            .iter()
            .map(|token| token.to_string_lossy().into_owned())
            .collect()
    });
    Some(holder_from(pid, &name, &argv))
}

/// Requests this proxy served in the last `minutes` wall-clock minutes, summed
/// over every live session's per-minute sparkline
/// ([`tcr_status_wire::SessionRow::req_per_minute`], 30 entries, oldest first).
///
/// The sparkline rather than [`crate::status::AccountStatus::requests`]: that
/// counter is cumulative since the server booted, so it answers "has this proxy
/// ever served anything", and a route that broke an hour ago would still read
/// as healthy.
pub fn requests_in_window(payload: &StatusPayload, minutes: usize) -> u64 {
    payload
        .sessions
        .iter()
        .map(|session| {
            let minutes = session.req_per_minute.len().min(minutes);
            session
                .req_per_minute
                .iter()
                .rev()
                .take(minutes)
                .map(|&requests| u64::from(requests))
                .sum::<u64>()
        })
        .sum()
}

/// Everything `tcr doctor` prints, decided.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub base_url: String,
    pub base_url_source: BaseUrlSource,
    /// The port this tcr's config says the proxy uses.
    pub proxy_port: u16,
    /// Whether [`Self::base_url`] reaches a proxy on this machine on that port.
    pub routed_here: bool,
    pub port_holder: Option<PortHolder>,
    /// Requests served in the last [`REQUEST_WINDOW_MINUTES`] minutes, or
    /// `None` when no proxy answered on the port, which is a different fact
    /// from zero and must never be flattened into one.
    pub requests_in_window: Option<u64>,
    /// Anything that got in the way of resolving the route (an unreadable
    /// settings file, most often).
    pub problems: Vec<String>,
}

impl Report {
    /// 0 routed here and serving, 2 routed somewhere else, 3 routed here with
    /// nothing answering.
    ///
    /// "Somewhere else" outranks "nothing answered": when Claude is pointed at
    /// another URL, whether THIS proxy is up is not the operator's problem.
    pub fn exit_code(&self) -> i32 {
        if !self.routed_here {
            2
        } else if self.requests_in_window.is_none() {
            3
        } else {
            0
        }
    }

    /// The one-sentence answer, on its own `verdict:` line.
    pub fn verdict(&self) -> String {
        let source = self.base_url_source.label();
        if !self.routed_here {
            return format!(
                "verdict: this proxy is not on Claude's route: {source} sets {}",
                self.base_url
            );
        }
        match self.requests_in_window {
            Some(served) => format!(
                "verdict: Claude is routed to {} ({source}); this proxy has served {served} requests",
                self.base_url
            ),
            None => format!(
                "verdict: Claude is routed to {} ({source}); no proxy answered on 127.0.0.1:{}",
                self.base_url, self.proxy_port
            ),
        }
    }

    /// The greppable `key: value` block, verdict last.
    pub fn render_text(&self) -> String {
        let holder = match &self.port_holder {
            Some(holder) => format!("pid {} {}", holder.pid, holder.name),
            None => "none".to_string(),
        };
        let holder_is_tcr = match &self.port_holder {
            Some(holder) if holder.is_tcr => "yes",
            Some(_) => "no",
            None => "unknown",
        };
        let requests = match self.requests_in_window {
            Some(served) => served.to_string(),
            None => "none (no proxy answered)".to_string(),
        };
        let mut lines = vec![
            format!("baseUrl: {}", self.base_url),
            format!("baseUrlSource: {}", self.base_url_source.label()),
            format!("proxyPort: {}", self.proxy_port),
            format!("portHolder: {holder}"),
            format!("portHolderIsTcr: {holder_is_tcr}"),
            format!("requests{REQUEST_WINDOW_MINUTES}m: {requests}"),
        ];
        for problem in &self.problems {
            lines.push(format!("problem: {problem}"));
        }
        lines.push(self.verdict());
        lines.join("\n")
    }

    /// The same report as one JSON object, with the two derived fields the text
    /// form carries (`verdict`, `exitCode`) written out rather than left for
    /// the reader to re-derive.
    pub fn to_json(&self) -> serde_json::Value {
        let mut value = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        if let Some(object) = value.as_object_mut() {
            object.insert("verdict".to_string(), self.verdict().into());
            object.insert("exitCode".to_string(), self.exit_code().into());
            object.insert(
                "requestWindowMinutes".to_string(),
                REQUEST_WINDOW_MINUTES.into(),
            );
        }
        value
    }
}

/// Build the report for `port` from this process's own environment, HOME and
/// working directory, plus the request count the caller read off the live
/// status endpoint (`None` when nothing answered).
///
/// The live read stays with the caller: `crate::cli` already owns the one
/// client that speaks to the status endpoint, and a second one here would be a
/// second set of timeouts and api-key rules to keep in step.
pub fn inspect(port: u16, requests_in_window: Option<u64>) -> Report {
    let env_value = std::env::var(BASE_URL_VAR).ok();
    let home = std::env::var_os("HOME").map_or_else(PathBuf::new, PathBuf::from);
    let cwd = std::env::current_dir().unwrap_or_default();
    let resolution = resolve_base_url(env_value.as_deref(), &home, &cwd);
    let routed_here = routes_to_port(&resolution.route.url, port);
    Report {
        base_url: resolution.route.url,
        base_url_source: resolution.route.source,
        proxy_port: port,
        routed_here,
        port_holder: port_holder(port),
        requests_in_window,
        problems: resolution.problems,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create the settings directory");
        }
        std::fs::write(path, body).expect("write the settings file");
    }

    fn settings_with(url: &str) -> String {
        format!("{{\"env\": {{\"{BASE_URL_VAR}\": \"{url}\"}}}}")
    }

    /// The defect this verb exists for: the launcher exported the variable and
    /// a settings file overrides it afterwards, so the FILE is the answer.
    #[test]
    fn a_settings_file_outranks_the_process_environment() {
        let home = tempfile::tempdir().expect("temp home");
        let cwd = tempfile::tempdir().expect("temp cwd");
        let settings = home.path().join(".claude").join("settings.json");
        write(&settings, &settings_with("https://gateway.example.com"));

        let resolved = resolve_base_url(Some("http://127.0.0.1:3456"), home.path(), cwd.path());

        assert_eq!(resolved.route.url, "https://gateway.example.com");
        assert_eq!(
            resolved.route.source,
            BaseUrlSource::UserSettings { path: settings }
        );
    }

    #[test]
    fn the_project_settings_outrank_the_user_settings_and_local_outranks_both() {
        let home = tempfile::tempdir().expect("temp home");
        let cwd = tempfile::tempdir().expect("temp cwd");
        write(
            &home.path().join(".claude").join("settings.json"),
            &settings_with("https://user.example.com"),
        );
        write(
            &cwd.path().join(".claude").join("settings.json"),
            &settings_with("https://project.example.com"),
        );

        let project = resolve_base_url(None, home.path(), cwd.path());
        assert_eq!(project.route.url, "https://project.example.com");

        write(
            &cwd.path().join(".claude").join("settings.local.json"),
            &settings_with("https://local.example.com"),
        );
        let local = resolve_base_url(None, home.path(), cwd.path());
        assert_eq!(local.route.url, "https://local.example.com");
        assert_eq!(
            local.route.source,
            BaseUrlSource::ProjectLocalSettings {
                path: cwd.path().join(".claude").join("settings.local.json")
            }
        );
    }

    #[test]
    fn the_environment_wins_when_no_settings_file_sets_one_and_the_default_is_last() {
        let home = tempfile::tempdir().expect("temp home");
        let cwd = tempfile::tempdir().expect("temp cwd");
        write(
            &home.path().join(".claude").join("settings.json"),
            "{\"model\": \"opus\"}",
        );

        let from_env = resolve_base_url(Some("http://127.0.0.1:9999"), home.path(), cwd.path());
        assert_eq!(from_env.route.source, BaseUrlSource::ProcessEnv);
        assert_eq!(from_env.route.url, "http://127.0.0.1:9999");

        let bare = resolve_base_url(None, home.path(), cwd.path());
        assert_eq!(bare.route.source, BaseUrlSource::Default);
        assert_eq!(bare.route.url, DEFAULT_BASE_URL);

        // An empty value is not a route. Claude Code ignores it, and reporting
        // it as the answer would name a source that decides nothing.
        let empty = resolve_base_url(Some("  "), home.path(), cwd.path());
        assert_eq!(empty.route.source, BaseUrlSource::Default);
    }

    /// A settings file that cannot be parsed is the likeliest hiding place for
    /// the answer, so it is reported rather than read as "sets nothing".
    #[test]
    fn an_unreadable_settings_file_is_reported_and_resolution_continues() {
        let home = tempfile::tempdir().expect("temp home");
        let cwd = tempfile::tempdir().expect("temp cwd");
        write(
            &cwd.path().join(".claude").join("settings.json"),
            "{ not json",
        );
        write(
            &home.path().join(".claude").join("settings.json"),
            &settings_with("https://user.example.com"),
        );

        let resolved = resolve_base_url(None, home.path(), cwd.path());

        assert_eq!(resolved.route.url, "https://user.example.com");
        assert_eq!(resolved.problems.len(), 1, "{:?}", resolved.problems);
        assert!(
            resolved.problems[0].contains("settings.json"),
            "the problem must name the file: {}",
            resolved.problems[0]
        );
    }

    #[test]
    fn a_non_string_base_url_is_a_problem_not_a_value() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("settings.json");
        write(&path, &format!("{{\"env\": {{\"{BASE_URL_VAR}\": 8080}}}}"));

        assert!(matches!(
            read_settings_base_url(&path),
            SettingsRead::Unreadable(_)
        ));
    }

    #[test]
    fn authority_reads_host_and_port_including_ipv6_and_userinfo() {
        assert_eq!(
            authority("http://127.0.0.1:3456"),
            Some(("127.0.0.1".to_string(), 3456))
        );
        assert_eq!(
            authority("https://api.anthropic.com"),
            Some(("api.anthropic.com".to_string(), 443))
        );
        assert_eq!(
            authority("http://localhost/v1/messages?x=1"),
            Some(("localhost".to_string(), 80))
        );
        assert_eq!(
            authority("http://[::1]:3456/v1"),
            Some(("::1".to_string(), 3456))
        );
        assert_eq!(
            authority("http://user:pw@127.0.0.1:3456"),
            Some(("127.0.0.1".to_string(), 3456))
        );
        assert_eq!(authority("api.anthropic.com"), None);
        assert_eq!(authority("ftp://127.0.0.1:3456"), None);
    }

    #[test]
    fn routing_is_loopback_plus_the_configured_port() {
        assert!(routes_to_port("http://127.0.0.1:3456", 3456));
        assert!(routes_to_port("http://localhost:3456", 3456));
        assert!(routes_to_port("http://[::1]:3456", 3456));
        assert!(!routes_to_port("http://127.0.0.1:3457", 3456));
        assert!(!routes_to_port("https://api.anthropic.com", 3456));
        // Another machine's tcr on the same port is still not this proxy.
        assert!(!routes_to_port("http://192.168.1.9:3456", 3456));
    }

    /// The enumeration half, against a listener this test process is holding:
    /// the pid reported must be this process. A holder that reads as `none`
    /// while something is demonstrably listening is the failure that makes the
    /// whole `portHolder:` line worthless, and it cannot be caught by a test
    /// that only feeds [`holder_from`] canned argv.
    #[test]
    fn port_holder_names_the_process_actually_listening() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a scratch port");
        let port = listener.local_addr().expect("local addr").port();

        let holder = port_holder(port).expect("a held port must have a holder");

        assert_eq!(holder.pid, std::process::id());
    }

    #[test]
    fn the_holder_is_a_tcr_by_argv0_or_by_name() {
        let tcr = holder_from(11, "tcr", &["/usr/local/bin/tcr".into(), "server".into()]);
        assert!(tcr.is_tcr);
        let running_claude = holder_from(12, "tcr", &["/opt/tcr".into(), "run".into()]);
        assert!(running_claude.is_tcr, "`tcr run` hosts a proxy too");
        let stranger = holder_from(13, "node", &["/usr/bin/node".into(), "gateway.js".into()]);
        assert!(!stranger.is_tcr);
        assert_eq!(stranger.name, "node");
    }

    fn session(req_per_minute: Vec<u16>) -> tcr_status_wire::SessionRow {
        tcr_status_wire::SessionRow {
            session_id: "11111111-1111-1111-1111-111111111111".to_string(),
            account: None,
            model: None,
            first_seen_ms: 0,
            last_seen_ms: 0,
            requests: 0,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            tools: Default::default(),
            req_per_minute,
            cost_usd: 0.0,
        }
    }

    fn payload_with(sessions: Vec<tcr_status_wire::SessionRow>) -> StatusPayload {
        StatusPayload {
            kind: crate::status::STATUS_KIND.to_string(),
            accounts: Vec::new(),
            build: Default::default(),
            http1_only: false,
            control: None,
            group_colors: Default::default(),
            sessions,
            sessions_summary: Default::default(),
            peers: Vec::new(),
            peers_error: None,
        }
    }

    /// The window is the TAIL of the sparkline (newest minutes), so a session
    /// that was busy twenty minutes ago and idle since contributes nothing.
    #[test]
    fn the_window_counts_only_the_newest_ten_minutes() {
        let mut old_traffic = vec![0u16; 30];
        old_traffic[0] = 40;
        old_traffic[5] = 7;
        let mut recent = vec![0u16; 30];
        recent[29] = 3;
        recent[25] = 2;

        let payload = payload_with(vec![session(old_traffic), session(recent)]);

        assert_eq!(requests_in_window(&payload, REQUEST_WINDOW_MINUTES), 5);
    }

    #[test]
    fn a_short_or_absent_sparkline_counts_what_it_has() {
        let payload = payload_with(vec![session(vec![1, 2]), session(Vec::new())]);
        assert_eq!(requests_in_window(&payload, REQUEST_WINDOW_MINUTES), 3);
    }

    fn report_for(url: &str, source: BaseUrlSource, requests: Option<u64>) -> Report {
        Report {
            base_url: url.to_string(),
            base_url_source: source,
            proxy_port: 3456,
            routed_here: routes_to_port(url, 3456),
            port_holder: None,
            requests_in_window: requests,
            problems: Vec::new(),
        }
    }

    #[test]
    fn a_route_to_another_gateway_exits_two_and_names_the_file() {
        let home = tempfile::tempdir().expect("temp home");
        let settings = home.path().join(".claude").join("settings.json");
        let report = report_for(
            "https://gateway.example.com",
            BaseUrlSource::UserSettings {
                path: settings.clone(),
            },
            None,
        );

        assert_eq!(report.exit_code(), 2);
        assert_eq!(
            report.verdict(),
            format!(
                "verdict: this proxy is not on Claude's route: {} sets https://gateway.example.com",
                settings.display()
            )
        );
    }

    #[test]
    fn a_route_here_with_a_serving_proxy_exits_zero() {
        let report = report_for("http://127.0.0.1:3456", BaseUrlSource::ProcessEnv, Some(4));

        assert_eq!(report.exit_code(), 0);
        assert_eq!(
            report.verdict(),
            "verdict: Claude is routed to http://127.0.0.1:3456 (ANTHROPIC_BASE_URL); this proxy has served 4 requests"
        );
    }

    /// Zero served is NOT the same fact as nothing answering, and the exit code
    /// has to keep them apart: a proxy that is up and idle is healthy.
    #[test]
    fn nothing_answering_exits_three_and_zero_served_does_not() {
        let silent = report_for("http://127.0.0.1:3456", BaseUrlSource::ProcessEnv, None);
        assert_eq!(silent.exit_code(), 3);
        assert!(silent.verdict().contains("no proxy answered"));

        let idle = report_for("http://127.0.0.1:3456", BaseUrlSource::ProcessEnv, Some(0));
        assert_eq!(idle.exit_code(), 0);
    }

    #[test]
    fn the_text_block_is_greppable_and_the_json_carries_the_derived_fields() {
        let mut report = report_for("http://127.0.0.1:3456", BaseUrlSource::ProcessEnv, Some(2));
        report.port_holder = Some(holder_from(77, "tcr", &["/usr/local/bin/tcr".into()]));
        report
            .problems
            .push("settings.json: not readable as JSON".to_string());

        let text = report.render_text();
        assert!(text.contains("baseUrl: http://127.0.0.1:3456"), "{text}");
        assert!(text.contains("baseUrlSource: ANTHROPIC_BASE_URL"), "{text}");
        assert!(text.contains("portHolder: pid 77 tcr"), "{text}");
        assert!(text.contains("portHolderIsTcr: yes"), "{text}");
        assert!(text.contains("requests10m: 2"), "{text}");
        assert!(
            text.contains("problem: settings.json: not readable as JSON"),
            "{text}"
        );
        assert!(
            text.lines()
                .last()
                .is_some_and(|last| last.starts_with("verdict: ")),
            "the verdict is the last line: {text}"
        );

        let json = report.to_json();
        assert_eq!(json["exitCode"], 0);
        assert_eq!(json["requestsInWindow"], 2);
        assert_eq!(json["baseUrl"], "http://127.0.0.1:3456");
        assert_eq!(json["portHolder"]["isTcr"], true);
        assert_eq!(json["requestWindowMinutes"], 10);
        assert!(json["verdict"]
            .as_str()
            .is_some_and(|v| v.starts_with("verdict: ")));
    }
}
