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
use futures::FutureExt as _;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::manager::Manager;
use crate::{affinity, build_info, cli, mitm, singleton};

/// How long [`ServerHandle::shutdown`] waits for a task to stop before aborting
/// it. Long enough for a loop to finish an iteration and an in-progress atomic
/// write to land; short enough that quitting `tcr` on a wedged filesystem is a
/// pause and not a hang.
pub const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

// The floor on what `ShutdownHandle::shutdown_within` gives the usage ledger to
// drain. The ledger flush runs LAST, so a shutdown that spent its whole grace on
// a wedged task would otherwise hand it zero and lose the queue by construction.
// Defined in `usage`, which owns the shutdown it bounds and floors the writer
// join at the same value: two copies of "how long is a real attempt" is how the
// two steps come to disagree.
use crate::usage::MIN_LEDGER_SHUTDOWN;

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

/// What [`serve`] may do to a process that already holds the port.
///
/// This was a `bool` named `replace`, which is how the most destructive
/// operation in this system ended up one keystroke from every caller: setting it
/// reaches [`singleton::takeover_port`], which SIGTERMs and then SIGKILLs a
/// command-verified proxy holding the port, wiping its session→account pin map.
/// Under the `--replace` flag that is the intended, operator-typed recovery.
/// From an embedder or a test it is a catastrophe with the same spelling.
///
/// So the signalling choices are not values a caller can land on — they are
/// *named constructors* that say what they do, and the private field means
/// `Default`, struct-literal syntax and `..Default::default()` can only ever
/// produce [`never_signal`](Self::never_signal). "Follow the docs and you cannot
/// kill anything" is the property being bought.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IncumbentPolicy(Signal);

/// The private half of [`IncumbentPolicy`]. Deliberately unnameable outside this
/// module so no caller can construct the signalling variants directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Signal {
    /// Signal nothing, ever. A recognized incumbent means stand down.
    #[default]
    Never,
    /// The binary's default (`--no-replace`): replace a legacy JS proxy, stand
    /// down for a `tcr` peer.
    LegacyJsOnly,
    /// `--replace`: replace whichever recognized proxy holds the port.
    Recognized,
}

impl IncumbentPolicy {
    /// **The default.** Send no signal to any process under any circumstance; a
    /// recognized proxy on the port produces [`ServeOutcome::StoodDown`].
    ///
    /// The only correct policy for a library caller or a test, which is why it
    /// is what you get for free.
    pub fn never_signal() -> Self {
        Self(Signal::Never)
    }

    /// `tcr server` without `--replace`: SIGTERM/SIGKILL a **legacy JS**
    /// `teamclaude` proxy on the port (displacing it is why the takeover exists,
    /// and leaving it running would token-war over single-use refresh tokens),
    /// but stand down for a `tcr` peer.
    pub fn replace_legacy_js_only() -> Self {
        Self(Signal::LegacyJsOnly)
    }

    /// `tcr server --replace`: **SIGTERM, then SIGKILL after 800ms**, whichever
    /// recognized proxy holds the port — including a live `tcr` serving real
    /// traffic. That wipes its in-memory session→account pin map and every live
    /// session then pays a full cold prompt-cache prefix.
    ///
    /// Reserve this for an operator who typed `--replace`. Nothing in a test or
    /// an embedder should call it.
    pub fn kill_the_incumbent_proxy() -> Self {
        Self(Signal::Recognized)
    }

    /// Whether this policy can signal a process at all — for a caller that wants
    /// to assert it is holding the harmless one.
    pub fn signals_anything(&self) -> bool {
        !matches!(self.0, Signal::Never)
    }
}

/// Everything [`serve`] needs, derived from what `run_server` actually read.
///
/// Every field that can reach outside this process — the config file, the pin
/// cache, the incumbent on the port — defaults to the inert choice, so the
/// *dangerous* configuration is the one you have to spell out. See
/// [`ServeOptions::new`].
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
    ///
    /// `None` also means **refreshed OAuth tokens are never written to disk**.
    /// Anthropic's refresh tokens are single-use, so a long-lived embedder that
    /// leaves this `None` while pointing at a real config leaves already-spent
    /// tokens on disk and every account fails to refresh on the next boot. If
    /// you serve from a real config, pass its path.
    pub persist_path: Option<PathBuf>,
    /// Overrides `config.proxy.port` when set — the `--port` flag. `Some(0)`
    /// binds an ephemeral port, which is how a test gets a real server without
    /// contending for the configured one.
    pub port: Option<u16>,
    /// What to do about a recognized proxy already holding the port. Defaults to
    /// [`IncumbentPolicy::never_signal`]; the binary maps `--replace` onto it.
    pub incumbent: IncumbentPolicy,
    /// The session-affinity pin cache, or `None` to keep pins **in memory only**
    /// — nothing is read at boot and nothing is written at shutdown.
    ///
    /// `None` is the default because the binary's path ([`affinity::default_path`])
    /// is one shared file: a second process that serves briefly and shuts down
    /// atomically replaces the live proxy's pin map with its own (usually empty)
    /// one, and the live proxy — whose flusher only writes when its map changed —
    /// never repairs it. The next boot then cold-starts every session's prompt
    /// cache. Point this somewhere disposable, or leave it `None`.
    pub affinity_path: Option<PathBuf>,
    /// The usage-ledger DIRECTORY, or `None` to keep usage **in memory only** —
    /// nothing is replayed at boot and nothing is written, so a restart starts
    /// the day at zero.
    ///
    /// `None` is the default for the same reason as [`Self::affinity_path`]:
    /// the binary's path ([`crate::usage::default_dir`]) is one shared
    /// directory, and a second process that serves briefly would append its
    /// traffic into the live proxy's day file and replay the live proxy's
    /// traffic into its own totals. Point this somewhere disposable, or leave
    /// it `None`.
    pub usage_dir: Option<PathBuf>,
    /// Where the MITM TLS material comes from.
    pub tls: TlsSetup,
    /// What is hosting this proxy — a standalone `tcr` process
    /// ([`singleton::ProxyHost::Cli`]) or an application serving it in-process
    /// ([`singleton::ProxyHost::Embedded`]).
    ///
    /// **Every caller states this; the library never infers it.** Inferring it
    /// from `argv[0]` is precisely the bug the owner file exists to fix (see
    /// [`crate::singleton`]): an embedded proxy's `argv[0]` is the host
    /// application's, which the name matcher does not recognize at all, and the
    /// consequence is `tcr login` no longer refusing to run beside a live server
    /// that will then overwrite its fresh single-use refresh tokens.
    ///
    /// Recorded in the owner file, and only there — so it has no effect unless
    /// [`Self::owner_dir`] is set.
    pub host: singleton::ProxyHost,
    /// The DIRECTORY to write the port claim in, or `None` to write no claim at
    /// all. The file name inside it is not the caller's to choose: [`serve`]
    /// derives it with [`singleton::owner_path_in`] from the port it actually
    /// bound.
    ///
    /// A directory rather than a path, because the NAME is a contract. Every
    /// reader — [`singleton::live_proxy_server`], [`singleton::takeover_port`] —
    /// looks the claim up as `proxy-owner-<port>.json`; a claim written under any
    /// other name is consulted by nothing, with no error anywhere. Handing the
    /// caller a free-form `owner_path` made that a one-typo failure, and worse, it
    /// let the caller name the file after a port it did not bind: `port: Some(0)`
    /// resolves to an ephemeral port at bind time, so the name and the contents
    /// disagreed and the proxy stayed invisible while the write logged success.
    ///
    /// `None` is the default because the directory is *shared state*: a second
    /// process serving briefly would otherwise leave its own claim where the live
    /// proxy's belongs. The binary passes [`singleton::default_owner_dir`]; a test
    /// points it somewhere disposable.
    ///
    /// Omitting it is safe, never silent: identity then falls back to the
    /// command-line matcher, which is what every `tcr` did before this file
    /// existed. It is only an *embedded* proxy that the matcher cannot see, and an
    /// embedder must therefore pass a directory.
    pub owner_dir: Option<PathBuf>,
}

