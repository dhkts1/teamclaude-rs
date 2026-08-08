//! The proxy's boot sequence as a **library** call.
//!
//! This is the body of what used to be `main.rs::run_server`, with the two
//! things a library may not do removed:
//!
//! * it never calls `std::process::exit` — the stand-down is returned as a
//!   [`StandDown`] value and the *binary* maps it to an exit code;
//! * it never blocks until shutdown — [`serve`] returns as soon as the listener
//!   is bound, handing back a [`ServerHandle`] that OWNS every task it spawned.
//!   The caller decides how to wait (`tcr` runs the TUI or blocks on Ctrl-C; a
//!   test issues one request and shuts down).
//!
//! Everything else is unchanged on purpose: the same takeover decision, the same
//! affinity restore/flush, the same background loops, the same `server started`
//! boot marker emitted only after a SUCCESSFUL bind.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::manager::Manager;
use crate::{affinity, build_info, cli, mitm, singleton};

/// Where the MITM listener's TLS material comes from.
///
/// `tcr` always uses [`TlsSetup::Load`], which is what `run_server` did inline.
/// [`TlsSetup::Disabled`] exists because loading mints/reads a CA on disk, and a
/// caller that only needs base-URL mode (an in-process test) must be able to opt
/// out of that side effect rather than work around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TlsSetup {
    /// Load (or mint) the MITM CA + leaf, as the binary does. A failure is
    /// non-fatal: base-URL mode still serves, CONNECT answers 503.
    #[default]
    Load,
    /// Do not touch the TLS material at all. CONNECT answers 503.
    Disabled,
}

/// Everything [`serve`] needs, derived from what `run_server` actually read.
///
/// Note what is NOT here. `--headless` is not a serving parameter: it selects
/// the *logging subscriber* and *how the caller waits*, both of which stay with
/// the binary. The config is passed already loaded rather than as a path,
/// because loading it prints operator-facing `[tcr]` diagnostics and decides
/// whether the file may be written back — a binary concern (`main::load_config`).
pub struct ServeOptions {
    /// The already-loaded config.
    pub config: Config,
    /// Where the config may be written back, or `None` to make every persist a
    /// no-op (a corrupt file must never be clobbered with defaults).
    pub persist_path: Option<PathBuf>,
    /// Overrides `config.proxy.port` when set — the `--port` flag. `Some(0)`
    /// binds an ephemeral port, which is how a test gets a real server without
    /// contending for the configured one.
    pub port: Option<u16>,
    /// Take the port over from a recognized proxy incumbent (`--replace`).
    pub replace: bool,
    /// The session-affinity pin cache. Split out so a caller can point it
    /// somewhere disposable; the binary passes [`affinity::default_path`].
    pub affinity_path: PathBuf,
    /// Where the MITM TLS material comes from.
    pub tls: TlsSetup,
}

impl ServeOptions {
    /// The binary's defaults for everything but the config itself.
    pub fn new(config: Config) -> Self {
        Self {
            config,
            persist_path: None,
            port: None,
            replace: false,
            affinity_path: affinity::default_path(),
            tls: TlsSetup::Load,
        }
    }
}

/// A recognized proxy incumbent holds the port, so we did not bind.
///
/// Returned as DATA. The exit code, the build line and the operator warning are
/// the binary's job (`main::run_server`); a library caller gets the same facts
/// without a process exit and can decide for itself.
pub struct StandDown {
    /// The port that was contended.
    pub port: u16,
    /// The incumbent's pid, as `singleton` identified it.
    pub pid: u32,
    /// ONE probe of the incumbent: which build it runs, and whether it answers
    /// at all. Both halves are needed to pick an exit code.
    pub probe: cli::IncumbentProbe,
    /// The build comparison, verdict + human line produced together.
    pub report: build_info::StandDownReport,
}

/// What [`serve`] did.
pub enum ServeOutcome {
    /// The listener is bound and every background task is running.
    Started(ServerHandle),
    /// An incumbent holds the port and was deliberately left alone. Nothing was
    /// bound and nothing was spawned.
    StoodDown(StandDown),
}

impl ServeOutcome {
    /// The handle, or a panic — for callers (tests) that require a bound server.
    ///
    /// # Panics
    /// If the outcome was a stand-down.
    pub fn expect_started(self) -> ServerHandle {
        match self {
            ServeOutcome::Started(handle) => handle,
            ServeOutcome::StoodDown(stand_down) => panic!(
                "expected a bound server; an incumbent (pid {}) holds :{}",
                stand_down.pid, stand_down.port
            ),
        }
    }
}

