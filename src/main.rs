//! `tcr` — the teamclaude-rs binary.
//!
//! Boot sequence (DESIGN §main): load the drop-in config → build the [`Manager`]
//! → spawn the axum proxy task and the background probe loop → run the TUI (or
//! block in `--headless`) → flush the config on exit.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;
use clap::{Parser, Subcommand};

use std::sync::Arc;

use teamclaude_rs::cli::{self, PriorityArg};
use teamclaude_rs::config::{self, Config, ConfigError};
use teamclaude_rs::manager::Manager;
use teamclaude_rs::{demo, mitm, oauth, singleton, tui, update};

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
    /// Account name (email) or a case-insensitive substring of it.
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
    /// Account name (email) or a case-insensitive substring of it.
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
    /// Account name (email) or a case-insensitive substring of it.
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
    /// Account name (email) or a case-insensitive substring of it.
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
    /// Log in even when a proxy server is already running on the configured port.
    /// Unsafe: the server's next token refresh will overwrite this login — stop the
    /// server first instead. This is the deliberate escape hatch.
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
    /// Do not replace an existing proxy already listening on the port. Default is
    /// to take over the port (kill the incumbent proxy), since two proxies on the
    /// same accounts mutually invalidate each other's single-use refresh tokens.
    #[arg(long)]
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
        Some(Command::Enable(args)) => run_enable(args),
        Some(Command::Disable(args)) => run_disable(args),
        Some(Command::Status(args)) => run_status(args).await,
        Some(Command::Update(args)) => update::run_update(args.force),
        Some(Command::Demo) => demo::run_demo().await.map_err(anyhow::Error::from),
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

/// `tcr enable <query> [--org]` — clear an account's `disabled` flag.
fn run_enable(args: EnableArgs) -> anyhow::Result<()> {
    let config_path = args.config.unwrap_or_else(config::default_path);
    cli::set_enabled(&config_path, &args.query, args.org.as_deref(), false)
}

/// `tcr disable <query> [--org]` — hold an account out of rotation.
fn run_disable(args: DisableArgs) -> anyhow::Result<()> {
    let config_path = args.config.unwrap_or_else(config::default_path);
    cli::set_enabled(&config_path, &args.query, args.org.as_deref(), true)
}

/// `tcr status [--json]` — probe every account's live quota and print it.
async fn run_status(args: StatusArgs) -> anyhow::Result<()> {
    let config_path = args.config.unwrap_or_else(config::default_path);
    cli::status(&config_path, args.json).await
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
        cmd.env("ANTHROPIC_BASE_URL", format!("http://127.0.0.1:{port}"));
        if let Some(key) = config.proxy.api_key.as_deref() {
            cmd.env("ANTHROPIC_API_KEY", key);
        }
        // We are the proxy, we speak plain HTTP on loopback, and we have NO MITM.
        // An ambient HTTPS_PROXY (e.g. the JS teamclaude on :3456) would hijack
        // claude's traffic away from us, and its NODE_EXTRA_CA_CERTS would be
        // verifying a leaf we never present. Strip both so `tcr run` is
        // self-contained and can't be captured by a stale env.
        for var in [
            "HTTPS_PROXY",
            "https_proxy",
            "HTTP_PROXY",
            "http_proxy",
            "NODE_EXTRA_CA_CERTS",
        ] {
            cmd.env_remove(var);
        }
        eprintln!("[tcr] routing claude through http://127.0.0.1:{port}");
    } else {
        eprintln!("[tcr] proxy not listening on :{port} — launching claude directly");
    }

    let status = cmd
        .status()
        .context("failed to launch `claude` — is it on PATH?")?;
    std::process::exit(status.code().unwrap_or(1));
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

