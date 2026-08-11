//! `tcr` — the teamclaude-rs binary.
//!
//! Boot sequence (DESIGN §main): load the drop-in config → hand it to
//! [`teamclaude_rs::server::serve`], which builds the [`Manager`], spawns the
//! axum proxy task and the background loops, and binds → run the TUI (or block
//! in `--headless`) → shut the handle down, which flushes on exit.
//!
//! What is left in THIS file is what only a binary may do: parse clap, install a
//! logging subscriber, print operator diagnostics, and turn a stand-down into a
//! process exit code. Everything reusable lives in the library.
//!
//! [`Manager`]: teamclaude_rs::manager::Manager

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use clap::{Parser, Subcommand};

use teamclaude_rs::cli::{self, PriorityArg};
use teamclaude_rs::config::{self, Config, ConfigError};
use teamclaude_rs::{affinity, build_info, demo, mitm, oauth, server, singleton, tui, update};

#[derive(Parser)]
#[command(
    name = "tcr",
    version,
    about = "Lean single-user rotating Anthropic proxy with a live TUI",
    // Let `tcr [flags]` (no subcommand) behave as the default server run, while
    // `tcr server [flags]` is the explicit form. The two cannot be mixed.
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    server: ServerArgs,
}

#[derive(Subcommand)]
enum Command {
    /// Run the proxy server (the default when no subcommand is given).
    Server(ServerArgs),
    /// Run Claude Code through the proxy (launches it directly if we're not up).
    Run(RunArgs),
    /// Authenticate a Claude account via the browser and add it to the config.
    Login(LoginArgs),
    /// List the configured accounts (offline; `--probe` refreshes live quota).
    Accounts(AccountsArgs),
    /// Remove an account from the config.
    Remove(RemoveArgs),
    /// Set an account's rotation priority (lower value = preferred).
    Priority(PriorityArgs),
    /// Enable an account (clears the `disabled` flag).
    Enable(EnableArgs),
    /// Disable an account (holds it out of rotation).
    Disable(DisableArgs),
    /// Probe every account's live quota and print the fleet status.
    Status(StatusArgs),
    /// Self-update: `git pull --ff-only` + `cargo build --release` in the checkout.
    Update(UpdateArgs),
    /// Render the TUI against fake accounts (for a sanitized README screenshot).
    Demo,
    /// Open TcrBar, the macOS menu-bar app (macOS only).
    Ui,
}

#[derive(clap::Args)]
struct AccountsArgs {
    /// Path to the config file (default: ~/.config/teamclaude.json).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Refresh each account's live quota before listing (network probe).
    #[arg(long)]
    probe: bool,
}

#[derive(clap::Args)]
struct RemoveArgs {
    /// Path to the config file (default: ~/.config/teamclaude.json).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Account name, or its bare email if the name carries an org suffix. Exact
    /// and case-sensitive — not a substring.
    query: String,
    /// Narrow an ambiguous match to a single org (name or uuid).
    #[arg(long)]
    org: Option<String>,
}

#[derive(clap::Args)]
struct PriorityArgs {
    /// Path to the config file (default: ~/.config/teamclaude.json).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Account name, or its bare email if the name carries an org suffix. Exact
    /// and case-sensitive — not a substring.
    query: String,
    /// The explicit priority value (lower = preferred). Omit with --first/--last.
    #[arg(conflicts_with_all = ["first", "last"])]
    value: Option<i64>,
    /// Move the account to the front of rotation (min priority - 1).
    #[arg(long, conflicts_with = "last")]
    first: bool,
    /// Move the account to the back of rotation (max priority + 1).
    #[arg(long)]
    last: bool,
    /// Narrow an ambiguous match to a single org (name or uuid).
    #[arg(long)]
    org: Option<String>,
}

#[derive(clap::Args)]
struct EnableArgs {
    /// Path to the config file (default: ~/.config/teamclaude.json).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Account name, or its bare email if the name carries an org suffix. Exact
    /// and case-sensitive — not a substring.
    query: String,
    /// Narrow an ambiguous match to a single org (name or uuid).
    #[arg(long)]
    org: Option<String>,
}

#[derive(clap::Args)]
struct DisableArgs {
    /// Path to the config file (default: ~/.config/teamclaude.json).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Account name, or its bare email if the name carries an org suffix. Exact
    /// and case-sensitive — not a substring.
    query: String,
    /// Narrow an ambiguous match to a single org (name or uuid).
    #[arg(long)]
    org: Option<String>,
}

#[derive(clap::Args)]
struct StatusArgs {
    /// Path to the config file (default: ~/.config/teamclaude.json).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Emit the fleet status as a JSON array instead of greppable text.
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
struct UpdateArgs {
    /// Rebuild even when `git pull` reports the checkout is already up to date.
    #[arg(long)]
    force: bool,
}

#[derive(clap::Args)]
struct LoginArgs {
    /// Path to the config file (default: ~/.config/teamclaude.json).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Skip the refusal a running proxy would otherwise trigger, and write the
    /// login to the config file directly. Still probes the proxy first and still
    /// prefers its live account-add route when one is confirmed and safe — this
    /// only overrides the cases login would otherwise refuse or error on (an
    /// older proxy with no route, one that answered unusably, or a rejected
    /// api-key). Unsafe when it takes the file path: the running server's next
    /// token refresh can overwrite what was just written.
    #[arg(long)]
    force: bool,
}

#[derive(clap::Args)]
struct RunArgs {
    /// Path to the config file (default: ~/.config/teamclaude.json).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Args passed verbatim to `claude` (e.g. `tcr run -- -p "hi"`).
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

#[derive(clap::Args)]
struct ServerArgs {
    /// Port to bind (overrides `proxy.port` from the config).
    #[arg(long)]
    port: Option<u16>,
    /// Path to the config file (default: ~/.config/teamclaude.json).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Run without the TUI, logging to stdout.
    #[arg(long)]
    headless: bool,
    /// Take over the port: kill a proxy already listening on it, then bind. The
    /// default is to leave a healthy incumbent alone and exit — replacing it wipes
    /// its session→account pin map and cold-starts every live session's prompt
    /// cache, which is the most expensive event in this system.
    #[arg(long)]
    replace: bool,
    /// DEPRECATED and now a no-op: this is the default. Kept accepted so existing
    /// scripts and launch agents that pass it keep working. Pass `--replace` for
    /// the old default (take the port over).
    ///
    /// `conflicts_with` rather than a silent precedence rule: the two flags are a
    /// contradiction, and the previous wiring resolved it by quietly discarding
    /// `--replace`. An operator whose launchd plist or shell alias already carries
    /// `--no-replace`, adding `--replace` to force a rebuilt binary onto the port,
    /// got a stand-down and exit 0 — while `--help` told them the flag they left
    /// in place does nothing. clap now rejects the pair by name, which is the only
    /// outcome that cannot be misread.
    #[arg(long, conflicts_with = "replace")]
    no_replace: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Server(args)) => run_server(args).await,
        Some(Command::Run(args)) => run_claude(args),
        Some(Command::Login(args)) => run_login(args).await,
        Some(Command::Accounts(args)) => run_accounts(args).await,
        Some(Command::Remove(args)) => run_remove(args),
        Some(Command::Priority(args)) => run_priority(args),
        Some(Command::Enable(args)) => run_enable(args).await,
        Some(Command::Disable(args)) => run_disable(args).await,
        Some(Command::Status(args)) => run_status(args).await,
        Some(Command::Update(args)) => update::run_update(args.force),
        Some(Command::Demo) => demo::run_demo().await.map_err(anyhow::Error::from),
        Some(Command::Ui) => run_ui(),
        None => run_server(cli.server).await,
    }
}