impl ServeOptions {
    /// Serve this config while touching **nothing outside this process**: no
    /// config write-back, no pin cache, no signal to whatever holds the port,
    /// and (via [`TlsSetup::Load`]) the same TLS material the binary uses.
    ///
    /// This deliberately is NOT "the binary's defaults" — it used to say so
    /// while defaulting `persist_path` to `None`, which is the opposite of what
    /// the binary passes. The binary spells its own options out in
    /// `main::run_server`, because every one of them is a decision about files
    /// and processes a library caller must not make by accident.
    pub fn new(config: Config) -> Self {
        Self {
            config,
            persist_path: None,
            port: None,
            incumbent: IncumbentPolicy::never_signal(),
            affinity_path: None,
            usage_dir: None,
            tls: TlsSetup::Load,
            // Inert, like every other field here: with no owner dir, `host` is
            // recorded nowhere and this value cannot be read by anyone. A caller
            // that DOES claim the port must state its host, and both callers that
            // write a file spell the pair out together.
            host: singleton::ProxyHost::Cli,
            owner_dir: None,
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
    /// WHICH proxy that pid is. Carried because it decides what a caller may tell
    /// the operator to do about it: [`singleton::ProxyKind::TcrEmbedded`] means
    /// the pid belongs to a host application, so neither a signal nor `--replace`
    /// is an available recovery — advising either is advising the loss of the
    /// app's shutdown and its final session→account pin write.
    pub kind: singleton::ProxyKind,
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

/// What the final session-affinity pin write did.
///
/// Three states, not `Option<usize>`. "Affinity is off" and "the write FAILED
/// and every pin is lost" are the same value in an `Option`, and the only place
/// the difference survived was a `tracing::warn!` — which a library embedder,
/// having installed no subscriber, never sees. A caller reading a clean report
/// while the next boot cold-starts every session's prompt cache is exactly the
/// silence this type exists to break.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AffinityFlush {
    /// Nothing to write: session affinity is off in the config, or no pin cache
    /// path was configured ([`ServeOptions::affinity_path`] was `None`).
    Disabled,
    /// The pins were written for the next boot. Zero is a normal count.
    Written(usize),
    /// The write failed and the pins are **lost**. Carries the rendered error
    /// because the caller may have no tracing subscriber to read the warning in.
    Failed(String),
}

impl AffinityFlush {
    /// The pin count when one was actually written, else `None`.
    pub fn pins_written(&self) -> Option<usize> {
        match self {
            AffinityFlush::Written(count) => Some(*count),
            _ => None,
        }
    }

    /// Did a write that was supposed to happen fail? The one condition a caller
    /// should surface even though shutdown is not fallible.
    pub fn failed(&self) -> bool {
        matches!(self, AffinityFlush::Failed(_))
    }
}

/// What a clean [`ServerHandle::shutdown`] did, so a caller can assert on it
/// instead of trusting that the function returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownReport {
    /// How many tasks the handle owned and joined: the accept loop plus every
    /// background loop that was actually spawned for this config.
    pub tasks_joined: usize,
    /// How many tasks did NOT stop inside the grace period and were aborted.
    /// Non-zero means a loop was wedged — the flush below still ran, which is
    /// the whole reason the grace exists.
    pub tasks_aborted: usize,
    /// What the final pin write did.
    pub affinity: AffinityFlush,
    /// What the usage ledger's drain-and-join did: `Flushed` means every line
    /// this process queued is on disk, `Abandoned` means the writer outlasted
    /// the grace and its tail is gone.
    pub ledger: crate::usage::LedgerShutdown,
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
    affinity_path: Option<PathBuf>,
    /// The port claim written after the bind, to be removed on shutdown. `None`
    /// when the caller asked for no claim (or the write failed — a claim that was
    /// never written is nothing to remove).
    owner_path: Option<PathBuf>,
    /// Tasks joined and tasks aborted so far. Kept on the handle, not in a local,
    /// so a `shutdown` future that is dropped mid-join and re-issued reports the
    /// whole truth rather than only what the last attempt saw.
    tasks_joined: usize,
    tasks_aborted: usize,
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
    ///
    /// Bounded by [`DEFAULT_SHUTDOWN_GRACE`] and **cannot hang** — see
    /// [`shutdown_within`](Self::shutdown_within) for why that is not optional.
    pub async fn shutdown(&mut self) -> ShutdownReport {
        self.shutdown_within(DEFAULT_SHUTDOWN_GRACE).await
    }

    /// [`shutdown`](Self::shutdown) with the join deadline spelled out.
    ///
    /// # Why there is a deadline at all
    ///
    /// `run_server` called `server.abort()` and flushed immediately; it could not
    /// hang. Joining instead is better — a loop gets to finish its current
    /// iteration — but only if the join is bounded, because the affinity flusher
    /// performs a **blocking `std::fs` write inside async code**
    /// (`flush_affinity` → `config::write_atomic`) and cancellation only lands at
    /// an await point. On a full, slow or hung filesystem an unbounded join means
    /// `tcr` quits into a hang with the terminal already restored, no listener
    /// bound, no prompt — and `persist_now` never runs. So a task that has not
    /// stopped within `grace` is aborted, counted in
    /// [`ShutdownReport::tasks_aborted`], and left behind; the config persist and
    /// the final pin write happen either way.
    ///
    /// # Cancel-safety
    ///
    /// Takes `&mut self`, and a task is removed from the handle only once it has
    /// actually been joined. A caller that bounds this with its own deadline and
    /// drops the future keeps a usable handle, keeps every un-joined task owned
    /// (so `Drop` still aborts them), and may simply call it again — the counters
    /// carry over and the flushes are idempotent.
    pub async fn shutdown_within(&mut self, grace: Duration) -> ShutdownReport {
        let _ = self.shutdown.send(true);
        let deadline = tokio::time::Instant::now() + grace;

        if let Some(server) = self.server.as_mut() {
            // `&mut JoinHandle` polls the join without consuming it: losing the
            // race to the caller's own timeout leaves the task owned here.
            let stopped = self.server_finished
                || match tokio::time::timeout_at(deadline, &mut *server).await {
                    Ok(_) => true,
                    Err(_) => {
                        // NOT awaited after the abort: a task wedged in a
                        // synchronous write never reaches a cancellation point,
                        // so awaiting it back would reintroduce the hang.
                        server.abort();
                        false
                    }
                };
            self.server = None;
            if stopped {
                self.server_finished = true;
                self.tasks_joined += 1;
            } else {
                self.tasks_aborted += 1;
            }
        }
        while let Some(task) = self.background.last_mut() {
            match tokio::time::timeout_at(deadline, &mut *task).await {
                Ok(_) => self.tasks_joined += 1,
                Err(_) => {
                    task.abort();
                    self.tasks_aborted += 1;
                }
            }
            self.background.pop();
        }

        // Withdraw the port claim once the accept loop is done, so the next `tcr`
        // does not read a claim for a proxy that has stopped listening. Ordered
        // after the joins above and before the persists below for exactly that
        // reason. Taken (not just read) so a re-issued `shutdown` does not try
        // again — and a leftover file is harmless anyway: `singleton` re-checks the
        // pid against the live listeners before believing any claim.
        //
        // Removed only if it still names US: the listener was freed at the top of
        // this function while the joins below it can take hundreds of milliseconds,
        // so a successor may already have bound the port and written its own claim
        // to this same port-named path. See `singleton::remove_owner_file_if_owned`.
        if let Some(path) = self.owner_path.take() {
            singleton::remove_owner_file_if_owned(&path, std::process::id(), self.addr.port());
        }

        self.manager.persist_now();

        // Final pin flush on a CLEAN shutdown, capturing whatever changed inside
        // the last flusher interval. Belt-and-braces only — the 5s timer is what
        // makes the pins survive a SIGKILL, which is the case that matters.
        let affinity = self.flush_affinity_finally();

        // Then the usage ledger, LAST, because it is the one flush whose input
        // is still arriving: an in-flight request that finishes during the joins
        // above records its usage, and that line is in the writer's queue rather
        // than on disk. Bounded by whatever is left of the caller's grace — a
        // stalled volume must not turn a quit into a hang — with a floor, so a
        // shutdown that has already spent its whole budget still gets one real
        // attempt instead of a guaranteed loss.
        let ledger_budget = deadline
            .saturating_duration_since(tokio::time::Instant::now())
            .max(MIN_LEDGER_SHUTDOWN);
        let ledger = self.shutdown_ledger_within(ledger_budget).await;
        match ledger {
            crate::usage::LedgerShutdown::NotAttached => {}
            crate::usage::LedgerShutdown::Flushed => tracing::info!(
                dropped_lines = self.manager.usage_dropped_lines(),
                "the usage ledger drained and closed; today's totals will replay at the next boot"
            ),
            crate::usage::LedgerShutdown::Abandoned => tracing::warn!(
                budget_ms = ledger_budget.as_millis(),
                dropped_lines = self.manager.usage_dropped_lines(),
                "the usage ledger did not drain inside the shutdown budget; the last requests \
                 it served are not on disk"
            ),
        }

        ShutdownReport {
            tasks_joined: self.tasks_joined,
            tasks_aborted: self.tasks_aborted,
            affinity,
            ledger,
        }
    }

    /// The ledger drain, on a BLOCKING thread and awaited with a timeout.
    ///
    /// `UsageTracker::shutdown_ledger` is synchronous through and through — a
    /// retry loop around `try_send`, a `recv_timeout`, a writer-finish poll —
    /// and it can occupy its whole budget. Called inline it would be a
    /// multi-second stretch of an `async fn` with no await point in it, which
    /// breaks both promises this function makes: a caller that wraps it in its
    /// own `tokio::time::timeout` cannot interrupt it (cancellation only lands
    /// at an await), and a Tokio worker is held off its other tasks for the
    /// duration.
    ///
    /// So it runs on the blocking pool and this awaits it, bounded by the same
    /// budget. Dropping the returned future — the caller's timeout firing —
    /// stops the WAIT, not the drain: the blocking thread finishes the flush and
    /// the join on its own, which is the right half to keep going, and the
    /// process exits when it is done either way. A drain nobody is left to hear
    /// about is reported as `Abandoned`, which is what a caller that stopped
    /// waiting has to assume.
    async fn shutdown_ledger_within(&self, budget: Duration) -> crate::usage::LedgerShutdown {
        let manager = Arc::clone(&self.manager);
        let drain = tokio::task::spawn_blocking(move || manager.shutdown_usage_ledger(budget));
        // The drain's own worst case is the flush budget PLUS the writer join's
        // floor, so bounding this at `budget` alone would cut off exactly the
        // slow-but-alive writer the floor exists to let finish — and report a
        // completed drain as abandoned.
        let bound = budget + MIN_LEDGER_SHUTDOWN;
        match tokio::time::timeout(bound, drain).await {
            Ok(Ok(report)) => report,
            // The blocking pool is gone, or the drain panicked: either way this
            // process has no evidence the day file is complete.
            Ok(Err(err)) => {
                tracing::warn!(
                    error = %err,
                    "the usage-ledger drain did not run to completion"
                );
                crate::usage::LedgerShutdown::Abandoned
            }
            Err(_) => crate::usage::LedgerShutdown::Abandoned,
        }
    }

    /// The shutdown pin write, as a value. Logs as before for the binary's
    /// operator, and returns the same fact for a caller with no subscriber.
    fn flush_affinity_finally(&self) -> AffinityFlush {
        let Some(path) = self.affinity_path.as_ref() else {
            return AffinityFlush::Disabled;
        };
        if !self.manager.session_affinity_enabled() {
            return AffinityFlush::Disabled;
        }
        match self.manager.flush_affinity(path) {
            Ok(count) => {
                tracing::info!(
                    path = %path.display(),
                    pins = count,
                    "session-affinity pins written for the next boot"
                );
                AffinityFlush::Written(count)
            }
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "final session-affinity pin write failed; pins will not survive this restart"
                );
                AffinityFlush::Failed(err.to_string())
            }
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

/// Extract a human-readable message from a caught panic payload. Panics carry
/// either a `&'static str` (the common `panic!("literal")` case) or a `String`
/// (`panic!("{}", x)`, `.unwrap()`'s formatted message); anything else falls
/// back to a fixed string rather than failing to log at all.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// Supervise a background loop: log (via `tracing::error!`) if it ends by
/// panicking, and otherwise do nothing else — it is NOT restarted. This repo's
/// three background loops (affinity flusher, quota prober, keep-warm) run
/// forever until the shutdown signal, and until now nothing observed them
/// while serving: a panic inside one silently stopped it forever with no
/// signal anywhere, and whatever it was keeping current (quota data, pin
/// flushes, keep-warm sweeps) then froze at its last value while `tcr status`
/// kept rendering it as live.
///
/// Catching the panic HERE, inside the task body, rather than adding a second
/// watcher task over the `JoinHandle` outside it, is deliberate: a watcher that
/// merely awaited the real task's `JoinHandle` would need to be the thing
/// stored in `background` for `ServerHandle::shutdown_within`/`Drop` to
/// join/abort — and aborting the WATCHER does not abort the task it was
/// watching, silently detaching it. Catching the panic in-place instead means
/// the spawned task's own `JoinHandle` always completes normally, so every
/// existing join/abort site in this file needs no change at all, and
/// `tasks_joined`/`tasks_aborted` accounting stays exactly as before.
///
/// `task` identifies which loop died, so the log line is actionable without
/// re-deriving it from a bare panic backtrace.
async fn supervise(task: &'static str, fut: impl std::future::Future<Output = ()>) {
    if let Err(payload) = std::panic::AssertUnwindSafe(fut).catch_unwind().await {
        let reason = panic_message(&*payload);
        tracing::error!(
            task,
            reason,
            "background task panicked and stopped; it will NOT be restarted \
             — whatever it was keeping current is now frozen at its last value"
        );
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
        incumbent,
        affinity_path,
        usage_dir,
        tls,
        host,
        owner_dir,
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
    //
    // Which of those a caller gets is [`IncumbentPolicy`], and the default sends
    // no signal at all: it uses `live_proxy_server`, the detection-only half of
    // the same port-scoped, command-verified decision, and stands down for
    // anything it finds. Port 0 short-circuits the whole question — the kernel
    // picks an ephemeral port, so no process can be "holding" it and there is
    // nothing an ephemeral-port caller could possibly want signalled.
    let takeover = match (port, incumbent.0) {
        (0, _) => singleton::Takeover::Proceed,
        (_, Signal::Never) => match singleton::live_proxy_server(port) {
            Some(incumbent) => singleton::Takeover::IncumbentPresent(incumbent),
            None => singleton::Takeover::Proceed,
        },
        (_, Signal::LegacyJsOnly) => singleton::takeover_port(port, false),
        (_, Signal::Recognized) => singleton::takeover_port(port, true),
    };
    if let singleton::Takeover::IncumbentPresent(incumbent) = takeover {
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
            pid: incumbent.pid,
            kind: incumbent.kind,
            probe,
            report,
        }));
    }