async fn run_server(args: ServerArgs) -> anyhow::Result<()> {
    let config_path = args.config.clone().unwrap_or_else(config::default_path);
    let (mut config, persist_path) = load_config(&config_path);
    if let Some(port) = args.port {
        config.proxy.port = port;
    }
    let port = config.proxy.port;

    init_tracing(args.headless);

    // Take over the port BEFORE the Manager starts probing/refreshing, so our own
    // startup can never token-war with the incumbent proxy we're replacing. Only a
    // command-verified teamclaude/tcr server on THIS port is ever signalled.
    singleton::takeover_port(port, args.no_replace);

    let manager = Manager::with_live_refresher(config, persist_path);

    // Background probe loop: refresh every account's quota on the configured
    // cadence (a value <= 0 in `quotaProbeSeconds` disables it). The first tick
    // fires immediately, so the bars populate at startup rather than after a lag.
    let probe_seconds = manager.probe_interval_seconds();
    if probe_seconds > 0 {
        let prober = manager.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(probe_seconds));
            loop {
                ticker.tick().await;
                prober.probe_all().await;
            }
        });
    }

    // Opt-in keep-warm loop: periodically warm idle accounts so their 5h session
    // window stays live. Ships DARK — `warmupSeconds` defaults to 0, and when it is
    // absent/0 NO task is spawned here at all (unlike the probe, warming spends real
    // quota). `MissedTickBehavior::Skip` drops a missed tick rather than bursting a
    // catch-up warm after the process was suspended.
    let warmup_seconds = manager.warmup_interval_seconds();
    if warmup_seconds > 0 {
        let m = manager.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(warmup_seconds));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                m.warm_all().await;
            }
        });
    }

    // Load the MITM TLS material (reuse the existing leaf, else mint one). A
    // failure here is non-fatal: base-URL mode still serves; only CONNECT
    // (forward-proxy) mode is unavailable until the cert issue is fixed.
    let tls = match mitm::load_tls() {
        Ok(assets) => {
            if let Some(ca) = &assets.ca_path {
                tracing::info!(ca = %ca.display(), "MITM: advertise this CA via NODE_EXTRA_CA_CERTS");
            }
            Some(Arc::new(assets.acceptor))
        }
        Err(err) => {
            tracing::warn!(error = %err, "MITM disabled: could not load/generate TLS material (base-URL mode still works)");
            None
        }
    };

    // Hybrid proxy server task: base-URL mode and HTTPS_PROXY/CONNECT mode on the
    // same port. The listener peeks each connection and routes accordingly.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("failed to bind 127.0.0.1:{port}"))?;
    let bound = listener.local_addr()?;

    // The boot marker. `$TMPDIR/teamclaude-rs.log` is appended forever and never
    // rotated, so without this line a restart is invisible: request lines run
    // unbroken across a bounce and the log cannot be sliced "since this boot".
    // Emitted here deliberately — after `init_tracing` (else it goes nowhere) and
    // after the bind SUCCEEDED — so one line means "this pid is live on this port",
    // not "this pid tried". A restart also wipes the in-memory session→account pin
    // map, the most expensive cache event in this system; counting these lines is
    // how that cost becomes measurable:  rg 'server started' "$TMPDIR/teamclaude-rs.log"
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        pid = std::process::id(),
        port = bound.port(),
        "server started"
    );

    let serve_manager = manager.clone();
    let mut server = tokio::spawn(async move {
        mitm::serve(listener, serve_manager, tls).await;
    });

    if args.headless {
        tracing::info!("teamclaude-rs listening on http://{bound} (headless)");
        // Block until Ctrl-C or the server task exits.
        tokio::select! {
            _ = tokio::signal::ctrl_c() => tracing::info!("shutdown signal received"),
            res = &mut server => {
                if let Err(err) = res {
                    tracing::error!(error = %err, "server task join error");
                }
            }
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
        let tui_fut = tui::run(manager.clone());
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

    // Stop serving, then flush the config (refreshed tokens already persisted
    // incrementally; this is the final belt-and-suspenders write).
    server.abort();
    manager.persist_now();
    Ok(())
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

/// Initialise tracing. Headless logs to stdout; the TUI redirects logs to a file
/// so they never corrupt the alternate screen (stderr fallback if it won't open).
fn init_tracing(headless: bool) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    if headless {
        tracing_subscriber::fmt().with_env_filter(filter).init();
        return;
    }
    // The TUI log holds account emails + request paths; keep it owner-only (0600)
    // rather than the umask default (typically world-readable 0644).
    use std::os::unix::fs::OpenOptionsExt as _;
    let log_path = std::env::temp_dir().join("teamclaude-rs.log");
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&log_path)
    {
        Ok(file) => {
            tracing_subscriber::fmt()
                .with_ansi(false)
                .with_env_filter(filter)
                .with_writer(std::sync::Mutex::new(file))
                .init();
        }
        Err(err) => {
            eprintln!(
                "[tcr] could not open TUI log file {}: {err}",
                log_path.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

    /// The TUI log file (account emails + request paths) must be created
    /// owner-only. `.mode(0o600)` carries only owner bits, so it survives any
    /// reasonable umask (0o022/0o077 clear group/other only).
    #[test]
    fn tui_log_is_created_owner_only() {
        let path = std::env::temp_dir().join(format!(
            "teamclaude-rs-perms-test-{}-{:?}.log",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&path)
            .expect("open temp log file");
        let mode = file
            .metadata()
            .expect("stat temp log file")
            .permissions()
            .mode()
            & 0o777;
        std::fs::remove_file(&path).ok();
        assert_eq!(mode, 0o600, "log file must be owner-only (0600)");
    }
}