/// `tcr accounts [--probe]` — list the configured accounts (offline).
async fn run_accounts(args: AccountsArgs) -> anyhow::Result<()> {
    let config_path = args.config.unwrap_or_else(config::default_path);
    cli::list_accounts(&config_path, args.probe).await
}

/// `tcr remove <query> [--org]` — delete an account from the config.
fn run_remove(args: RemoveArgs) -> anyhow::Result<()> {
    let config_path = args.config.unwrap_or_else(config::default_path);
    cli::remove_account(&config_path, &args.query, args.org.as_deref())
}

/// `tcr priority <query> [N|--first|--last] [--org]` — set rotation priority.
fn run_priority(args: PriorityArgs) -> anyhow::Result<()> {
    let config_path = args.config.unwrap_or_else(config::default_path);
    let priority = if args.first {
        PriorityArg::First
    } else if args.last {
        PriorityArg::Last
    } else if let Some(n) = args.value {
        PriorityArg::N(n)
    } else {
        anyhow::bail!("provide a priority value, or one of --first / --last");
    };
    cli::set_priority(&config_path, &args.query, priority, args.org.as_deref())
}

/// `tcr enable <query> [--org]` — clear an account's `disabled` flag, in the
/// RUNNING proxy where there is one (async for that reason alone).
async fn run_enable(args: EnableArgs) -> anyhow::Result<()> {
    let config_path = args.config.unwrap_or_else(config::default_path);
    cli::set_enabled(&config_path, &args.query, args.org.as_deref(), false).await
}

/// `tcr disable <query> [--org]` — hold an account out of rotation, in the RUNNING
/// proxy where there is one.
async fn run_disable(args: DisableArgs) -> anyhow::Result<()> {
    let config_path = args.config.unwrap_or_else(config::default_path);
    cli::set_enabled(&config_path, &args.query, args.org.as_deref(), true).await
}

/// `tcr status [--json]` — probe every account's live quota and print it.
async fn run_status(args: StatusArgs) -> anyhow::Result<()> {
    let config_path = args.config.unwrap_or_else(config::default_path);
    cli::status(&config_path, args.json).await
}

/// `tcr ui` — open TcrBar, the macOS menu-bar app.
///
/// This exists for discoverability, not capability: `open -a TcrBar` already
/// works. Without it nothing in `tcr --help` reveals that a UI exists at all, so
/// the app is only findable by knowing it is there.
///
/// It deliberately does NOT build the app or know where the checkout is. It asks
/// LaunchServices to open a bundle id, which resolves wherever the app was
/// installed. A `tcr` that shells into a source tree would break the moment the
/// checkout moved.
#[cfg(target_os = "macos")]
fn run_ui() -> anyhow::Result<()> {
    use anyhow::Context;

    let status = std::process::Command::new("open")
        .args(["-b", "io.github.dhkts1.tcrbar"])
        .status()
        .context("failed to run `open`")?;

    if status.success() {
        return Ok(());
    }

    // `open -b` fails when the bundle id is not registered, which almost always
    // means "not installed" rather than "broken". Say which, and say how to fix
    // it, rather than surfacing LaunchServices' own opaque exit code.
    anyhow::bail!(
        "TcrBar is not installed. Build and install it with:\n    \
         bash apps/macos/scripts/install.sh"
    )
}

/// Non-macOS builds keep the subcommand so `--help` is identical everywhere, and
/// fail with the reason rather than a missing-subcommand error.
#[cfg(not(target_os = "macos"))]
fn run_ui() -> anyhow::Result<()> {
    anyhow::bail!("`tcr ui` opens the macOS menu-bar app, and this is not macOS.")
}

/// `tcr run [-- args…]` — launch Claude Code already pointed at this proxy.
///
/// Mirrors the JS `teamclaude run` passthrough contract: if the proxy is not
/// listening we launch `claude` untouched, so a stopped proxy never breaks the
/// shell alias.
fn run_claude(args: RunArgs) -> anyhow::Result<()> {
    let config_path = args.config.clone().unwrap_or_else(config::default_path);
    let (config, _) = load_config(&config_path);
    let port = config.proxy.port;

    let mut cmd = std::process::Command::new("claude");
    cmd.args(&args.args);

    if cli::proxy_is_up(port) {
        if let Some(notice) = withheld_api_key_notice(
            config.proxy.api_key.is_some(),
            std::env::var_os("ANTHROPIC_API_KEY").is_some(),
        ) {
            eprintln!("{notice}");
        }
        // Two ways to route claude at ourselves, and they are NOT equivalent to
        // Claude Code — see `apply_see_through_env` for why we prefer the first.
        // Anything missing from the MITM material lands us in base-URL mode, which
        // always works; there is no half-applied third state.
        match see_through_ca() {
            Some(ca) => apply_see_through_env(&mut cmd, port, &ca),
            None => apply_base_url_env(&mut cmd, port),
        }
    } else {
        eprintln!("[tcr] proxy not listening on :{port} — launching claude directly");
    }

    let status = cmd
        .status()
        .context("failed to launch `claude` — is it on PATH?")?;
    std::process::exit(status.code().unwrap_or(1));
}

/// The CA to advertise for see-through mode, or `None` when we must fall back to
/// base-URL mode. Prints the reason on every `None` — a silent downgrade would
/// look exactly like a working see-through session while the capabilities it
/// exists to preserve stay off.
///
/// See-through needs BOTH halves of the MITM contract: the proxy must be able to
/// present a leaf for `api.anthropic.com` (so `mitm::load_tls` has to succeed)
/// AND we must be able to name the CA that signed it (so `claude` can be told to
/// trust it). `load_tls` is the same loader the server ran at boot against the
/// same dir, so it resolves to the same material rather than a second opinion.
fn see_through_ca() -> Option<PathBuf> {
    match mitm::load_tls() {
        Ok(assets) => match assets.ca_path {
            Some(ca) if ca.is_file() => Some(ca),
            // A path we cannot read is not a CA we can hand to claude.
            Some(ca) => {
                eprintln!(
                    "[tcr] see-through off: CA {} is not a readable file",
                    ca.display()
                );
                None
            }
            None => {
                eprintln!("[tcr] see-through off: no CA on disk for the MITM leaf we present");
                None
            }
        },
        Err(err) => {
            eprintln!("[tcr] see-through off: MITM TLS material unavailable ({err})");
            None
        }
    }
}

/// Why `tcr run` does NOT hand the configured proxy key to `claude` as
/// `ANTHROPIC_API_KEY`, and says so.
///
/// It used to. Setting that variable makes Claude Code treat an API key as its auth
/// source AHEAD of the claude.ai login, and that **disables every claude.ai
/// connector** — announced once, in one startup line that scrolls away, after which
/// the tools are simply absent. It bought nothing in exchange: the `x-api-key` gate
/// exempts loopback clients (see `proxy::handle`), and the server binds 127.0.0.1
/// only, so a `tcr run` child is always exempt. Measured with the variable absent:
/// `/v1/messages` served and rotated across accounts, and every connector loaded.
///
/// A value the CALLER exported is inherited untouched — an explicit choice wins, and
/// it is the escape hatch for a `claude` with no claude.ai login of its own, which
/// does need some credential to start.
fn withheld_api_key_notice(configured: bool, caller_set: bool) -> Option<&'static str> {
    (configured && !caller_set).then_some(
        "[tcr] not exporting ANTHROPIC_API_KEY from proxy.apiKey: it would outrank claude's \
         claude.ai login and disable every claude.ai connector. The proxy does not need it — \
         its api-key gate exempts loopback clients. Export it yourself if this `claude` has no \
         claude.ai login of its own.",
    )
}