    let manager = Manager::with_live_refresher(config, persist_path);

    // Usage accounting is restored before the listener binds, for the same
    // reason the affinity pins are: the first request after a bounce must not
    // see a day that has reset to zero. Only when a directory was given — see
    // `ServeOptions::usage_dir` on why that is not the default.
    //
    // Never fatal. A ledger that cannot be read or written costs the restart's
    // history and nothing else, so `attach_usage_ledger` reports rather than
    // errors and the proxy serves either way.
    if let Some(usage_dir) = &usage_dir {
        crate::usage::warn_if_local_offset_unavailable();
        let retention = manager.usage_retention_days();
        let report = manager.attach_usage_ledger(usage_dir.clone(), retention);
        tracing::info!(
            path = %usage_dir.display(),
            replayed = report.replayed,
            unresolved = report.unresolved,
            malformed = report.malformed,
            pruned = report.pruned,
            retention_days = retention,
            // The two the writer thread owns: whether it is actually writing
            // (false if it could not start, or has already failed), and how
            // many lines its queue had no room for. Both are the answer to
            // "will today's totals survive the next restart", which the count
            // of replayed lines alone cannot give.
            persisting = manager.usage_is_persisting(),
            dropped_lines = manager.usage_dropped_lines(),
            "usage ledger attached"
        );
    }

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
    //
    // And only when a pin cache path was given: with `affinity_path: None` the
    // pins are an in-memory routing table for this process alone, so there is
    // nothing to restore from and nothing to spawn a flusher for.
    if let (true, Some(affinity_path)) = (manager.session_affinity_enabled(), &affinity_path) {
        let report = manager.restore_affinity(affinity_path, affinity::PIN_TTL_MS);
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
        background.push(tokio::spawn(supervise("affinity-flusher", async move {
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
        })));
    }