/// What a clean [`ServerHandle::shutdown`] did, so a caller can assert on it
/// instead of trusting that the function returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShutdownReport {
    /// How many tasks the handle owned and joined: the accept loop plus every
    /// background loop that was actually spawned for this config.
    pub tasks_joined: usize,
    /// Pins written by the final affinity flush, `None` when affinity is off or
    /// the write failed (it is a cache; a failure is logged, never fatal).
    pub affinity_pins_written: Option<usize>,
}

/// A bound, running proxy — and the owner of every task [`serve`] spawned.
///
/// Ownership is the point. `run_server` could hand its background loops to "the
/// process is about to exit"; a library caller cannot, so dropping this handle
/// aborts them and [`shutdown`](Self::shutdown) stops them in order and gives
/// the affinity map its final write.
pub struct ServerHandle {
    addr: SocketAddr,
    shutdown: watch::Sender<bool>,
    manager: Arc<Manager>,
    affinity_path: PathBuf,
    /// The `mitm::serve` accept loop. `None` once [`Self::shutdown`] joined it.
    server: Option<JoinHandle<()>>,
    /// Whether that task has already been awaited to completion. A `JoinHandle`
    /// may only be polled to completion once, and [`Self::serving_stopped`] can
    /// get there before [`Self::shutdown`] does.
    server_finished: bool,
    /// The affinity flusher / quota prober / keep-warm loops, whichever this
    /// config enabled.
    background: Vec<JoinHandle<()>>,
}

impl ServerHandle {
    /// The address actually bound — the resolved port when `0` was requested.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The live [`Manager`], for a caller that wants the same state the TUI reads.
    pub fn manager(&self) -> &Arc<Manager> {
        &self.manager
    }

    /// How many background loops this config spawned (the accept loop excluded).
    pub fn background_task_count(&self) -> usize {
        self.background.len()
    }

    /// Resolves when the accept loop stops **on its own** — which in practice
    /// means it panicked, since it otherwise runs until shutdown. Pends forever
    /// once that has happened (or if shutdown already joined it), so it is safe
    /// to park a `select!` arm on it.
    ///
    /// Cancel-safe: nothing is taken out of `self` until the join actually
    /// completes, so losing the race leaves the task owned — and therefore still
    /// aborted by `Drop`.
    pub async fn serving_stopped(&mut self) {
        if self.server_finished {
            std::future::pending::<()>().await;
        }
        let result = match self.server.as_mut() {
            Some(server) => server.await,
            None => std::future::pending().await,
        };
        self.server_finished = true;
        if let Err(err) = result {
            tracing::error!(error = %err, "server task join error");
        }
    }

    /// Stop serving, then flush.
    ///
    /// The order is `run_server`'s: stop the accept loop, persist the config
    /// (refreshed tokens are already written incrementally; this is the final
    /// belt-and-braces write), then write the session-affinity pins.
    ///
    /// **In-flight connections are left to finish.** Each accepted connection
    /// runs on its own detached task, exactly as before this extraction, so
    /// cancelling the accept loop has never cut one; a proxied response can be a
    /// long stream and killing it mid-flight would be a behaviour change and a
    /// worse one. What stops immediately is *accepting new* connections — the
    /// listener is dropped with the accept loop, so the port refuses at once.
    pub async fn shutdown(mut self) -> ShutdownReport {
        let _ = self.shutdown.send(true);
        let mut tasks_joined = 0usize;
        if let Some(server) = self.server.take() {
            if !self.server_finished {
                let _ = server.await;
                self.server_finished = true;
            }
            tasks_joined += 1;
        }
        for task in std::mem::take(&mut self.background) {
            let _ = task.await;
            tasks_joined += 1;
        }

        self.manager.persist_now();

        // Final pin flush on a CLEAN shutdown, capturing whatever changed inside
        // the last flusher interval. Belt-and-braces only — the 5s timer is what
        // makes the pins survive a SIGKILL, which is the case that matters.
        let mut affinity_pins_written = None;
        if self.manager.session_affinity_enabled() {
            match self.manager.flush_affinity(&self.affinity_path) {
                Ok(count) => {
                    tracing::info!(
                        path = %self.affinity_path.display(),
                        pins = count,
                        "session-affinity pins written for the next boot"
                    );
                    affinity_pins_written = Some(count);
                }
                Err(err) => tracing::warn!(
                    path = %self.affinity_path.display(),
                    error = %err,
                    "final session-affinity pin write failed; pins will not survive this restart"
                ),
            }
        }

        ShutdownReport {
            tasks_joined,
            affinity_pins_written,
        }
    }
}

/// Dropping the handle must not leak the accept loop, the prober, the warmer or
/// the affinity flusher. The binary never relied on this (the process exited);
/// a library caller has nothing else to fall back on.
impl Drop for ServerHandle {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(server) = &self.server {
            server.abort();
        }
        for task in &self.background {
            task.abort();
        }
    }
}