/// SEE-THROUGH mode — the preferred route. `claude` keeps the REAL first-party
/// base URL and reaches us as a CONNECT proxy instead, so we still see (and
/// rotate) every request while Claude Code's first-party check keeps passing.
///
/// That check is a pure string compare on `ANTHROPIC_BASE_URL` — no DNS, no
/// socket, no certificate inspection, just `new URL(e).host === "api.anthropic.com"`.
/// So the fix is to stop lying to it: leave the base URL alone and move the
/// interception down a layer to `HTTPS_PROXY`, where tcr's CONNECT handler
/// MITM-terminates `api.anthropic.com` with a leaf `NODE_EXTRA_CA_CERTS` makes
/// node trust.
///
/// The proxy vars are set in BOTH cases deliberately: clients disagree about
/// which spelling they read, and one that reads only the one we skipped would go
/// direct — bypassing rotation entirely, silently.
fn apply_see_through_env(cmd: &mut std::process::Command, port: u16, ca: &Path) {
    let proxy = format!("http://127.0.0.1:{port}");
    cmd.env("ANTHROPIC_BASE_URL", "https://api.anthropic.com");
    cmd.env("HTTPS_PROXY", &proxy);
    cmd.env("https_proxy", &proxy);
    cmd.env("NODE_EXTRA_CA_CERTS", ca);
    // Unnecessary here — we ARE first-party in this mode, so nothing gates them
    // off — but harmless, and they keep the session whole if it ever falls back.
    apply_capability_defaults(cmd);
    eprintln!(
        "[tcr] see-through mode: claude keeps https://api.anthropic.com, tunnelling via {proxy}"
    );
    eprintln!(
        "[tcr] trusting our MITM leaf via NODE_EXTRA_CA_CERTS={}",
        ca.display()
    );
}

/// BASE-URL mode — the fallback, used when see-through material is unavailable.
/// `claude` talks plain HTTP to us on loopback, which costs first-party status
/// and everything gated on it (hence [`apply_capability_defaults`]).
fn apply_base_url_env(cmd: &mut std::process::Command, port: u16) {
    cmd.env("ANTHROPIC_BASE_URL", format!("http://127.0.0.1:{port}"));
    // Here we speak plain HTTP on loopback and present NO leaf. An ambient
    // HTTPS_PROXY (e.g. the JS teamclaude on :3456) would hijack claude's traffic
    // away from us, and its NODE_EXTRA_CA_CERTS would be verifying a cert we never
    // send. Strip both so `tcr run` is self-contained and can't be captured by a
    // stale env. See-through mode does the opposite — it SETS these two, which is
    // precisely why the strip cannot live at the branch above.
    for var in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "NODE_EXTRA_CA_CERTS",
    ] {
        cmd.env_remove(var);
    }
    apply_capability_defaults(cmd);
    eprintln!("[tcr] base-URL mode: routing claude through http://127.0.0.1:{port}");
}

/// Re-enable the capabilities a non-first-party `ANTHROPIC_BASE_URL` silently
/// switches off.
///
/// Claude Code gates tool search, the stall watchdog, and fine-grained tool
/// streaming on `xn()==="firstParty" && Yd()`, and base-URL mode fails that check
/// -- no error, at most a [DEBUG] line. We caused it, so we carry the
/// compensation. Never applied with the proxy down: we launch claude untouched
/// there and genuinely are first-party.
///
/// Measured 2026-07-29 against Claude Code 2.1.220 (gate at bundled-JS abs offset
/// 230310702): 1 of 4 same-day sessions lost tool search outright -- the one that
/// reached the gate ~30ms sooner, before settings.json's env block was applied to
/// process.env. Setting these here puts them in the child env at EXEC time, so
/// that ordering race cannot occur at all.
///
/// Only set what the user has not already chosen; an explicit value always wins.
fn apply_capability_defaults(cmd: &mut std::process::Command) {
    for (var, val) in [
        // Without this ~130 tool schemas load eagerly every request. Requires that we
        // forward `tool_reference` blocks upstream untouched -- we do, since
        // build_upstream_headers uses a denylist rather than an allowlist.
        ("ENABLE_TOOL_SEARCH", "true"),
        // Stall detection on the response stream; without it a hung response is never
        // proactively aborted.
        ("CLAUDE_ENABLE_BYTE_WATCHDOG", "1"),
        // Incremental tool-input streaming rather than batched delivery.
        ("CLAUDE_CODE_ENABLE_FINE_GRAINED_TOOL_STREAMING", "true"),
    ] {
        if std::env::var_os(var).is_none() {
            cmd.env(var, val);
        }
    }
}

/// `tcr login` — browser OAuth PKCE flow that authenticates a Claude account
/// and appends (or updates) it in the drop-in config. The heavy lifting lives
/// in [`oauth::login`]; this just resolves the config path and reports.
async fn run_login(args: LoginArgs) -> anyhow::Result<()> {
    let config_path = args.config.clone().unwrap_or_else(config::default_path);
    let name = oauth::login(&config_path, args.force)
        .await
        .context("OAuth login failed")?;
    println!("Logged in as '{name}'.");
    Ok(())
}

/// A stand-down that resolved cleanly: a peer proxy holds the port and is
/// serving this binary's code (or something we have no reason to doubt).
const EXIT_STOOD_DOWN_OK: i32 = 0;
/// A stand-down where the incumbent is serving a DIFFERENT commit than the
/// binary that was just run.
///
/// `cargo build && tcr` used to GUARANTEE the new binary was serving; standing
/// down silently broke that guarantee, and a warning on stderr is routinely
/// unread in a headless or piped context. This is the machine-readable half, so
/// `tcr && <next step>` stops instead of proceeding as if the new build were
/// live. Not `1`: that is a genuine startup failure, and not `2`, which clap
/// uses for a usage error.
const EXIT_STOOD_DOWN_STALE: i32 = 3;
/// A stand-down where the incumbent never answered the liveness probe — it holds
/// the listening socket and serves nothing. Distinct from [`EXIT_STOOD_DOWN_STALE`]
/// because the operator's next command is different: `--replace` is a recovery
/// here, not an upgrade.
const EXIT_STOOD_DOWN_NOT_ANSWERING: i32 = 4;

/// The stand-down's exit code, as a pure function of what was actually measured.
///
/// Keyed on the probe's verdict values, never on the rendered sentence: an exit
/// code grepped out of our own prose is a gate any rewording silently disarms.
///
/// Liveness outranks build skew because it is the more urgent fact — nothing is
/// serving at all — and because a proxy that would not answer also could not
/// report a build, so its build verdict is `Unknown` by construction.
fn stand_down_exit_code(liveness: &cli::Liveness, verdict: build_info::StandDownBuild) -> i32 {
    if matches!(liveness, cli::Liveness::Silent { .. }) {
        return EXIT_STOOD_DOWN_NOT_ANSWERING;
    }
    match verdict {
        build_info::StandDownBuild::Stale => EXIT_STOOD_DOWN_STALE,
        // `Unknown` stays 0: an older tcr that answers but ships no build stamp
        // is a working proxy, and failing every such start would be noise for a
        // question that was never answered either way.
        build_info::StandDownBuild::InSync
        | build_info::StandDownBuild::DirtyBuild
        | build_info::StandDownBuild::Unknown => EXIT_STOOD_DOWN_OK,
    }
}