    // Background probe: refresh every account's quota around the configured
    // cadence (a value <= 0 in `quotaProbeSeconds` disables it).
    //
    // Boot does ONE immediate whole-fleet sweep, then hands over to `schedule`,
    // which runs each account on its own randomly drawn schedule (`cadence +/-
    // 30%`, random initial offset — see `crate::schedule`). The boot sweep is
    // kept deliberately: `interval`'s first tick used to fire immediately so the
    // bars populate at startup rather than after a lag, and a random first offset
    // would otherwise leave a fresh proxy showing blank bars for up to a whole
    // cadence. It is a single sweep at a known-quiet moment, not a repeating
    // synchronization — every subsequent probe is per-account and random.
    let probe_seconds = manager.probe_interval_seconds();
    if probe_seconds > 0 {
        let prober = manager.clone();
        let mut stop = shutdown_tx.subscribe();
        background.push(tokio::spawn(supervise("quota-prober", async move {
            let probe = async {
                prober.probe_all().await;
                crate::schedule::run(prober.clone(), crate::schedule::Job::Probe, probe_seconds)
                    .await;
            };
            tokio::select! {
                _ = probe => {}
                _ = stop.changed() => {}
            }
        })));
    }

    // Opt-in keep-warm loop: periodically warm idle accounts so their 5h session
    // window stays live. Ships DARK — `warmupSeconds` defaults to 0, and when it is
    // absent/0 NO task is spawned here at all (unlike the probe, warming spends real
    // quota).
    //
    // Same treatment as the probe and for the same reason — it had the identical
    // synchronized shape — with one deliberate difference: there is NO boot sweep
    // here, because a warm spends real quota and `warm_targets`' boot gate exists
    // precisely to stop a restart from firing one at every account. Each account
    // gets a random initial offset and a random interval thereafter
    // (`crate::schedule`), and the edge-triggered `warm_wake` still starts a
    // one-shot sweep: it is what keeps the boot gate from being a kill switch on
    // a proxy restarted more often than `warmupSeconds`. That wake is handled
    // inside `schedule::run`, where a permit stored by `notify_one` while a warm
    // is in flight is consumed by the next `notified()` rather than lost.
    let warmup_seconds = manager.warmup_interval_seconds();
    if warmup_seconds > 0 {
        let m = manager.clone();
        let mut stop = shutdown_tx.subscribe();
        background.push(tokio::spawn(supervise("keep-warm", async move {
            let warm = crate::schedule::run(m, crate::schedule::Job::Warm, warmup_seconds);
            tokio::select! {
                _ = warm => {}
                _ = stop.changed() => {}
            }
        })));
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

    // The boot marker. The durable log at `~/.cache/teamclaude/logs/` rotates
    // daily (5 files kept) rather than growing forever, but within one day's
    // file a restart is still invisible without this line: request lines run
    // unbroken across a bounce and the log cannot be sliced "since this boot".
    // Emitted here deliberately — after the subscriber is installed (else it goes
    // nowhere) and after the bind SUCCEEDED — so one line means "this pid is live
    // on this port", not "this pid tried". A restart also wipes the in-memory
    // session→account pin map, the most expensive cache event in this system;
    // counting these lines is how that cost becomes measurable:
    //   rg 'server started' ~/.cache/teamclaude/logs/*
    //
    // `version` alone could not tell two boots apart: it is `CARGO_PKG_VERSION`,
    // the literal 0.1.0 from Cargo.toml, identical across every build ever made.
    // The build stamp beside it is the field that actually identifies the code
    // this pid is executing — the thing that used to need an `lsof -p <pid>`
    // inode comparison to establish. See `build_info`.
    // `http1_only` rides on the SAME boot line rather than a separate one,
    // deliberately: this repo lost seven hours of prompt-cache once to a
    // default-off knob whose state was invisible from outside the process,
    // and the fix is making the state show up at the one place every boot is
    // already guaranteed to log — not adding a second line an operator has to
    // know to look for. See `Config::http1_only`. `divert_budget` rides the same
    // line for the same reason: `0` (unlimited, today's behaviour) and a nonzero
    // budget are both silent from outside the process otherwise. See
    // `Manager::divert_budget` and the divert-budget design notes §4.7.
    let http1_only = manager.http1_only();
    // `throttle_exempt_noise` rides on this same line for the same reason as
    // `http1_only` above: it is a default-OFF knob (see
    // `Manager::throttle_exempt_noise_enabled`) and this is the one place
    // every boot is already guaranteed to log.
    let throttle_exempt_noise = manager.throttle_exempt_noise_enabled();
    let divert_budget = manager.divert_budget();
    // Every other load-bearing knob rides this same line for the same reason:
    // `sessionAffinity`, `revalidationServe` and `loadBalanceMigration` are
    // each default-ON opt-OUT switches (a config that silently disables one
    // reads identically to the feature working, from outside the process);
    // `quotaProbeSeconds`/`warmupSeconds` decide whether the two timer loops
    // spawned above exist at all; `pacing`/`throttle` are each all-`None`
    // (inert) unless the operator opted in, and a `PacingConfig`/
    // `ThrottleConfig` typo that resolves to "still inert" is otherwise
    // invisible; `lockAccount`/`controlAccount` resolve a NAME to an account
    // index at construction and log an `error!`/nothing respectively on a
    // typo, but neither of those lines carries the RESOLVED identity next to
    // the rest of the boot state.
    let session_affinity = manager.session_affinity_enabled();
    let revalidation_serve = manager.revalidation_serve_enabled();
    let load_balance_migration = manager.load_balance_migration_enabled();
    // Reuse the bindings the probe/warm spawn gates above already computed
    // rather than reading the manager twice for the same values.
    let quota_probe_seconds = probe_seconds;
    let pacing_active = manager.pacing_active();
    let throttle_active = manager.throttle_active();
    let lock_account = manager.locked_account_name();
    let control_account = manager.control_name();
    let control_pooled = manager.control_pooled();
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        sha = build_info::SHA,
        dirty = build_info::DIRTY,
        built_at = build_info::BUILT_AT,
        pid = std::process::id(),
        port = bound.port(),
        http1_only,
        throttle_exempt_noise,
        divert_budget,
        session_affinity,
        revalidation_serve,
        load_balance_migration,
        quota_probe_seconds,
        warmup_seconds,
        pacing_active,
        throttle_active,
        lock_account,
        control_account,
        control_pooled,
        "server started"
    );