/// Boot the proxy and return once the listener is bound.
///
/// Returns [`ServeOutcome::StoodDown`] rather than exiting when a recognized
/// proxy incumbent holds the port. Errors only when the bind itself fails.
pub async fn serve(options: ServeOptions) -> anyhow::Result<ServeOutcome> {
    let ServeOptions {
        mut config,
        persist_path,
        port: port_override,
        replace,
        affinity_path,
        tls,
    } = options;

    if let Some(port) = port_override {
        config.proxy.port = port;
    }
    let port = config.proxy.port;

    // Resolve the port to ONE proxy BEFORE the Manager starts probing/refreshing,
    // so our own startup can never token-war with the incumbent. Only a
    // command-verified teamclaude/tcr server on THIS port is ever signalled — a
    // `tcr` peer only under `--replace`, a legacy JS `teamclaude` always, since
    // displacing that one is what the takeover exists for. `--no-replace` is the
    // default now, and clap rejects it alongside `--replace`.
    if let singleton::Takeover::IncumbentPresent(pid) = singleton::takeover_port(port, replace) {
        // ONE probe of the incumbent, answering two questions: which build it is
        // executing, and whether it is executing anything at all.
        let probe = cli::probe_incumbent(&config).await;
        // Read the checkout LIVE. The build stamps alone cannot see an edit made
        // since the last commit (build.rs re-runs only when a git ref moves), so
        // comparing two stamps would print "build in sync" for a proxy that
        // predates the edit — see `build_info::stand_down_build_report`.
        let checkout = std::env::current_dir()
            .ok()
            .and_then(|cwd| build_info::find_tcr_checkout(&cwd))
            .map(|root| build_info::read_checkout_state(&root, build_info::SHA));
        // Standing down is cheap and correct, but silent success here would mean
        // `cargo build && tcr` exits 0 with the OLD build still serving — the
        // caller is handed which build actually holds the port so it can say so.
        let report = build_info::stand_down_build_report(
            port,
            &build_info::BuildInfo::current(),
            probe.build.as_ref(),
            checkout.as_ref(),
        );
        return Ok(ServeOutcome::StoodDown(StandDown {
            port,
            pid,
            probe,
            report,
        }));
    }

    let manager = Manager::with_live_refresher(config, persist_path);

    // One trigger for every task this function spawns. `watch` rather than a
    // one-shot so each loop can hold its own receiver and shutdown is
    // idempotent (both `shutdown()` and `Drop` may fire it).
    let (shutdown_tx, _) = watch::channel(false);
    let mut background: Vec<JoinHandle<()>> = Vec::new();

    // Session-affinity pins survive a restart via their own cache file (NOT the
    // credential config — see `teamclaude_rs::affinity`). Restore before the
    // listener binds, so the first request after a bounce already routes on its
    // old pin instead of cold-starting the account's prompt cache.
    //
    // Only when affinity is enabled: with the feature off the map is never
    // consulted, and a restore would put entries in it that nothing reads.
    if manager.session_affinity_enabled() {
        let report = manager.restore_affinity(&affinity_path, affinity::PIN_TTL_MS);
        if let Some(reason) = &report.degraded {
            // Never fatal: the pin file is a cache, so an unusable one costs the
            // warm start it would have bought and nothing else.
            tracing::warn!(
                path = %affinity_path.display(),
                reason = %reason,
                "session-affinity pins ignored; starting with an empty pin map"
            );
        } else {
            tracing::info!(
                path = %affinity_path.display(),
                restored = report.pins.len(),
                expired = report.expired,
                unresolved = report.unresolved,
                ambiguous = report.ambiguous,
                ttl_minutes = affinity::PIN_TTL_MS / 60_000,
                "session-affinity pins restored"
            );
        }

        // Debounced incremental flush. Shutdown-only would miss the case this
        // exists to survive: `--replace` follows SIGTERM with SIGKILL, and a
        // SIGKILL runs no shutdown path at all. A 5-second timer that writes only
        // when the map actually changed bounds the loss to one interval while
        // keeping a busy proxy to at most one small atomic write per interval;
        // pins settle early in a session, so steady state is no writes.
        let flusher = manager.clone();
        let flush_path = affinity_path.clone();
        let mut stop = shutdown_tx.subscribe();
        background.push(tokio::spawn(async move {
            let flush = async {
                let mut ticker = tokio::time::interval(Duration::from_secs(5));
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    ticker.tick().await;
                    if !flusher.take_affinity_dirty() {
                        continue;
                    }
                    if let Err(err) = flusher.flush_affinity(&flush_path) {
                        tracing::warn!(
                            path = %flush_path.display(),
                            error = %err,
                            "could not write the session-affinity pin file; pins will not survive this restart"
                        );
                    }
                }
            };
            // The write itself is synchronous, so cancelling here can never tear
            // a half-written pin file; the loss is at most one interval, which
            // `ServerHandle::shutdown`'s final flush then recovers.
            tokio::select! {
                _ = flush => {}
                _ = stop.changed() => {}
            }
        }));
    }

    // Background probe loop: refresh every account's quota on the configured
    // cadence (a value <= 0 in `quotaProbeSeconds` disables it). The first tick
    // fires immediately, so the bars populate at startup rather than after a lag.
    let probe_seconds = manager.probe_interval_seconds();
    if probe_seconds > 0 {
        let prober = manager.clone();
        let mut stop = shutdown_tx.subscribe();
        background.push(tokio::spawn(async move {
            let probe = async {
                let mut ticker = tokio::time::interval(Duration::from_secs(probe_seconds));
                loop {
                    ticker.tick().await;
                    prober.probe_all().await;
                }
            };
            tokio::select! {
                _ = probe => {}
                _ = stop.changed() => {}
            }
        }));
    }

    // Opt-in keep-warm loop: periodically warm idle accounts so their 5h session
    // window stays live. Ships DARK — `warmupSeconds` defaults to 0, and when it is
    // absent/0 NO task is spawned here at all (unlike the probe, warming spends real
    // quota). `MissedTickBehavior::Skip` drops a missed tick rather than bursting a
    // catch-up warm after the process was suspended.
    let warmup_seconds = manager.warmup_interval_seconds();
    if warmup_seconds > 0 {
        let m = manager.clone();
        let mut stop = shutdown_tx.subscribe();
        background.push(tokio::spawn(async move {
            let warm = async {
                let mut ticker = tokio::time::interval(Duration::from_secs(warmup_seconds));
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    // Two things start a sweep: the configured cadence, and the probe
                    // reporting that it has READ an account's quota for the first time.
                    // The second is what keeps `warm_targets`' boot gate from being a
                    // kill switch — the ticker's immediate first tick necessarily finds
                    // no targets (no quota read yet) and `Skip` puts the next one a whole
                    // `warmupSeconds` away, so at 3600s a proxy restarted more often than
                    // hourly would otherwise warm nothing, ever. A wake arriving while
                    // this task is inside `warm_all` is stored as a permit by
                    // `notify_one` and consumed by the next `notified()`, so it is never
                    // lost; the flip fires at most once per account per process, so this
                    // cannot spin.
                    tokio::select! {
                        _ = ticker.tick() => {}
                        _ = m.warm_wake().notified() => {}
                    }
                    m.warm_all().await;
                }
            };
            tokio::select! {
                _ = warm => {}
                _ = stop.changed() => {}
            }
        }));
    }

    // Load the MITM TLS material (reuse the existing leaf, else mint one). A
    // failure here is non-fatal: base-URL mode still serves; only CONNECT
    // (forward-proxy) mode is unavailable until the cert issue is fixed.
    let tls = match tls {
        TlsSetup::Load => match mitm::load_tls() {
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
        },
        TlsSetup::Disabled => None,
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
    // Emitted here deliberately — after the subscriber is installed (else it goes
    // nowhere) and after the bind SUCCEEDED — so one line means "this pid is live
    // on this port", not "this pid tried". A restart also wipes the in-memory
    // session→account pin map, the most expensive cache event in this system;
    // counting these lines is how that cost becomes measurable:
    //   rg 'server started' "$TMPDIR/teamclaude-rs.log"
    //
    // `version` alone could not tell two boots apart: it is `CARGO_PKG_VERSION`,
    // the literal 0.1.0 from Cargo.toml, identical across every build ever made.
    // The build stamp beside it is the field that actually identifies the code
    // this pid is executing — the thing that used to need an `lsof -p <pid>`
    // inode comparison to establish. See `build_info`.
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        sha = build_info::SHA,
        dirty = build_info::DIRTY,
        built_at = build_info::BUILT_AT,
        pid = std::process::id(),
        port = bound.port(),
        "server started"
    );

    let serve_manager = manager.clone();
    let mut stop = shutdown_tx.subscribe();
    let server = tokio::spawn(async move {
        mitm::serve_with_shutdown(listener, serve_manager, tls, async move {
            let _ = stop.changed().await;
        })
        .await;
    });

    Ok(ServeOutcome::Started(ServerHandle {
        addr: bound,
        shutdown: shutdown_tx,
        manager,
        affinity_path,
        server: Some(server),
        server_finished: false,
        background,
    }))
}