/// `tcr server` — the clap→[`server::ServeOptions`] adapter.
///
/// Everything that actually boots the proxy lives in [`teamclaude_rs::server`],
/// which is usable from a test or any other embedder. What stays here is what
/// only a *binary* may do: choose the logging subscriber, print the operator's
/// stand-down diagnosis, turn that stand-down into a process exit code, and pick
/// how to wait (the TUI, or a headless block on Ctrl-C).
async fn run_server(args: ServerArgs) -> anyhow::Result<()> {
    let config_path = args.config.clone().unwrap_or_else(config::default_path);
    let (config, persist_path) = load_config(&config_path);

    init_tracing(args.headless);

    // Spelled out rather than built from `ServeOptions::new`, which is
    // deliberately inert: writing the config back, owning the shared pin cache
    // and signalling whatever holds the port are all things only the BINARY may
    // do, so the binary is the place they are written down.
    let options = server::ServeOptions {
        config,
        persist_path,
        port: args.port,
        incumbent: if args.replace {
            server::IncumbentPolicy::kill_the_incumbent_proxy()
        } else {
            server::IncumbentPolicy::replace_legacy_js_only()
        },
        affinity_path: Some(affinity::default_path()),
        tls: server::TlsSetup::Load,
        // This is a standalone `tcr` process, stated rather than sniffed from
        // `argv[0]`: the owner file is what makes a proxy identifiable when its
        // process name is NOT `tcr` (see `teamclaude_rs::singleton`), so the value
        // has to come from the caller that knows.
        host: singleton::ProxyHost::Cli,
        // Claim the port for the next `tcr` (and for `tcr login`) to read. A
        // binary-only side effect, like the pin cache above: it is a shared
        // directory, so a library caller must opt in with its own.
        //
        // The DIRECTORY, not a path: `serve` names the file after the port it
        // actually binds. Re-deriving `--port unwrap_or config.proxy.port` here to
        // build the name would be a second copy of a resolution rule that lives in
        // `serve`, and the two silently disagreeing means a claim named for a port
        // this process never bound — which every reader looks straight past.
        owner_dir: Some(singleton::default_owner_dir()),
    };

    let handle = match server::serve(options).await? {
        server::ServeOutcome::Started(handle) => handle,
        // A binary may exit; the library returned this as a value.
        server::ServeOutcome::StoodDown(stand_down) => stand_down_exit(&stand_down),
    };
    let bound = handle.addr();

    let mut handle = handle;
    if args.headless {
        tracing::info!("teamclaude-rs listening on http://{bound} (headless)");
        // Block until Ctrl-C or the server task exits.
        tokio::select! {
            _ = tokio::signal::ctrl_c() => tracing::info!("shutdown signal received"),
            () = handle.serving_stopped() => {}
        }
    } else {
        // The TUI owns the foreground. Under raw mode Ctrl-C arrives as a keystroke,
        // so the loop (not a signal) handles it. But an EXTERNAL SIGTERM — e.g. the
        // singleton replacing us on the port — would otherwise terminate the process
        // with the terminal still in raw + alternate-screen mode, wrecking the
        // caller's shell. Race the TUI against SIGTERM: on the signal the `select`
        // drops the TUI future, and dropping it runs `TerminalGuard`'s destructor,
        // which restores the terminal (the same restore path as a clean quit or a
        // panic) BEFORE the shutdown/flush below and any SIGKILL fallback.
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate()).ok();
        if sigterm.is_none() {
            tracing::warn!("could not install SIGTERM handler; terminal may not restore if killed");
        }
        let tui_fut = tui::run(handle.manager().clone());
        tokio::pin!(tui_fut);
        tokio::select! {
            res = &mut tui_fut => {
                if let Err(err) = res {
                    tracing::error!(error = %err, "tui error");
                }
            }
            _ = async {
                match sigterm.as_mut() {
                    Some(sig) => {
                        sig.recv().await;
                    }
                    None => std::future::pending::<()>().await,
                }
            } => {
                tracing::info!("SIGTERM received; restoring terminal and shutting down");
            }
        }
    }

    // Stop serving, then flush the config and the affinity pins. The whole
    // sequence — including the final pin write a clean shutdown owes the next
    // boot — lives in `ServerHandle::shutdown`, so an embedder gets it too.
    // Bounded there, so a wedged filesystem cannot turn quitting the TUI into a
    // hang with the terminal already restored and nothing left serving.
    let report = handle.shutdown().await;
    if report.tasks_aborted > 0 {
        tracing::warn!(
            aborted = report.tasks_aborted,
            "background task(s) did not stop within the shutdown grace and were aborted"
        );
    }
    Ok(())
}

/// How to recover from a WEDGED incumbent — the half of the not-answering warning
/// that depends on which proxy is holding the port.
///
/// `--replace` is the recovery for a proxy this process may signal. It is not one
/// for a [`singleton::ProxyKind::TcrEmbedded`] incumbent, and offering it there is
/// worse than offering nothing: `takeover_decision` refuses that kind on every
/// path, so the operator runs the suggested command, sees the same stand-down, and
/// the advice that DOES work — quitting the host application — was never printed.
/// An instruction that cannot succeed is a bug even when the code behind it is
/// correct.
fn wedged_incumbent_recovery(kind: singleton::ProxyKind) -> &'static str {
    match kind {
        singleton::ProxyKind::TcrEmbedded => {
            "`tcr --replace` cannot take this one over: the pid belongs to the host application \
             serving the proxy in-process, and signalling it would kill the app without its \
             normal shutdown, losing the session→account pin map. Quit the host application and \
             start it again to recover a wedged embedded proxy."
        }
        singleton::ProxyKind::Tcr | singleton::ProxyKind::LegacyJs => {
            "Run `tcr --replace` to take the port over; that is the recovery for a wedged proxy, \
             and it is not being done automatically because it also wipes the pin map of a proxy \
             that was merely slow to answer."
        }
    }
}

/// Print the stand-down diagnosis and exit with the code it earned. Never returns.
///
/// This is the half of the stand-down a *library* must not do, which is why
/// [`server::serve`] hands the facts back as a [`server::StandDown`] instead.
/// The wording is a cross-language contract: TcrBar scans this stderr, and
/// `ServerController.StandDownExit` switches on the code.
fn stand_down_exit(stand_down: &server::StandDown) -> ! {
    // Standing down is cheap and correct, but silent success here would mean
    // `cargo build && tcr` exits 0 with the OLD build still serving — say which
    // build actually holds the port before we go.
    eprintln!("{}", stand_down.report.line);
    if let cli::Liveness::Silent { why } = &stand_down.probe.liveness {
        let port = stand_down.port;
        let pid = stand_down.pid;
        let recovery = wedged_incumbent_recovery(stand_down.kind);
        eprintln!(
            "[tcr] WARNING incumbent-not-answering: port={port} pid={pid} probe={why:?} — the \
             process holding :{port} did not respond, so standing down leaves NOTHING serving \
             on it. {recovery}"
        );
    }
    std::process::exit(stand_down_exit_code(
        &stand_down.probe.liveness,
        stand_down.report.verdict,
    ));
}

/// Load the config, deciding what may be written back:
/// - missing file → in-memory defaults, keep the path so the first refresh
///   creates it;
/// - corrupt/unreadable existing file → fail loudly and fall back to in-memory
///   defaults, but DROP the persist path so the user's file is never clobbered
///   with defaults — it can be fixed by hand and the proxy restarted.
fn load_config(path: &Path) -> (Config, Option<PathBuf>) {
    match config::load(path) {
        Ok(config) => (config, Some(path.to_path_buf())),
        Err(ConfigError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
            eprintln!(
                "[tcr] no config at {} — starting with defaults",
                path.display()
            );
            (default_config(), Some(path.to_path_buf()))
        }
        Err(err) => {
            eprintln!(
                "[tcr] config at {} is unreadable/corrupt: {err}",
                path.display()
            );
            eprintln!(
                "[tcr] starting with in-memory defaults; the file will NOT be overwritten — fix it and restart"
            );
            (default_config(), None)
        }
    }
}