    // Claim the port by NAME-FREE identity, in the same place and for the same
    // reason as the boot marker above: after the bind SUCCEEDED, so the file means
    // "this pid is serving this port" rather than "this pid tried". A `tcr` in
    // another terminal, and `tcr login`, then recognize this proxy whatever program
    // is hosting it — see [`crate::singleton`] for the silent token loss that
    // depends on it.
    //
    // A write failure is NOT fatal. The claim is an optimisation over the
    // command-line matcher for the CLI host, and refusing to serve because a cache
    // directory is unwritable would be a worse outcome than the matcher we had
    // before. It is loud, because for an EMBEDDED host the matcher recognizes
    // nothing and this file is the only identity there is.
    //
    // A claim that could not be written is dropped from the handle: shutdown then
    // has nothing to remove, rather than deleting a path this process never owned.
    //
    // The file NAME is derived here, from the port actually bound, and not taken
    // from the caller: every reader looks a claim up as `proxy-owner-<port>.json`
    // for the port it is resolving, so a name that does not match is a claim
    // nothing consults. With `port: Some(0)` a caller cannot know the name in
    // advance — the kernel picks the port during this function — which is why the
    // caller supplies a directory and `serve` supplies the name.
    let owner_path = owner_dir.and_then(|dir| {
        let path = singleton::owner_path_in(&dir, bound.port());
        let owner = singleton::ProxyOwner {
            pid: std::process::id(),
            port: bound.port(),
            sha: build_info::SHA.to_string(),
            host,
        };
        match singleton::write_owner_file(&path, &owner) {
            Ok(()) => {
                tracing::info!(
                    path = %path.display(),
                    pid = owner.pid,
                    port = owner.port,
                    host = ?host,
                    "proxy owner file written; this proxy is identifiable without its process name"
                );
                Some(path)
            }
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    host = ?host,
                    "could not write the proxy owner file; identity falls back to command-line matching, which does NOT recognize an embedded proxy"
                );
                None
            }
        }
    });

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
        owner_path,
        tasks_joined: 0,
        tasks_aborted: 0,
        server: Some(server),
        server_finished: false,
        background,
    }))
}