/// A default config with every serde default applied (correct `upstream` and
/// `switchThreshold`, empty accounts) — parsing `{}` reuses the config's own
/// `#[serde(default)]` wiring instead of a hand-rolled `Default`.
fn default_config() -> Config {
    serde_json::from_str("{}").expect("an empty JSON object is always a valid default config")
}

/// Base cache directory: `$XDG_CACHE_HOME/teamclaude`, else `$HOME/.cache/teamclaude`.
///
/// Deliberately independent of [`affinity::default_path`] (same env-var
/// resolution order, duplicated rather than shared) so that a bug or a test
/// touching the log path can never brush the live session-affinity pin file's
/// neighbourhood by construction — the two are computed by different code, not
/// just used at different leaf paths under a shared helper.
fn cache_base_dir() -> std::path::PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .map(|home| home.join(".cache"))
        })
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("teamclaude")
}

/// The one shared, well-known, non-private directory both modes log into:
/// `~/.cache/teamclaude/logs/` (or `$XDG_CACHE_HOME/teamclaude/logs/`).
///
/// The path is fixed for **human discoverability** — so `rg 'server started'
/// ~/.cache/teamclaude/logs/*` always finds the right directory — not because
/// any code depends on the literal string. Nothing in this codebase opens this
/// path programmatically outside this module; see `open_log_appender` below and
/// the doc-comment on `the_log_directory_is_the_shared_cache_location` for the
/// census that established that.
fn log_dir_path() -> std::path::PathBuf {
    cache_base_dir().join("logs")
}

/// Open the durable, rotating log directory, injectable for tests so they never
/// touch the real `~/.cache/teamclaude/logs/` — pass a unique temp dir instead
/// of routing through [`log_dir_path`].
///
/// `tracing_appender::rolling` has no per-file `.mode()` hook (checked against
/// its 0.2.5 source: `create_writer` in its `rolling.rs` opens with
/// `OpenOptions::append(true).create(true)` only), so a rotating file cannot be
/// made owner-only the way the old single file was. Confidentiality is enforced
/// on the *directory* instead, and it is load-bearing on Linux CI targets, not
/// belt-and-braces: the parent `~/.cache/teamclaude/` is `0755` (protects its
/// existing contents by *file* mode — `session-affinity.json` is `0600`), so
/// without an owner-only `logs/` subdirectory, files landing at the crate's
/// default `0644` would be world-readable on any multi-user box. The mode is
/// requested atomically at directory-creation time via `DirBuilder::mode`, not
/// create-then-`chmod` — the latter leaves a TOCTOU window where the directory
/// is briefly world-traversable while it starts to hold sensitive log content.
///
/// The 0700 mode is applied at directory creation and re-asserted once at
/// process startup (below). It is **not** a standing invariant:
/// `tracing_appender`'s own fallback (`rolling.rs:795`) recreates the
/// directory with `create_dir_all` and no mode if it is removed externally —
/// at construction and at every rotation — so a `logs/` deleted mid-run
/// silently returns at 0755 for the life of the process. This function closes
/// the pre-existing-directory case (a stale `mkdir -p`, a `tar` restore
/// without `-p`, a baked container path); it cannot reach that crate-internal
/// recreation path, which is not this function's to fix.
///
/// The mode is checked on the **resolved** target, not the path itself:
/// `std::fs::metadata`/`set_permissions` both follow symlinks, and
/// `DirBuilder` succeeds against an existing symlink-to-directory, so a
/// symlinked `logs/` is validated by where it points, not by the link. That
/// is deliberate, not an oversight: pointing the log directory at another
/// volume is a legitimate operator setup, and refusing to start over a
/// symlink would break a working configuration to close a hole that is not a
/// privilege boundary anyway — planting such a symlink first requires write
/// access to the 0755, user-owned `~/.cache/teamclaude/` parent, i.e. the
/// user or root, who already has more reach than this would buy them.
///
/// `.recursive(true)` creates every missing path component, parents included,
/// **carrying the same `0700` mode** — this is not confined to `logs/` itself.
/// On a fresh install or in a container where `~/.cache/teamclaude/` (or even
/// `~/.cache/`) does not yet exist, this call creates it at `0700`, changing a
/// directory shared with every other application on the machine. That is
/// invisible on a dev box where both already exist at `0755`, but it is real,
/// measured behaviour of `DirBuilder::recursive`, not a hypothetical.
///
/// Rotation is `DAILY` with `max_log_files(5)`: measured against the live log
/// (2026-08-08) at ~13.5 MiB/day, this bounds this directory's steady-state
/// disk use to roughly 65-70 MiB instead of genuinely unbounded growth. That
/// is **not** a wash against the old file: the pre-upgrade
/// `$TMPDIR/teamclaude-rs.log` (~62 MiB, measured 2026-08-08) is left in
/// place deliberately (deprecate, don't delete — nothing here removes an
/// operator's existing evidence), so the true post-upgrade footprint is that
/// ~62 MiB orphan **plus** the new 65-70 MiB, until an operator cleans the
/// orphan up by hand. It is a wall-clock bound, not a byte-size bound: a single unusually
/// verbose day can still exceed the per-file average before the next
/// rotation, so this is a soft cap, not a hard one. Rotation also rolls on
/// `Rotation::DAILY`'s UTC clock (`rolling.rs` uses `OffsetDateTime::now_utc`),
/// not local midnight — for the stated goal of human discoverability, that
/// means the daily boundary lands at 03:00 in Asia/Jerusalem, not midnight.
fn open_log_appender(
    log_dir: &std::path::Path,
) -> std::io::Result<tracing_appender::rolling::RollingFileAppender> {
    use std::os::unix::fs::DirBuilderExt as _;
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(log_dir)?;

    // `DirBuilder::mode` only governs directories it *creates* — a directory
    // that already existed (operator `mkdir -p`, a `tar` restore without
    // `-p`, a baked container path) is untouched by the call above and could
    // be sitting at 0755 or worse. Re-assert here so every process start
    // closes that window, not just a genuinely-fresh directory.
    let mode = std::fs::metadata(log_dir)?.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        std::fs::set_permissions(log_dir, std::fs::Permissions::from_mode(0o700))?;
        let confirmed = std::fs::metadata(log_dir)?.permissions().mode() & 0o777;
        if confirmed & 0o077 != 0 {
            return Err(std::io::Error::other(format!(
                "log directory {} is not owner-only after chmod (mode {confirmed:#o}); \
                 refusing to log account emails into a world-readable directory",
                log_dir.display()
            )));
        }
    }

    tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("teamclaude-rs.log")
        .max_log_files(5)
        .build(log_dir)
        .map_err(std::io::Error::other)
}

/// Build the headless subscriber: every event goes to **both** `stdout_sink` and
/// (when it could be opened) the durable log file.
///
/// Headless used to be stdout-only, which meant its logs were destroyed outright
/// whenever a supervisor discarded the child's stdout — as TcrBar does
/// (`ServerController.swift`, `standardOutput = FileHandle.nullDevice`, and older
/// builds hand it an undrained pipe). A crashing proxy then left no evidence of
/// why. Stdout is kept as well, because someone running `tcr server --headless`
/// in a terminal expects output, and launchd-style supervisors capture it.
///
/// `file` is an `Option` on purpose: a logging failure must never take the proxy
/// down, so a log that will not open degrades to stdout-only.
///
/// `RollingFileAppender` is used directly as the writer, never through
/// `tracing_appender::non_blocking()`. `non_blocking()` returns a `WorkerGuard`
/// that must live as long as logging should happen — drop it (as this
/// function's `()` return type would force, if it owned one) and the
/// background writer thread shuts down with no error and no warning: every
/// event after that silently stops reaching disk while every gate stays green.
/// `RollingFileAppender` itself implements `Write`/`MakeWriter` synchronously,
/// so it needs no guard and no lifetime plumbing.
fn headless_subscriber<W>(
    filter: tracing_subscriber::EnvFilter,
    file: Option<tracing_appender::rolling::RollingFileAppender>,
    stdout_sink: W,
) -> impl tracing::Subscriber + Send + Sync
where
    W: for<'w> tracing_subscriber::fmt::MakeWriter<'w> + Send + Sync + 'static,
{
    use tracing_subscriber::layer::SubscriberExt as _;
    // The file sink is non-ANSI: escape codes in a log read back as garbage.
    let file_layer = file.map(|file| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
    });
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(stdout_sink))
        .with(file_layer)
}