/// Unit tests for what only in-crate access can pin: [`Drop for ServerHandle`].
///
/// The integration test (`tests/serve_library_path.rs`) drops a real handle and
/// watches the port refuse — which proves the loop stopped, but NOT that `Drop`
/// stopped it: the handle owns the `watch::Sender`, so dropping it closes the
/// channel and every `stop.changed()` returns `Err` on its own. That test stayed
/// green with `impl Drop` deleted.
///
/// These build a `ServerHandle` by hand and hold a **`Sender` clone**, so the
/// channel survives the drop and sender-close is off the table as an
/// explanation. Each half of `Drop` then has exactly one thing that can satisfy
/// it: `send(true)` for the tasks that watch the channel, `abort()` for the ones
/// that do not.
#[cfg(test)]
mod tests {
    use super::*;

    /// A manager with no accounts: nothing to probe, refresh or warm, and
    /// `config_path: None` so no file can be written.
    fn inert_manager() -> Arc<Manager> {
        manager_with(r#"{"accounts": []}"#)
    }

    fn manager_with(config: &str) -> Arc<Manager> {
        let config: Config = serde_json::from_str(config).expect("the inline test config parses");
        Manager::with_live_refresher(config, None)
    }

    /// A path under an existing REGULAR FILE, so any write to it fails. Never
    /// near [`affinity::default_path`] — a test may not touch the live cache.
    fn unwritable_path(tag: &str) -> PathBuf {
        let blocker = std::env::temp_dir().join(format!(
            "tcr-server-unit-{}-{tag}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::write(&blocker, b"not a directory").expect("the scratch blocker file is writable");
        blocker.join("affinity.json")
    }

    /// A handle owning `server` plus `background`, wired to `shutdown`, that can
    /// touch nothing outside this process when dropped.
    fn handle_owning(
        shutdown: watch::Sender<bool>,
        server: JoinHandle<()>,
        background: Vec<JoinHandle<()>>,
    ) -> ServerHandle {
        handle_full(shutdown, server, background, inert_manager(), None)
    }

    fn handle_full(
        shutdown: watch::Sender<bool>,
        server: JoinHandle<()>,
        background: Vec<JoinHandle<()>>,
        manager: Arc<Manager>,
        affinity_path: Option<PathBuf>,
    ) -> ServerHandle {
        ServerHandle {
            addr: "127.0.0.1:0"
                .parse()
                .expect("a literal loopback addr parses"),
            shutdown,
            manager,
            affinity_path,
            // No claim: a hand-built handle in a unit test may not delete a file
            // on shutdown, least of all one named after the live proxy's port.
            owner_path: None,
            tasks_joined: 0,
            tasks_aborted: 0,
            server: Some(server),
            server_finished: false,
            background,
        }
    }

    async fn all_finished_within(
        aborts: &[tokio::task::AbortHandle],
        budget: Duration,
    ) -> Result<(), usize> {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            let live = aborts.iter().filter(|a| !a.is_finished()).count();
            if live == 0 {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(live);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// `Drop` must ABORT. Every task here ignores the shutdown channel entirely,
    /// so nothing but `JoinHandle::abort` can end it — and a retained `Sender`
    /// clone keeps the channel open, so closing it is not available either.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_the_handle_aborts_tasks_that_ignore_the_shutdown_signal() {
        let (shutdown, _rx) = watch::channel(false);
        let keepalive = shutdown.clone();

        let deaf = || tokio::spawn(std::future::pending::<()>());
        let server = deaf();
        let background: Vec<JoinHandle<()>> = (0..3).map(|_| deaf()).collect();
        let aborts: Vec<tokio::task::AbortHandle> = std::iter::once(server.abort_handle())
            .chain(background.iter().map(|task| task.abort_handle()))
            .collect();

        let handle = handle_owning(shutdown, server, background);
        assert_eq!(
            aborts.iter().filter(|a| a.is_finished()).count(),
            0,
            "the tasks must still be running before the handle is dropped"
        );

        drop(handle);

        match all_finished_within(&aborts, Duration::from_secs(5)).await {
            Ok(()) => {}
            Err(live) => panic!(
                "Drop leaked {live} of {} tasks: a task that ignores the shutdown \
                 channel can only be stopped by `abort()`",
                aborts.len()
            ),
        }
        drop(keepalive);
    }

    /// `shutdown` must not be able to HANG on a task that will not stop.
    ///
    /// The affinity flusher writes with blocking `std::fs` inside async code, so
    /// on a full or wedged filesystem it reaches no cancellation point; an
    /// unbounded join there meant `tcr` quitting into a hang with the terminal
    /// already restored, nothing serving, and `persist_now` never reached. A
    /// `pending()` task is that condition with the filesystem left out of it.
    ///
    /// The outer `timeout` is the assertion: it is 30x the grace, so it can only
    /// fire if the join is unbounded.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_aborts_a_task_that_will_not_stop_instead_of_hanging() {
        let (shutdown, _rx) = watch::channel(false);
        let keepalive = shutdown.clone();
        let wedged = tokio::spawn(std::future::pending::<()>());
        let wedged_abort = wedged.abort_handle();
        let mut handle =
            handle_owning(shutdown, tokio::spawn(std::future::ready(())), vec![wedged]);

        let grace = Duration::from_millis(100);
        let report = tokio::time::timeout(grace * 30, handle.shutdown_within(grace))
            .await
            .expect("shutdown hung on a task that never stops");

        assert_eq!(report.tasks_joined, 1, "the accept loop stopped on its own");
        assert_eq!(
            report.tasks_aborted, 1,
            "the wedged task must be aborted, not waited for"
        );
        assert!(
            wedged_abort.is_finished() || {
                tokio::time::sleep(grace).await;
                wedged_abort.is_finished()
            },
            "the wedged task was abandoned rather than aborted"
        );
        drop(keepalive);
    }

    /// `shutdown` must be cancel-safe and re-issuable.
    ///
    /// It used to consume `self`, so a caller bounding it with a deadline — the
    /// pattern the integration test itself models — lost the handle along with
    /// the future, skipping `persist_now` and the final pin write with nothing
    /// left to retry. Here the first attempt is cancelled mid-join and the second
    /// still accounts for every task.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_shutdown_can_be_re_issued_and_still_accounts_for_every_task() {
        let (shutdown, _rx) = watch::channel(false);
        let keepalive = shutdown.clone();
        // Ignores the signal for 300ms, then stops: long enough for the first
        // attempt to be cancelled while joining it. The accept loop is already
        // finished, so the cancellation lands inside the BACKGROUND join — the
        // loop whose bookkeeping is what cancel-safety is about. (Cancelling on
        // the server join instead leaves the background vec untouched, which a
        // consuming implementation would also survive.)
        let slow = || {
            tokio::spawn(async {
                tokio::time::sleep(Duration::from_millis(300)).await;
            })
        };
        let mut handle = handle_owning(
            shutdown,
            tokio::spawn(std::future::ready(())),
            vec![slow(), slow()],
        );

        assert!(
            tokio::time::timeout(
                Duration::from_millis(20),
                handle.shutdown_within(Duration::from_secs(30))
            )
            .await
            .is_err(),
            "the first attempt was supposed to be cancelled mid-join"
        );

        // The handle survived the cancellation, and every task it still owns is
        // joinable — nothing was dropped on the floor by the abandoned future.
        let report = tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
            .await
            .expect("the re-issued shutdown must finish");
        assert_eq!(
            report.tasks_joined + report.tasks_aborted,
            3,
            "every task must be accounted for across the cancelled and re-issued \
             attempts, saw {report:?}"
        );
        assert_eq!(
            report.tasks_aborted, 0,
            "the tasks stop well inside the grace, so none should need aborting: {report:?}"
        );
        drop(keepalive);
    }

    /// FINDING 2 (round 3). `shutdown_within` promises a caller that bounds it
    /// with its own deadline and drops the future keeps a usable handle. The
    /// usage-ledger drain is synchronous through and through — a `try_send`
    /// retry loop, a `recv_timeout`, a writer-finish poll — and it can occupy
    /// the whole remaining grace. Called inline it is a multi-second stretch of
    /// an `async fn` with no await point in it, and cancellation only lands at
    /// an await: the caller's timeout, which exists to turn a wedged shutdown
    /// into a bounded quit, is silently inert, and a Tokio worker is held off
    /// its other tasks for the duration.
    ///
    /// The ledger writer here is wedged for the whole test, so the drain will
    /// take its full budget; the assertion is that the CALLER's much shorter
    /// timeout is the one that decides.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_caller_can_interrupt_a_wedged_usage_ledger_drain() {
        let dir = std::env::temp_dir().join(format!(
            "tcr-server-ledger-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let (shutdown, _rx) = watch::channel(false);
        let manager = inert_manager();
        manager.attach_usage_ledger(dir.clone(), 90);
        let wedge = manager.wedge_usage_writer_for_test();

        let mut handle = handle_full(
            shutdown,
            tokio::spawn(std::future::ready(())),
            Vec::new(),
            Arc::clone(&manager),
            None,
        );

        let started = std::time::Instant::now();
        let interrupted = tokio::time::timeout(
            Duration::from_millis(150),
            handle.shutdown_within(Duration::from_secs(5)),
        )
        .await;
        let waited = started.elapsed();

        assert!(
            interrupted.is_err(),
            "the caller's 150ms timeout must be what ends this wait, not the ledger's \
             5s budget"
        );
        assert!(
            waited < Duration::from_secs(2),
            "a blocking drain inside the async fn cannot be cancelled at all: waited \
             {waited:?}"
        );

        // The drain is still running on the blocking pool, which is the point:
        // freeing the writer lets it finish on its own.
        drop(wedge);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A FAILED final pin write must be distinguishable from affinity being off.
    ///
    /// Both were `affinity_pins_written: None`, and the difference lived only in
    /// a `tracing::warn!` that a library caller — which installs no subscriber —
    /// never sees. So total pin loss reported as a clean shutdown.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_final_pin_write_is_reported_not_swallowed() {
        let path = unwritable_path("flush-fails");
        let (shutdown, _rx) = watch::channel(false);
        let mut handle = handle_full(
            shutdown,
            tokio::spawn(std::future::ready(())),
            Vec::new(),
            manager_with(r#"{"sessionAffinity": true, "accounts": []}"#),
            Some(path.clone()),
        );

        let report = handle.shutdown().await;
        assert!(
            report.affinity.failed(),
            "a pin write to {} cannot have succeeded; report said {:?}",
            path.display(),
            report.affinity
        );
        assert_eq!(
            report.affinity.pins_written(),
            None,
            "a failed write wrote no pins"
        );
        assert_ne!(
            report.affinity,
            AffinityFlush::Disabled,
            "a failed write must not read as 'affinity is off'"
        );
    }

    /// With no pin cache path the shutdown flush must be a no-op, not a write to
    /// some default — this is the guard on `affinity_path: None` meaning
    /// "in memory only".
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_pin_cache_path_means_no_final_write() {
        let (shutdown, _rx) = watch::channel(false);
        let mut handle = handle_full(
            shutdown,
            tokio::spawn(std::future::ready(())),
            Vec::new(),
            manager_with(r#"{"sessionAffinity": true, "accounts": []}"#),
            None,
        );
        assert_eq!(handle.shutdown().await.affinity, AffinityFlush::Disabled);
    }

    /// `Drop` must also SIGNAL — the accept loop and every background loop stop
    /// on `stop.changed()`, and the sender-close that currently masks this is
    /// removed here by holding a `Sender` clone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_the_handle_signals_tasks_that_watch_the_shutdown_channel() {
        let (shutdown, _rx) = watch::channel(false);
        let keepalive = shutdown.clone();

        // Ends only on a `true`: a bare close would leave `changed()` returning
        // `Err` and this loop spinning back to `pending`, never finishing.
        let watcher = |mut stop: watch::Receiver<bool>| {
            tokio::spawn(async move {
                loop {
                    if stop.changed().await.is_err() {
                        std::future::pending::<()>().await;
                    }
                    if *stop.borrow_and_update() {
                        return;
                    }
                }
            })
        };
        let server = watcher(shutdown.subscribe());
        let background: Vec<JoinHandle<()>> =
            (0..2).map(|_| watcher(shutdown.subscribe())).collect();
        let aborts: Vec<tokio::task::AbortHandle> = std::iter::once(server.abort_handle())
            .chain(background.iter().map(|task| task.abort_handle()))
            .collect();

        drop(handle_owning(shutdown, server, background));

        assert!(
            *keepalive.borrow(),
            "Drop must publish `true` on the shutdown channel, not merely close it"
        );
        if let Err(live) = all_finished_within(&aborts, Duration::from_secs(5)).await {
            panic!("{live} task(s) never saw the shutdown signal");
        }
        drop(keepalive);
    }

    // --- background task supervision (part 1) --------------------------------

    /// A `MakeWriter` that keeps what was written, so a test can inspect
    /// tracing output without capturing the process's real stdout. Same shape
    /// as `main.rs`'s/`proxy.rs`'s `SharedBuf` — duplicated locally because
    /// those are private to their own test modules.
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

    /// THE catch: `supervise` must swallow a panic from the wrapped future so
    /// the spawned task's own `JoinHandle` completes **normally** (this is
    /// what lets `ServerHandle::shutdown_within`/`Drop` stay unchanged — see
    /// `supervise`'s doc-comment) while still logging which task died and why,
    /// via `tracing::error!`. `#[tokio::test]` defaults to current-thread, so
    /// the thread-local subscriber set below stays in effect for the spawned
    /// task too (same reasoning as `proxy.rs`'s usage-log test).
    #[tokio::test]
    async fn a_panicking_background_task_is_logged_and_not_respawned() {
        let sink = SharedBuf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(sink.clone())
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let handle = tokio::spawn(supervise("test-quota-prober", async {
            panic!("boom: deliberate test panic");
        }));

        let result = handle.await;
        assert!(
            result.is_ok(),
            "supervise() must catch the panic so the spawned task's own \
             JoinHandle completes normally instead of returning a JoinError; \
             saw {result:?}"
        );

        let log = sink.contents();
        assert!(
            log.contains("test-quota-prober"),
            "the log line must name which task died: {log:?}"
        );
        assert!(
            log.contains("background task panicked"),
            "the log line must say the task panicked: {log:?}"
        );
        assert!(
            log.contains("boom: deliberate test panic"),
            "the log line must carry the panic message: {log:?}"
        );
    }

    /// A task that finishes WITHOUT panicking must log nothing — `supervise`
    /// only reacts to a panic, never to an ordinary return (which is exactly
    /// what every one of these loops does today on a clean shutdown: the
    /// `tokio::select!` around `stop.changed()` returns `()`, not a panic).
    #[tokio::test]
    async fn a_clean_return_is_not_logged_as_a_panic() {
        let sink = SharedBuf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(sink.clone())
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let handle = tokio::spawn(supervise("test-clean-task", async {}));
        handle
            .await
            .expect("a non-panicking task never returns a JoinError");

        assert!(
            sink.contents().is_empty(),
            "a clean return must not be logged as a panic: {:?}",
            sink.contents()
        );
    }

    // --- boot-line knob completeness (part 2) ---------------------------------

    /// A config with every new boot-line knob set to a **non-default** value,
    /// plus one fabricated (non-real) account so `lockAccount`/`controlAccount`
    /// each resolve to a real index instead of logging the "did not match any
    /// account" error path. Port 0 (ephemeral) so this test can never contend
    /// the live proxy's port, and `tls: Disabled` so it touches no TLS material
    /// on disk. `access_token` is a fixed non-secret placeholder — this repo is
    /// public (see `CLAUDE.md`).
    fn boot_line_test_config() -> Config {
        serde_json::from_str(
            r#"{
                "proxy": { "port": 0 },
                "sessionAffinity": false,
                "revalidationServe": false,
                "loadBalanceMigration": true,
                "quotaProbeSeconds": 1234,
                "warmupSeconds": 777,
                "pacing": { "minSpacingMs": 500 },
                "accountThrottle": {},
                "fleetThrottle": {},
                "lockAccount": "test-fixture-account",
                "controlAccount": "test-fixture-account",
                "accounts": [
                    {
                        "name": "test-fixture-account",
                        "accessToken": "not-a-real-token"
                    }
                ]
            }"#,
        )
        .expect("the inline test config parses")
    }

    /// THE bite: every knob this unit added to the "server started" boot line
    /// must actually be ON that line, with the value this config set — not
    /// merely present in config/`Manager`. A caller diagnosing a cold-cache
    /// incident reads this ONE line (see the module doc-comment on why it is
    /// one line, not several); a knob resolved correctly but never logged is
    /// invisible to exactly that read.
    #[tokio::test]
    async fn boot_line_carries_every_new_knob_with_its_configured_value() {
        let sink = SharedBuf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(sink.clone())
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let mut handle = serve(ServeOptions {
            tls: TlsSetup::Disabled,
            ..ServeOptions::new(boot_line_test_config())
        })
        .await
        .expect("bind must succeed on an ephemeral port")
        .expect_started();

        let log = sink.contents();
        let boot_line = log
            .lines()
            .find(|line| line.contains("server started"))
            .unwrap_or_else(|| panic!("no \"server started\" line in captured log: {log:?}"));

        for expected in [
            "session_affinity=false",
            "revalidation_serve=false",
            "load_balance_migration=true",
            "quota_probe_seconds=1234",
            "warmup_seconds=777",
            "pacing_active=true",
            // An empty `{}` object is the documented per-bucket escape hatch
            // (see `ThrottleConfig`'s doc-comment) — it overrides that bucket's
            // default-ON setting to fully inert. `throttle_active` is the OR of
            // the two buckets, so BOTH must be `{}` to make it false, and
            // `false` here IS the non-default assertion: the default build's
            // boot line reads `throttle_active=true`.
            //
            // Note this fixture goes through `serde_json::from_str`, not
            // `config::load`, so it does NOT get the legacy-`throttle`-key
            // rejection. That is exactly how this test caught the rename: a
            // stale `"throttle": {}` here landed silently in the `extra`
            // catch-all and both buckets defaulted back ON.
            "throttle_active=false",
            "lock_account=\"test-fixture-account\"",
            "control_account=\"test-fixture-account\"",
        ] {
            assert!(
                boot_line.contains(expected),
                "boot line missing {expected:?}; full line: {boot_line:?}"
            );
        }

        handle.shutdown().await;
    }
}