/// Initialise tracing. Headless logs to stdout *and* the durable log file; the
/// TUI logs only to the file, so events never corrupt the alternate screen.
/// Either way, a log file that will not open is a warning, never a failure.
fn init_tracing(headless: bool) {
    use tracing_subscriber::util::SubscriberInitExt as _;
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let log_dir = log_dir_path();
    let file = match open_log_appender(&log_dir) {
        Ok(file) => Some(file),
        Err(err) => {
            eprintln!(
                "[tcr] could not open log directory {}: {err}",
                log_dir.display()
            );
            None
        }
    };
    if headless {
        headless_subscriber(filter, file, std::io::stdout).init();
        return;
    }
    // No file? Already warned above. The TUI cannot fall back to stdout without
    // corrupting the alternate screen, so it runs without tracing rather than
    // refusing to start.
    if let Some(file) = file {
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_env_filter(filter)
            .with_writer(std::sync::Mutex::new(file))
            .init();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use build_info::StandDownBuild;
    use std::os::unix::fs::PermissionsExt as _;

    fn silent() -> cli::Liveness {
        cli::Liveness::Silent {
            why: "the server did not answer within 5s".to_string(),
        }
    }

    /// Pins that a configured proxy key is withheld from `claude`, and says why.
    #[test]
    fn the_proxy_key_is_withheld_from_claude_with_the_reason() {
        let notice = withheld_api_key_notice(true, false).expect("a configured key is withheld");
        assert!(
            notice.contains("connector"),
            "the notice must name what exporting it costs: {notice}"
        );
        assert!(
            notice.contains("Export it yourself"),
            "the notice must name the escape hatch: {notice}"
        );

        assert_eq!(
            withheld_api_key_notice(false, false),
            None,
            "nothing is withheld when no proxy key is configured"
        );
        assert_eq!(
            withheld_api_key_notice(true, true),
            None,
            "a key the caller exported is their choice — we neither replace it nor comment"
        );
    }

    /// Neither routing mode may hand `claude` an `ANTHROPIC_API_KEY`.
    #[test]
    fn no_routing_mode_gives_claude_an_api_key() {
        for (label, apply) in [
            (
                "see-through",
                &(|cmd: &mut std::process::Command| {
                    apply_see_through_env(cmd, 3456, Path::new("/tmp/ca.pem"))
                }) as &dyn Fn(&mut std::process::Command),
            ),
            (
                "base-URL",
                &(|cmd: &mut std::process::Command| apply_base_url_env(cmd, 3456)),
            ),
        ] {
            let mut cmd = std::process::Command::new("claude");
            apply(&mut cmd);
            assert!(
                !cmd.get_envs()
                    .any(|(k, v)| k == "ANTHROPIC_API_KEY" && v.is_some()),
                "{label} mode must not set ANTHROPIC_API_KEY — it outranks the claude.ai \
                 login and disables every claude.ai connector"
            );
        }
    }

    /// A WEDGED INCUMBENT MUST NOT BE OFFERED A RECOVERY THAT CANNOT WORK.
    ///
    /// `--replace` is refused for an embedded incumbent on every path in
    /// `singleton::takeover_decision`, deliberately: the pid is the host
    /// application's. Printing "Run `tcr --replace`" for that kind sends the
    /// operator around a loop that ends in the same stand-down, while the
    /// instruction that does work — quit the host application — never appears.
    #[test]
    fn a_wedged_embedded_incumbent_is_not_told_to_run_replace() {
        let embedded = wedged_incumbent_recovery(singleton::ProxyKind::TcrEmbedded);
        assert!(
            !embedded.contains("Run `tcr --replace`"),
            "must not prescribe an override that this kind refuses: {embedded}"
        );
        assert!(
            embedded.contains("Quit the host application"),
            "must name the recovery that works: {embedded}"
        );
        // The control: for the kinds `--replace` CAN take over, the prescription is
        // unchanged — so the assertions above are about the kind, not about the
        // advice having been dropped for everyone.
        for kind in [singleton::ProxyKind::Tcr, singleton::ProxyKind::LegacyJs] {
            let advice = wedged_incumbent_recovery(kind);
            assert!(
                advice.contains("Run `tcr --replace`"),
                "{kind:?} is recoverable by --replace: {advice}"
            );
        }
    }

    /// The ordinary stand-down: a peer is serving this binary's commit. Exit 0,
    /// or every `tcr` in a script becomes a failure.
    #[test]
    fn a_clean_stand_down_exits_zero() {
        for verdict in [
            StandDownBuild::InSync,
            StandDownBuild::DirtyBuild,
            StandDownBuild::Unknown,
        ] {
            assert_eq!(
                stand_down_exit_code(&cli::Liveness::Answering, verdict),
                0,
                "{verdict:?} is a working incumbent"
            );
        }
    }

    /// DETECTED BUILD SKEW MUST NOT EXIT 0. The whole point of the stale-server
    /// warning is that `cargo build && tcr` no longer guarantees the new binary
    /// is serving; returning success anyway leaves the guarantee broken for every
    /// script, CI step and launchd job, which read the code and not the stderr.
    #[test]
    fn a_stale_incumbent_exits_non_zero() {
        let code = stand_down_exit_code(&cli::Liveness::Answering, StandDownBuild::Stale);
        assert_ne!(code, 0, "a detected skew must be visible to `tcr && next`");
        assert_ne!(code, 1, "1 is a genuine startup failure");
        assert_ne!(code, 2, "2 is clap's usage error");
        assert_eq!(code, EXIT_STOOD_DOWN_STALE);
    }

    /// THE WEDGED PROXY. Nothing is serving on the port, so exiting 0 tells every
    /// caller — and TcrBar — that the server is up. The code has to say
    /// otherwise, and it outranks the build verdict: a proxy that will not answer
    /// cannot report a build either, so `Unknown` is what it always comes with.
    #[test]
    fn a_silent_incumbent_exits_its_own_non_zero_code() {
        assert_eq!(
            stand_down_exit_code(&silent(), StandDownBuild::Unknown),
            EXIT_STOOD_DOWN_NOT_ANSWERING
        );
        assert_ne!(EXIT_STOOD_DOWN_NOT_ANSWERING, 0);
        assert_ne!(
            EXIT_STOOD_DOWN_NOT_ANSWERING, EXIT_STOOD_DOWN_STALE,
            "the two need different recoveries, so they need different codes"
        );
        // Liveness outranks every build verdict, including a comparable one.
        assert_eq!(
            stand_down_exit_code(&silent(), StandDownBuild::InSync),
            EXIT_STOOD_DOWN_NOT_ANSWERING,
            "a matching sha from a process that answers nothing is not a healthy port"
        );
    }

    /// THE EXIT CODES ARE A CROSS-LANGUAGE CONTRACT, exactly like
    /// `singleton::INCUMBENT_MARKER`, and nothing but this test couples them.
    ///
    /// TcrBar switches on the numbers: `ServerController.StandDownExit` in
    /// `apps/macos/Sources/TcrBarCore/ServerController.swift` declares
    /// `stale = 3` and `notAnswering = 4`, and `classifyExit` turns them into
    /// `.incumbentIsStale` and `.incumbentNotAnswering`. Renumbering a constant
    /// here is a one-character edit that every other Rust test survives, while
    /// the menu-bar app silently falls through to a bare `.exited(5, …)` and
    /// reports a wedged proxy — one serving NOTHING — as a clean exit. That is
    /// the misreport this whole round exists to eliminate.
    ///
    /// The numbers are SPELLED OUT rather than referenced through the constants,
    /// deliberately: `assert_eq!(EXIT_STOOD_DOWN_STALE, EXIT_STOOD_DOWN_STALE)`
    /// compares a value with itself and passes for every value of it. The
    /// constant is the thing that must not drift, so the test has to hold the
    /// other copy — the one Swift carries.
    #[test]
    fn the_stand_down_exit_codes_are_the_numbers_tcrbar_switches_on() {
        // Transcribed from ServerController.StandDownExit.
        let tcrbar_stale: i32 = 3;
        let tcrbar_not_answering: i32 = 4;

        assert_eq!(
            EXIT_STOOD_DOWN_OK, 0,
            "a clean stand-down is success; anything else fails every `tcr && next`"
        );
        assert_eq!(
            EXIT_STOOD_DOWN_STALE, tcrbar_stale,
            "ServerController.StandDownExit.stale is 3 — change one, change both"
        );
        assert_eq!(
            EXIT_STOOD_DOWN_NOT_ANSWERING, tcrbar_not_answering,
            "ServerController.StandDownExit.notAnswering is 4 — change one, change both"
        );

        // The constants being right is worthless if the mapping does not emit
        // them, so the contract is asserted through the function TcrBar's input
        // actually comes from, against the same literals.
        assert_eq!(
            stand_down_exit_code(&cli::Liveness::Answering, StandDownBuild::InSync),
            0
        );
        assert_eq!(
            stand_down_exit_code(&cli::Liveness::Answering, StandDownBuild::Stale),
            3,
            "a stale incumbent must reach Swift as .incumbentIsStale"
        );
        assert_eq!(
            stand_down_exit_code(&silent(), StandDownBuild::Unknown),
            4,
            "a wedged incumbent must reach Swift as .incumbentNotAnswering"
        );

        // Liveness outranks build skew across the boundary too: a proxy that
        // answers nothing is not merely stale, and 4 must win over 3 — the
        // operator's next command differs (recover, not upgrade).
        assert_eq!(
            stand_down_exit_code(&silent(), StandDownBuild::Stale),
            4,
            "NOT SERVING outranks a stale build; reporting 3 here understates it"
        );

        // Three outcomes, three codes. Two of them collapsing would make the
        // Swift switch pick one arm for both.
        let codes = [
            EXIT_STOOD_DOWN_OK,
            EXIT_STOOD_DOWN_STALE,
            EXIT_STOOD_DOWN_NOT_ANSWERING,
        ];
        for (i, a) in codes.iter().enumerate() {
            for b in &codes[i + 1..] {
                assert_ne!(a, b, "the stand-down codes must stay distinct: {codes:?}");
            }
        }
        // And neither may take a code that already means something else.
        assert!(
            !codes[1..].contains(&1),
            "1 is a genuine startup failure (anyhow::Error out of main)"
        );
        assert!(
            !codes[1..].contains(&2),
            "2 is clap's usage error, which TcrBar maps to the unknown-argument hint"
        );
    }

    /// `--no-replace` is documented as a deprecated no-op, and used to be wired as
    /// a SILENT VETO over `--replace`: an operator whose launchd plist or alias
    /// already carried it, adding `--replace` to force a rebuilt binary onto the
    /// port, took over nothing and got exit 0. clap must reject the contradiction
    /// by name instead.
    #[test]
    fn replace_and_no_replace_together_are_a_usage_error() {
        // `let Err(..) else`, not `expect_err`: the Ok side is `Cli`, which does
        // not implement Debug (nor should it — it would print the config path).
        let Err(err) = Cli::try_parse_from(["tcr", "server", "--replace", "--no-replace"]) else {
            panic!("the pair is a contradiction, not a precedence puzzle — clap accepted it");
        };
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::ArgumentConflict,
            "it must fail as a conflict, not as some other parse error: {err}"
        );
        let rendered = err.to_string();
        assert!(
            rendered.contains("--no-replace") && rendered.contains("--replace"),
            "the message must name BOTH flags so the operator knows what to remove: {rendered}"
        );
    }

    /// Each flag alone still parses — the deprecated one is accepted, as promised
    /// to the scripts and launch agents that already pass it.
    #[test]
    fn each_replace_flag_alone_still_parses() {
        for args in [
            vec!["tcr", "server", "--replace"],
            vec!["tcr", "server", "--no-replace"],
            vec!["tcr", "--no-replace"],
            vec!["tcr", "server"],
        ] {
            Cli::try_parse_from(&args).unwrap_or_else(|e| panic!("{args:?} must parse: {e}"));
        }
    }

    /// A unique, test-only log directory keyed by pid + nanosecond timestamp.
    /// Never the real `~/.cache/teamclaude/logs/` — tests must not touch the
    /// live proxy's cache directory (`session-affinity.json` lives one level up
    /// and is being written by a running process).
    fn unique_log_dir(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "teamclaude-rs-test-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ))
    }

    /// The rotating log directory (account emails + request paths inside it)
    /// must be created owner-only. `tracing_appender` has no per-file
    /// `.mode()` hook (verified against its 0.2.5 source — `create_writer` in
    /// `rolling.rs` opens with `OpenOptions::append(true).create(true)` only),
    /// so confidentiality is enforced on the directory, not the file: `0700`
    /// blocks traversal into the directory for everyone but the owner, even
    /// though the files landing inside it carry whatever mode the process
    /// umask gives a freshly `OpenOptions::create`d file (commonly `0644` —
    /// world-readable in their *own* bits, but unreachable because nothing
    /// outside the owner can resolve a path through a `0700` parent).
    #[test]
    fn log_directory_is_created_owner_only() {
        let dir = unique_log_dir("dirmode");
        // Goes through the production opener: re-implementing the mode here
        // would assert 0o700 == 0o700 and pass however `open_log_appender`
        // drifts.
        let _appender = open_log_appender(&dir).expect("open temp log dir");
        let dir_mode = std::fs::metadata(&dir)
            .expect("stat temp log dir")
            .permissions()
            .mode()
            & 0o777;
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(dir_mode, 0o700, "log directory must be owner-only (0700)");
    }

    /// `DirBuilder::mode` only sets the mode of directories it *creates* — a
    /// directory that already exists (operator `mkdir -p`, a `tar` restore
    /// without `-p`, a baked container path) is left exactly as it was found.
    /// `open_log_appender` must re-assert `0700` on an already-existing
    /// directory, not merely request it at creation time, or a pre-existing
    /// `0755` `logs/` silently ships every rotated file world-readable.
    #[test]
    fn log_directory_is_re_asserted_owner_only_when_it_already_exists() {
        use std::os::unix::fs::DirBuilderExt as _;
        let dir = unique_log_dir("dirmode-preexisting");
        std::fs::DirBuilder::new()
            .mode(0o755)
            .recursive(true)
            .create(&dir)
            .expect("pre-create the dir at 0755, simulating an external mkdir/restore");
        let preexisting_mode = std::fs::metadata(&dir)
            .expect("stat pre-created dir")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            preexisting_mode, 0o755,
            "test setup sanity: the pre-created dir must actually be 0755"
        );

        let _appender = open_log_appender(&dir).expect("open pre-existing log dir");
        let dir_mode = std::fs::metadata(&dir)
            .expect("stat temp log dir")
            .permissions()
            .mode()
            & 0o777;
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(
            dir_mode, 0o700,
            "a pre-existing, wrongly-permissioned log directory must be re-asserted to 0700"
        );
    }

    /// A `MakeWriter` that keeps what was written, so a test can inspect the
    /// stdout sink without capturing the process's real stdout.
    #[derive(Clone, Default)]
    struct SharedBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl SharedBuf {
        fn contents(&self) -> String {
            let bytes = self.0.lock().expect("shared buffer poisoned").clone();
            String::from_utf8_lossy(&bytes).into_owned()
        }
    }

    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("shared buffer poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuf {
        type Writer = SharedBuf;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Reads back whatever `open_log_appender` actually wrote inside `dir` —
    /// there is exactly one matching file after a single, unrotated write.
    /// This is the "it writes", not merely "it initialised", proof: a dropped
    /// `WorkerGuard` (see `headless_subscriber`'s doc-comment) would make
    /// `init_tracing`-style construction succeed while nothing ever lands here.
    fn read_the_one_log_file(dir: &std::path::Path) -> String {
        let mut matches: Vec<_> = std::fs::read_dir(dir)
            .expect("read temp log dir")
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("teamclaude-rs.log")
            })
            .collect();
        assert_eq!(
            matches.len(),
            1,
            "expected exactly one log file in {dir:?}, found {matches:?}"
        );
        std::fs::read_to_string(matches.pop().unwrap().path()).expect("read the log file")
    }

    /// HEADLESS MUST REACH DISK. TcrBar spawns `tcr server --headless` and throws
    /// the child's stdout away, so a stdout-only headless subscriber destroys
    /// 100% of the proxy's logs — which is exactly why a restart loop could not
    /// be diagnosed. Both sinks are asserted: dropping either one is a
    /// regression, and asserting only the file would let stdout silently die for
    /// everyone running it in a terminal.
    #[test]
    fn headless_logging_reaches_both_the_file_and_stdout() {
        let dir = unique_log_dir("headless-test");
        let appender = open_log_appender(&dir).expect("open temp log dir");
        let stdout = SharedBuf::default();
        let marker = format!("headless-sink-probe-{}", std::process::id());

        let subscriber = headless_subscriber(
            tracing_subscriber::EnvFilter::new("info"),
            Some(appender),
            stdout.clone(),
        );
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("{marker}");
        });

        let on_disk = read_the_one_log_file(&dir);
        std::fs::remove_dir_all(&dir).ok();
        assert!(
            on_disk.contains(&marker),
            "headless event never reached the log file; file held: {on_disk:?}"
        );
        assert!(
            !on_disk.contains('\u{1b}'),
            "the log file must be non-ANSI; escape codes read back as garbage"
        );
        assert!(
            stdout.contents().contains(&marker),
            "headless event never reached stdout; stdout held: {:?}",
            stdout.contents()
        );
    }

    /// A log file that will not open must degrade to stdout-only, never take the
    /// proxy down: logging is not worth the traffic it is observing.
    #[test]
    fn headless_logging_survives_an_unopenable_log_file() {
        let stdout = SharedBuf::default();
        let marker = format!("headless-degraded-probe-{}", std::process::id());

        let subscriber = headless_subscriber(
            tracing_subscriber::EnvFilter::new("info"),
            None,
            stdout.clone(),
        );
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("{marker}");
        });

        assert!(
            stdout.contents().contains(&marker),
            "with no log file, stdout must still receive every event"
        );
    }

    /// `init_tracing` must target the one shared, well-known cache directory,
    /// not a private/generated path — so a human running the documented
    /// `rg 'server started' ~/.cache/teamclaude/logs/*` recipe can always find
    /// it. This is **not** because any code reads it back (a census —
    /// `rg -n 'teamclaude-rs\.log|log_file_path|log_dir_path' src/ scripts/
    /// apps/`, repo-relevant scope, not `src/` alone: a `src/`-only census once
    /// missed `scripts/validate-cache.sh`'s own copy of the old fixed path —
    /// found no programmatic reader anywhere outside this module; the previous
    /// version of this test justified itself with "this is what `tcr status`
    /// and every diagnostic already read", which was false and is not
    /// repeated here).
    #[test]
    fn the_log_directory_is_the_shared_cache_location() {
        assert_eq!(log_dir_path(), cache_base_dir().join("logs"));
        assert!(
            log_dir_path().ends_with(std::path::Path::new("teamclaude").join("logs")),
            "log directory must live under the shared teamclaude cache dir: {:?}",
            log_dir_path()
        );
    }

    /// `max_log_files` pruning is real, not merely configured, AND it is the
    /// production `open_log_appender`'s own hardcoded `max_log_files(5)`
    /// being exercised — not a hand-rolled duplicate of the config, so a
    /// regression that drops or weakens the production call is what this test
    /// is sensitive to. Pre-creates 5 dated files (the `prefix.date` shape
    /// `join_date()` produces for any non-`NEVER` rotation), each with a
    /// distinct creation time, then calls the real production opener —
    /// `prune_old_logs()` runs at *construction*, before the first new file is
    /// created (verified against 0.2.5 source, `rolling.rs:615-617`), which is
    /// what makes this deterministic: no wall-clock rotation boundary needs to
    /// be crossed.
    ///
    /// This is portable across the two CI targets, not merely convenient on
    /// one. `prune_old_logs()` sorts by `metadata.created()` where the
    /// platform supports it, but explicitly falls back to parsing the date out
    /// of the filename itself when it does not (`rolling.rs:689-696`,
    /// `parse_date_from_filename`) — and this test's embedded dates
    /// (`2020-02-01` .. `2020-02-05`) sort in the same order as their real
    /// creation timestamps, so the assertions hold identically whichever path
    /// the pruner takes. Verified directly against this repo's own targets:
    /// macOS (APFS) supports `created()`; the crate's own filename-parsing
    /// fallback exists specifically because ext4/most Linux filesystems (this
    /// repo's `ci` job runs `ubuntu-latest`) commonly return
    /// `ErrorKind::Unsupported` for it — an attempt to break this test via
    /// that path does not succeed, because the fallback is exercised, not
    /// skipped.
    #[test]
    fn old_log_files_are_pruned_at_max_log_files() {
        let dir = unique_log_dir("pruning");
        std::fs::create_dir_all(&dir).expect("create temp log dir");

        // Five pre-existing dated files, oldest first, each with a distinct
        // creation time (the pruner sorts by `metadata.created()`).
        let oldest = "2020-02-01";
        let survivors = ["2020-02-02", "2020-02-03", "2020-02-04", "2020-02-05"];
        for date in std::iter::once(&oldest).chain(survivors.iter()) {
            std::fs::write(dir.join(format!("teamclaude-rs.log.{date}")), b"old\n")
                .expect("write stale log file");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        // Production max_log_files(5): prune keeps (5 - 1) = 4 of the 5
        // existing files, then the appender creates one new file for today —
        // 5 total, with only the single oldest pre-existing file removed.
        let _appender = open_log_appender(&dir).expect("open temp log dir");

        let remaining: std::collections::BTreeSet<String> = std::fs::read_dir(&dir)
            .expect("read temp log dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        std::fs::remove_dir_all(&dir).ok();

        assert!(
            remaining.len() >= 2,
            "at least 2 files must survive pruning: {remaining:?}"
        );
        assert!(
            !remaining.contains(&format!("teamclaude-rs.log.{oldest}")),
            "the single oldest file must have been pruned: {remaining:?}"
        );
        for date in survivors {
            assert!(
                remaining.contains(&format!("teamclaude-rs.log.{date}")),
                "pre-existing file {date} must survive pruning: {remaining:?}"
            );
        }
    }
}
