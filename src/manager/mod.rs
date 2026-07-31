//! Account rotation, token freshness, and the live state the proxy/TUI share.
//!
//! Selection order:
//!   1. lowest `priority` value wins (operator-controlled; default 0);
//!   2. within a priority tier, the least-recently-*selected* account goes next,
//!      so consecutive requests fan out across the fleet instead of hammering one
//!      account. (A single request barely moves a weekly bar, so ordering by
//!      quota headroom would pin one account until its bar caught up.) A
//!      never-selected account sorts first; soonest weekly reset is the
//!      cold-start tiebreak.
//!
//! Eligibility = not disabled, not in an active rate-limit hold, not in a hard
//! `error` state, and under its threshold (per-account `switchThreshold` else the
//! global one), all evaluated **live** against `now`.
//!
//! Token refresh is **coalesced per account**: a `tokio::sync::Mutex` per account
//! plus a re-check under the guard means N concurrent requests on the same
//! hard-expired account trigger exactly one upstream refresh (bug the JS avoided
//! with a shared `_refreshPromise`).
//!
//! Usage/request counters increment on the **actual serving index** passed by the
//! proxy, never a mutated "current" pointer — that is bug #3 designed out.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use time::OffsetDateTime;
use tokio::sync::{Mutex as AsyncMutex, Notify};

use crate::config::{self, Config, PacingConfig, ThrottleConfig};
use crate::oauth::{self, LiveRefresher, TokenRefresher, Tokens};
use crate::probe::{LiveUsageProber, ProbeStatus, Usage, UsageProber};
use crate::quota::Quota;
use crate::stats::{
    AccountSnapshot, GateReason, RequestLogEntry, SessionKind, SessionSnapshot, StatsSnapshot,
};
use crate::warmer::{AccountWarmer, LiveWarmer};

mod probing;
mod refresh;
mod select;
mod snapshot;
mod state;
mod throttle;
mod usage;
mod warm;

const REQUEST_LOG_CAPACITY: usize = 200;

/// Upper bound on a single rate-limit hold. A 429 `retry-after` larger than this
/// is clamped so an account is always revalidated within the window rather than
/// pinned out for hours with no live request to clear the hold (finding #5).
const MAX_RATE_LIMIT_HOLD_SECONDS: i64 = 3600;

/// The dividing line between a rate-limit hold a session should WAIT OUT on its
/// pinned account and one long enough that re-keying is the cheaper trade:
/// **Anthropic's default ephemeral prompt-cache TTL, 5 minutes.**
///
/// A hold is a TIMER, not a death, and the timers we arm are mostly short — a
/// no-guidance transient 429 parks `NO_GUIDANCE_HOLD_SECS` (15s) plus jitter, and
/// a `retry-after` park is clamped to 300s ([`crate::proxy`]). Discarding a
/// per-account prompt cache that will still be warm 15 seconds later, and never
/// returning to it, is the same cache-loss defect the soft gates already fixed,
/// reached through a different signal.
///
/// So a hold with LESS than this remaining is SOFT — divert this ONE request and
/// keep the pin, and the session comes home warm. A hold with this much or MORE
/// remaining stays HARD: the old cache is dead by the time the account frees, and
/// re-keying settles the session on one account that then warms up, whereas
/// holding a long-dead pin would divert through the LRU pick on every turn and
/// scatter the conversation cold across the fleet.
///
/// 300 is the CONSERVATIVE choice. Anthropic's 1-hour extended cache would justify
/// a much larger value (up to `MAX_RATE_LIMIT_HOLD_SECONDS`), but we cannot tell
/// from a hold which TTL a given session's prefix was written under. Erring low
/// costs at most one extra diverted request on a hold that would in fact have come
/// home warm; erring high costs a whole conversation prefix.
const CACHE_WARM_HOLD_SECS: i64 = 300;

/// How long a *transient* refresh failure holds off further refreshes of the
/// same account. A transient failure leaves the token unchanged, so the
/// access-token coalescing guard can't stop a follower queued on the lock from
/// re-POSTing the single-use refresh token; this short window collapses the
/// concurrent batch to one POST. Long enough to swallow a burst, short enough
/// that a later sequential retry (the account is not stuck) stays invisible.
const REFRESH_RETRY_COOLDOWN_MS: i64 = 2_000;
/// First recovery attempt for an `Error` row, and the floor the backoff starts at.
const ERROR_REPROBE_BASE_MS: i64 = 60_000;
/// Ceiling for the recovery backoff — a permanently dead credential is retried at
/// most ~twice an hour, so re-probing can never hammer the OAuth endpoint.
const ERROR_REPROBE_CAP_MS: i64 = 30 * 60_000;

/// Hard state of an account.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AccountStatus {
    /// Healthy and in rotation.
    Active,
    /// Temporarily held out by a 429 (until `rate_limited_until_ms`).
    Throttled,
    /// Dead credential — sidelined until a config reload / re-login.
    Error,
}

impl AccountStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            AccountStatus::Active => "active",
            AccountStatus::Throttled => "throttled",
            AccountStatus::Error => "error",
        }
    }
}

/// The mutable runtime state for one account (credentials + learned quota +
/// counters). Kept separate from the persisted [`config::Account`].
#[derive(Debug, Clone)]
pub struct AccountRuntime {
    pub name: String,
    pub account_type: String,
    /// The pooled account's own UUID, injected into the outbound request body's
    /// `metadata.user_id.account_uuid` so it matches the token we serve with
    /// (see [`crate::account_uuid`]). `None` leaves the body unchanged.
    pub account_uuid: Option<String>,
    /// The org this pooled account is scoped to (UUID and name). Part of the
    /// account's identity so token rotation persists to the RIGHT config entry
    /// when the same email is logged into two orgs (finding #9).
    pub org_uuid: Option<String>,
    pub org_name: Option<String>,
    pub priority: i64,
    pub disabled: bool,
    pub switch_threshold: Option<f64>,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at_ms: Option<i64>,
    pub status: AccountStatus,
    pub quota: Quota,
    /// Latched `true` the first time this account's quota was actually READ from
    /// the usage endpoint (in [`Manager::apply_usage`], which runs only on a
    /// successful probe). Never cleared — once we have read an account's quota we
    /// have read it, and a later probe FAILURE deliberately leaves the
    /// last-learned windows in place rather than blanking them.
    ///
    /// This, and NOT `probe_status`, is [`Manager::warm_targets`]' boot gate.
    /// `record_probe` stamps `Error`/`Timeout`/`RateLimited` on a FAILED probe too,
    /// so `probe_status != Never` while `quota` is still `Quota::default()` — and a
    /// gate keyed on that lifts on blank quota, which is the boot burst coming
    /// straight back (a real fleet-wide false-error sweep is documented in
    /// `probing.rs`). It is equally NOT `quota.five_hour.is_some()`: `apply_bucket`
    /// early-returns when the endpoint omits the bucket, so an account whose
    /// responses never carry a 5h bucket would become permanently warm-INELIGIBLE —
    /// a dark feature that reads as enabled.
    pub quota_known: bool,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Cache-read input tokens (a SUBSET of `input_tokens`, not additional quota).
    /// Accumulated so the prompt-cache hit ratio (`cache_read / input_tokens`) is
    /// visible per account; `> 0` on post-first turns means the cache is warm.
    pub cache_read_tokens: u64,
    /// Cache-creation input tokens (also a subset of `input_tokens`).
    pub cache_creation_tokens: u64,
    pub requests: u64,
    pub last_used_ms: Option<i64>,
    /// Monotonic tick of the last time [`Manager::select`] *chose* this account
    /// (0 = never chosen). Ordering rotation by this — not by a wall-clock stamp —
    /// makes the spread clock-granularity-independent: a burst of selects in the
    /// same millisecond still fans out, because each pick takes the next tick.
    pub last_selected_seq: u64,
    /// Requests currently being served on this account. Incremented when the proxy
    /// picks this account to forward a request and decremented on completion via
    /// the RAII [`InFlightGuard`] (every drop path — success, rotate, error, panic).
    /// Mutated ONLY under the accounts write-lock (no second lock). Read by
    /// [`Manager::eligible`] to skip an account at/over the pacing concurrency cap.
    pub in_flight: u32,
    /// Wall-clock ms of the last time [`Manager::select`] chose this account to
    /// serve (0 = never). Read by [`Manager::eligible`] to enforce the pacing
    /// min-spacing between two selects of the SAME account.
    pub last_served_ms: i64,
    pub rate_limited_until_ms: Option<i64>,
    /// A short self-clearing cooldown after a *transient* refresh failure. A
    /// transient failure leaves the access token UNCHANGED, so the access-token
    /// guard in [`Manager::ensure_fresh_inner`] can't catch a follower already
    /// queued on the coalescing lock — it would re-POST the same single-use
    /// refresh token. This stamp (set to `now + REFRESH_RETRY_COOLDOWN_MS`)
    /// collapses the concurrent batch to one POST, yet self-clears by time so a
    /// later sequential retry still refreshes. Cleared on a successful refresh.
    pub refresh_retry_after_ms: Option<i64>,
    /// When an `Error` row may next attempt RECOVERY (a forced refresh). `Error`
    /// means "the refresh token was rejected", but a rejection can be transient (a
    /// token-war race, an upstream hiccup) — so it must not be a life sentence.
    /// `None` on a non-errored row. See [`Manager::grow_error_backoff`].
    pub error_retry_after_ms: Option<i64>,
    /// The current recovery backoff, doubled on each rejected recovery attempt and
    /// capped, so a genuinely dead credential costs ~2 refresh POSTs/hour rather
    /// than hammering the OAuth endpoint. `0` when the row is not errored.
    pub error_backoff_ms: i64,
    pub probe_status: ProbeStatus,
    pub last_probe_ms: Option<i64>,
    pub probe_error: Option<String>,
}

impl AccountRuntime {
    fn from_config(account: &config::Account) -> Self {
        Self {
            name: account.name.clone(),
            account_type: account.account_type.clone(),
            account_uuid: account.account_uuid.clone(),
            org_uuid: account.org_uuid.clone(),
            org_name: account.org_name.clone(),
            priority: account.priority.unwrap_or(0),
            disabled: account.disabled.unwrap_or(false),
            switch_threshold: account.switch_threshold,
            access_token: account.access_token.clone(),
            refresh_token: account.refresh_token.clone(),
            // Defensive normalize: a stored value in seconds becomes ms.
            expires_at_ms: account.expires_at.map(oauth::normalize_expires_at),
            status: AccountStatus::Active,
            quota: Quota::default(),
            // Nothing restores the last known windows across a restart, so at boot
            // every account's quota is genuinely unread.
            quota_known: false,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            requests: 0,
            last_used_ms: None,
            last_selected_seq: 0,
            in_flight: 0,
            last_served_ms: 0,
            rate_limited_until_ms: None,
            refresh_retry_after_ms: None,
            error_retry_after_ms: None,
            error_backoff_ms: 0,
            probe_status: ProbeStatus::Never,
            last_probe_ms: None,
            probe_error: None,
        }
    }
}

/// Per-session serving stats, keyed on the stable session key. Independent of
/// `affinity` (which holds only the current pin) so routing is unaffected.
struct SessionStat {
    account_idx: usize,
    requests: u64,
    last_seen_ms: i64,
    /// [`SessionKind::Stable`] when this session was keyed on a stable client identity
    /// (x-api-key / `metadata.user_id`); [`SessionKind::Fallback`] when there was none
    /// and the request served unpinned. DISPLAY provenance only — never routing.
    kind: SessionKind,
}

/// Owns all rotation state and the machinery to refresh tokens and reach upstream.
pub struct Manager {
    accounts: RwLock<Vec<AccountRuntime>>,
    /// One coalescing gate per account, indexed by account position.
    refresh_locks: Vec<Arc<AsyncMutex<()>>>,
    refresher: Arc<dyn TokenRefresher>,
    /// Reads each account's quota from the zero-spend usage endpoint on a timer.
    prober: Arc<dyn UsageProber>,
    /// Warms idle accounts to keep their 5h window live (opt-in; see [`Self::warm_all`]).
    warmer: Arc<dyn AccountWarmer>,
    /// Ensures two keep-warm sweeps never overlap (mirrors the JS `_running` flag).
    warm_in_flight: AtomicBool,
    /// Wakes the keep-warm loop the moment an account's quota is first READ, so the
    /// boot gate on [`Self::warm_targets`] costs a probe cycle rather than a full
    /// `warmupSeconds` of dark time.
    ///
    /// Without it the gate is a silent kill switch: the warm loop's ticker fires its
    /// first tick immediately, that sweep necessarily finds no targets (no quota has
    /// been read yet), and `MissedTickBehavior::Skip` puts the next tick a whole
    /// interval away — so at `warmupSeconds: 3600` a proxy restarted more often than
    /// hourly warms NOTHING, ever, while reading as enabled in config and TUI.
    ///
    /// `Notify` rather than a channel because a permit stored by `notify_one` before
    /// the loop is waiting is consumed by its next `notified()` — the wake survives
    /// the race between the first probe and the loop reaching its `select!`, and a
    /// `Notified` dropped by a losing `select!` branch hands its permit back. Fired
    /// ONLY on the false→true `quota_known` flip, which happens at most once per
    /// account per process, so it can never become a self-feeding loop of sweeps.
    warm_wake: Notify,
    /// Client used for upstream forwarding — deliberately no total timeout so
    /// long SSE streams are never cut (an idle guard belongs on the read side).
    http: reqwest::Client,
    /// The persisted config, kept so token refreshes can be written back with
    /// every unmodelled field intact.
    config: Mutex<Config>,
    config_path: Option<PathBuf>,
    upstream: String,
    proxy_api_key: Option<String>,
    global_threshold: f64,
    /// Per-account request pacing knobs, snapshotted from the config at
    /// construction. Default (all `None`) → inert → selection is byte-identical to
    /// the no-pacing build. See [`config::PacingConfig`].
    pacing: PacingConfig,
    /// Global outbound throttle knobs, snapshotted from config at construction
    /// (default all-`None` → inert → byte-identical to the no-throttle build).
    throttle: ThrottleConfig,
    /// Resolved hard-lock target (index of the account named by `config.lockAccount`),
    /// or `None` when unlocked / the name did not match. When `Some(i)`, [`Self::select`]
    /// returns `i` unconditionally (bypassing rotation/affinity/migration) — no failover.
    locked_idx: Option<usize>,
    /// GCRA theoretical-arrival-time (epoch ms) for the global outbound throttle.
    /// Guarded by an async mutex held ONLY for the O(1) slot update, released
    /// before any sleep so concurrent callers stagger and sleep concurrently.
    throttle_tat_ms: AsyncMutex<i64>,
    log: Mutex<VecDeque<RequestLogEntry>>,
    current: Mutex<Option<usize>>,
    /// Monotonic counter handed out one tick at a time by [`Manager::select`] to
    /// stamp the account it picks, so the next select prefers a different one
    /// (load spread). Starts at 1 so 0 reads unambiguously as "never selected".
    select_seq: AtomicU64,
    /// Session affinity (opt-in): a stable identity hash → `(account index it is
    /// pinned to, last-touch ms)`. Populated only when `sessionAffinity` is enabled
    /// and a `SessionKey` extension flows in; empty (and never consulted) otherwise,
    /// so the disabled path is provably inert. Bounded by a size cap
    /// (`AFFINITY_CAP`) + LRU-by-last-touch eviction in [`Manager::select`] — stable
    /// pins intentionally SURVIVE reconnects (that is the point of a stable key), so
    /// there is no disconnect-release. Kept a plain `std::sync::Mutex` and **never**
    /// held while the accounts lock is taken (read the pin, drop this lock, then do
    /// eligibility) so the two can never deadlock.
    affinity: Mutex<HashMap<u64, (usize, i64)>>,
    /// Per-session serving stats (session key → account/count/last-seen), for
    /// live per-session visibility in the TUI. Separate from `affinity` so the
    /// routing pin stays byte-for-byte unchanged; bounded in `record_served`.
    sessions: Mutex<HashMap<u64, SessionStat>>,
    /// Monotonic session-key source handed out by [`Manager::next_session_key`],
    /// one per connection. Starts at 1 so the first key is a nonzero, unique u64.
    session_seq: AtomicU64,
    /// Anti-storm valve for over-threshold revalidation-serve: epoch-ms before which
    /// no new revalidation serve is issued. See [`Manager::select_revalidation`].
    next_revalidation_at_ms: std::sync::atomic::AtomicI64,
}

/// Resets the keep-warm in-flight flag on drop, so a sweep that unwinds early
/// still clears the guard (mirrors the JS `finally { _running = false }`).
struct WarmInFlightGuard<'a>(&'a AtomicBool);

impl Drop for WarmInFlightGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// RAII counter for a request currently being served on one account. Taken via
/// [`Manager::enter_in_flight`] when the proxy picks an account to forward to, it
/// decrements that account's `in_flight` on Drop — so EVERY exit path of a serve
/// attempt (success, 429/401 rotate, transport error, panic-unwind) releases the
/// slot and a counter can never leak and strand an account out of rotation.
/// Modeled on [`WarmInFlightGuard`]; the count lives under the accounts write-lock
/// (no second lock — preserves the no-lock-nesting discipline).
///
/// Holds an owned `Arc<Manager>` (not a borrow) so the guard is `'static` and can be
/// MOVED into a streamed response body — its Drop then fires at stream completion
/// rather than at handler return. The shared `Arc` is the one the proxy already
/// threads into the handler; no second lock and no detached counter are introduced.
pub struct InFlightGuard {
    manager: Arc<Manager>,
    idx: usize,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let mut accounts = self
            .manager
            .accounts
            .write()
            .expect("accounts lock poisoned");
        if let Some(account) = accounts.get_mut(self.idx) {
            // saturating_sub so a double-drop / underflow can never wrap to u32::MAX
            // and pin the account out forever.
            account.in_flight = account.in_flight.saturating_sub(1);
        }
    }
}

fn odt_to_ms(now: OffsetDateTime) -> i64 {
    (now.unix_timestamp_nanos() / 1_000_000) as i64
}

/// Pure GCRA slot computation for the global outbound throttle. Given the current
/// theoretical-arrival-time `tat_ms`, the arrival `now_ms`, the emission interval
/// `spacing_ms` (T) and bucket capacity `burst` (B), returns
/// `(new_tat_ms, allow_at_ms)`. The caller advances the stored TAT to `new_tat_ms`
/// and sleeps until `allow_at_ms` if it is in the future. `burst` requests admit
/// instantly after idle (allow_at <= now), then one per T.
fn throttle_slot(tat_ms: i64, now_ms: i64, spacing_ms: i64, burst: u32) -> (i64, i64) {
    let tau = spacing_ms * (burst.max(1) as i64 - 1); // burst tolerance (B-1)*T
    let base = tat_ms.max(now_ms); // can't schedule in the past
    (base + spacing_ms, base - tau) // (new TAT, earliest allowed)
}

fn ms_to_odt(ms: i64) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000).ok()
}

/// Fold a u64 session key to a short, stable hex id for display (24 bits).
fn short_session_id(key: u64) -> String {
    format!("{:06x}", ((key ^ (key >> 40)) & 0xFF_FFFF))
}

impl Manager {
    /// Build a manager over `config`, using `refresher` for token refresh,
    /// `prober` for background usage reads, and persisting refreshes to
    /// `config_path` (if given).
    pub fn new(
        config: Config,
        refresher: Arc<dyn TokenRefresher>,
        prober: Arc<dyn UsageProber>,
        warmer: Arc<dyn AccountWarmer>,
        config_path: Option<PathBuf>,
    ) -> Arc<Self> {
        let accounts: Vec<AccountRuntime> = config
            .accounts
            .iter()
            .map(AccountRuntime::from_config)
            .collect();
        Self::assemble(config, refresher, prober, warmer, config_path, accounts)
    }

    /// Assemble the `Arc<Manager>` from an already-built runtime vec. Shared by
    /// [`Self::new`] (which derives the runtimes from `config`) and
    /// [`Self::from_runtimes`] (which is handed pre-seeded runtimes for the demo).
    fn assemble(
        config: Config,
        refresher: Arc<dyn TokenRefresher>,
        prober: Arc<dyn UsageProber>,
        warmer: Arc<dyn AccountWarmer>,
        config_path: Option<PathBuf>,
        accounts: Vec<AccountRuntime>,
    ) -> Arc<Self> {
        let refresh_locks = accounts
            .iter()
            .map(|_| Arc::new(AsyncMutex::new(())))
            .collect();
        let upstream = config.upstream.clone();
        let proxy_api_key = config.proxy.api_key.clone();
        let global_threshold = config.switch_threshold;
        let pacing = config.pacing.clone();
        let throttle = config.throttle.clone();

        let locked_idx = config.lock_account.as_ref().and_then(|name| {
            let idx = accounts.iter().position(|a| a.name == *name);
            if idx.is_none() {
                let names: Vec<&str> = accounts.iter().map(|a| a.name.as_str()).collect();
                tracing::error!(
                    lock_account = %name, available = ?names,
                    "lockAccount name did not match any account — running UNLOCKED (normal routing)"
                );
            }
            idx
        });
        // Capture the locked account name BEFORE `accounts` is moved into the struct.
        let locked_name = locked_idx.and_then(|i| accounts.get(i).map(|a| a.name.clone()));

        let manager = Arc::new(Self {
            accounts: RwLock::new(accounts),
            refresh_locks,
            refresher,
            prober,
            warmer,
            warm_in_flight: AtomicBool::new(false),
            warm_wake: Notify::new(),
            // no_proxy(): reqwest honors HTTPS_PROXY/HTTP_PROXY by default. We ARE the
            // proxy — routing our upstream through an ambient proxy (e.g. the JS
            // teamclaude on :3456) loops us through the thing we replace and every
            // request dies as "upstream unreachable". Always reach Anthropic directly.
            http: reqwest::Client::builder()
                .no_proxy()
                // Cap only the CONNECT phase. A blackholed route (no RST, no reply)
                // otherwise stalls the attempt until the OS TCP timeout, and with a
                // retry budget of `account_count * 2 + 4` that is many minutes of a
                // hung request. `oauth.rs` and `probe.rs` both already set one.
                //
                // DELIBERATELY NOT a total `.timeout(...)`, and do not add one: these
                // responses are long-lived SSE streams that legitimately run longer
                // than any bound worth setting, and a total timeout would truncate
                // them mid-stream. `connect_timeout` cannot — it applies only before
                // the response headers arrive, so once a stream is flowing it is out
                // of the picture.
                .connect_timeout(std::time::Duration::from_secs(10))
                // Keep the single HTTP/2 connection to Anthropic warm across
                // interactive think-time pauses. reqwest reaps idle connections
                // after 90s by default, but a coding session routinely pauses
                // longer — so the next request would pay a fresh TCP+TLS handshake
                // (~100-300ms). h2 keep-alive PINGs + a 5-min idle timeout hold the
                // connection open so a post-pause request skips the reconnect.
                .http2_keep_alive_interval(std::time::Duration::from_secs(30))
                .http2_keep_alive_while_idle(true)
                .pool_idle_timeout(std::time::Duration::from_secs(300))
                .build()
                .expect("build reqwest client"),
            config: Mutex::new(config),
            config_path,
            upstream,
            proxy_api_key,
            global_threshold,
            pacing,
            throttle,
            locked_idx,
            throttle_tat_ms: AsyncMutex::new(0),
            log: Mutex::new(VecDeque::with_capacity(REQUEST_LOG_CAPACITY)),
            current: Mutex::new(None),
            select_seq: AtomicU64::new(1),
            affinity: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            session_seq: AtomicU64::new(1),
            next_revalidation_at_ms: std::sync::atomic::AtomicI64::new(0),
        });

        if let (Some(i), Some(name)) = (locked_idx, locked_name) {
            tracing::warn!(
                account = %name, idx = i,
                "ACCOUNT LOCK ACTIVE — all traffic pinned to one account; rotation/affinity/migration bypassed, no failover"
            );
        }

        manager
    }

    /// Convenience constructor for the real binary (live OAuth refresher + live
    /// usage prober).
    pub fn with_live_refresher(config: Config, config_path: Option<PathBuf>) -> Arc<Self> {
        Self::new(
            config,
            Arc::new(LiveRefresher::new()),
            Arc::new(LiveUsageProber::new()),
            Arc::new(LiveWarmer::new()),
            config_path,
        )
    }

    /// Build a manager from pre-seeded runtime rows, for the `tcr demo` dashboard
    /// (`src/demo.rs`). The live refresher/prober/warmer are held but NEVER invoked
    /// — the demo only ever calls [`Self::snapshot`] and [`Self::set_disabled`], so
    /// no token, probe, or network I/O ever happens. `config_path = None` makes
    /// every persist a no-op, so nothing can be written to disk.
    pub fn from_runtimes(accounts: Vec<AccountRuntime>) -> Arc<Self> {
        let config: Config = serde_json::from_str("{}")
            .expect("an empty JSON object is always a valid default config");
        Self::assemble(
            config,
            Arc::new(LiveRefresher::new()),
            Arc::new(LiveUsageProber::new()),
            Arc::new(LiveWarmer::new()),
            None,
            accounts,
        )
    }

    /// Seed one fake live session into the sessions map, for the `tcr demo`
    /// dashboard alone — never touched by the real serving path (which upserts
    /// sessions in [`Self::record_served`]). Lets the demo paint a populated
    /// sessions pane without routing any real traffic.
    pub fn seed_session(
        &self,
        key: u64,
        account_idx: usize,
        requests: u64,
        last_seen_ms: i64,
        kind: SessionKind,
    ) {
        let mut sessions = self.sessions.lock().expect("sessions lock poisoned");
        sessions.insert(
            key,
            SessionStat {
                account_idx,
                requests,
                last_seen_ms,
                kind,
            },
        );
    }

    /// Hard state of account `idx` — lets the proxy skip an account that a
    /// refresh just proved dead (`Error`) without wasting an upstream round-trip.
    pub fn account_status(&self, idx: usize) -> Option<AccountStatus> {
        self.accounts
            .read()
            .expect("accounts lock poisoned")
            .get(idx)
            .map(|a| a.status)
    }

    /// A `retry-after` hint (seconds) for a synthetic 429 when every account is
    /// exhausted: the soonest instant at which SOME account genuinely re-enters
    /// rotation, clamped to at least 1s, defaulting to 60s when nothing is known.
    ///
    /// Honest by construction: it minimises over each account's
    /// [`Self::account_gate`] `free_at` — the instant ALL of *that* account's
    /// active gates clear — instead of the raw min over every window's reset. The
    /// raw-min was bug-shaped: it counted a 5-hour reset of an account that stays
    /// gated on its weekly bucket, and the reset of an `Error`/disabled account
    /// that never self-frees at all, so it promised a recovery that would not
    /// happen. Accounts that contribute nothing here (`Ok`, `Login`, `Disabled`,
    /// or a gating window with no known reset) are correctly skipped.
    ///
    /// `is_fable` scopes the evaluation exactly as selection does: only a Fable
    /// request is gated by the model-scoped weekly (`7d_oi`) bucket, so an
    /// all-Fable-exhausted fleet reports that bucket's reset while non-Fable
    /// traffic ignores it.
    pub fn retry_after_hint(&self, now: OffsetDateTime, is_fable: bool) -> i64 {
        let now_ms = odt_to_ms(now);
        let accounts = self.accounts.read().expect("accounts lock poisoned");
        let soonest = accounts
            .iter()
            .filter_map(|account| {
                let threshold = account.switch_threshold.unwrap_or(self.global_threshold);
                let (_, free_at) = Self::account_gate(account, threshold, now, now_ms, is_fable);
                free_at.map(odt_to_ms)
            })
            .filter(|&at| at > now_ms)
            .min();
        match soonest {
            Some(at) => ((at - now_ms + 999) / 1000).max(1),
            None => 60,
        }
    }

    /// Mark account `idx` as serving one more in-flight request: bump its
    /// `in_flight` count and stamp `last_served_ms`, returning an [`InFlightGuard`]
    /// that decrements the count on Drop. The proxy takes this the moment it picks
    /// an account to forward to, so the guard's lifetime spans the whole serve
    /// attempt and releases the slot on every exit path (success, rotate, error,
    /// panic). For a terminal SSE (`text/event-stream`) serve the proxy MOVES the
    /// owned guard into the streamed body, so the decrement fires at stream
    /// completion — not at handler return, when axum has yet to poll the body.
    /// Mutates only under the accounts write-lock (no second lock).
    pub fn enter_in_flight(self: &Arc<Self>, idx: usize) -> InFlightGuard {
        {
            let mut accounts = self.accounts.write().expect("accounts lock poisoned");
            if let Some(account) = accounts.get_mut(idx) {
                account.in_flight = account.in_flight.saturating_add(1);
                account.last_served_ms = crate::now_ms();
            }
        }
        InFlightGuard {
            manager: Arc::clone(self),
            idx,
        }
    }

    /// Flush the refreshed TOKENS to disk. Token refreshes already persist
    /// incrementally via [`Self::persist_tokens`], so this is the
    /// belt-and-suspenders final flush on shutdown (DESIGN §main). A missing
    /// `config_path` (tests, corrupt-source boot) is a silent no-op so a corrupt
    /// user file is never clobbered with defaults.
    ///
    /// Writes via [`config::save_tokens`], NOT [`config::save`]: the in-memory
    /// `Config` is a boot-time snapshot, so flushing it whole would revert every
    /// setting the user edited while the proxy was running.
    pub fn persist_now(&self) {
        let Some(path) = &self.config_path else {
            return;
        };
        // Save UNDER the lock (not clone-then-save-unlocked) so a shutdown flush
        // can't race a concurrent persist_tokens and clobber a just-rotated token.
        // The lock also serializes save_tokens' read-modify-write of the file.
        let config = self.config.lock().expect("config lock poisoned");
        if let Err(err) = config::save_tokens(path, &config) {
            tracing::error!(error = %err, "failed to flush config on shutdown");
        }
    }

    fn persist_tokens(
        &self,
        name: &str,
        account_uuid: Option<String>,
        org_uuid: Option<String>,
        org_name: Option<String>,
        tokens: &Tokens,
    ) {
        let Some(path) = &self.config_path else {
            return;
        };
        // Match the config entry by full identity (account_uuid + org), not name
        // alone, so a rotated token lands on the RIGHT entry when the same email
        // is logged into two orgs (finding #9). With all identity fields None
        // (today's config) same_identity falls back to name equality — unchanged.
        let probe = crate::identity::probe(name, account_uuid, org_uuid, org_name);
        // Modify AND save while HOLDING the config lock. Cloning under the lock
        // then saving unlocked lets two concurrent refreshes race on the file: a
        // stale save clobbers the other account's just-rotated refresh token,
        // which then 400s ("invalid_grant") on its next refresh. Holding the lock
        // through the save serializes writes — including save_tokens' whole
        // read-modify-write of the file — so every rotation lands on disk.
        let mut config = self.config.lock().expect("config lock poisoned");
        if let Some(account) = config
            .accounts
            .iter_mut()
            .find(|a| crate::identity::same_identity(a, &probe))
        {
            account.access_token = tokens.access_token.clone();
            account.refresh_token = Some(tokens.refresh_token.clone());
            account.expires_at = Some(tokens.expires_at_ms);
        }
        // Tokens only: the in-memory config is a boot-time snapshot, so writing
        // it whole would stamp stale settings over the user's live file.
        if let Err(err) = config::save_tokens(path, &config) {
            tracing::error!(error = %err, "failed to persist refreshed token to config");
        }
    }

    /// Flush account `idx`'s `disabled` flag to the config file, so an account
    /// deliberately benched from the TUI is still benched after a restart.
    ///
    /// Takes the SAME `self.config` lock as [`Self::persist_tokens`], for the same
    /// reason: that lock is what serializes this file write against a concurrent
    /// token rotation's whole read-modify-write. Writing outside it lets this
    /// write's snapshot of the document clobber a refresh token that rotated in
    /// between — and a refresh token is single-use, so the clobbered account 400s
    /// (`invalid_grant`) on its next refresh and is dead until re-authed by hand.
    ///
    /// Writes the flag ONLY, via [`config::save_disabled`], never the whole config:
    /// the in-memory `Config` is a boot-time snapshot, so flushing it whole would
    /// revert every setting the user edited while the proxy was running.
    ///
    /// A missing `config_path` (tests, `tcr demo`, `tcr status --probe`) is a
    /// SILENT no-op — those managers must never touch a real config file.
    fn persist_disabled(&self, idx: usize, target: &config::Account, disabled: bool) {
        let Some(path) = &self.config_path else {
            return;
        };
        let mut config = self.config.lock().expect("config lock poisoned");
        // Keep the boot-time snapshot in step with the file so the two views of
        // the flag cannot diverge. Matched by identity, exactly as persist_tokens
        // matches, so both land on the same entry.
        if let Some(account) = config
            .accounts
            .iter_mut()
            .find(|a| crate::identity::same_identity(a, target))
        {
            account.disabled = if disabled { Some(true) } else { None };
        }
        match config::save_disabled(path, target, disabled) {
            Ok(config::DisabledWrite::Updated) => tracing::info!(
                account = %target.name,
                index = idx,
                disabled,
                "persisted account disabled flag to config"
            ),
            Ok(config::DisabledWrite::Unchanged) => tracing::debug!(
                account = %target.name,
                index = idx,
                disabled,
                "config already carries this disabled state; nothing written"
            ),
            Ok(config::DisabledWrite::NoEntry) => tracing::warn!(
                account = %target.name,
                index = idx,
                path = %path.display(),
                "no config entry carries this account's identity; the disabled flag will NOT survive a restart"
            ),
            Ok(config::DisabledWrite::Ambiguous) => tracing::warn!(
                account = %target.name,
                index = idx,
                path = %path.display(),
                "more than one config entry carries this account's identity; refusing to guess which one to flag, so the disabled flag will NOT survive a restart"
            ),
            Err(err) => tracing::error!(
                error = %err,
                account = %target.name,
                index = idx,
                path = %path.display(),
                "failed to persist the disabled flag to config"
            ),
        }
    }

    /// Sideline account `idx` on a proven-dead credential. Arms the recovery backoff
    /// with it, so even a hand-sidelined row is re-probed rather than stuck forever.
    pub fn mark_error(&self, idx: usize) {
        {
            let mut accounts = self.accounts.write().expect("accounts lock poisoned");
            if let Some(account) = accounts.get_mut(idx) {
                account.status = AccountStatus::Error;
            }
        }
        self.grow_error_backoff(idx);
    }

    /// Enable/disable account `idx`. Re-enabling clears a stuck error/hold.
    ///
    /// The flag is also PERSISTED (see [`Self::persist_disabled`]) — memory-only
    /// was the bug: a restart silently returned a deliberately benched account to
    /// rotation, because the server writes nothing but credentials back.
    pub fn set_disabled(&self, idx: usize, disabled: bool) {
        // Take the identity out under the accounts lock and RELEASE that lock
        // before persisting. The persist path takes the config lock, and holding
        // both at once here would invert the order `warm_targets` reads them in
        // (config, then accounts) — the shape a deadlock is made of.
        let target = {
            let mut accounts = self.accounts.write().expect("accounts lock poisoned");
            let Some(account) = accounts.get_mut(idx) else {
                return;
            };
            account.disabled = disabled;
            if !disabled && account.status == AccountStatus::Error {
                account.status = AccountStatus::Active;
                account.rate_limited_until_ms = None;
            }
            crate::identity::probe(
                &account.name,
                account.account_uuid.clone(),
                account.org_uuid.clone(),
                account.org_name.clone(),
            )
        };
        self.persist_disabled(idx, &target, disabled);
    }

    /// Record which account actually served the most recent request.
    pub fn set_current(&self, idx: usize) {
        *self.current.lock().expect("current lock poisoned") = Some(idx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Account, PacingConfig, ProxyConfig};
    use crate::oauth::{OAuthError, RefreshFuture};
    use crate::probe::{ProbeError, ProbeFuture, Usage, UsageBucket};
    use crate::warmer::{AccountWarmer, WarmError, WarmFuture};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use time::Duration;

    /// A rate-limit hold that clears while the pinned account's prompt cache is
    /// still warm — the SOFT case. The live value of a no-guidance transient park
    /// (`proxy::NO_GUIDANCE_HOLD_SECS`), which is the hold this fleet arms most.
    const SHORT_HOLD_SECS: i64 = 15;

    /// A hold that OUTLIVES the prompt cache — the only kind that is ACCOUNT-level
    /// death and may re-key a session. Derived from the threshold rather than
    /// written as a literal so that raising [`CACHE_WARM_HOLD_SECS`] can never
    /// silently turn a re-key test into a divert test.
    const LONG_HOLD_SECS: i64 = CACHE_WARM_HOLD_SECS + 60;

    #[test]
    fn throttle_slot_burst1_is_strict_spacing() {
        // B=1 (tau=0): threading the TAT across 3 calls at a fixed `now` yields
        // allow_at points spaced by exactly `spacing_ms`.
        let now = 1000;
        let spacing = 100;
        let (tat1, allow1) = throttle_slot(0, now, spacing, 1);
        assert_eq!(allow1, now); // first send: instant (allow_at == now)
        let (tat2, allow2) = throttle_slot(tat1, now, spacing, 1);
        assert_eq!(allow2, allow1 + spacing);
        let (_tat3, allow3) = throttle_slot(tat2, now, spacing, 1);
        assert_eq!(allow3, allow2 + spacing);
    }

    #[test]
    fn throttle_slot_burst3_admits_then_paces() {
        // B=3, now=1000, T=100 (tau=200): first 3 fire instantly (allow_at <= now),
        // the 4th is paced to now + spacing_ms.
        let now = 1000;
        let spacing = 100;
        let burst = 3;
        let (tat1, allow1) = throttle_slot(0, now, spacing, burst);
        assert_eq!((tat1, allow1), (1100, 800));
        assert!(allow1 <= now);
        let (tat2, allow2) = throttle_slot(tat1, now, spacing, burst);
        assert_eq!((tat2, allow2), (1200, 900));
        assert!(allow2 <= now);
        let (tat3, allow3) = throttle_slot(tat2, now, spacing, burst);
        assert_eq!((tat3, allow3), (1300, 1000));
        assert!(allow3 <= now);
        let (tat4, allow4) = throttle_slot(tat3, now, spacing, burst);
        assert_eq!((tat4, allow4), (1400, 1100));
        assert_eq!(allow4, now + spacing); // 4th is paced
    }

    fn account(name: &str, priority: i64) -> Account {
        Account {
            name: name.to_string(),
            account_type: "oauth".to_string(),
            account_uuid: None,
            org_uuid: None,
            org_name: None,
            access_token: format!("at-{name}"),
            refresh_token: Some(format!("rt-{name}")),
            expires_at: Some(crate::now_ms() + 3_600_000),
            priority: Some(priority),
            switch_threshold: None,
            disabled: None,
            extra: serde_json::Map::new(),
        }
    }

    fn config_with(accounts: Vec<Account>) -> Config {
        Config {
            proxy: ProxyConfig::default(),
            upstream: "https://api.anthropic.com".to_string(),
            switch_threshold: 0.90,
            pacing: PacingConfig::default(),
            throttle: ThrottleConfig::default(),
            lock_account: None,
            accounts,
            extra: serde_json::Map::new(),
        }
    }

    /// Like [`config_with`] but with request pacing configured (for the pacing tests).
    fn config_with_pacing(accounts: Vec<Account>, pacing: PacingConfig) -> Config {
        let mut config = config_with(accounts);
        config.pacing = pacing;
        config
    }

    /// A refresher that counts how many times it is invoked and mints a token
    /// valid far into the future (so a coalesced follower sees it as fresh).
    struct CountingRefresher {
        calls: Arc<AtomicUsize>,
    }

    impl TokenRefresher for CountingRefresher {
        fn refresh(&self, _refresh_token: String) -> RefreshFuture {
            let calls = self.calls.clone();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                // Simulate a little upstream latency so concurrent callers pile
                // up on the coalescing lock.
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                Ok::<Tokens, OAuthError>(Tokens {
                    access_token: "fresh-access".to_string(),
                    refresh_token: "fresh-refresh".to_string(),
                    expires_at_ms: crate::now_ms() + 3_600_000,
                })
            })
        }
    }

    /// A refresher whose FIRST call fails transiently (the leader — after a
    /// short delay so the herd queues) and whose every later call succeeds.
    /// Records the refresh token POSTed on each call so a test can assert the
    /// single-use token was sent exactly once across a concurrent batch.
    struct TransientThenOkRefresher {
        calls: Arc<AtomicUsize>,
        tokens_seen: Arc<Mutex<Vec<String>>>,
    }

    impl TokenRefresher for TransientThenOkRefresher {
        fn refresh(&self, refresh_token: String) -> RefreshFuture {
            let calls = self.calls.clone();
            let tokens_seen = self.tokens_seen.clone();
            Box::pin(async move {
                tokens_seen
                    .lock()
                    .expect("tokens_seen lock poisoned")
                    .push(refresh_token);
                let n = calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    // Leader: hold the coalescing lock long enough for the herd
                    // to queue, then fail transiently (access token unchanged).
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    Err::<Tokens, OAuthError>(OAuthError::Transient(
                        "simulated network blip".into(),
                    ))
                } else {
                    Ok(Tokens {
                        access_token: "fresh-access".to_string(),
                        refresh_token: "fresh-refresh".to_string(),
                        expires_at_ms: crate::now_ms() + 3_600_000,
                    })
                }
            })
        }
    }

    /// A prober that never hits the network — the default for selection/refresh
    /// tests that do not exercise probing.
    /// Gil, 2026-07-17: "are error accounts rechecked?" — they must be. An `Error`
    /// row whose refresh now SUCCEEDS is recovered by the probe, because a successful
    /// refresh is proof the credential was never dead (a dead one cannot produce one).
    /// Before this, `Error` was a life sentence: nothing probed or selected an errored
    /// row and only a refresh cleared it, so one transient rejection sidelined a
    /// healthy account until restart — observed live with 7/7 tokens still probing 200.
    #[tokio::test]
    async fn errored_account_is_rechecked_and_recovers_when_its_refresh_succeeds() {
        let calls = Arc::new(AtomicUsize::new(0));
        let manager = build_manager_with_prober(
            config_with(vec![account("stuck", 0)]),
            Arc::new(CountingRefresher {
                calls: calls.clone(),
            }),
            Arc::new(NoProber),
        );

        manager.mark_error(0);
        assert_eq!(
            manager.account_status(0),
            Some(AccountStatus::Error),
            "precondition: the row is errored"
        );
        assert!(
            manager.probeable_indices().is_empty(),
            "while its recovery backoff is live an errored row stays sidelined"
        );

        // Its backoff elapses …
        {
            let mut a = manager.accounts.write().expect("accounts lock poisoned");
            a[0].error_retry_after_ms = Some(crate::now_ms() - 1);
        }
        assert_eq!(
            manager.probeable_indices(),
            vec![0],
            "once the backoff elapses the row IS re-probed"
        );

        // … and the recovery attempt's refresh succeeds → back in rotation, no restart.
        manager.probe_all().await;
        assert_eq!(
            manager.account_status(0),
            Some(AccountStatus::Active),
            "an errored row whose refresh now succeeds must be RECOVERED — a successful \
             refresh proves it was never dead"
        );
    }

    /// A GENUINELY dead credential must never reach `Active`. Its recovery refresh keeps
    /// being rejected, so it stays `Error` and the backoff doubles — which is why
    /// re-probing cannot "silently re-insert a dead account into rotation".
    #[tokio::test]
    async fn permanently_rejected_account_never_recovers_and_its_backoff_grows() {
        struct AlwaysRejects;
        impl TokenRefresher for AlwaysRejects {
            fn refresh(&self, _refresh_token: String) -> RefreshFuture {
                Box::pin(async { Err(OAuthError::AuthRejected { status: 400 }) })
            }
        }
        let manager = build_manager_with_prober(
            config_with(vec![account("dead", 0)]),
            Arc::new(AlwaysRejects),
            Arc::new(NoProber),
        );

        manager.mark_error(0);
        let first_backoff = manager.accounts.read().expect("lock")[0].error_backoff_ms;
        assert_eq!(
            first_backoff, ERROR_REPROBE_BASE_MS,
            "the first rejection arms the base backoff"
        );

        for _ in 0..3 {
            {
                let mut a = manager.accounts.write().expect("accounts lock poisoned");
                a[0].error_retry_after_ms = Some(crate::now_ms() - 1);
            }
            manager.probe_all().await;
            assert_eq!(
                manager.account_status(0),
                Some(AccountStatus::Error),
                "a dead credential must NEVER be revived by a re-probe"
            );
        }

        let grown = manager.accounts.read().expect("lock")[0].error_backoff_ms;
        assert!(
            grown > first_backoff,
            "each rejected recovery must back off harder (was {first_backoff}, now {grown}) \
             so a dead row can never hammer the OAuth endpoint"
        );
        assert!(
            grown <= ERROR_REPROBE_CAP_MS,
            "the backoff is capped at {ERROR_REPROBE_CAP_MS}, got {grown}"
        );
    }

    struct NoProber;
    impl UsageProber for NoProber {
        fn probe(&self, _access_token: String) -> ProbeFuture {
            Box::pin(async {
                Err(ProbeError {
                    status: None,
                    message: "no prober configured".into(),
                })
            })
        }
    }

    /// A prober returning a canned success for one account and an error for
    /// another, keyed on the access token so probe-health can be asserted.
    struct ScriptedProber {
        ok_token: String,
    }
    impl UsageProber for ScriptedProber {
        fn probe(&self, access_token: String) -> ProbeFuture {
            let ok = access_token == self.ok_token;
            Box::pin(async move {
                if ok {
                    Ok(Usage {
                        five_hour: Some(UsageBucket {
                            utilization: Some(0.25),
                            reset_at_ms: Some(crate::now_ms() + 3_600_000),
                        }),
                        seven_day: None,
                        seven_day_oi: None,
                    })
                } else {
                    Err(ProbeError {
                        status: Some(500),
                        message: "upstream boom".into(),
                    })
                }
            })
        }
    }

    /// A prober that succeeds on its first call (learning a 5-hour bar) and then
    /// returns a 429 on every call after — for asserting a probe-429 keeps the
    /// last-good utilization and reads as `RateLimited`, not `Error`.
    struct FirstOkThen429Prober {
        calls: Arc<AtomicUsize>,
    }
    impl UsageProber for FirstOkThen429Prober {
        fn probe(&self, _access_token: String) -> ProbeFuture {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    Ok(Usage {
                        five_hour: Some(UsageBucket {
                            utilization: Some(0.25),
                            reset_at_ms: Some(crate::now_ms() + 3_600_000),
                        }),
                        seven_day: None,
                        seven_day_oi: None,
                    })
                } else {
                    Err(ProbeError {
                        status: Some(429),
                        message: "Too Many Requests".into(),
                    })
                }
            })
        }
    }

    /// A prober that SUCCEEDS for every account and reports a weekly window only —
    /// no 5h bucket at all. That is the shape that separates the two candidate boot
    /// predicates: the quota was genuinely read (so `quota_known` latches and the
    /// account is warm-eligible), while `quota.five_hour` stays `None` because
    /// `apply_bucket` early-returns on an absent bucket. Gating on
    /// `five_hour.is_some()` would leave such an account warm-INELIGIBLE forever.
    struct ColdOkProber;
    impl UsageProber for ColdOkProber {
        fn probe(&self, _access_token: String) -> ProbeFuture {
            Box::pin(async {
                Ok(Usage {
                    five_hour: None,
                    seven_day: Some(UsageBucket {
                        utilization: Some(0.10),
                        reset_at_ms: Some(crate::now_ms() + 86_400_000),
                    }),
                    seven_day_oi: None,
                })
            })
        }
    }

    /// A warmer that never hits the network — the default for tests that do not
    /// exercise keep-warm. If invoked it records nothing and returns empty headers.
    struct NoWarmer;
    impl AccountWarmer for NoWarmer {
        fn warm(&self, _access_token: String, _upstream: String) -> WarmFuture {
            Box::pin(async {
                Err(WarmError {
                    status: None,
                    message: "no warmer configured".into(),
                })
            })
        }
    }

    /// A warmer that records every access token it was asked to warm (so the warm
    /// sweep's targeting can be asserted) and returns headers carrying a live 5h
    /// window — exactly what a real warm response yields for the fold-back.
    struct RecordingWarmer {
        warmed: Arc<Mutex<Vec<String>>>,
    }
    impl AccountWarmer for RecordingWarmer {
        fn warm(&self, access_token: String, _upstream: String) -> WarmFuture {
            let warmed = self.warmed.clone();
            Box::pin(async move {
                warmed
                    .lock()
                    .expect("warmed lock poisoned")
                    .push(access_token);
                let reset = (OffsetDateTime::now_utc() + Duration::hours(5)).unix_timestamp();
                let mut h = reqwest::header::HeaderMap::new();
                h.insert(
                    "anthropic-ratelimit-unified-5h-utilization",
                    "0.01".parse().unwrap(),
                );
                h.insert(
                    "anthropic-ratelimit-unified-5h-reset",
                    reset.to_string().parse().unwrap(),
                );
                Ok(h)
            })
        }
    }

    fn build_manager(config: Config, refresher: Arc<dyn TokenRefresher>) -> Arc<Manager> {
        Manager::new(
            config,
            refresher,
            Arc::new(NoProber),
            Arc::new(NoWarmer),
            None,
        )
    }

    fn build_manager_with_prober(
        config: Config,
        refresher: Arc<dyn TokenRefresher>,
        prober: Arc<dyn UsageProber>,
    ) -> Arc<Manager> {
        Manager::new(config, refresher, prober, Arc::new(NoWarmer), None)
    }

    fn build_manager_with_warmer(
        config: Config,
        refresher: Arc<dyn TokenRefresher>,
        warmer: Arc<dyn AccountWarmer>,
    ) -> Arc<Manager> {
        Manager::new(config, refresher, Arc::new(NoProber), warmer, None)
    }

    /// A manager that DOES have a config file behind it — the live server's shape.
    /// Every other builder here passes `config_path = None`, which makes each
    /// persist a silent no-op.
    fn build_manager_with_path(config: Config, path: PathBuf) -> Arc<Manager> {
        Manager::new(
            config,
            Arc::new(CountingRefresher {
                calls: Arc::new(AtomicUsize::new(0)),
            }),
            Arc::new(NoProber),
            Arc::new(NoWarmer),
            Some(path),
        )
    }

    /// A unique temp path per test — the suite runs tests in parallel threads of
    /// ONE process, so a pid-only name collides.
    fn tmp_config_path(tag: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("tcr-mgr-{tag}-{}-{seq}.json", std::process::id()))
    }

    /// Drive account `idx`'s 5-hour window to `util` with a reset `hours_from_now`
    /// (negative = past reset), via the real rate-limit headers the proxy learns.
    fn set_5h(manager: &Manager, idx: usize, util: &str, hours_from_now: i64) {
        let now = OffsetDateTime::now_utc();
        let reset = (now + Duration::hours(hours_from_now)).unix_timestamp();
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(
            "anthropic-ratelimit-unified-5h-utilization",
            util.parse().unwrap(),
        );
        h.insert(
            "anthropic-ratelimit-unified-5h-reset",
            reset.to_string().parse().unwrap(),
        );
        manager.update_quota(idx, &h);
    }

    /// Set account `idx`'s weekly (`7d`) window near/over threshold with a future
    /// reset — isolates the near-threshold gate from the live-5h gate.
    fn set_7d(manager: &Manager, idx: usize, util: &str) {
        let now = OffsetDateTime::now_utc();
        let reset = (now + Duration::hours(2)).unix_timestamp();
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(
            "anthropic-ratelimit-unified-7d-utilization",
            util.parse().unwrap(),
        );
        h.insert(
            "anthropic-ratelimit-unified-7d-reset",
            reset.to_string().parse().unwrap(),
        );
        manager.update_quota(idx, &h);
    }

    /// Build a config carrying an unmodelled top-level `warmupSeconds` value.
    fn config_with_warmup(accounts: Vec<Account>, warmup_seconds: i64) -> Config {
        let mut config = config_with(accounts);
        config.extra.insert(
            "warmupSeconds".to_string(),
            serde_json::Value::from(warmup_seconds),
        );
        config
    }

    /// Build a config carrying an unmodelled top-level `quotaProbeSeconds` value.
    /// `0` turns probing OFF, which is what makes the keep-warm boot gate fall
    /// back to its pre-gate behaviour.
    fn config_with_probe_seconds(accounts: Vec<Account>, probe_seconds: i64) -> Config {
        let mut config = config_with(accounts);
        config.extra.insert(
            "quotaProbeSeconds".to_string(),
            serde_json::Value::from(probe_seconds),
        );
        config
    }

    /// Mark every account as having had its quota successfully READ at least once.
    ///
    /// `warm_targets` skips an account whose quota was never read while probing is
    /// enabled (blank quota is unknown, not known-cold), and `config_with` leaves
    /// `quotaProbeSeconds` at its default 75 — so without this, every keep-warm test
    /// below would be measuring the boot gate instead of the predicate it is
    /// actually about. `update_quota` (which `set_5h`/`set_7d` drive) is the
    /// response-header path and deliberately touches neither `quota_known` nor
    /// `probe_status`; only a successful probe does.
    fn mark_all_probed(manager: &Manager) {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        for account in accounts.iter_mut() {
            account.probe_status = ProbeStatus::Ok;
            account.quota_known = true;
        }
    }

    // ---- hard account lock (`lockAccount`; no failover) -----------------------

    fn lock_refresher() -> Arc<CountingRefresher> {
        Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        })
    }

    fn config_with_lock(accounts: Vec<Account>, lock: &str) -> Config {
        let mut config = config_with(accounts);
        config.lock_account = Some(lock.to_string());
        config
    }

    /// A hard lock pins EVERY select to the locked index, ignoring the LRU
    /// preference and any session-affinity key that points elsewhere.
    #[test]
    fn select_lock_always_returns_locked_idx() {
        let manager = build_manager(
            config_with_lock(
                vec![account("zero", 0), account("one", 0), account("two", 0)],
                "one",
            ),
            lock_refresher(),
        );
        assert_eq!(manager.locked_idx, Some(1));
        let now = OffsetDateTime::now_utc();
        // No affinity, empty tried → locked account regardless of LRU (index 0
        // would be the natural first pick here).
        assert_eq!(manager.select(&HashSet::new(), now, None, None), Some(1));
        // An affinity key returns the SAME locked account (lock ignores affinity).
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(42)),
            Some(1)
        );
        // Bias the LRU toward index 0 by pinning affinity elsewhere first — lock
        // still wins.
        assert_eq!(manager.select(&HashSet::new(), now, None, Some(7)), Some(1));
    }

    /// The lock has NO failover: once the locked account is in `tried`, select
    /// returns None rather than rotating to the pool.
    #[test]
    fn select_lock_returns_none_when_locked_tried() {
        let manager = build_manager(
            config_with_lock(vec![account("zero", 0), account("one", 0)], "one"),
            lock_refresher(),
        );
        assert_eq!(manager.locked_idx, Some(1));
        let now = OffsetDateTime::now_utc();
        let mut tried = HashSet::new();
        tried.insert(1usize);
        assert_eq!(manager.select(&tried, now, None, None), None);
    }

    /// `assemble` resolves the configured name to its account index; a name that
    /// matches no account resolves to None (runs unlocked).
    #[test]
    fn assemble_resolves_lock_name() {
        let matched = build_manager(
            config_with_lock(vec![account("a", 0), account("b", 0), account("c", 0)], "c"),
            lock_refresher(),
        );
        assert_eq!(matched.locked_idx, Some(2));

        let unmatched = build_manager(
            config_with_lock(vec![account("a", 0), account("b", 0)], "ghost"),
            lock_refresher(),
        );
        assert_eq!(unmatched.locked_idx, None);
    }

    /// Absent `lockAccount` → `locked_idx == None` → select is unchanged.
    #[test]
    fn unlocked_default_leaves_locked_idx_none() {
        let manager = build_manager(config_with(vec![account("solo", 0)]), lock_refresher());
        assert_eq!(manager.locked_idx, None);
        let now = OffsetDateTime::now_utc();
        assert_eq!(manager.select(&HashSet::new(), now, None, None), Some(0));
    }

    #[test]
    fn account_uuid_accessor_returns_configured_value() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let mut with_uuid = account("has-uuid", 0);
        with_uuid.account_uuid = Some("abcdef01-2345-6789-abcd-ef0123456789".to_string());
        let without_uuid = account("no-uuid", 0);
        let manager = build_manager(config_with(vec![with_uuid, without_uuid]), refresher);

        assert_eq!(
            manager.account_uuid(0).as_deref(),
            Some("abcdef01-2345-6789-abcd-ef0123456789"),
            "configured account_uuid is returned"
        );
        assert_eq!(manager.account_uuid(1), None, "absent config → None");
        assert_eq!(manager.account_uuid(99), None, "out-of-range idx → None");
    }

    // ---- per-account request pacing (default-OFF; soft; RAII in-flight) --------

    fn pacing_refresher() -> Arc<CountingRefresher> {
        Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// #1 Default-OFF is inert: an account that WOULD be paced (saturated
    /// in_flight + just served) is still selected exactly as before, so an
    /// unconfigured proxy is byte-identical to the pre-pacing build.
    #[test]
    fn pacing_off_is_inert() {
        let manager = build_manager(config_with(vec![account("solo", 0)]), pacing_refresher());
        {
            let mut a = manager.accounts.write().expect("accounts lock poisoned");
            a[0].in_flight = 5; // would exceed any cap …
            a[0].last_served_ms = crate::now_ms(); // … and be inside any spacing window.
        }
        let now = OffsetDateTime::now_utc();
        assert_eq!(
            manager.select(&HashSet::new(), now, None, None),
            Some(0),
            "with PacingConfig::default() the account is selected regardless of load"
        );
    }

    /// #2 An account at the in-flight cap is skipped and a second, un-capped
    /// account is chosen instead.
    #[test]
    fn in_flight_cap_makes_account_ineligible() {
        let pacing = PacingConfig {
            max_in_flight_per_account: Some(1),
            min_spacing_ms: None,
        };
        let manager = build_manager(
            config_with_pacing(vec![account("busy", 0), account("free", 0)], pacing),
            pacing_refresher(),
        );
        {
            let mut a = manager.accounts.write().expect("accounts lock poisoned");
            a[0].in_flight = 1; // at cap=1 → ineligible under pacing.
        }
        let now = OffsetDateTime::now_utc();
        assert_eq!(
            manager.select(&HashSet::new(), now, None, None),
            Some(1),
            "the capped account is skipped and the free one is chosen"
        );
    }

    /// #3 An account selected less than `min_spacing_ms` ago is skipped; once the
    /// window elapses it is eligible again. Tested at `eligible()` so the soft
    /// select() fallback can't mask the gate.
    #[test]
    fn min_spacing_skips_recently_served() {
        let pacing = PacingConfig {
            max_in_flight_per_account: None,
            min_spacing_ms: Some(1000),
        };
        let manager = build_manager(
            config_with_pacing(vec![account("a", 0)], pacing.clone()),
            pacing_refresher(),
        );
        let now = OffsetDateTime::now_utc();
        let now_ms = odt_to_ms(now);
        {
            let mut a = manager.accounts.write().expect("accounts lock poisoned");
            a[0].last_served_ms = now_ms; // served 0ms ago.
        }
        let a = manager.accounts.read().expect("accounts lock poisoned");
        assert!(
            !Manager::eligible(
                &a[0],
                manager.global_threshold,
                &pacing,
                true,
                now,
                now_ms,
                false
            ),
            "served 0ms ago (< 1000ms) → skipped"
        );
        let later_ms = now_ms + 1000;
        let later = ms_to_odt(later_ms).expect("valid timestamp");
        assert!(
            Manager::eligible(
                &a[0],
                manager.global_threshold,
                &pacing,
                true,
                later,
                later_ms,
                false
            ),
            "after the spacing window → eligible again"
        );
    }

    /// #4 SOFT invariant: when EVERY account is paced out but otherwise healthy,
    /// select() still returns Some (the least-loaded) — pacing never turns a
    /// servable request into a failure.
    #[test]
    fn all_paced_falls_back_to_serve() {
        let pacing = PacingConfig {
            max_in_flight_per_account: Some(1),
            min_spacing_ms: None,
        };
        let manager = build_manager(
            config_with_pacing(vec![account("a", 0), account("b", 0)], pacing),
            pacing_refresher(),
        );
        {
            let mut a = manager.accounts.write().expect("accounts lock poisoned");
            a[0].in_flight = 1;
            a[1].in_flight = 1; // both over cap=1.
        }
        let now = OffsetDateTime::now_utc();
        assert!(
            manager.select(&HashSet::new(), now, None, None).is_some(),
            "all-paced but healthy → soft fallback serves least-loaded, never None"
        );
    }

    /// #5 The RAII in-flight guard decrements the count on Drop, so no serve-path
    /// exit can leak a counter and strand an account.
    #[test]
    fn in_flight_guard_decrements_on_drop() {
        let manager = build_manager(config_with(vec![account("a", 0)]), pacing_refresher());
        {
            let _guard = manager.enter_in_flight(0);
            assert_eq!(
                manager.accounts.read().expect("accounts lock poisoned")[0].in_flight,
                1,
                "entering bumps the in-flight count"
            );
        }
        assert_eq!(
            manager.accounts.read().expect("accounts lock poisoned")[0].in_flight,
            0,
            "dropping the guard decrements back to 0"
        );
    }

    /// #6 The SSE fix: once the owned guard is MOVED into a streamed body, the
    /// account's `in_flight` stays incremented for the life of the STREAM — not the
    /// life of the handler scope — and only decrements when the stream is dropped.
    /// A handler-local guard (the bug) would have decremented at handler return,
    /// before axum ever polls the body; this asserts the lifetime moved.
    #[test]
    fn in_flight_guard_lifetime_follows_stream_not_scope() {
        use futures::StreamExt;

        let manager = build_manager(config_with(vec![account("a", 0)]), pacing_refresher());

        // Build the guarded stream exactly as the SSE passthrough does: move the owned
        // guard into a `move` map closure so the stream body OWNS it. The `&guard`
        // anchor mirrors the proxy's forced edition-2021 capture.
        let stream = {
            let guard = manager.enter_in_flight(0);
            assert_eq!(
                manager.accounts.read().expect("accounts lock poisoned")[0].in_flight,
                1,
                "entering bumps the in-flight count"
            );
            futures::stream::iter(0..3u8).map(move |item| {
                let _anchor = &guard;
                item
            })
            // `guard`'s binding scope ENDS here. Were it handler-local, its Drop would
            // fire now and in_flight would fall to 0. It must NOT — the stream owns it.
        };
        assert_eq!(
            manager.accounts.read().expect("accounts lock poisoned")[0].in_flight,
            1,
            "guard moved into the stream: count stays incremented after its binding scope ends"
        );

        drop(stream);
        assert_eq!(
            manager.accounts.read().expect("accounts lock poisoned")[0].in_flight,
            0,
            "dropping the stream drops the guard → the account's in_flight returns to 0"
        );
    }

    /// #7 A configured in-flight cap of `0` is treated as DISABLED (identical to
    /// unset), never as "hold out every account". A literal `Some(0)` would make
    /// `in_flight >= 0` true for the whole fleet — pacing out every request, flooding
    /// the "skip in selection" log, and collapsing onto the least-loaded fallback.
    #[test]
    fn in_flight_cap_zero_is_disabled_not_dark_pool() {
        let pacing = PacingConfig {
            max_in_flight_per_account: Some(0),
            min_spacing_ms: None,
        };
        assert_eq!(
            pacing.effective_max_in_flight(),
            None,
            "cap=0 normalises to None (disabled)"
        );
        assert!(
            !pacing.is_active(),
            "cap=0 as the only knob leaves pacing fully inert"
        );

        let manager = build_manager(
            config_with_pacing(vec![account("a", 0), account("b", 0)], pacing),
            pacing_refresher(),
        );
        {
            let mut a = manager.accounts.write().expect("accounts lock poisoned");
            a[0].in_flight = 7; // would exceed any positive cap …
            a[1].in_flight = 9;
        }
        let now = OffsetDateTime::now_utc();
        let now_ms = odt_to_ms(now);
        let a = manager.accounts.read().expect("accounts lock poisoned");
        assert!(
            Manager::eligible(
                &a[0],
                manager.global_threshold,
                &manager.pacing,
                true,
                now,
                now_ms,
                false
            ),
            "cap=0 → account stays eligible regardless of in_flight (no dark pool)"
        );
        // And selection still serves normally rather than flooding the fallback.
        drop(a);
        assert!(
            manager.select(&HashSet::new(), now, None, None).is_some(),
            "cap=0 selects a servable account without pacing anyone out"
        );
    }

    #[test]
    fn select_prefers_lower_priority_value() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(
            config_with(vec![account("low-pri", 5), account("high-pri", 0)]),
            refresher,
        );
        let now = OffsetDateTime::now_utc();
        assert_eq!(manager.select(&HashSet::new(), now, None, None), Some(1));
    }

    #[test]
    fn select_skips_disabled_account() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0)]),
            refresher,
        );
        manager.set_disabled(0, true);
        let now = OffsetDateTime::now_utc();
        assert_eq!(manager.select(&HashSet::new(), now, None, None), Some(1));
    }

    #[test]
    fn select_skips_rate_limited_account() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0)]),
            refresher,
        );
        manager.mark_rate_limited(0, 300);
        let now = OffsetDateTime::now_utc();
        assert_eq!(manager.select(&HashSet::new(), now, None, None), Some(1));
    }

    #[test]
    fn select_skips_account_over_threshold() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0)]),
            refresher,
        );
        let now = OffsetDateTime::now_utc();
        // Drive account 0 over the 0.90 threshold via real headers.
        let reset = (now + Duration::hours(1)).unix_timestamp();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "anthropic-ratelimit-unified-5h-utilization",
            "0.95".parse().unwrap(),
        );
        headers.insert(
            "anthropic-ratelimit-unified-5h-reset",
            reset.to_string().parse().unwrap(),
        );
        manager.update_quota(0, &headers);
        assert_eq!(manager.select(&HashSet::new(), now, None, None), Some(1));
    }

    #[test]
    fn select_returns_none_when_all_tried() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(config_with(vec![account("a", 0)]), refresher);
        let now = OffsetDateTime::now_utc();
        let tried: HashSet<usize> = [0].into_iter().collect();
        assert_eq!(manager.select(&tried, now, None, None), None);
    }

    /// Load-balancing: within a priority tier, consecutive selects fan out across
    /// every eligible account (least-recently-selected first) instead of pinning
    /// the same one. Six sequential picks over three equal-priority accounts must
    /// land exactly twice on each. A FIXED `now` (no clock advance) proves the
    /// spread is driven by the monotonic select tick, not the wall clock — so a
    /// same-millisecond burst still rotates.
    #[test]
    fn select_spreads_load_across_a_priority_tier() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0), account("c", 0)]),
            refresher,
        );
        let now = OffsetDateTime::now_utc();
        let mut counts = [0usize; 3];
        for _ in 0..6 {
            let idx = manager
                .select(&HashSet::new(), now, None, None)
                .expect("an account is eligible");
            counts[idx] += 1;
        }
        assert_eq!(
            counts,
            [2, 2, 2],
            "load must spread evenly across the tier, not pin one account"
        );
    }

    /// Priority still dominates the spread: a lower-priority-value account is
    /// picked every time while it is eligible, even as its select tick advances —
    /// load-balancing is *within* a tier, never across tiers.
    #[test]
    fn select_spread_respects_priority_tiers() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(
            config_with(vec![account("primary", 0), account("pillow", 10)]),
            refresher,
        );
        let now = OffsetDateTime::now_utc();
        for _ in 0..5 {
            assert_eq!(
                manager.select(&HashSet::new(), now, None, None),
                Some(0),
                "the primary tier is never abandoned for a higher-priority-value account"
            );
        }
    }

    /// Regression: `persist_tokens` must save the config UNDER the lock, or two
    /// concurrent token refreshes race on the file and one account's just-rotated
    /// refresh token is lost — that account then 400s on its next refresh. Fire N
    /// concurrent persists for distinct accounts and assert the reloaded file has
    /// EVERY account's new refresh token.
    #[test]
    fn concurrent_token_persists_do_not_lose_updates() {
        use std::thread;
        let tmp = std::env::temp_dir().join(format!(
            "tcr-persist-race-{}-{}.json",
            std::process::id(),
            crate::now_ms()
        ));
        let n = 8usize;
        let accts: Vec<Account> = (0..n).map(|i| account(&format!("a{i}"), 0)).collect();
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = Manager::new(
            config_with(accts),
            refresher,
            Arc::new(NoProber),
            Arc::new(NoWarmer),
            Some(tmp.clone()),
        );

        let handles: Vec<_> = (0..n)
            .map(|i| {
                let m = manager.clone();
                thread::spawn(move || {
                    m.persist_tokens(
                        &format!("a{i}"),
                        None,
                        None,
                        None,
                        &Tokens {
                            access_token: format!("new-at-{i}"),
                            refresh_token: format!("new-rt-{i}"),
                            expires_at_ms: crate::now_ms() + 3_600_000,
                        },
                    );
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let reloaded = config::load(&tmp).expect("reload persisted config");
        for i in 0..n {
            let a = reloaded
                .accounts
                .iter()
                .find(|a| a.name == format!("a{i}"))
                .expect("account present after concurrent persist");
            assert_eq!(
                a.refresh_token.as_deref(),
                Some(format!("new-rt-{i}").as_str()),
                "account a{i} lost its rotated refresh token under concurrent persist"
            );
        }
        std::fs::remove_file(&tmp).ok();
    }

    /// Refresh coalescing: N concurrent `ensure_fresh` calls on the SAME
    /// hard-expired account trigger exactly ONE upstream refresh.
    #[tokio::test]
    async fn concurrent_ensure_fresh_coalesces_to_single_refresh() {
        let calls = Arc::new(AtomicUsize::new(0));
        let refresher = Arc::new(CountingRefresher {
            calls: calls.clone(),
        });
        let mut acct = account("expired", 0);
        acct.expires_at = Some(crate::now_ms() - 60_000); // already expired
        let manager = build_manager(config_with(vec![acct]), refresher);

        let mut handles = Vec::new();
        for _ in 0..8 {
            let m = manager.clone();
            handles.push(tokio::spawn(async move { m.ensure_fresh(0).await }));
        }
        for h in handles {
            h.await.unwrap();
        }

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // And the token is now the refreshed one.
        assert_eq!(manager.access_token(0).as_deref(), Some("fresh-access"));
    }

    /// Finding #10: the FORCE path coalesces too. N concurrent `ensure_fresh_force`
    /// on the same (not-yet-expired) account trigger exactly ONE upstream refresh,
    /// so a burst of 401s never rotates the refresh token N times back-to-back.
    #[tokio::test]
    async fn concurrent_ensure_fresh_force_coalesces_to_single_refresh() {
        let calls = Arc::new(AtomicUsize::new(0));
        let refresher = Arc::new(CountingRefresher {
            calls: calls.clone(),
        });
        // A perfectly valid (unexpired) token — only `force` drives the refresh.
        let manager = build_manager(config_with(vec![account("valid", 0)]), refresher);

        let mut handles = Vec::new();
        for _ in 0..8 {
            let m = manager.clone();
            handles.push(tokio::spawn(async move { m.ensure_fresh_force(0).await }));
        }
        for h in handles {
            h.await.unwrap();
        }

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(manager.access_token(0).as_deref(), Some("fresh-access"));
    }

    /// Transient-leader coalescing (the bug this fixes): when the leader's
    /// refresh fails TRANSIENTLY the access token is UNCHANGED, so the
    /// access-token guard alone can't stop a follower already queued on the lock
    /// from re-POSTing the SAME single-use refresh token. The self-clearing
    /// cooldown must collapse the concurrent batch to exactly ONE upstream POST —
    /// and still permit a later sequential retry (no suppress-forever).
    #[tokio::test]
    async fn transient_leader_coalesces_refresh_and_cooldown_self_clears() {
        let calls = Arc::new(AtomicUsize::new(0));
        let tokens_seen = Arc::new(Mutex::new(Vec::new()));
        let refresher = Arc::new(TransientThenOkRefresher {
            calls: calls.clone(),
            tokens_seen: tokens_seen.clone(),
        });
        let mut acct = account("expired", 0);
        acct.expires_at = Some(crate::now_ms() - 60_000); // hard-expired → refresh planned
        let manager = build_manager(config_with(vec![acct]), refresher);

        // A burst of concurrent refreshers. The leader fails transiently; every
        // follower must be coalesced by the cooldown, NOT re-POST the token.
        let mut handles = Vec::new();
        for _ in 0..8 {
            let m = manager.clone();
            handles.push(tokio::spawn(async move { m.ensure_fresh(0).await }));
        }
        for h in handles {
            h.await.unwrap();
        }

        // Exactly ONE upstream POST across the whole batch...
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a transient-leader failure must still coalesce the batch to ONE refresh POST"
        );
        // ...and the token POSTed was the single-use refresh token, sent once.
        assert_eq!(
            *tokens_seen.lock().unwrap(),
            vec!["rt-expired".to_string()],
            "the single-use refresh token must be sent exactly once across the batch"
        );
        // The transient failure left the access token unchanged.
        assert_eq!(manager.access_token(0).as_deref(), Some("at-expired"));

        // Self-clearing: once the cooldown window elapses, a later call refreshes
        // again. Rewind the stamp into the past to simulate the elapsed window
        // deterministically (crate::now_ms is wall-clock, so a real wait would
        // cost the full cooldown); this exercises the same time-based guard.
        {
            let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
            accounts[0].refresh_retry_after_ms = Some(crate::now_ms() - 1);
        }
        manager.ensure_fresh(0).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "after the cooldown self-clears, a later sequential refresh must be allowed"
        );
        assert_eq!(manager.access_token(0).as_deref(), Some("fresh-access"));
    }

    /// Finding #1: the background probe must SKIP disabled and errored accounts —
    /// never spend upstream traffic on a sidelined account nor let a probe's
    /// refresh silently flip an errored account back into rotation.
    #[tokio::test]
    async fn probe_all_skips_disabled_and_errored_accounts() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let prober = Arc::new(ScriptedProber {
            ok_token: "at-ok".to_string(),
        });
        let manager = build_manager_with_prober(
            config_with(vec![
                account("ok", 0),
                account("off", 0),
                account("dead", 0),
            ]),
            refresher,
            prober,
        );
        manager.set_disabled(1, true);
        manager.mark_error(2);

        manager.probe_all().await;
        let snap = manager.snapshot(OffsetDateTime::now_utc());

        // Only the healthy account was probed.
        assert_eq!(snap.accounts[0].probe_status, ProbeStatus::Ok);
        assert_eq!(
            snap.accounts[1].probe_status,
            ProbeStatus::Never,
            "a disabled account must not be probed"
        );
        assert!(snap.accounts[1].last_probe.is_none());
        assert_eq!(
            snap.accounts[2].probe_status,
            ProbeStatus::Never,
            "an errored account must not be probed (nor reactivated)"
        );
        // And the errored account was NOT flipped back to active by a probe refresh.
        assert_eq!(manager.account_status(2), Some(AccountStatus::Error));
    }

    #[tokio::test]
    async fn ensure_fresh_skips_when_token_still_valid() {
        let calls = Arc::new(AtomicUsize::new(0));
        let refresher = Arc::new(CountingRefresher {
            calls: calls.clone(),
        });
        // Default account expiry is 1h out — not hard-expired.
        let manager = build_manager(config_with(vec![account("valid", 0)]), refresher);
        manager.ensure_fresh(0).await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    /// U1 (storm fix): a token within the proactive EXPIRING_SOON buffer refreshes
    /// on a NON-force `ensure_fresh` (before it hard-expires), while a token
    /// comfortably outside the buffer is still left alone.
    #[tokio::test]
    async fn ensure_fresh_refreshes_expiring_soon_but_not_fresh() {
        // now+60s is inside the 5-min buffer → proactive non-force refresh fires.
        let soon_calls = Arc::new(AtomicUsize::new(0));
        let mut soon = account("soon", 0);
        soon.expires_at = Some(crate::now_ms() + 60_000);
        let soon_mgr = build_manager(
            config_with(vec![soon]),
            Arc::new(CountingRefresher {
                calls: soon_calls.clone(),
            }),
        );
        soon_mgr.ensure_fresh(0).await;
        assert_eq!(
            soon_calls.load(Ordering::SeqCst),
            1,
            "a token expiring within the 5-min buffer must refresh proactively"
        );
        assert_eq!(soon_mgr.access_token(0).as_deref(), Some("fresh-access"));

        // now+10min is outside the buffer → no refresh.
        let fresh_calls = Arc::new(AtomicUsize::new(0));
        let mut fresh = account("fresh", 0);
        fresh.expires_at = Some(crate::now_ms() + 600_000);
        let fresh_mgr = build_manager(
            config_with(vec![fresh]),
            Arc::new(CountingRefresher {
                calls: fresh_calls.clone(),
            }),
        );
        fresh_mgr.ensure_fresh(0).await;
        assert_eq!(
            fresh_calls.load(Ordering::SeqCst),
            0,
            "a token outside the 5-min buffer must not refresh"
        );
    }

    /// U2a (storm fix): a SUCCESSFUL forced refresh must LEAVE the refresh cooldown
    /// armed — `apply_refresh` installs the token pair and the re-arm deadline in a
    /// single write (the forced deadline is passed in as `cooldown_after`), so the
    /// throttle can never be clobbered by a success-path clear.
    #[tokio::test]
    async fn forced_success_re_arms_refresh_cooldown() {
        let calls = Arc::new(AtomicUsize::new(0));
        let refresher = Arc::new(CountingRefresher {
            calls: calls.clone(),
        });
        let manager = build_manager(config_with(vec![account("valid", 0)]), refresher);
        let before = crate::now_ms();
        assert!(
            manager.ensure_fresh_force(0).await,
            "a forced refresh that rotated the token returns true"
        );
        let until = {
            let accounts = manager.accounts.read().expect("accounts lock poisoned");
            accounts[0].refresh_retry_after_ms
        };
        assert!(
            until.is_some_and(|u| u >= before),
            "a successful forced refresh must leave the cooldown armed"
        );
    }

    /// U2b (storm fix): two sequential `ensure_fresh_force` on a non-expired account
    /// within the cooldown window collapse to exactly ONE upstream POST.
    #[tokio::test]
    async fn sequential_forced_refresh_within_cooldown_posts_once() {
        let calls = Arc::new(AtomicUsize::new(0));
        let refresher = Arc::new(CountingRefresher {
            calls: calls.clone(),
        });
        let manager = build_manager(config_with(vec![account("valid", 0)]), refresher);
        assert!(
            manager.ensure_fresh_force(0).await,
            "the first forced refresh rotates the token"
        );
        assert!(
            !manager.ensure_fresh_force(0).await,
            "a second forced refresh inside the cooldown is suppressed (no new token)"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the cooldown collapses back-to-back forced refreshes to ONE POST"
        );
    }

    /// U2c (storm fix): the hard-expired force override in `refresh_plan` is intact.
    /// A hard-expired token under `force` is PLANNED for refresh even during a live
    /// cooldown (the `!(force && is_expired)` gate), so a dead-in-hand token is never
    /// pinned un-refreshable by the throttle. The inner coalescing guard still
    /// collapses a *concurrent* batch to ONE POST — that guard must NOT get the same
    /// override, or a follower would re-POST the single-use refresh token
    /// (`transient_leader_coalesces_refresh_and_cooldown_self_clears` is the gate);
    /// the throttle instead self-clears by time (`..._after_cooldown_elapses`).
    #[test]
    fn hard_expired_force_override_intact_in_plan() {
        let mut acct = account("expired", 0);
        acct.expires_at = Some(crate::now_ms() - 60_000);
        let manager = build_manager(
            config_with(vec![acct]),
            Arc::new(CountingRefresher {
                calls: Arc::new(AtomicUsize::new(0)),
            }),
        );
        let now = crate::now_ms();
        {
            let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
            accounts[0].refresh_retry_after_ms = Some(now + REFRESH_RETRY_COOLDOWN_MS);
        }
        assert!(
            manager.refresh_plan(0, true, now).is_some(),
            "a hard-expired token under force must be planned for refresh despite a live cooldown"
        );

        // The contrasting half: a NON-force expiring-soon plan during the same live
        // cooldown STAYS suppressed (the U1 proactive path never overrides).
        let mut soon = account("soon", 0);
        soon.expires_at = Some(now + 60_000);
        let soon_mgr = build_manager(
            config_with(vec![soon]),
            Arc::new(CountingRefresher {
                calls: Arc::new(AtomicUsize::new(0)),
            }),
        );
        {
            let mut accounts = soon_mgr.accounts.write().expect("accounts lock poisoned");
            accounts[0].refresh_retry_after_ms = Some(now + REFRESH_RETRY_COOLDOWN_MS);
        }
        assert!(
            soon_mgr.refresh_plan(0, false, now).is_none(),
            "a non-force expiring-soon plan during a live cooldown stays suppressed"
        );
    }

    /// U2d (storm fix): once the re-armed cooldown elapses, a later forced refresh
    /// is allowed again — the throttle self-clears by time, never suppress-forever.
    #[tokio::test]
    async fn forced_refresh_allowed_after_cooldown_elapses() {
        let calls = Arc::new(AtomicUsize::new(0));
        let refresher = Arc::new(CountingRefresher {
            calls: calls.clone(),
        });
        let manager = build_manager(config_with(vec![account("valid", 0)]), refresher);
        assert!(manager.ensure_fresh_force(0).await);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Rewind the re-armed cooldown into the past to simulate elapse
        // deterministically (crate::now_ms is wall-clock).
        {
            let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
            accounts[0].refresh_retry_after_ms = Some(crate::now_ms() - 1);
        }
        assert!(
            manager.ensure_fresh_force(0).await,
            "once the cooldown elapses a forced refresh is allowed again"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// The background probe refreshes EVERY OAuth row (not just the serving one)
    /// and a FAILING probe surfaces as visible probe health — a message and a
    /// timestamp — rather than a silently-frozen quota bar. A transient upstream
    /// 5xx is a *soft* `RateLimited`, not a red error (the endpoint, not the
    /// credential, is unhappy), and it must not fabricate a fresh bar.
    #[tokio::test]
    async fn probe_all_updates_every_account_and_records_failures() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        // Account 0's token ("at-ok") probes OK; account 1's does not.
        let prober = Arc::new(ScriptedProber {
            ok_token: "at-ok".to_string(),
        });
        let manager = build_manager_with_prober(
            config_with(vec![account("ok", 0), account("bad", 0)]),
            refresher,
            prober,
        );

        manager.probe_all().await;
        let now = OffsetDateTime::now_utc();
        let snap = manager.snapshot(now);

        // Row 0: probe succeeded and its quota window was learned live.
        assert_eq!(snap.accounts[0].probe_status, ProbeStatus::Ok);
        assert!(snap.accounts[0].probe_error.is_none());
        assert!(snap.accounts[0].last_probe.is_some());
        assert_eq!(snap.accounts[0].five_hour, Some(0.25));

        // Row 1: the 5xx probe failed and the failure is visible, not hidden —
        // but as a soft RateLimited (transient upstream), never a red error.
        assert_eq!(snap.accounts[1].probe_status, ProbeStatus::RateLimited);
        assert!(snap.accounts[1].probe_error.is_some());
        assert!(snap.accounts[1].last_probe.is_some());
        // A failed probe must NOT fabricate a fresh bar for the bad account.
        assert_eq!(snap.accounts[1].five_hour, None);
    }

    /// Probe 429-tolerance: the usage endpoint throttling the *probe* is benign.
    /// After a good probe learns a bar, a following 429 probe must (a) read as
    /// `RateLimited`, never `Error`, and (b) leave the last-good utilization
    /// intact — so a throttled sweep never blanks the bars nor cries false error.
    #[tokio::test]
    async fn probe_429_is_rate_limited_and_keeps_last_utilization() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let prober = Arc::new(FirstOkThen429Prober {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager =
            build_manager_with_prober(config_with(vec![account("a", 0)]), refresher, prober);

        // First sweep: probe succeeds, the 5-hour bar is learned.
        manager.probe_all().await;
        let snap = manager.snapshot(OffsetDateTime::now_utc());
        assert_eq!(snap.accounts[0].probe_status, ProbeStatus::Ok);
        assert_eq!(snap.accounts[0].five_hour, Some(0.25));

        // Second sweep: the probe is 429'd. Soft state, and the bar is untouched.
        manager.probe_all().await;
        let snap = manager.snapshot(OffsetDateTime::now_utc());
        assert_eq!(
            snap.accounts[0].probe_status,
            ProbeStatus::RateLimited,
            "a probe 429 is soft, never a red error"
        );
        assert_eq!(
            snap.accounts[0].five_hour,
            Some(0.25),
            "a probe 429 must preserve the last-good utilization, not blank it"
        );
        assert!(snap.accounts[0].probe_error.is_some());
    }

    /// A network/transport probe failure (no HTTP status, not a timeout) stays a
    /// VISIBLE `Error` — a persistent connectivity problem (upstream down, DNS/TLS,
    /// a proxy-env regression) must never hide behind a benign "busy". Only
    /// endpoint-side 429/5xx soften; a transport failure is the credential's whole
    /// path being down, exactly the signal probe-health exists to surface.
    #[tokio::test]
    async fn probe_transport_failure_is_visible_error() {
        struct NetFailProber;
        impl UsageProber for NetFailProber {
            fn probe(&self, _access_token: String) -> ProbeFuture {
                Box::pin(async {
                    Err(ProbeError {
                        status: None,
                        message: "error sending request: connection refused".into(),
                    })
                })
            }
        }
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager_with_prober(
            config_with(vec![account("a", 0)]),
            refresher,
            Arc::new(NetFailProber),
        );
        manager.probe_all().await;
        let snap = manager.snapshot(OffsetDateTime::now_utc());
        assert_eq!(
            snap.accounts[0].probe_status,
            ProbeStatus::Error,
            "a transport failure must stay a visible error, not a soft busy"
        );
    }

    /// Change 3: an account near/over its weekly quota is held out of rotation but
    /// stays operationally ACTIVE — never a red "error". The snapshot exposes this
    /// as a `quota_state` (NearLimit / Exhausted) while `status` remains "active".
    #[test]
    fn near_and_over_quota_render_active_with_honest_quota_state() {
        use crate::stats::QuotaState;
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(
            config_with(vec![account("near", 0), account("full", 0)]),
            refresher,
        );
        let now = OffsetDateTime::now_utc();
        let reset = (now + Duration::hours(2)).unix_timestamp();
        let set_7d = |idx: usize, util: &str| {
            let mut h = reqwest::header::HeaderMap::new();
            h.insert(
                "anthropic-ratelimit-unified-7d-utilization",
                util.parse().unwrap(),
            );
            h.insert(
                "anthropic-ratelimit-unified-7d-reset",
                reset.to_string().parse().unwrap(),
            );
            manager.update_quota(idx, &h);
        };
        set_7d(0, "0.95"); // over the 0.90 threshold, under 100% → NearLimit
        set_7d(1, "1.20"); // over 100% → Exhausted

        let snap = manager.snapshot(now);
        // Both are held out of rotation …
        assert_eq!(
            manager.select(&HashSet::new(), now, None, None),
            None,
            "over-threshold accounts are held out of rotation"
        );
        // … yet neither is an "error": status stays active, quota_state is honest.
        assert_eq!(snap.accounts[0].status, "active");
        assert_eq!(snap.accounts[1].status, "active");
        assert_eq!(snap.accounts[0].quota_state, QuotaState::NearLimit);
        assert_eq!(snap.accounts[1].quota_state, QuotaState::Exhausted);
    }

    /// Biting test for the 0.90 → 0.95 default-threshold raise. Keyed off the
    /// SHIPPED config default (parsed from a minimal config), not a hardcoded
    /// literal, so it tracks the constant: an account at 0.92 weekly utilization
    /// has real headroom under the 0.95 default → stays Normal and eligible. On
    /// the pre-change tree (default 0.90) that same 0.92 tripped the line → held
    /// NearLimit and select() would refuse it, so this fails before the raise.
    /// 0.96 → NearLimit (held, credential fine); 1.00 → Exhausted (fully spent).
    #[test]
    fn default_threshold_raise_gives_92pct_headroom_normal_near_full() {
        use crate::stats::QuotaState;
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        // The default the shipped config applies when `switchThreshold` is absent.
        let default_threshold = serde_json::from_str::<Config>(r#"{ "accounts": [] }"#)
            .expect("minimal config parses")
            .switch_threshold;
        let mut config = config_with(vec![
            account("headroom", 0),
            account("near", 0),
            account("full", 0),
        ]);
        config.switch_threshold = default_threshold;
        let manager = build_manager(config, refresher);
        let now = OffsetDateTime::now_utc();
        let reset = (now + Duration::hours(2)).unix_timestamp();
        let set_7d = |idx: usize, util: &str| {
            let mut h = reqwest::header::HeaderMap::new();
            h.insert(
                "anthropic-ratelimit-unified-7d-utilization",
                util.parse().unwrap(),
            );
            h.insert(
                "anthropic-ratelimit-unified-7d-reset",
                reset.to_string().parse().unwrap(),
            );
            manager.update_quota(idx, &h);
        };
        set_7d(0, "0.92"); // under the 0.95 default → real headroom, Normal
        set_7d(1, "0.96"); // over 0.95, under 100% → NearLimit
        set_7d(2, "1.00"); // at 100% → Exhausted

        let snap = manager.snapshot(now);
        assert_eq!(
            snap.accounts[0].quota_state,
            QuotaState::Normal,
            "0.92 has headroom under the 0.95 default (pre-change 0.90 held it NearLimit)"
        );
        assert_eq!(snap.accounts[1].quota_state, QuotaState::NearLimit);
        assert_eq!(snap.accounts[2].quota_state, QuotaState::Exhausted);
        // The headroom account is not merely labelled Normal — it is actually
        // servable (pre-change it was held out and select() skipped it).
        assert_eq!(
            manager.select(&HashSet::new(), now, None, None),
            Some(0),
            "the 0.92 account is eligible under the raised default"
        );
    }

    /// Best-practices review (Apollo ch7 — no stale shadow of a live value): the
    /// `Throttled` enum is cleared only when the account next serves a non-429, so a
    /// naturally-expired hold would show a STALE "throttled" in the snapshot while
    /// routing already treats the account as eligible (routing reads the live
    /// `rate_limited_until_ms`, not the enum). The snapshot must derive the displayed
    /// status from the live deadline, like the quota bars and `rate_limited_until`.
    #[test]
    fn snapshot_status_is_active_once_the_throttle_hold_expires() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(
            config_with(vec![account("live-hold", 0), account("expired-hold", 0)]),
            refresher,
        );
        let now = OffsetDateTime::now_utc();

        manager.mark_rate_limited(0, 300); // a LIVE hold → still "throttled"
        manager.mark_rate_limited(1, 300); // mark, then rewind the deadline into the past:
        {
            let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
            accounts[1].rate_limited_until_ms = Some(crate::now_ms() - 1_000);
        }

        let snap = manager.snapshot(now);
        assert_eq!(
            snap.accounts[0].status, "throttled",
            "a live hold still displays throttled"
        );
        assert_eq!(
            snap.accounts[1].status, "active",
            "an expired hold must display active, not a stale throttled — routing already \
             treats it as eligible"
        );
        assert!(
            snap.accounts[1].rate_limited_until.is_none(),
            "the live rate_limited_until is already filtered to future-only"
        );
    }

    /// Bug #4: the request counter increments ONCE per client request, not once
    /// per upstream response. A request that folds quota headers from several
    /// upstream responses (a retry that rotated accounts) still counts one.
    #[test]
    fn request_counter_counts_once_across_retries() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(config_with(vec![account("a", 0)]), refresher);
        let now = OffsetDateTime::now_utc();

        // Simulate two upstream responses for a single client request (e.g. a
        // 429 that recorded a spent bucket, then the retry's 200).
        let headers = reqwest::header::HeaderMap::new();
        manager.update_quota(0, &headers);
        manager.update_quota(0, &headers);
        // Exactly one terminal serve.
        manager.record_served(0, now, None, SessionKind::Fallback);

        let snap = manager.snapshot(now);
        assert_eq!(snap.accounts[0].requests, 1);
        assert_eq!(snap.current, Some(0));
    }

    /// `record_served`'s `kind` threads through the per-session stat into the
    /// snapshot, so the TUI can fold fallback sessions without touching routing.
    /// A stable serve persists `SessionKind::Stable`; a fallback serve `Fallback`.
    #[test]
    fn record_served_threads_stable_flag_into_snapshot() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(config_with(vec![account("a", 0)]), refresher);
        let now = OffsetDateTime::now_utc();

        // Two distinct session keys: one stable-identity serve, one fallback.
        manager.record_served(0, now, Some(0xABCD), SessionKind::Stable);
        manager.record_served(0, now, Some(0x1234), SessionKind::Fallback);

        let snap = manager.snapshot(now);
        let stable = snap
            .sessions
            .iter()
            .find(|s| s.id == short_session_id(0xABCD))
            .expect("stable session present in snapshot");
        let fallback = snap
            .sessions
            .iter()
            .find(|s| s.id == short_session_id(0x1234))
            .expect("fallback session present in snapshot");
        assert_eq!(
            stable.kind,
            SessionKind::Stable,
            "stable serve must persist SessionKind::Stable"
        );
        assert_eq!(
            fallback.kind,
            SessionKind::Fallback,
            "fallback serve must persist SessionKind::Fallback"
        );
    }

    /// The sessions pane must report the PIN, never whoever happened to serve the
    /// last request. A hold that clears while the pinned account's prompt cache is
    /// still warm DIVERTS one request and deliberately keeps the pin (see
    /// [`CACHE_WARM_HOLD_SECS`]) — but the snapshot used to take its account from the
    /// SERVING index, so every divert made the session visibly jump accounts in the
    /// TUI although the pin never moved. A display that misreports state is worse
    /// than no display: it made a fleet measured at a 1.70% switch rate read as
    /// "sessions keep jumping".
    #[test]
    fn snapshot_reports_the_pin_not_the_last_serve() {
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0)]),
            lock_refresher(),
        );
        let now = OffsetDateTime::now_utc();
        let key = 0xFEEDu64;

        // Pin the session to `a`, and serve one request there.
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(key)),
            Some(0),
            "precondition: the first select pins the session to `a`"
        );
        manager.record_served(0, now, Some(key), SessionKind::Stable);

        // `a` picks up a SHORT hold — one that clears while its cache is still warm.
        {
            let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
            accounts[0].rate_limited_until_ms = Some(odt_to_ms(now) + SHORT_HOLD_SECS * 1000);
        }
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(key)),
            Some(1),
            "precondition: the short hold DIVERTS this one request to `b`"
        );
        manager.record_served(1, now, Some(key), SessionKind::Stable);

        let snap = manager.snapshot(now);
        let session = snap
            .sessions
            .iter()
            .find(|s| s.id == short_session_id(key))
            .expect("the session is present in the snapshot");
        assert_eq!(
            session.account, "a",
            "the snapshot must report the PIN, which a divert never moves"
        );
        assert_eq!(
            session.last_served_account, "b",
            "the divert must stay OBSERVABLE in its own field — the goal is honesty, \
             not concealment"
        );
    }

    /// The other half of the same contract, so the fix cannot swing too far and start
    /// hiding genuine re-keys behind a stale pin: when the pin DURABLY moves, the
    /// snapshot follows it. Only an ACCOUNT-level hard gate re-keys, so sideline `a`
    /// and let the next select re-pin the session to `b`.
    #[test]
    fn snapshot_account_follows_a_real_rekey() {
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0)]),
            lock_refresher(),
        );
        let now = OffsetDateTime::now_utc();
        let key = 0xBEEFu64;

        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(key)),
            Some(0),
            "precondition: the session starts pinned to `a`"
        );
        manager.record_served(0, now, Some(key), SessionKind::Stable);

        // A dead credential is ACCOUNT-level death → the pin is durably re-keyed.
        manager.mark_error(0);
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(key)),
            Some(1),
            "precondition: an errored pin re-keys the session to `b`"
        );
        manager.record_served(1, now, Some(key), SessionKind::Stable);

        let snap = manager.snapshot(now);
        let session = snap
            .sessions
            .iter()
            .find(|s| s.id == short_session_id(key))
            .expect("the session is present in the snapshot");
        assert_eq!(
            session.account, "b",
            "a REAL re-key must move the reported account — the pin is the authority \
             in both directions"
        );
        assert_eq!(session.last_served_account, "b");
    }

    /// Serving a request must not re-order the sessions pane. Rows used to sort
    /// most-recent-first, so every single request threw its session to the top and
    /// the pane churned under the operator's eyes — the other half of why a stable
    /// fleet looked like it was thrashing.
    #[test]
    fn session_rows_do_not_reorder_on_a_serve() {
        let manager = build_manager(config_with(vec![account("a", 0)]), lock_refresher());
        let now = OffsetDateTime::now_utc();
        let keys = [0x11u64, 0x22, 0x33];

        // Three sessions pinned to the one account, last seen 3s/2s/1s ago — so a
        // recency order and a stable order are genuinely different orders here.
        for key in keys {
            manager.select(&HashSet::new(), now, None, Some(key));
        }
        for (offset, key) in [3i64, 2, 1].into_iter().zip(keys) {
            manager.record_served(
                0,
                now - Duration::seconds(offset),
                Some(key),
                SessionKind::Stable,
            );
        }

        let ids = |snap: StatsSnapshot| -> Vec<String> {
            snap.sessions.iter().map(|s| s.id.clone()).collect()
        };
        let before = ids(manager.snapshot(now));
        assert_eq!(
            before.len(),
            3,
            "precondition: all three sessions are shown"
        );

        // The OLDEST session — not the first row — serves one request.
        manager.record_served(0, now, Some(keys[0]), SessionKind::Stable);
        let after = ids(manager.snapshot(now));

        assert_eq!(
            before, after,
            "a serve must never move a session's row; the age belongs in the `Last` \
             column, not in the row order"
        );
    }

    /// Cache tokens count: `update_usage` accumulates whatever the caller sums,
    /// which for the proxy includes cache-creation + cache-read input tokens.
    #[test]
    fn update_usage_accumulates_input_and_output() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(config_with(vec![account("a", 0)]), refresher);
        // e.g. input_tokens=10 + cache_creation=100 + cache_read=1000 = 1110.
        // The last two args are the cache-read / cache-creation SUBSETS of that sum.
        manager.update_usage(0, 1110, 42, 1000, 100);
        manager.update_usage(0, 0, 8, 0, 0);
        let snap = manager.snapshot(OffsetDateTime::now_utc());
        // R1: input_tokens is the SUM, byte-identical to the pre-cache behaviour.
        assert_eq!(snap.accounts[0].input_tokens, 1110);
        assert_eq!(snap.accounts[0].output_tokens, 50);
        // NEW: the cache components accumulate separately (a subset of the sum).
        assert_eq!(snap.accounts[0].cache_read_tokens, 1000);
        assert_eq!(snap.accounts[0].cache_creation_tokens, 100);
    }

    /// Drive an account's model-scoped weekly (`7d_oi`, Fable) bucket over
    /// threshold via the real rate-limit headers the proxy learns from.
    fn exhaust_oi_bucket(manager: &Manager, idx: usize, now: OffsetDateTime) {
        let reset = (now + Duration::hours(2)).unix_timestamp();
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(
            "anthropic-ratelimit-unified-7d_oi-utilization",
            "0.95".parse().unwrap(),
        );
        h.insert(
            "anthropic-ratelimit-unified-7d_oi-reset",
            reset.to_string().parse().unwrap(),
        );
        manager.update_quota(idx, &h);
    }

    /// HEADLINE per-model routing: a Fable request SKIPS an account whose `7d_oi`
    /// (Fable weekly) bucket is exhausted, and rotates to the next eligible one.
    #[test]
    fn fable_request_skips_oi_exhausted_account() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0)]),
            refresher,
        );
        let now = OffsetDateTime::now_utc();
        exhaust_oi_bucket(&manager, 0, now);
        assert_eq!(
            manager.select(&HashSet::new(), now, Some("claude-fable-5"), None),
            Some(1),
            "a Fable request must skip the OI-exhausted account 0"
        );
    }

    /// The SAME OI-exhausted account still serves every non-Fable model: with
    /// account 1 already tried, a non-Fable request lands right back on account 0.
    #[test]
    fn fable_exhausted_account_still_serves_non_fable() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0)]),
            refresher,
        );
        let now = OffsetDateTime::now_utc();
        exhaust_oi_bucket(&manager, 0, now);
        let tried: HashSet<usize> = [1].into_iter().collect();
        assert_eq!(
            manager.select(&tried, now, Some("claude-opus-4-6"), None),
            Some(0),
            "a non-Fable model must still serve from the OI-exhausted account 0"
        );
    }

    /// A request with no known model never consults the `7d_oi` bucket: account 0
    /// (OI-exhausted) is still picked when it is the only untried account.
    #[test]
    fn no_model_ignores_oi_bucket() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0)]),
            refresher,
        );
        let now = OffsetDateTime::now_utc();
        exhaust_oi_bucket(&manager, 0, now);
        let tried: HashSet<usize> = [1].into_iter().collect();
        assert_eq!(
            manager.select(&tried, now, None, None),
            Some(0),
            "no-model traffic must ignore the model-scoped OI bucket"
        );
    }

    /// Session affinity, headline: a session key pins to ONE account. Six selects
    /// with the SAME key over three equal-priority accounts all land on the same
    /// idx — the exact inverse of `select_spreads_load_across_a_priority_tier`.
    #[test]
    fn affinity_pins_session_to_one_account() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0), account("c", 0)]),
            refresher,
        );
        let now = OffsetDateTime::now_utc();
        let first = manager
            .select(&HashSet::new(), now, None, Some(7))
            .expect("an account is eligible");
        for _ in 0..5 {
            assert_eq!(
                manager.select(&HashSet::new(), now, None, Some(7)),
                Some(first),
                "the same session key must stay pinned to one account"
            );
        }
    }

    /// Regression guard: the disabled path (`affinity = None`) is byte-unchanged
    /// behaviour — six selects over three accounts still fan out evenly [2,2,2].
    #[test]
    fn affinity_none_still_spreads() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0), account("c", 0)]),
            refresher,
        );
        let now = OffsetDateTime::now_utc();
        let mut counts = [0usize; 3];
        for _ in 0..6 {
            let idx = manager
                .select(&HashSet::new(), now, None, None)
                .expect("an account is eligible");
            counts[idx] += 1;
        }
        assert_eq!(
            counts,
            [2, 2, 2],
            "the disabled affinity path must spread exactly like today"
        );
    }

    /// Migration: when a session's pinned account becomes ineligible (rate
    /// limited long enough to outlive its prompt cache), the next same-key select
    /// re-pins to a DIFFERENT eligible account and then sticks to that one.
    #[test]
    fn affinity_repins_when_pinned_account_ineligible() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0)]),
            refresher,
        );
        let now = OffsetDateTime::now_utc();
        let pinned = manager
            .select(&HashSet::new(), now, None, Some(42))
            .expect("an account is eligible");
        manager.mark_rate_limited(pinned, LONG_HOLD_SECS);
        let repinned = manager
            .select(&HashSet::new(), now, None, Some(42))
            .expect("the other account is eligible");
        assert_ne!(repinned, pinned, "must migrate off the ineligible pin");
        // And it sticks to the new pin.
        for _ in 0..3 {
            assert_eq!(
                manager.select(&HashSet::new(), now, None, Some(42)),
                Some(repinned),
                "the re-pin must be durable"
            );
        }
    }

    /// PACING gates DIVERT, they never RE-KEY: a pinned account that is merely paced
    /// (at the in-flight cap, every hard gate clear) yields THIS request to another
    /// account while the pin stays put, and the session returns to it the moment the
    /// guards drop. Before the fix the fall-through re-pin at the bottom of `select`
    /// rewrote the pin to the diverted account, permanently cold-starting that
    /// session's per-account prompt cache.
    ///
    /// Contrast `soft_gated_pin_is_served_not_diverted`, where the OTHER soft gate —
    /// our own utilization threshold — does not even divert. The difference is what
    /// each gate knows: `in_flight` is a fact we measure exactly and continuously
    /// about our own concurrency, and spreading a burst is the whole reason the cap
    /// exists; utilization is arithmetic over headers that go stale by minutes.
    #[test]
    fn soft_paced_pin_diverts_without_repinning() {
        let pacing = PacingConfig {
            max_in_flight_per_account: Some(1),
            min_spacing_ms: None,
        };
        let manager = build_manager(
            config_with_pacing(vec![account("a", 0), account("b", 0)], pacing),
            pacing_refresher(),
        );
        let now = OffsetDateTime::now_utc();
        let pinned = manager
            .select(&HashSet::new(), now, None, Some(77))
            .expect("an account is eligible");
        // Saturate ONLY the pinned account: at cap=1 it is soft-paced while every
        // hard gate (disabled/error/hold/quota) stays clear.
        {
            let mut a = manager.accounts.write().expect("accounts lock poisoned");
            a[pinned].in_flight = 1;
        }
        let diverted = manager
            .select(&HashSet::new(), now, None, Some(77))
            .expect("the un-paced account serves this request");
        assert_ne!(
            diverted, pinned,
            "a soft-paced pin must yield THIS request to the cooler account"
        );
        assert_eq!(
            pin_of(&manager, 77),
            Some(pinned),
            "a PACING divert must not move the pin"
        );
        // The pacing guard clears → the session snaps back to its warm account.
        {
            let mut a = manager.accounts.write().expect("accounts lock poisoned");
            a[pinned].in_flight = 0;
        }
        for _ in 0..3 {
            assert_eq!(
                manager.select(&HashSet::new(), now, None, Some(77)),
                Some(pinned),
                "once un-paced the session must return to its original account"
            );
        }
    }

    /// The last-touch stamp is still refreshed on a pacing divert, so a heavily
    /// diverted session can never become the `AFFINITY_CAP` eviction victim (the
    /// eviction sorts on exactly this field).
    #[test]
    fn soft_paced_divert_refreshes_pin_last_touch() {
        let pacing = PacingConfig {
            max_in_flight_per_account: Some(1),
            min_spacing_ms: None,
        };
        let manager = build_manager(
            config_with_pacing(vec![account("a", 0), account("b", 0)], pacing),
            pacing_refresher(),
        );
        let now = OffsetDateTime::now_utc();
        let pinned = manager
            .select(&HashSet::new(), now, None, Some(88))
            .expect("an account is eligible");
        {
            let mut a = manager.accounts.write().expect("accounts lock poisoned");
            a[pinned].in_flight = 1;
        }
        // A later `now` is what the divert must stamp onto the surviving pin.
        let later = now + time::Duration::seconds(30);
        let later_ms = odt_to_ms(later);
        manager
            .select(&HashSet::new(), later, None, Some(88))
            .expect("the un-paced account serves this request");
        let pins = manager.affinity.lock().expect("affinity lock poisoned");
        assert_eq!(
            pins.get(&88).copied(),
            Some((pinned, later_ms)),
            "the divert must re-insert the OLD index with a FRESH last-touch"
        );
    }

    /// If the pinned account is in `tried` AND fails a HARD gate, affinity falls
    /// through to a normal pick and re-pins to the new account. Being in `tried`
    /// is on its own only a SOFT signal (see
    /// `transient_tried_failure_keeps_the_pin`) — it takes a hard gate, here a hold
    /// that outlives the prompt cache, to make the fall-through durable. A SHORT
    /// hold would not: it diverts and keeps the pin (see
    /// `short_hold_diverts_but_keeps_the_pin`), so the fall-through would serve the
    /// other account while leaving the session pinned where it was.
    #[test]
    fn affinity_falls_through_when_pinned_in_tried_and_hard_gated() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0)]),
            refresher,
        );
        let now = OffsetDateTime::now_utc();
        let a = manager
            .select(&HashSet::new(), now, None, Some(9))
            .expect("an account is eligible");
        // The failover that put A in `tried` was durable: it armed a hold that
        // outlives A's prompt cache, so there is nothing left to come home to.
        manager.mark_rate_limited(a, LONG_HOLD_SECS);
        let tried: HashSet<usize> = [a].into_iter().collect();
        let b = manager
            .select(&tried, now, None, Some(9))
            .expect("the untried account is eligible");
        assert_ne!(b, a, "must fall through the tried pin to the other account");
        // The pin updated to B: a fresh same-key select with nothing tried sticks to B.
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(9)),
            Some(b),
            "the pin must have migrated to the fallen-through account"
        );
    }

    // ---- only a HARD gate may re-key a session ---------------------------------

    /// THE cache-loss fix: an account crossing the SOFT utilization threshold used to
    /// dump every session pinned to it at once, because `eligible()` folds
    /// `quota.is_near` into the same bool as the hard gates. Keeping the pin while
    /// still diverting the request — the first half of the fix — bought nothing: the
    /// diverted request paid the cold prefix anyway (measured live: a 44.4%
    /// account-switch rate on SUCCESSFUL serves, with zero hard failures). The
    /// threshold is our own arithmetic, and it routinely benches an account Anthropic
    /// is answering 200s for, so it may not bench a warm pin at all: the pinned
    /// account SERVES. With eleven of thirteen live accounts sitting at 98-100%, this
    /// is the single largest remaining cause of per-account prompt-cache loss.
    #[test]
    fn soft_gated_pin_is_served_not_diverted() {
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0)]),
            pacing_refresher(),
        );
        let now = OffsetDateTime::now_utc();
        let pinned = manager
            .select(&HashSet::new(), now, None, Some(1234))
            .expect("an account is eligible");
        // Over the soft threshold, but Anthropic still says `allowed_warning`:
        // every HARD gate is clear.
        set_over_threshold(&manager, pinned, 0.995, "allowed_warning");

        for _ in 0..3 {
            assert_eq!(
                manager.select(&HashSet::new(), now, None, Some(1234)),
                Some(pinned),
                "an over-threshold pin that clears every HARD gate still serves its \
                 own session — upstream, not our utilization arithmetic, is the oracle"
            );
        }
        assert_eq!(
            pin_of(&manager, 1234),
            Some(pinned),
            "crossing the SOFT utilization threshold must not re-key the session"
        );

        // The threshold is not dead — it still steers selection with no pin to
        // protect, so a session arriving cold lands on the cooler account.
        let unpinned = manager
            .select(&HashSet::new(), now, None, None)
            .expect("the under-threshold account is eligible");
        assert_ne!(
            unpinned, pinned,
            "the soft threshold still gates UNPINNED selection"
        );
    }

    /// The guard against over-correcting: a HARD gate still re-keys durably. A
    /// rate-limit hold that outlives the prompt cache means the account is gone for
    /// longer than the session's prefix survives, so the session moves and STAYS
    /// moved. (A SHORTER hold is the SOFT case — see
    /// `short_hold_diverts_but_keeps_the_pin`.)
    #[test]
    fn pin_with_live_hold_is_rekeyed() {
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0)]),
            pacing_refresher(),
        );
        let now = OffsetDateTime::now_utc();
        let pinned = manager
            .select(&HashSet::new(), now, None, Some(2345))
            .expect("an account is eligible");
        manager.mark_rate_limited(pinned, LONG_HOLD_SECS);

        let served = manager
            .select(&HashSet::new(), now, None, Some(2345))
            .expect("the un-held account serves this request");
        assert_ne!(served, pinned, "a held pin cannot serve");
        assert_eq!(
            manager
                .affinity
                .lock()
                .expect("affinity lock poisoned")
                .get(&2345)
                .map(|&(idx, _)| idx),
            Some(served),
            "a hold outliving the prompt cache is a HARD gate — it must re-key the \
             session"
        );
    }

    /// THE guard against over-correcting serve-the-pin into serve-anything: an
    /// account that is over the soft threshold AND holds a live 429 must still
    /// rotate. This is the exact combination the serve-the-pin path could swallow —
    /// the soft gate that now serves, stacked on the hard gate that must still win —
    /// and it is also the self-healing loop that makes serving over the threshold
    /// safe: a genuinely rejected account answers with a 429, that 429 arms
    /// `rate_limited_until_ms`, and this select is the one that moves the session off
    /// it. `hard_ok` stays the sole authority in both directions.
    #[test]
    fn pin_that_fails_a_hard_gate_still_rotates() {
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0)]),
            pacing_refresher(),
        );
        let now = OffsetDateTime::now_utc();
        let pinned = manager
            .select(&HashSet::new(), now, None, Some(5678))
            .expect("an account is eligible");
        set_over_threshold(&manager, pinned, 0.995, "allowed_warning");
        // The serve-over-threshold hit a real 429, which armed a real hold — and a
        // long one, past the point where waiting it out could still hit a warm cache.
        manager.mark_rate_limited(pinned, LONG_HOLD_SECS);

        let served = manager
            .select(&HashSet::new(), now, None, Some(5678))
            .expect("the un-held account serves this request");
        assert_ne!(
            served, pinned,
            "a live hold is HARD — it outranks the serve-the-pin path"
        );
        assert_eq!(
            pin_of(&manager, 5678),
            Some(served),
            "and it re-keys the session DURABLY, not just for this request"
        );
        // Durable: a fresh select with nothing tried stays on the new account.
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(5678)),
            Some(served),
            "the session must not snap back to the held account"
        );
    }

    /// `quota.status == "rejected"` is Anthropic's own verdict, so it is HARD even
    /// though the account looks identical to the `allowed_warning` case above in
    /// every other field. This pair is the whole soft/hard distinction in two tests.
    #[test]
    fn pin_rejected_by_upstream_is_rekeyed() {
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0)]),
            pacing_refresher(),
        );
        let now = OffsetDateTime::now_utc();
        let pinned = manager
            .select(&HashSet::new(), now, None, Some(3456))
            .expect("an account is eligible");
        set_over_threshold(&manager, pinned, 0.995, "rejected");

        let served = manager
            .select(&HashSet::new(), now, None, Some(3456))
            .expect("the allowed account serves this request");
        assert_ne!(served, pinned, "a rejected pin cannot serve");
        assert_eq!(
            manager
                .affinity
                .lock()
                .expect("affinity lock poisoned")
                .get(&3456)
                .map(|&(idx, _)| idx),
            Some(served),
            "an upstream `rejected` is a HARD gate — it must re-key the session"
        );
    }

    /// A pin landing in `tried` means this ONE request failed over it — a dropped
    /// connection or a 5xx, not proof the account is gone. Divert, keep the pin.
    /// Self-healing: when the failure is durable it arms a hold, and the very next
    /// select re-keys on that evidence instead of on a single blip.
    #[test]
    fn transient_tried_failure_keeps_the_pin() {
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0)]),
            pacing_refresher(),
        );
        let now = OffsetDateTime::now_utc();
        let pinned = manager
            .select(&HashSet::new(), now, None, Some(4567))
            .expect("an account is eligible");
        let tried: HashSet<usize> = [pinned].into_iter().collect();

        let served = manager
            .select(&tried, now, None, Some(4567))
            .expect("the untried account serves this request");
        assert_ne!(served, pinned, "the tried pin cannot serve THIS request");
        assert_eq!(
            manager
                .affinity
                .lock()
                .expect("affinity lock poisoned")
                .get(&4567)
                .map(|&(idx, _)| idx),
            Some(pinned),
            "a transient failover must not re-key the session"
        );
        // Nothing tried → straight back to the warm pin.
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(4567)),
            Some(pinned),
            "the session must return to its original account"
        );

        // Now the failure proves durable (a 429 armed a hold long enough to outlive
        // the account's prompt cache) — that IS hard, so the same tried-pin select
        // re-keys.
        manager.mark_rate_limited(pinned, LONG_HOLD_SECS);
        let moved = manager
            .select(&tried, now, None, Some(4567))
            .expect("the un-held account serves this request");
        assert_eq!(
            manager
                .affinity
                .lock()
                .expect("affinity lock poisoned")
                .get(&4567)
                .map(|&(idx, _)| idx),
            Some(moved),
            "a durable failure still re-keys — the divert-and-keep is self-healing"
        );
    }

    /// Drive account `idx` over the soft threshold on its MODEL-SCOPED weekly
    /// (`7d_oi`, the Fable bucket) with a reset 48h out, leaving every shared
    /// dimension healthy: the account is out of Fable and serves everything else
    /// perfectly. The companion to [`set_over_threshold`], which moves the SHARED
    /// weekly instead.
    fn set_fable_exhausted(manager: &Manager, idx: usize, util: f64) {
        let now = OffsetDateTime::now_utc();
        let mut a = manager.accounts.write().expect("accounts lock poisoned");
        a[idx].quota.seven_day_oi = Some(crate::quota::QuotaWindow {
            utilization: util,
            reset: Some(now + Duration::hours(48)),
        });
    }

    /// Model-scoped exhaustion is a property of the REQUEST CLASS, not of the
    /// account: an account out of its Fable weekly still answers Opus perfectly. So a
    /// Fable request whose pin is Fable-exhausted diverts THAT ONE request and keeps
    /// the pin — the session's next Opus turn must still land on the account holding
    /// its warm prefix.
    ///
    /// This is the live shape, not a corner case: every Claude Code session mixes
    /// classes (Opus for the conversation, a one-line Fable call for titles and
    /// summaries) while a real fleet reads 95-99% on the Fable weekly across nearly
    /// every account. Counted as account death, one cheap title request re-keyed a
    /// 200k-token conversation onto a cold account.
    #[test]
    fn fable_exhaustion_diverts_the_request_but_keeps_the_pin() {
        let manager = build_manager(
            config_with(vec![account("home", 0), account("other", 0)]),
            pacing_refresher(),
        );
        let now = OffsetDateTime::now_utc();
        let key = 909_090u64;
        let home = manager
            .select(&HashSet::new(), now, Some("claude-opus-4-6"), Some(key))
            .expect("an account is eligible");
        set_fable_exhausted(&manager, home, 0.999);

        let served = manager
            .select(&HashSet::new(), now, Some("claude-fable-5"), Some(key))
            .expect("the other account can still serve Fable");
        assert_ne!(
            served, home,
            "a Fable request must not be served from an exhausted Fable weekly"
        );
        assert_eq!(
            pin_of(&manager, key),
            Some(home),
            "model-scoped exhaustion is a per-REQUEST fact — it may divert a \
             request, it may never re-key the session"
        );
        assert_eq!(
            manager.select(&HashSet::new(), now, Some("claude-opus-4-6"), Some(key)),
            Some(home),
            "the next non-Fable request must come home to the warm prefix"
        );
    }

    /// THE over-correction guard for the test above: the model gate softened, and
    /// nothing else did. Same Fable-exhausted pin, but now the account is also under a
    /// 429 hold that outlives its prompt cache — ACCOUNT-level death, which must still
    /// re-key durably.
    #[test]
    fn account_death_still_rekeys_a_fable_session() {
        let manager = build_manager(
            config_with(vec![account("home", 0), account("other", 0)]),
            pacing_refresher(),
        );
        let now = OffsetDateTime::now_utc();
        let key = 808_080u64;
        let home = manager
            .select(&HashSet::new(), now, Some("claude-opus-4-6"), Some(key))
            .expect("an account is eligible");
        set_fable_exhausted(&manager, home, 0.999);
        // The account is not merely out of Fable — it is gone for every model class,
        // and for longer than its prompt cache survives.
        manager.mark_rate_limited(home, LONG_HOLD_SECS);

        let served = manager
            .select(&HashSet::new(), now, Some("claude-fable-5"), Some(key))
            .expect("the un-held account serves this request");
        assert_ne!(served, home, "a held pin cannot serve");
        assert_eq!(
            pin_of(&manager, key),
            Some(served),
            "a hold outliving the prompt cache is ACCOUNT-level death — it must \
             still re-key the session"
        );
        assert_eq!(
            manager.select(&HashSet::new(), now, Some("claude-opus-4-6"), Some(key)),
            Some(served),
            "and durably: the Opus turn stays on the failover, it does not snap back"
        );
    }

    /// The same invariant one path over. When the whole fleet reads over the SOFT
    /// threshold, `select` returns `None` and the request lands in
    /// `select_revalidation` — which also re-pins. A Fable request whose pin is
    /// Fable-exhausted must be SERVED elsewhere there too while the pin stays put.
    ///
    /// Live-reachable, not theoretical: a fleet at 95-99% Fable has no Fable-eligible
    /// account for `select` to pick, so this is the path a title request actually
    /// takes.
    #[test]
    fn revalidation_fable_divert_keeps_the_pin() {
        let manager = build_manager(
            config_with(vec![account("home", 0), account("other", 0)]),
            pacing_refresher(),
        );
        let now = OffsetDateTime::now_utc();
        let key = 707_070u64;
        let home = manager
            .select(&HashSet::new(), now, Some("claude-opus-4-6"), Some(key))
            .expect("an account is eligible");
        let other = 1 - home;
        // The whole fleet crosses the SOFT threshold, and the pin is out of Fable.
        set_over_threshold(&manager, home, 0.99, "allowed_warning");
        set_over_threshold(&manager, other, 0.96, "allowed_warning");
        set_fable_exhausted(&manager, home, 0.999);

        assert_eq!(
            manager.select(&HashSet::new(), now, Some("claude-fable-5"), Some(key)),
            None,
            "every account is over the soft threshold → normal select benches all"
        );
        assert_eq!(
            manager.select_revalidation(&HashSet::new(), now, Some("claude-fable-5"), Some(key)),
            Some(other),
            "the revalidation serve routes the Fable request to an account that can \
             answer it"
        );
        assert_eq!(
            pin_of(&manager, key),
            Some(home),
            "and it must NOT have re-keyed the session — the pin is model-blocked \
             for this request only"
        );
        // A non-Fable request comes straight home: `select`'s pin-honor path serves
        // the over-threshold pin rather than diverting it.
        assert_eq!(
            manager.select(&HashSet::new(), now, Some("claude-opus-4-6"), Some(key)),
            Some(home),
            "the Opus turn is served by the warm pin"
        );
    }

    /// A rate-limit hold is a TIMER, not a death — and the timers this proxy arms
    /// are mostly SHORT: a no-guidance transient 429 parks 15s + jitter, and a
    /// `retry-after` park is clamped to 300s (`src/proxy.rs`). Anthropic's default
    /// ephemeral prompt cache lives 5 minutes, so re-keying on a 15-second park
    /// throws away a prefix that would still be warm 15 seconds later — and the
    /// session never returns to it, because a re-key is durable.
    ///
    /// So a hold that clears inside [`CACHE_WARM_HOLD_SECS`] is SOFT: it diverts
    /// THIS one request (the account really is parked and would only answer another
    /// 429) and leaves the pin alone, and the session comes home warm when the timer
    /// runs out. The over-correction guard is `long_hold_rekeys_the_session`, which
    /// is this same fixture with only the hold duration changed.
    #[test]
    fn short_hold_diverts_but_keeps_the_pin() {
        let manager = build_manager(
            config_with(vec![account("home", 0), account("other", 0)]),
            pacing_refresher(),
        );
        let now = OffsetDateTime::now_utc();
        let key = 515_151u64;
        let home = manager
            .select(&HashSet::new(), now, None, Some(key))
            .expect("an account is eligible");
        manager.mark_rate_limited(home, SHORT_HOLD_SECS);

        let diverted = manager
            .select(&HashSet::new(), now, None, Some(key))
            .expect("the un-held account serves this request");
        assert_ne!(
            diverted, home,
            "a parked pin cannot serve THIS request — serving it only buys a 429"
        );
        assert_eq!(
            pin_of(&manager, key),
            Some(home),
            "a hold that clears while the cache is still warm may divert a request, \
             it may never re-key the session"
        );

        // The timer runs out. A past hold reads as expired live, no mutation needed.
        let after = now + Duration::seconds(SHORT_HOLD_SECS + 5);
        for _ in 0..3 {
            assert_eq!(
                manager.select(&HashSet::new(), after, None, Some(key)),
                Some(home),
                "once the hold clears the session must come home to the account \
                 still holding its warm prefix"
            );
        }
    }

    /// THE over-correction guard for the test above: only the SHORT holds softened.
    /// The same fixture with a hold that outlives the prompt cache must still re-key
    /// durably — by the time that account frees, the prefix it was holding is gone,
    /// so keeping the pin would divert through the LRU pick on every turn and
    /// scatter the conversation cold across the fleet instead of settling it on one
    /// account that can warm up.
    #[test]
    fn long_hold_rekeys_the_session() {
        let manager = build_manager(
            config_with(vec![account("home", 0), account("other", 0)]),
            pacing_refresher(),
        );
        let now = OffsetDateTime::now_utc();
        let key = 525_252u64;
        let home = manager
            .select(&HashSet::new(), now, None, Some(key))
            .expect("an account is eligible");
        manager.mark_rate_limited(home, LONG_HOLD_SECS);

        let served = manager
            .select(&HashSet::new(), now, None, Some(key))
            .expect("the un-held account serves this request");
        assert_ne!(served, home, "a parked pin cannot serve");
        assert_eq!(
            pin_of(&manager, key),
            Some(served),
            "a hold that outlives the prompt cache is ACCOUNT-level death — it must \
             re-key the session"
        );
        // Durable: a fresh select with nothing tried stays on the new account.
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(key)),
            Some(served),
            "the session must not snap back to the held account"
        );
    }

    /// Pin a session, park its account with EXACTLY `remaining_ms` left to run, and
    /// report whether the next select RE-KEYED the session (`true`) or merely
    /// diverted the request while keeping the pin (`false`).
    ///
    /// Writes `rate_limited_until_ms` directly instead of going through
    /// `mark_rate_limited`, which anchors the deadline to the process wall clock and
    /// therefore cannot express an exact remaining duration relative to the `now`
    /// the select is given — the whole point of a boundary test.
    fn rekeys_with_hold_remaining(remaining_ms: i64) -> bool {
        let manager = build_manager(
            config_with(vec![account("home", 0), account("other", 0)]),
            pacing_refresher(),
        );
        let now = OffsetDateTime::now_utc();
        let now_ms = odt_to_ms(now);
        let key = 424_242u64;
        let home = manager
            .select(&HashSet::new(), now, None, Some(key))
            .expect("an account is eligible");
        {
            let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
            accounts[home].status = AccountStatus::Throttled;
            accounts[home].rate_limited_until_ms = Some(now_ms + remaining_ms);
        }
        let served = manager
            .select(&HashSet::new(), now, None, Some(key))
            .expect("the un-held account serves this request");
        assert_ne!(
            served, home,
            "a parked pin cannot serve, at any hold length"
        );
        pin_of(&manager, key) != Some(home)
    }

    /// Pins the dividing line itself, from BOTH sides, so a refactor cannot drift it
    /// silently: a hold with exactly [`CACHE_WARM_HOLD_SECS`] left is LONG (the cache
    /// dies at the same instant the account frees, so there is nothing to come home
    /// to), and one millisecond less is SHORT.
    ///
    /// Asserted through `select`, not against the predicate, so it is the ROUTING
    /// decision that is nailed down rather than an implementation detail.
    #[test]
    fn hold_exactly_at_the_boundary_is_treated_as_long() {
        let boundary_ms = CACHE_WARM_HOLD_SECS * 1000;
        assert!(
            rekeys_with_hold_remaining(boundary_ms),
            "a hold with exactly CACHE_WARM_HOLD_SECS left must re-key — the prefix \
             is gone at the same instant the account frees"
        );
        assert!(
            !rekeys_with_hold_remaining(boundary_ms - 1),
            "one millisecond under the line must keep the pin — the boundary is \
             `>=`, and this is the assertion that stops it drifting"
        );
    }

    /// Once a hold LONG enough to have re-keyed a session has moved it onto a
    /// failover account, the expiry of that hold does NOT bring the session home:
    /// the pin is simply the failover from then on.
    ///
    /// That is the whole justification for re-keying on a long hold at all. By the
    /// time the original account frees, the prefix it was holding is gone
    /// ([`CACHE_WARM_HOLD_SECS`]), so coming home would buy a cold start on the way
    /// back and no cache hit at the other end — strictly worse than staying put.
    ///
    /// The complement is `short_hold_diverts_but_keeps_the_pin`: a hold that clears
    /// while the cache is still warm never re-keys in the first place, so there is
    /// nothing to come home FROM. The two together are the whole rule.
    #[test]
    fn hold_expiry_leaves_the_session_on_its_failover() {
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0)]),
            pacing_refresher(),
        );
        let now = OffsetDateTime::now_utc();
        let key = 606_060u64;
        let home = manager
            .select(&HashSet::new(), now, None, Some(key))
            .expect("an account is eligible");
        manager.mark_rate_limited(home, LONG_HOLD_SECS);

        let failover = manager
            .select(&HashSet::new(), now, None, Some(key))
            .expect("the un-held account serves this request");
        assert_ne!(
            failover, home,
            "a hold outliving the prompt cache re-keys the session"
        );
        assert_eq!(pin_of(&manager, key), Some(failover));

        // The hold expires. A past hold reads as expired live, no mutation needed.
        let after = now + Duration::hours(1);
        assert_eq!(
            manager.select(&HashSet::new(), after, None, Some(key)),
            Some(failover),
            "the session stays on its failover once the hold clears — the original \
             account's prompt cache is long gone, so coming home would only buy a \
             second cold start"
        );
        assert_eq!(
            pin_of(&manager, key),
            Some(failover),
            "and the pin is not walked back either"
        );
    }

    /// Distinct session keys fan out to different accounts (each initial pin is a
    /// normal LRU pick) and then each key stays on its own account.
    #[test]
    fn affinity_distinct_keys_fan_out_then_stick() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0)]),
            refresher,
        );
        let now = OffsetDateTime::now_utc();
        let x = manager
            .select(&HashSet::new(), now, None, Some(1))
            .expect("an account is eligible");
        let y = manager
            .select(&HashSet::new(), now, None, Some(2))
            .expect("an account is eligible");
        assert_ne!(x, y, "distinct keys' initial pins fan out across the tier");
        // Each key repeats onto its own account.
        for _ in 0..3 {
            assert_eq!(manager.select(&HashSet::new(), now, None, Some(1)), Some(x));
            assert_eq!(manager.select(&HashSet::new(), now, None, Some(2)), Some(y));
        }
    }

    /// The affinity map is bounded by `AFFINITY_CAP` + LRU-by-last-touch: filling it
    /// past the cap evicts the single oldest-last-touch pin, and a recently-touched
    /// pin survives even when it was the oldest by insertion order.
    #[test]
    fn affinity_map_is_bounded_by_cap_with_lru_eviction() {
        // Mirror of the private `AFFINITY_CAP` in `select`.
        const AFFINITY_CAP: usize = 1024;
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0)]),
            refresher,
        );
        let base = OffsetDateTime::now_utc();

        // Fill to exactly CAP distinct keys, each stamped at a strictly later
        // last-touch than the previous (key i+1 touched at base + i seconds).
        for i in 0..AFFINITY_CAP {
            let key = (i + 1) as u64;
            manager
                .select(
                    &HashSet::new(),
                    base + time::Duration::seconds(i as i64),
                    None,
                    Some(key),
                )
                .expect("an account is eligible");
        }
        assert_eq!(
            manager
                .affinity
                .lock()
                .expect("affinity lock poisoned")
                .len(),
            AFFINITY_CAP,
            "map fills to exactly CAP without evicting yet (len == CAP, not > CAP)"
        );

        // Refresh the oldest-by-insertion pin (key 1) at the latest time, so its
        // last-touch becomes the newest and it must survive the next eviction. Key 2
        // is now the oldest-last-touch entry.
        manager.select(
            &HashSet::new(),
            base + time::Duration::seconds((AFFINITY_CAP + 10) as i64),
            None,
            Some(1),
        );

        // One more distinct key pushes len over CAP → evict the single oldest.
        manager.select(
            &HashSet::new(),
            base + time::Duration::seconds((AFFINITY_CAP + 11) as i64),
            None,
            Some((AFFINITY_CAP + 1) as u64),
        );

        let pins = manager.affinity.lock().expect("affinity lock poisoned");
        assert_eq!(pins.len(), AFFINITY_CAP, "eviction holds the map at CAP");
        assert!(pins.contains_key(&1), "the recently-touched pin survives");
        assert!(
            !pins.contains_key(&2),
            "the oldest-last-touch pin is evicted"
        );
        assert!(
            pins.contains_key(&((AFFINITY_CAP + 1) as u64)),
            "the just-inserted pin is present"
        );
    }

    /// Directly pin `key` to account `idx` in the affinity map (last-touch = now),
    /// bypassing a select() call — lets a migration test set up an arbitrary stacked
    /// starting layout without depending on how the initial pins were formed.
    fn pin_session(manager: &Manager, key: u64, idx: usize) {
        manager
            .affinity
            .lock()
            .expect("affinity lock poisoned")
            .insert(key, (idx, crate::now_ms()));
    }

    /// Like [`config_with`] but with the (default-OFF) load-balancing migration
    /// explicitly enabled — every test that exercises the migration scan itself
    /// must opt in, exactly as an operator would via `~/.config/teamclaude.json`.
    fn config_with_migration(accounts: Vec<Account>) -> Config {
        let mut config = config_with(accounts);
        config.extra.insert(
            "loadBalanceMigration".to_string(),
            serde_json::Value::Bool(true),
        );
        config
    }

    /// Read the account a session key is currently pinned to (None if unpinned).
    fn pin_of(manager: &Manager, key: u64) -> Option<usize> {
        manager
            .affinity
            .lock()
            .expect("affinity lock poisoned")
            .get(&key)
            .map(|&(idx, _)| idx)
    }

    /// Migration #1 — a LONE session is NEVER migrated: its warm cache is preserved
    /// even when an idle, eligible account sits right next to it. `count(X) < 2` must
    /// be byte-identical to honouring the pin.
    #[test]
    fn lone_session_never_migrates() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0)]),
            refresher,
        );
        let now = OffsetDateTime::now_utc();
        // One session pinned to acct 0; acct 1 is idle and eligible.
        pin_session(&manager, 100, 0);
        for _ in 0..6 {
            assert_eq!(
                manager.select(&HashSet::new(), now, None, Some(100)),
                Some(0),
                "a lone session must stay on its warm account (no migration)"
            );
        }
        assert_eq!(pin_of(&manager, 100), Some(0), "the pin never moved");
    }

    /// Migration #2 — two sessions stacked on acct 0 with acct 1 idle+eligible and
    /// `loadBalanceMigration` ENABLED: selecting for one of them migrates it to
    /// acct 1, and the affinity map records the new pin.
    #[test]
    fn stacked_session_migrates_to_idle() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(
            config_with_migration(vec![account("a", 0), account("b", 0)]),
            refresher,
        );
        let now = OffsetDateTime::now_utc();
        pin_session(&manager, 10, 0);
        pin_session(&manager, 11, 0);
        // count(0)=2, count(1)=0 → 0+1 < 2, so migrate this session onto acct 1.
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(10)),
            Some(1),
            "a stacked session migrates to the idle eligible account"
        );
        assert_eq!(
            pin_of(&manager, 10),
            Some(1),
            "the affinity map now pins the migrated session to acct 1"
        );
    }

    /// Migration #3 — convergence, no thrash (with `loadBalanceMigration` ENABLED):
    /// after #2's migration the layout is balanced (one session each), so further
    /// selects for BOTH sessions are stable — neither bounces back.
    #[test]
    fn migration_converges_no_thrash() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(
            config_with_migration(vec![account("a", 0), account("b", 0)]),
            refresher,
        );
        let now = OffsetDateTime::now_utc();
        pin_session(&manager, 10, 0);
        pin_session(&manager, 11, 0);
        // The one migration from #2.
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(10)),
            Some(1)
        );
        // Now 1-and-1: every further select is a lone session on its account.
        for _ in 0..5 {
            assert_eq!(
                manager.select(&HashSet::new(), now, None, Some(10)),
                Some(1),
                "the migrated session stays put (no bounce back)"
            );
            assert_eq!(
                manager.select(&HashSet::new(), now, None, Some(11)),
                Some(0),
                "the remaining session stays put"
            );
        }
    }

    /// Migration #4 — no eligible emptier account, no migration: two sessions stack on
    /// acct 0 and the only other account is errored, so the stacked session honours
    /// its pin rather than migrating onto a throttled/dead account.
    #[test]
    fn no_migration_when_no_emptier_eligible() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0)]),
            refresher,
        );
        let now = OffsetDateTime::now_utc();
        pin_session(&manager, 20, 0);
        pin_session(&manager, 21, 0);
        manager.mark_error(1); // the only alternative is ineligible
        for _ in 0..5 {
            assert_eq!(
                manager.select(&HashSet::new(), now, None, Some(20)),
                Some(0),
                "no eligible emptier account → honour the pin, never migrate onto a dead one"
            );
        }
        assert_eq!(pin_of(&manager, 20), Some(0), "the pin never moved");
    }

    /// Migration #5 — with `loadBalanceMigration` ENABLED, three sessions stacked on
    /// acct 0 with accts 1,2 idle spread to one-each after a round of selects
    /// (convergence across three accounts).
    #[test]
    fn three_sessions_spread_across_three_idle() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(
            config_with_migration(vec![account("a", 0), account("b", 0), account("c", 0)]),
            refresher,
        );
        let now = OffsetDateTime::now_utc();
        pin_session(&manager, 1, 0);
        pin_session(&manager, 2, 0);
        pin_session(&manager, 3, 0);
        // A round of selects, one per session.
        for key in [1u64, 2, 3] {
            manager.select(&HashSet::new(), now, None, Some(key));
        }
        let mut homes = [
            pin_of(&manager, 1),
            pin_of(&manager, 2),
            pin_of(&manager, 3),
        ]
        .map(|p| p.expect("each session stays pinned"));
        homes.sort_unstable();
        assert_eq!(
            homes,
            [0, 1, 2],
            "three stacked sessions spread to one-per-account after a round of selects"
        );
    }

    /// Migration #6 — the DEFAULT is no migration at all. Identical stacked layout to
    /// #2 (two sessions on acct 0, acct 1 idle and eligible): with a plain config
    /// NEITHER session moves, because a session that already has a pin is by
    /// definition warm and Anthropic's prompt cache is per-account, so re-pinning it
    /// merely to even out counts throws that cache away. The SAME layout under
    /// `"loadBalanceMigration": true` still migrates — proof the behaviour is gated,
    /// not deleted.
    #[test]
    fn migration_is_off_by_default_and_pin_is_honoured() {
        let now = OffsetDateTime::now_utc();

        // --- default config: the migration scan never runs ---
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0)]),
            Arc::new(CountingRefresher {
                calls: Arc::new(AtomicUsize::new(0)),
            }),
        );
        assert!(
            !manager.load_balance_migration_enabled(),
            "loadBalanceMigration must default to OFF"
        );
        pin_session(&manager, 30, 0);
        pin_session(&manager, 31, 0);
        // count(0)=2, count(1)=0 — exactly the condition that used to migrate.
        for _ in 0..5 {
            assert_eq!(
                manager.select(&HashSet::new(), now, None, Some(30)),
                Some(0),
                "a stacked session keeps its warm account when migration is off"
            );
            assert_eq!(
                manager.select(&HashSet::new(), now, None, Some(31)),
                Some(0),
                "its stack-mate keeps the same account too"
            );
        }
        assert_eq!(pin_of(&manager, 30), Some(0), "pin 30 never moved");
        assert_eq!(pin_of(&manager, 31), Some(0), "pin 31 never moved");

        // --- same layout, key explicitly true: the old behaviour is intact ---
        let enabled = build_manager(
            config_with_migration(vec![account("a", 0), account("b", 0)]),
            Arc::new(CountingRefresher {
                calls: Arc::new(AtomicUsize::new(0)),
            }),
        );
        assert!(
            enabled.load_balance_migration_enabled(),
            "the explicit key must read back as ON"
        );
        pin_session(&enabled, 30, 0);
        pin_session(&enabled, 31, 0);
        assert_eq!(
            enabled.select(&HashSet::new(), now, None, Some(30)),
            Some(1),
            "with loadBalanceMigration=true the stacked session still migrates"
        );
        assert_eq!(
            pin_of(&enabled, 30),
            Some(1),
            "and the affinity map records the migrated pin"
        );
    }

    /// When the only future reset across the fleet is the model-scoped weekly
    /// (`seven_day_oi`) window — the bucket Fable requests gate on —
    /// `retry_after_hint` advertises that reset, not the 60s fallthrough default.
    #[test]
    fn retry_after_hint_uses_seven_day_oi_window() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(config_with(vec![account("a", 0)]), refresher);
        let now = OffsetDateTime::now_utc();
        // Only the 7d_oi window has a future reset (~2h out); five_hour / seven_day /
        // rate-limit hold are all absent, so before the fix the hint fell through to 60s.
        let reset_ms = odt_to_ms(now) + 2 * 3_600_000;
        manager.apply_usage(
            0,
            &crate::probe::Usage {
                five_hour: None,
                seven_day: None,
                seven_day_oi: Some(crate::probe::UsageBucket {
                    utilization: Some(0.99),
                    reset_at_ms: Some(reset_ms),
                }),
            },
        );
        // A FABLE request (`is_fable = true`) is the only one gated by the 7d_oi
        // bucket, so it is what makes that window drive the hint. A non-Fable
        // request ignores 7d_oi entirely and falls through to the 60s default.
        let hint = manager.retry_after_hint(now, true);
        assert!(
            hint > 60,
            "the 7d_oi reset must drive the hint past the 60s default, got {hint}"
        );
        let expected = ((reset_ms - odt_to_ms(now) + 999) / 1000).max(1);
        assert_eq!(
            hint, expected,
            "hint equals the 7d_oi reset delta in seconds"
        );
        // The general (non-Fable) view does NOT see the Fable-only weekly, so with
        // no other gate the hint falls back to the 60s default — proof the
        // `is_fable` scoping is honoured, not hard-coded.
        assert_eq!(
            manager.retry_after_hint(now, false),
            60,
            "a non-Fable request ignores the 7d_oi bucket"
        );
    }

    /// A quota window at `util`, resetting at `reset` (`None` = unknown reset).
    fn window(util: f64, reset: Option<OffsetDateTime>) -> crate::quota::QuotaWindow {
        crate::quota::QuotaWindow {
            utilization: util,
            reset,
        }
    }

    /// A fresh active runtime with empty quota, for the [`Manager::account_gate`] tests.
    fn gate_runtime() -> AccountRuntime {
        AccountRuntime::from_config(&account("gate", 0))
    }

    #[test]
    fn account_gate_ok_when_healthy() {
        let now = OffsetDateTime::now_utc();
        let a = gate_runtime();
        assert_eq!(
            Manager::account_gate(&a, 0.90, now, odt_to_ms(now), false),
            (GateReason::Ok, None)
        );
    }

    #[test]
    fn account_gate_disabled_and_error_never_self_free() {
        let now = OffsetDateTime::now_utc();
        // Even with a gating window present, the terminal states dominate and carry
        // NO clear-instant — they never self-free.
        let mut disabled = gate_runtime();
        disabled.disabled = true;
        disabled.quota.five_hour = Some(window(0.99, Some(now + Duration::seconds(300))));
        assert_eq!(
            Manager::account_gate(&disabled, 0.90, now, odt_to_ms(now), false),
            (GateReason::Disabled, None)
        );

        let mut errored = gate_runtime();
        errored.status = AccountStatus::Error;
        errored.quota.five_hour = Some(window(0.99, Some(now + Duration::seconds(300))));
        assert_eq!(
            Manager::account_gate(&errored, 0.90, now, odt_to_ms(now), false),
            (GateReason::Login, None)
        );
    }

    #[test]
    fn account_gate_binds_on_latest_clearing_window() {
        // 5h clears SOON, 7d clears LATER — the account frees only when BOTH clear,
        // so the reason is the later 7d gate and free_at is its (max) reset. A naive
        // "soonest reset" would wrongly report the 5h instant.
        let now = OffsetDateTime::now_utc();
        let soon = now + Duration::seconds(300);
        let later = now + Duration::seconds(5_000);
        let mut a = gate_runtime();
        a.quota.five_hour = Some(window(0.99, Some(soon)));
        a.quota.seven_day = Some(window(0.99, Some(later)));
        assert_eq!(
            Manager::account_gate(&a, 0.90, now, odt_to_ms(now), false),
            (GateReason::SevenDay, Some(later))
        );
    }

    #[test]
    fn account_gate_unknown_reset_sorts_latest_and_hides_time() {
        // A window over threshold whose reset is unknown (None) is the longest
        // possible constraint: it becomes the reason and free_at is None (no promise
        // of a time), even beside a window with a known soon reset.
        let now = OffsetDateTime::now_utc();
        let mut a = gate_runtime();
        a.quota.five_hour = Some(window(0.99, Some(now + Duration::seconds(300))));
        a.quota.seven_day = Some(window(0.99, None));
        assert_eq!(
            Manager::account_gate(&a, 0.90, now, odt_to_ms(now), false),
            (GateReason::SevenDay, None)
        );
    }

    #[test]
    fn account_gate_fable_weekly_only_gates_fable() {
        // The 7d_oi bucket gates a Fable evaluation and is invisible to the general
        // one — the exact `is_fable` split `eligible` makes.
        let now = OffsetDateTime::now_utc();
        let reset = now + Duration::seconds(4_000);
        let mut a = gate_runtime();
        a.quota.seven_day_oi = Some(window(0.99, Some(reset)));
        assert_eq!(
            Manager::account_gate(&a, 0.90, now, odt_to_ms(now), false),
            (GateReason::Ok, None),
            "the non-Fable view ignores the model-scoped weekly"
        );
        assert_eq!(
            Manager::account_gate(&a, 0.90, now, odt_to_ms(now), true),
            (GateReason::FableWeekly, Some(reset)),
            "a Fable evaluation gates on it"
        );
    }

    /// A near standard (API-key) token limit with a future `standard_reset` and no
    /// other gate is reported as `(Standard, Some(reset))` — account_gate now sees
    /// the same dimension `eligible`/`is_near` enforces.
    #[test]
    fn account_gate_gates_a_near_standard_limit() {
        let now = OffsetDateTime::now_utc();
        let reset = now + Duration::seconds(600);
        let mut a = gate_runtime();
        a.quota.tokens_limit = Some(1_000);
        a.quota.tokens_remaining = Some(50); // 95% spent, over the 0.90 threshold
        a.quota.standard_reset = Some(reset);
        assert_eq!(
            Manager::account_gate(&a, 0.90, now, odt_to_ms(now), false),
            (GateReason::Standard, Some(reset))
        );
    }

    /// The exact G2 scenario: an account with BOTH a short transient Hold (+8s) AND
    /// a near standard limit (reset +600s) must bind on the later-clearing Standard
    /// gate, so `retry_after_hint` reports the true 600s recovery — not the 8s Hold
    /// that would trigger a soft-wait then hard-fail anyway.
    #[test]
    fn account_gate_standard_reset_outlasts_a_short_hold() {
        let now = OffsetDateTime::now_utc();
        let now_ms = odt_to_ms(now);
        let reset = now + Duration::seconds(600);
        let mut a = AccountRuntime::from_config(&account("std-hold", 0));
        a.switch_threshold = Some(0.90);
        a.rate_limited_until_ms = Some(now_ms + 8_000); // short Hold, +8s
        a.quota.requests_limit = Some(200);
        a.quota.requests_remaining = Some(5); // 97.5% spent
        a.quota.standard_reset = Some(reset);

        // The Standard gate (later reset) wins max_by_key over the +8s Hold.
        assert_eq!(
            Manager::account_gate(&a, 0.90, now, now_ms, false),
            (GateReason::Standard, Some(reset)),
            "the standard reset outlasts the short hold"
        );

        // retry_after_hint must now reflect the real 600s, not the 8s Hold.
        let manager = Manager::from_runtimes(vec![a]);
        let hint = manager.retry_after_hint(now, false);
        let expected = ((odt_to_ms(reset) - now_ms + 999) / 1000).max(1);
        assert_eq!(hint, expected, "hint must be the 600s reset, got {hint}");
        assert!(hint > 500, "must not report the 8s hold, got {hint}");
    }

    /// G-a: an OAuth account (all standard fields `None`) produces NO Standard gate,
    /// so account_gate is byte-identical to its pre-fix behavior for OAuth.
    #[test]
    fn account_gate_ignores_standard_for_oauth() {
        let now = OffsetDateTime::now_utc();
        let a = gate_runtime(); // all standard fields None by default
        assert_eq!(
            Manager::account_gate(&a, 0.90, now, odt_to_ms(now), false),
            (GateReason::Ok, None),
            "OAuth accounts never gate on the standard dimension"
        );
    }

    /// An expired `standard_reset` (in the PAST) means the upstream limit has
    /// refreshed, so a spent count must NOT keep gating — mirrors is_near's expiry
    /// rule so a spent standard window never pins an API-key account out forever.
    #[test]
    fn account_gate_ignores_expired_standard() {
        let now = OffsetDateTime::now_utc();
        let mut a = gate_runtime();
        a.quota.tokens_limit = Some(1_000);
        a.quota.tokens_remaining = Some(10); // 99% spent, over threshold
        a.quota.standard_reset = Some(now - Duration::seconds(1)); // already refreshed
        assert_eq!(
            Manager::account_gate(&a, 0.90, now, odt_to_ms(now), false),
            (GateReason::Ok, None),
            "an expired standard window no longer gates"
        );
    }

    #[test]
    fn rejected_account_reports_a_rejected_gate() {
        let now = OffsetDateTime::now_utc();
        let now_ms = odt_to_ms(now);
        let mut a = gate_runtime();
        a.quota.status = Some("rejected".to_string());

        // Was `Ok`: `account_hard_ok` held the account out on `rejected` while
        // `account_gate` had no arm for it, so the TUI showed a rejected account as
        // healthy and in rotation.
        assert_eq!(
            Manager::account_gate(&a, 0.90, now, now_ms, false),
            (GateReason::Rejected, None)
        );
        assert!(
            !Manager::account_hard_ok(&a, now_ms),
            "and it stays hard-gated, exactly as before"
        );

        // Terminal, so it dominates a live window and carries NO clear-instant —
        // `retry_after_hint` reads `free_at`, and a rejected account was never going
        // to come back at its 5h reset.
        a.quota.five_hour = Some(window(0.99, Some(now + Duration::seconds(300))));
        assert_eq!(
            Manager::account_gate(&a, 0.90, now, now_ms, false),
            (GateReason::Rejected, None),
            "a rejected account must not advertise a window reset as its recovery"
        );
    }

    /// The drift guard, and the reason R7 exists. [`Manager::account_gate`] (what
    /// the TUI and `retry_after_hint` read) and [`Manager::account_hard_ok`] (whether
    /// a session may lose its pin) once kept two hand-maintained gate lists, and they
    /// drifted: `rejected` was on one and missing from the other.
    ///
    /// The two are deliberately NOT equal — a quota window or a short hold gates the
    /// display while leaving the pin alone — so this pins the RELATIONSHIP per
    /// variant instead. The classification below is exhaustive over [`GateReason`],
    /// so adding a variant stops compiling until someone decides which side of the
    /// account/request line it falls on.
    #[test]
    fn gate_and_hard_ok_agree_on_every_variant() {
        let now = OffsetDateTime::now_utc();
        let now_ms = odt_to_ms(now);
        let reset = now + Duration::seconds(5_000);

        const ALL: [GateReason; 9] = [
            GateReason::Ok,
            GateReason::Hold,
            GateReason::FiveHour,
            GateReason::SevenDay,
            GateReason::FableWeekly,
            GateReason::Standard,
            GateReason::Login,
            GateReason::Disabled,
            GateReason::Rejected,
        ];

        for reason in ALL {
            // Only a Fable-scoped evaluation can ever surface the model-scoped gate.
            let is_fable = reason == GateReason::FableWeekly;

            // Per case: a label, a runtime that actually exhibits `reason`, and
            // whether that block is ACCOUNT-level (`account_hard_ok == false`) or
            // request-scoped (`account_hard_ok` stays true, the pin survives).
            let cases: Vec<(&str, AccountRuntime, bool)> = match reason {
                GateReason::Ok => vec![("healthy", gate_runtime(), true)],

                // Terminal: a fact about the credential, for every model class.
                GateReason::Disabled => {
                    let mut a = gate_runtime();
                    a.disabled = true;
                    vec![("operator-disabled", a, false)]
                }
                GateReason::Login => {
                    let mut a = gate_runtime();
                    a.status = AccountStatus::Error;
                    vec![("dead credential", a, false)]
                }
                GateReason::Rejected => {
                    let mut a = gate_runtime();
                    a.quota.status = Some("rejected".to_string());
                    vec![("upstream rejected", a, false)]
                }

                // The one reason that splits on DURATION: past the cache TTL a hold
                // is account death, under it a timer worth keeping the pin for.
                GateReason::Hold => {
                    let mut long = gate_runtime();
                    long.rate_limited_until_ms = Some(now_ms + (CACHE_WARM_HOLD_SECS + 60) * 1_000);
                    let mut short = gate_runtime();
                    short.rate_limited_until_ms = Some(now_ms + 30_000);
                    vec![
                        ("hold outliving the cache", long, false),
                        ("hold clearing while warm", short, true),
                    ]
                }

                // Windows are per-request facts: they gate the display and every
                // serve decision, but must never move a session's pin.
                GateReason::FiveHour => {
                    let mut a = gate_runtime();
                    a.quota.five_hour = Some(window(0.99, Some(reset)));
                    vec![("5h over threshold", a, true)]
                }
                GateReason::SevenDay => {
                    let mut a = gate_runtime();
                    a.quota.seven_day = Some(window(0.99, Some(reset)));
                    vec![("7d over threshold", a, true)]
                }
                GateReason::FableWeekly => {
                    let mut a = gate_runtime();
                    a.quota.seven_day_oi = Some(window(0.99, Some(reset)));
                    vec![("7d_oi over threshold", a, true)]
                }
                GateReason::Standard => {
                    let mut a = gate_runtime();
                    a.quota.tokens_limit = Some(1_000);
                    a.quota.tokens_remaining = Some(10); // 99% spent
                    a.quota.standard_reset = Some(reset);
                    vec![("standard limit spent", a, true)]
                }
            };

            for (label, runtime, account_level) in cases {
                let (gate, _) = Manager::account_gate(&runtime, 0.90, now, now_ms, is_fable);
                assert_eq!(gate, reason, "fixture `{label}` must exhibit {reason:?}");

                let hard_ok = Manager::account_hard_ok(&runtime, now_ms);
                assert_eq!(
                    hard_ok, account_level,
                    "`{label}`: account_hard_ok disagrees with {reason:?}'s classification"
                );

                // The invariant that actually broke: whatever `account_hard_ok` holds
                // out MUST have a reason to show for it. `rejected` violated this.
                if !hard_ok {
                    assert_ne!(
                        gate,
                        GateReason::Ok,
                        "`{label}` is hard-gated but renders Ok — the gate lists have drifted"
                    );
                }
            }
        }
    }

    /// The serving client must never carry a TOTAL request timeout: these responses
    /// are long-lived SSE streams, and a total timeout truncates them mid-stream.
    /// (The `connect_timeout` it does set is not observable here — `reqwest::Client`
    /// exposes no getters and its `Debug` omits it, since the value lives inside the
    /// connector. Only the total timeout is surfaced.)
    #[test]
    fn serving_client_has_no_total_timeout() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(config_with(vec![account("a", 0)]), refresher);

        // reqwest prints the total timeout as a `TotalTimeout` field, and only when
        // one is set. Prove the marker on a control client first, so a future reqwest
        // that renames it fails HERE loudly instead of making the guard below pass
        // vacuously.
        let control = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("build control client");
        assert!(
            format!("{control:?}").contains("TotalTimeout"),
            "reqwest no longer reports a total timeout in Debug — this guard needs rewriting"
        );

        assert!(
            !format!("{:?}", manager.http).contains("TotalTimeout"),
            "the serving client grew a total timeout; it will truncate SSE streams"
        );
    }

    /// The live-incident shape: a fleet-exhausted 429's `retry-after` must name the
    /// TRUE soonest recovery, never an earlier reset belonging to an account that
    /// stays gated for another reason or never self-frees at all.
    #[test]
    fn retry_after_hint_skips_error_and_still_gated_accounts() {
        let now = OffsetDateTime::now_utc();
        let at = |secs: i64| now + Duration::seconds(secs);

        // Account A — weekly-gated: its 5h resets SOON (200s) but its 7d stays over
        // threshold until 5000s, so A does NOT actually return until 5000s.
        let mut a = AccountRuntime::from_config(&account("a", 0));
        a.switch_threshold = Some(0.90);
        a.quota.five_hour = Some(window(0.99, Some(at(200))));
        a.quota.seven_day = Some(window(0.99, Some(at(5_000))));

        // Account B — a dead credential (Error) that holds the SOONEST raw reset
        // (100s). It never self-frees, so it must contribute nothing to the hint.
        let mut b = AccountRuntime::from_config(&account("b", 0));
        b.switch_threshold = Some(0.90);
        b.status = AccountStatus::Error;
        b.quota.five_hour = Some(window(0.99, Some(at(100))));

        // Account C — the TRUE first recovery: 5h-gated with a later reset (900s)
        // and a healthy 7d, so it genuinely returns at 900s.
        let mut c = AccountRuntime::from_config(&account("c", 0));
        c.switch_threshold = Some(0.90);
        c.quota.five_hour = Some(window(0.99, Some(at(900))));

        let manager = Manager::from_runtimes(vec![a, b, c]);
        let hint = manager.retry_after_hint(now, false);

        // Correct = C's 900s reset. The pre-fix raw-min over every window's reset
        // returned ~100s: it counted B's reset (an Error account that never returns)
        // and A's 200s 5h reset (A stays gated on its weekly until 5000s). Both are
        // now skipped, so the min of the real free_at instants is C's 900s.
        let expected = ((odt_to_ms(at(900)) - odt_to_ms(now) + 999) / 1000).max(1);
        assert_eq!(hint, expected, "hint must be C's 900s reset, got {hint}");
        assert!(
            hint > 800,
            "must not report B's 100s or A's 200s reset, got {hint}"
        );
    }

    /// `next_session_key` hands out strictly-increasing, unique u64s starting at 1.
    #[test]
    fn session_keys_are_monotonic_and_unique() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(config_with(vec![account("a", 0)]), refresher);
        let mut prev = 0u64;
        let mut seen = HashSet::new();
        for i in 0..5 {
            let key = manager.next_session_key();
            if i == 0 {
                assert_eq!(key, 1, "the first session key is 1");
            }
            assert!(key > prev, "session keys must strictly increase");
            assert!(seen.insert(key), "session keys must be unique");
            prev = key;
        }
    }

    // ---- durable disable (the TUI's `d`/`e` survives a restart) ----

    /// A config file for `accounts`, in the on-disk shape, carrying an unmodelled
    /// top-level key so collateral damage is visible.
    fn write_account_file(path: &std::path::Path, names: &[&str]) {
        let entries: Vec<String> = names
            .iter()
            .map(|n| {
                format!(
                    r#"{{ "name": "{n}", "type": "oauth", "accessToken": "at-{n}", "refreshToken": "rt-{n}" }}"#
                )
            })
            .collect();
        std::fs::write(
            path,
            format!(
                r#"{{ "warmupSeconds": 900, "accounts": [ {} ] }}"#,
                entries.join(", ")
            ),
        )
        .expect("write test config");
    }

    fn read_config_json(path: &std::path::Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(path).expect("read test config"))
            .expect("test config is valid JSON")
    }

    /// THE regression guard for this fix. Benching an account from the TUI wrote
    /// memory only, so a restart silently returned it to rotation. `set_disabled`
    /// must reach the file — and `e` must remove the key again, not write `false`.
    #[test]
    fn set_disabled_persists_through_to_the_config_file() {
        let path = tmp_config_path("durable-disable");
        write_account_file(&path, &["acct-a", "acct-b"]);
        let manager = build_manager_with_path(
            config_with(vec![account("acct-a", 0), account("acct-b", 0)]),
            path.clone(),
        );

        manager.set_disabled(0, true);

        let after = read_config_json(&path);
        assert_eq!(after["accounts"][0]["disabled"], serde_json::json!(true));
        assert!(
            after["accounts"][1].get("disabled").is_none(),
            "the unrelated account was flagged too: {after}"
        );
        assert_eq!(
            after["warmupSeconds"],
            serde_json::json!(900),
            "an unmodelled key was dropped by the flag write"
        );
        // The in-memory config must agree with the file, so the two views of the
        // flag cannot diverge.
        assert_eq!(
            manager.config.lock().unwrap().accounts[0].disabled,
            Some(true)
        );

        // Re-enabling DROPS the key (the CLI contract), never writes false.
        manager.set_disabled(0, false);
        let after = read_config_json(&path);
        assert!(
            after["accounts"][0].get("disabled").is_none(),
            "re-enable must drop the key, not write false: {after}"
        );
        assert_eq!(manager.config.lock().unwrap().accounts[0].disabled, None);
        std::fs::remove_file(&path).ok();
    }

    /// A manager with NO `config_path` — every test here, `tcr demo`, and
    /// `tcr status --probe` — must write nothing at all. The demo drives
    /// `set_disabled` on rows that have no config file behind them.
    ///
    /// Differential against the test above: same fixture, same call, and the only
    /// difference is the absent path. Without the pair, "the file did not change"
    /// would prove nothing.
    #[test]
    fn set_disabled_writes_nothing_without_a_config_path() {
        let path = tmp_config_path("durable-disable-none");
        write_account_file(&path, &["acct-a"]);
        let before = std::fs::read_to_string(&path).unwrap();

        let manager = build_manager(
            config_with(vec![account("acct-a", 0)]),
            Arc::new(CountingRefresher {
                calls: Arc::new(AtomicUsize::new(0)),
            }),
        );
        assert!(manager.config_path.is_none());

        manager.set_disabled(0, true);

        // The runtime row still flips — only the disk write is suppressed.
        assert!(manager.snapshot(OffsetDateTime::now_utc()).accounts[0].disabled);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "a manager with no config_path must never write to disk"
        );
        std::fs::remove_file(&path).ok();
    }

    /// An out-of-range index is a no-op on both halves: nothing flips in memory
    /// and nothing is written, rather than persisting a flag for an account that
    /// does not exist.
    #[test]
    fn set_disabled_out_of_range_writes_nothing() {
        let path = tmp_config_path("durable-disable-oob");
        write_account_file(&path, &["acct-a"]);
        let before = std::fs::read_to_string(&path).unwrap();
        let manager =
            build_manager_with_path(config_with(vec![account("acct-a", 0)]), path.clone());

        manager.set_disabled(9, true);

        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        std::fs::remove_file(&path).ok();
    }

    // ---- keep-warm (opt-in idle-account warming) ----

    /// `warmupSeconds` reads OFF by default (absent → 0), reads the configured
    /// value, and treats `<= 0` as disabled.
    #[test]
    fn warmup_interval_defaults_off_reads_value_and_disables_on_nonpositive() {
        let refresher = || {
            Arc::new(CountingRefresher {
                calls: Arc::new(AtomicUsize::new(0)),
            })
        };
        // Absent → 0 (dark default).
        let m = build_manager(config_with(vec![account("a", 0)]), refresher());
        assert_eq!(m.warmup_interval_seconds(), 0);
        // A positive value is read through.
        let m = build_manager(config_with_warmup(vec![account("a", 0)], 900), refresher());
        assert_eq!(m.warmup_interval_seconds(), 900);
        // A negative value disables (clamped to 0).
        let m = build_manager(config_with_warmup(vec![account("a", 0)], -5), refresher());
        assert_eq!(m.warmup_interval_seconds(), 0);
    }

    /// `warm_targets` includes only healthy idle OAuth accounts: a disabled,
    /// errored, or throttled account is never a target.
    #[test]
    fn warm_targets_skips_disabled_errored_and_throttled() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let mut api = account("api", 0);
        api.account_type = "api".to_string();
        let manager = build_manager(
            config_with(vec![
                account("idle", 0),
                account("off", 0),
                account("dead", 0),
                account("throttled", 0),
                api, // non-oauth
            ]),
            refresher,
        );
        mark_all_probed(&manager); // isolate from the never-probed boot gate
        manager.set_disabled(1, true);
        manager.mark_error(2);
        manager.mark_rate_limited(3, 300); // → Throttled

        assert_eq!(
            manager.warm_targets(),
            vec![0],
            "only the healthy idle OAuth account is a warm target"
        );
    }

    /// Core contract: an account whose 5h window is LIVE (a future reset) is already
    /// warm and skipped; a cold account (no 5h data) or one whose 5h reset has
    /// passed IS a target.
    #[test]
    fn warm_targets_skips_live_5h_window_keeps_cold_and_past() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(
            config_with(vec![
                account("live", 0),
                account("cold", 0),
                account("past", 0),
            ]),
            refresher,
        );
        mark_all_probed(&manager); // isolate from the never-probed boot gate
        set_5h(&manager, 0, "0.10", 3); // live: low util, reset 3h out → already warm
                                        // account 1 (cold): no 5h data at all
        set_5h(&manager, 2, "0.10", -1); // past: reset 1h ago → window elapsed

        assert_eq!(
            manager.warm_targets(),
            vec![1, 2],
            "skip the live-window account; warm the cold and past-reset ones"
        );
    }

    /// A near/over-threshold account is skipped — warming an exhausted account is
    /// pointless spend. Uses the weekly window so the gate is isolated from the
    /// live-5h check.
    #[test]
    fn warm_targets_skips_near_threshold_account() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(
            config_with(vec![account("full", 0), account("idle", 0)]),
            refresher,
        );
        mark_all_probed(&manager); // isolate from the never-probed boot gate
        set_7d(&manager, 0, "0.95"); // over the 0.90 threshold

        assert_eq!(
            manager.warm_targets(),
            vec![1],
            "an at/over-threshold account is not a warm target"
        );
    }

    /// `warm_all` warms ONLY the idle targets: the idle account's token is warmed,
    /// while a live-window account, a disabled one, and an errored one are not.
    #[tokio::test]
    async fn warm_all_warms_only_idle_targets() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let warmed = Arc::new(Mutex::new(Vec::new()));
        let warmer = Arc::new(RecordingWarmer {
            warmed: warmed.clone(),
        });
        let manager = build_manager_with_warmer(
            config_with(vec![
                account("idle", 0),
                account("live", 0),
                account("off", 0),
                account("dead", 0),
            ]),
            refresher,
            warmer,
        );
        mark_all_probed(&manager); // isolate from the never-probed boot gate
        set_5h(&manager, 1, "0.10", 3); // live window → already warm
        manager.set_disabled(2, true);
        manager.mark_error(3);

        manager.warm_all().await;

        let warmed = warmed.lock().unwrap().clone();
        assert_eq!(
            warmed,
            vec!["at-idle".to_string()],
            "only the idle account's token is warmed"
        );
    }

    /// `warm_all` is a pure no-op — the warmer is never invoked — when there are
    /// zero eligible targets.
    #[tokio::test]
    async fn warm_all_is_noop_when_no_targets() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let warmed = Arc::new(Mutex::new(Vec::new()));
        let warmer = Arc::new(RecordingWarmer {
            warmed: warmed.clone(),
        });
        let manager =
            build_manager_with_warmer(config_with(vec![account("off", 0)]), refresher, warmer);
        manager.set_disabled(0, true);

        manager.warm_all().await;

        assert!(
            warmed.lock().unwrap().is_empty(),
            "the warmer must never be invoked when there are no targets"
        );
    }

    /// `warm_account` folds the warm response's rate-limit headers into the
    /// account's quota, so its just-started 5h window is immediately visible
    /// (populated `five_hour`, a live future reset) — which suppresses a re-warm.
    #[tokio::test]
    async fn warm_account_folds_response_headers_into_five_hour() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let warmer = Arc::new(RecordingWarmer {
            warmed: Arc::new(Mutex::new(Vec::new())),
        });
        let manager =
            build_manager_with_warmer(config_with(vec![account("cold", 0)]), refresher, warmer);
        mark_all_probed(&manager); // isolate from the never-probed boot gate
                                   // Before: no 5h window, so this account is a target.
        assert_eq!(manager.warm_targets(), vec![0]);

        manager.warm_account(0).await;

        let now = OffsetDateTime::now_utc();
        let snap = manager.snapshot(now);
        assert!(
            snap.accounts[0].five_hour.is_some(),
            "the warm response's 5h window was folded into the account"
        );
        assert!(
            snap.accounts[0].five_hour_reset.is_some(),
            "the folded 5h window has a live future reset"
        );
        // And the fold suppresses a re-warm on the next sweep.
        assert!(
            manager.warm_targets().is_empty(),
            "a freshly-warmed account is no longer a target"
        );
    }

    /// THE boot gate. At startup every account's quota is blank (`from_config`
    /// seeds `Quota::default()` and nothing restores it), so before the gate every
    /// probeable account looked cold and the warm loop's immediate first tick
    /// would have fired a real quota-spending request at all of them — including
    /// accounts whose 5h window is genuinely live. A never-probed account's blank
    /// quota is unknown, not known-cold, so with probing on there are no targets.
    #[test]
    fn warm_targets_is_empty_at_boot_while_probing_is_enabled() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0), account("c", 0)]),
            refresher,
        );
        // Boot state, untouched: probing on by default (75s), nothing probed yet.
        assert!(manager.probe_interval_seconds() > 0);

        assert!(
            manager.warm_targets().is_empty(),
            "an unread account's blank quota is unknown, not known-cold"
        );
    }

    /// **THE spec-error guard.** The boot gate's predicate must be "a probe actually
    /// READ this account's quota", never "a probe reported *something*":
    /// `probe_account` calls `apply_usage` only on `Ok`, while `record_probe` stamps
    /// `Error`/`Timeout`/`RateLimited` on failure — so after a failed first sweep
    /// `probe_status != Never` with the quota still blank, and a `probe_status`-keyed
    /// gate lifts on blank quota and hands the boot burst straight back. A FAILED
    /// probe must leave the account a NON-target.
    #[tokio::test]
    async fn warm_targets_stays_empty_after_a_failed_probe() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        // `ScriptedProber` errors (500) on every token but `ok_token`; no account
        // here carries that token, so the whole sweep fails — the fleet-wide false
        // error `probing.rs` documents.
        let manager = build_manager_with_prober(
            config_with(vec![account("a", 0), account("b", 0)]),
            refresher,
            Arc::new(ScriptedProber {
                ok_token: "at-nobody".to_string(),
            }),
        );
        assert!(manager.probe_interval_seconds() > 0);

        manager.probe_all().await;

        {
            let accounts = manager.accounts.read().unwrap();
            // The probe SPOKE — this is what makes `probe_status` the wrong gate.
            assert!(
                accounts
                    .iter()
                    .all(|a| a.probe_status != ProbeStatus::Never),
                "the failed sweep must still stamp a terminal probe status on every row"
            );
            // …but it read nothing.
            assert!(
                accounts.iter().all(|a| !a.quota_known),
                "a FAILED probe must never latch quota_known"
            );
        }
        assert!(
            manager.warm_targets().is_empty(),
            "a failed probe leaves the quota unread, so nothing may be a warm target"
        );
    }

    /// FIX 2's guard, and the other half of FIX 1. A SUCCESSFUL probe latches the
    /// gate open and must WAKE the warm loop: its ticker fired its only immediate
    /// tick before any quota existed and `MissedTickBehavior::Skip` puts the next one
    /// a full `warmupSeconds` away, so without the wake a proxy restarted more often
    /// than its warm interval warms nothing, ever.
    ///
    /// The prober reports a weekly bucket and NO 5h bucket on purpose: it proves the
    /// latch is about the read, not about `five_hour.is_some()` — the account is a
    /// target with `five_hour` still `None`.
    #[tokio::test]
    async fn a_successful_probe_opens_the_gate_and_wakes_the_warm_loop() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager_with_prober(
            config_with(vec![account("a", 0)]),
            refresher,
            Arc::new(ColdOkProber),
        );
        let wake_window = std::time::Duration::from_millis(50);
        assert!(manager.warm_targets().is_empty(), "boot: nothing read yet");
        assert!(
            tokio::time::timeout(wake_window, manager.warm_wake().notified())
                .await
                .is_err(),
            "nothing may wake the warm loop before any quota has been read"
        );

        manager.probe_all().await;

        assert!(
            manager.accounts.read().unwrap()[0]
                .quota
                .five_hour
                .is_none(),
            "the fixture must leave the 5h window absent, or it proves nothing about the predicate"
        );
        assert_eq!(
            manager.warm_targets(),
            vec![0],
            "a read quota opens the gate even with no 5h bucket in the response"
        );
        tokio::time::timeout(wake_window, manager.warm_wake().notified())
            .await
            .expect("the first successful probe must wake the warm loop, not leave it a full interval away");
    }

    /// The wake is edge-triggered on the false→true flip, so a steady-state probe
    /// cadence cannot spin the warm loop: the SECOND successful sweep over the same
    /// accounts signals nothing.
    #[tokio::test]
    async fn a_repeat_probe_does_not_re_wake_the_warm_loop() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager_with_prober(
            config_with(vec![account("a", 0)]),
            refresher,
            Arc::new(ColdOkProber),
        );
        let wake_window = std::time::Duration::from_millis(50);

        manager.probe_all().await;
        // Consume the one wake the first read is entitled to.
        tokio::time::timeout(wake_window, manager.warm_wake().notified())
            .await
            .expect("first read wakes the loop");

        manager.probe_all().await;

        assert!(
            tokio::time::timeout(wake_window, manager.warm_wake().notified())
                .await
                .is_err(),
            "quota_known is already latched, so a repeat probe must not re-wake the loop"
        );
    }

    /// Once the probe has actually spoken, a genuinely cold or expired window IS
    /// warmed again — the gate defers the first sweep, it does not disable
    /// keep-warm. A probed account with a LIVE window stays skipped.
    #[test]
    fn warm_targets_resumes_once_the_probe_has_reported() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(
            config_with(vec![
                account("cold", 0),
                account("live", 0),
                account("dark", 0),
            ]),
            refresher,
        );
        // Only the first two have had their quota read; the third never has.
        {
            let mut accounts = manager.accounts.write().unwrap();
            accounts[0].probe_status = ProbeStatus::Ok;
            accounts[0].quota_known = true;
            accounts[1].probe_status = ProbeStatus::Ok;
            accounts[1].quota_known = true;
        }
        set_5h(&manager, 0, "0.10", -1); // probed, window expired → genuinely cold
        set_5h(&manager, 1, "0.10", 3); // probed, window live → already warm

        assert_eq!(
            manager.warm_targets(),
            vec![0],
            "a probed account with an expired window is a target; a live one and an unprobed one are not"
        );
    }

    /// The dark-feature guard. With `quotaProbeSeconds: 0` no probe task is ever
    /// spawned, so `quota_known` stays `false` forever. Gating on it
    /// unconditionally would make keep-warm structurally unable to fire while
    /// still reading as enabled — so with probing off, the gate does not apply.
    #[test]
    fn warm_targets_ignores_the_boot_gate_when_probing_is_disabled() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(
            config_with_probe_seconds(vec![account("a", 0), account("b", 0)], 0),
            refresher,
        );
        assert_eq!(manager.probe_interval_seconds(), 0);
        // Nothing has been (or ever will be) probed, so no quota is known.
        assert!(manager
            .accounts
            .read()
            .unwrap()
            .iter()
            .all(|a| !a.quota_known));

        assert_eq!(
            manager.warm_targets(),
            vec![0, 1],
            "with the probe off there is nothing to wait for; keep-warm must not go dark"
        );
    }

    /// Multi-thread stress: RACE `select()` / `enter_in_flight()` / guard-drop
    /// against the other account-lock writers (`mark_rate_limited` /
    /// `record_served`) across 8 threads on one shared `Arc<Manager>` with pacing
    /// ON. This is the race surface ThreadSanitizer watches in CI (the `tsan` job
    /// filters on this test name); TSan is unrunnable on arm64-macOS, so under
    /// normal `cargo test` this proves two things TSan does not: the harness runs
    /// without panicking, and the shared `in_flight` counter is *balanced* — every
    /// account returns to `in_flight == 0` once all guards drop. A leak, a
    /// double-decrement, or a lost increment across the check-then-act-across-locks
    /// path shows up here as a nonzero residual even without instrumentation.
    #[test]
    fn concurrent_pacing_stress() {
        const THREADS: usize = 8;
        const ITERS: usize = 2000;

        let pacing = PacingConfig {
            max_in_flight_per_account: Some(2),
            min_spacing_ms: None,
        };
        let manager = build_manager(
            config_with_pacing(
                vec![
                    account("a0", 0),
                    account("a1", 0),
                    account("a2", 0),
                    account("a3", 0),
                ],
                pacing,
            ),
            Arc::new(CountingRefresher {
                calls: Arc::new(AtomicUsize::new(0)),
            }),
        );

        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let manager = Arc::clone(&manager);
                std::thread::spawn(move || {
                    let empty: HashSet<usize> = HashSet::new();
                    for i in 0..ITERS {
                        let now = OffsetDateTime::now_utc();
                        // Vary the session key per thread AND per iteration so
                        // affinity pins spread across accounts and the pin map
                        // churns — maximising concurrent affinity-lock traffic.
                        let session_key = Some((t as u64) << 32 | (i as u64 % 16));
                        if let Some(idx) = manager.select(&empty, now, None, session_key) {
                            // Take the in-flight slot, hold it briefly to widen the
                            // window where a concurrent select/writer observes a
                            // nonzero count, then drop the guard (decrement).
                            let guard = manager.enter_in_flight(idx);
                            for _ in 0..8 {
                                std::hint::spin_loop();
                            }
                            // Occasionally fire the other account-lock writers so
                            // TSan sees select/enter racing real mutation, not just
                            // the in_flight path.
                            if i % 17 == 0 {
                                manager.record_served(idx, now, session_key, SessionKind::Fallback);
                            }
                            if i % 53 == 0 {
                                // Short hold so accounts recover and stay selectable
                                // for the rest of the run.
                                manager.mark_rate_limited(idx, 1);
                            }
                            drop(guard);
                        }
                    }
                })
            })
            .collect();

        for h in handles {
            // A panic in any thread (e.g. a poisoned lock from an unwind under the
            // write-lock) propagates here and fails the test.
            h.join().expect("stress thread panicked");
        }

        // Every guard has dropped, so the counter MUST be balanced back to zero on
        // every account. A nonzero residual is a real concurrency bug (leaked or
        // double-counted in_flight) — surface it, never silence it.
        let accounts = manager.accounts.read().expect("accounts lock poisoned");
        for (idx, account) in accounts.iter().enumerate() {
            assert_eq!(
                account.in_flight, 0,
                "account {idx} ({}) leaked in_flight={} after all guards dropped",
                account.name, account.in_flight
            );
        }
    }

    /// TOCTOU regression (concurrency) — the migration feature's OWN target workload,
    /// with `loadBalanceMigration` explicitly ENABLED (it ships OFF).
    /// Several sessions start STACKED on one account with idle, eligible accounts
    /// alongside. `N` threads select for those sessions in lockstep: a `Barrier`
    /// forces every round to fire simultaneously, which is the exact interleaving the
    /// adversarial review flagged — two concurrent selects both reading `count(Y)=0`
    /// for the same idle `Y` and BOTH migrating onto it, over-stacking `Y` to 2 while
    /// `X` empties to 0 (the inverse of the goal) and oscillating 0→1→0.
    ///
    /// The fix — re-validating `count(target)+1 < count(X)` against FRESH counts under
    /// the affinity lock in section 3, aborting the move if it no longer holds — makes
    /// each committed migration strictly reduce the load's sum-of-squares, so the
    /// system converges to exactly one session per account and STAYS there. Under the
    /// pre-fix code the stale-count commit over-migrates and the final distribution is
    /// NOT balanced. (Asserted on a balanced fleet — no pacing / rate-limit churn — so
    /// the only force acting on the distribution is the migration logic under test.)
    #[test]
    fn concurrent_stacked_sessions_do_not_over_migrate() {
        const N: usize = 4; // sessions == accounts → balanced fixed point is one-each
        const ROUNDS: usize = 100;

        let manager = build_manager(
            config_with_migration(vec![
                account("a0", 0),
                account("a1", 0),
                account("a2", 0),
                account("a3", 0),
            ]),
            Arc::new(CountingRefresher {
                calls: Arc::new(AtomicUsize::new(0)),
            }),
        );
        // Pathological start: ALL N sessions pinned to account 0.
        for key in 0..N as u64 {
            pin_session(&manager, key, 0);
        }

        let barrier = Arc::new(std::sync::Barrier::new(N));
        let handles: Vec<_> = (0..N as u64)
            .map(|key| {
                let manager = Arc::clone(&manager);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let empty: HashSet<usize> = HashSet::new();
                    for _ in 0..ROUNDS {
                        // Synchronise so all N selects contend on the same stacked
                        // account in the same instant — maximising the TOCTOU window.
                        barrier.wait();
                        let now = OffsetDateTime::now_utc();
                        manager.select(&empty, now, None, Some(key));
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("migration stress thread panicked");
        }

        // Every session is still pinned (none lost), and the distribution converged
        // to exactly one-per-account. An account holding >=2 while another sits at 0
        // is the over-migration / oscillation the re-validation exists to prevent.
        let pins = manager.affinity.lock().expect("affinity lock poisoned");
        assert_eq!(pins.len(), N, "every session must remain pinned");
        let mut per_account = [0usize; N];
        for &(idx, _) in pins.values() {
            per_account[idx] += 1;
        }
        assert_eq!(
            per_account,
            [1, 1, 1, 1],
            "stacked sessions must converge to one-per-account without over-migrating \
             (got {per_account:?}) — a stale-count commit would over-stack one account"
        );
    }

    // ---- over-threshold revalidation-serve (last-resort; default ON) ----------

    /// Drive account `idx` OVER the soft switch threshold on its shared weekly
    /// (`7d`) window (future reset) and stamp `unifiedStatus`, so normal `select`
    /// benches it but `select_revalidation` can still consider it. `util` is the
    /// weekly utilization; `status` the reported unified status.
    fn set_over_threshold(manager: &Manager, idx: usize, util: f64, status: &str) {
        let now = OffsetDateTime::now_utc();
        let mut a = manager.accounts.write().expect("accounts lock poisoned");
        let account = &mut a[idx];
        account.quota.seven_day = Some(crate::quota::QuotaWindow {
            utilization: util,
            reset: Some(now + Duration::hours(48)),
        });
        account.quota.status = Some(status.to_string());
    }

    /// #1 Whole fleet over the soft threshold, all `allowed_warning`: normal
    /// `select` is `None` but `select_revalidation` serves the LOWEST-utilization
    /// account.
    #[test]
    fn revalidation_serves_least_utilized_when_all_over_threshold() {
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0), account("c", 0)]),
            pacing_refresher(),
        );
        set_over_threshold(&manager, 0, 0.995, "allowed_warning");
        set_over_threshold(&manager, 1, 0.970, "allowed_warning"); // least utilized
        set_over_threshold(&manager, 2, 0.999, "allowed_warning");

        let now = OffsetDateTime::now_utc();
        assert_eq!(
            manager.select(&HashSet::new(), now, None, None),
            None,
            "every account is over the soft threshold → normal select benches all"
        );
        assert_eq!(
            manager.select_revalidation(&HashSet::new(), now, None, None),
            Some(1),
            "revalidation serves the least-utilized allowed account"
        );
    }

    /// #1b THE cache-warmth test: a session pinned (affinity) to an over-threshold
    /// but `allowed_warning` account X — with OTHER accounts LESS utilized — must be
    /// served X (its warm pin), NOT the least-utilized other, and must NOT consume
    /// the fallback anti-storm window. If X becomes `rejected`, it falls back to the
    /// least-utilized survivor and RE-PINS the session there.
    #[test]
    fn revalidation_honors_pin_through_threshold() {
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0), account("pinned", 0)]),
            pacing_refresher(),
        );
        // The pinned account (idx 2) is the MOST utilized; the others are cooler.
        set_over_threshold(&manager, 0, 0.950, "allowed_warning"); // least utilized
        set_over_threshold(&manager, 1, 0.970, "allowed_warning");
        set_over_threshold(&manager, 2, 0.999, "allowed_warning"); // the warm pin
        let key = 424_242u64;
        let now = OffsetDateTime::now_utc();
        {
            let mut pins = manager.affinity.lock().expect("affinity lock poisoned");
            pins.insert(key, (2, odt_to_ms(now)));
        }

        assert_eq!(
            manager.select_revalidation(&HashSet::new(), now, None, Some(key)),
            Some(2),
            "pin-honor serves the session's warm pinned account, NOT the least-utilized other"
        );
        // The pin-honor path must NOT have burned the fallback throttle window: an
        // unaffiliated (no-pin) fallback serve still succeeds immediately.
        assert_eq!(
            manager.select_revalidation(&HashSet::new(), now, None, None),
            Some(0),
            "pin-honor does not consume the fallback anti-storm window"
        );

        // When the pin becomes HARD-blocked (rejected), fall back to least-utilized
        // and RE-PIN the session there. Use a fresh manager (throttle valve reset).
        let manager2 = build_manager(
            config_with(vec![account("a", 0), account("b", 0), account("pinned", 0)]),
            pacing_refresher(),
        );
        set_over_threshold(&manager2, 0, 0.950, "allowed_warning"); // least utilized
        set_over_threshold(&manager2, 1, 0.970, "allowed_warning");
        set_over_threshold(&manager2, 2, 0.999, "rejected"); // pin now blocked
        {
            let mut pins = manager2.affinity.lock().expect("affinity lock poisoned");
            pins.insert(key, (2, odt_to_ms(now)));
        }
        assert_eq!(
            manager2.select_revalidation(&HashSet::new(), now, None, Some(key)),
            Some(0),
            "a rejected pin falls back to the least-utilized survivor"
        );
        let repinned = manager2
            .affinity
            .lock()
            .expect("affinity lock poisoned")
            .get(&key)
            .map(|&(idx, _)| idx);
        assert_eq!(
            repinned,
            Some(0),
            "the fallback serve re-pins the session to the account it chose"
        );
    }

    /// #1c `tried` governs whether an account may SERVE this request; it must never
    /// decide whether that session's PIN may MOVE. A pin that merely failed THIS
    /// request — a transport blip, a 5xx — while still clearing every ACCOUNT-level
    /// HARD gate stays put: the fallback serves elsewhere for this one request and
    /// the session comes home on the next.
    ///
    /// Before the fix the `account_hard_ok → keep_pin` test sat INSIDE
    /// `if !tried.contains(&idx)`, so a tried pin left `keep_pin` at `None` and the
    /// fallback's `pins.insert` durably re-keyed the session — a warm prompt cache
    /// discarded on the strength of one failed request. `select()` takes the opposite
    /// decision on the identical fact and documents why; this is the two agreeing.
    ///
    /// Also the first coverage of a NON-EMPTY `tried` on this path at all — every
    /// other `select_revalidation` test passes `&HashSet::new()`.
    #[test]
    fn revalidation_keeps_the_pin_when_it_is_merely_tried() {
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0), account("pinned", 0)]),
            pacing_refresher(),
        );
        set_over_threshold(&manager, 0, 0.950, "allowed_warning"); // least utilized
        set_over_threshold(&manager, 1, 0.970, "allowed_warning");
        set_over_threshold(&manager, 2, 0.999, "allowed_warning"); // the warm pin
        let key = 515_151u64;
        let now = OffsetDateTime::now_utc();
        {
            let mut pins = manager.affinity.lock().expect("affinity lock poisoned");
            pins.insert(key, (2, odt_to_ms(now)));
        }

        // The pin failed THIS request and nothing more: not disabled, not errored,
        // not rejected, no hold. It clears every ACCOUNT-level hard gate.
        let tried = HashSet::from([2usize]);
        assert_eq!(
            manager.select_revalidation(&tried, now, None, Some(key)),
            Some(0),
            "a tried pin cannot serve THIS request → the least-utilized survivor does"
        );
        let pin_after = manager
            .affinity
            .lock()
            .expect("affinity lock poisoned")
            .get(&key)
            .map(|&(idx, _)| idx);
        assert_eq!(
            pin_after,
            Some(2),
            "the pin must NOT move: membership in `tried` is a per-request fact, not \
             evidence the account is gone"
        );
    }

    /// #1d The over-correction guard for #1c. Hoisting the `keep_pin` test out of the
    /// `!tried` guard must not make a pin STICKY: a pin that fails an ACCOUNT-level
    /// HARD gate still re-keys, and being in `tried` as well changes nothing. Both
    /// hard shapes are exercised — `quota.status == "rejected"`, and a hold that
    /// outlives the prompt cache (>= `CACHE_WARM_HOLD_SECS`).
    #[test]
    fn revalidation_rekeys_when_the_pin_fails_a_hard_gate() {
        let key = 626_262u64;
        let now = OffsetDateTime::now_utc();
        let tried = HashSet::from([2usize]);
        let pin_session = |manager: &Manager, idx: usize| {
            let mut pins = manager.affinity.lock().expect("affinity lock poisoned");
            pins.insert(key, (idx, odt_to_ms(now)));
        };
        let pin_of = |manager: &Manager| {
            manager
                .affinity
                .lock()
                .expect("affinity lock poisoned")
                .get(&key)
                .map(|&(idx, _)| idx)
        };

        // (a) REJECTED and tried — a durable block, so the session must move off it.
        let rejected = build_manager(
            config_with(vec![account("a", 0), account("b", 0), account("pinned", 0)]),
            pacing_refresher(),
        );
        set_over_threshold(&rejected, 0, 0.950, "allowed_warning"); // least utilized
        set_over_threshold(&rejected, 1, 0.970, "allowed_warning");
        set_over_threshold(&rejected, 2, 0.999, "rejected");
        pin_session(&rejected, 2);
        assert_eq!(
            rejected.select_revalidation(&tried, now, None, Some(key)),
            Some(0),
            "a rejected pin is skipped; the least-utilized survivor serves"
        );
        assert_eq!(
            pin_of(&rejected),
            Some(0),
            "a rejected pin RE-KEYS — `tried` must not shield it from the hard gate"
        );

        // (b) HELD PAST THE CACHE and tried — also durable (a fresh manager, so the
        // anti-storm valve the serve above armed does not swallow this call).
        let held = build_manager(
            config_with(vec![account("a", 0), account("b", 0), account("pinned", 0)]),
            pacing_refresher(),
        );
        set_over_threshold(&held, 0, 0.950, "allowed_warning"); // least utilized
        set_over_threshold(&held, 1, 0.970, "allowed_warning");
        set_over_threshold(&held, 2, 0.999, "allowed_warning");
        {
            let mut a = held.accounts.write().expect("accounts lock poisoned");
            a[2].rate_limited_until_ms = Some(crate::now_ms() + 3_600_000);
        }
        pin_session(&held, 2);
        assert_eq!(
            held.select_revalidation(&tried, now, None, Some(key)),
            Some(0),
            "a pin held past its cache lifetime is skipped; the survivor serves"
        );
        assert_eq!(
            pin_of(&held),
            Some(0),
            "a hold that outlives the cache RE-KEYS — `tried` must not shield it"
        );
    }

    /// #2 A `rejected` account is skipped even when least-utilized; a higher-util
    /// `allowed` one is served. If ALL are `rejected` → `None`.
    #[test]
    fn revalidation_skips_rejected_accounts() {
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0)]),
            pacing_refresher(),
        );
        set_over_threshold(&manager, 0, 0.960, "rejected"); // least util but blocked
        set_over_threshold(&manager, 1, 0.990, "allowed_warning");

        let now = OffsetDateTime::now_utc();
        assert_eq!(
            manager.select_revalidation(&HashSet::new(), now, None, None),
            Some(1),
            "the least-utilized account is rejected → skip it, serve the allowed one"
        );

        // Now reject BOTH → nothing servable → None.
        set_over_threshold(&manager, 1, 0.990, "rejected");
        // A fresh manager to reset the anti-storm valve (previous serve armed it).
        let manager2 = build_manager(
            config_with(vec![account("a", 0), account("b", 0)]),
            pacing_refresher(),
        );
        set_over_threshold(&manager2, 0, 0.960, "rejected");
        set_over_threshold(&manager2, 1, 0.990, "rejected");
        assert_eq!(
            manager2.select_revalidation(&HashSet::new(), now, None, None),
            None,
            "all accounts rejected → no revalidation target, honest 429"
        );
    }

    /// #3 An account under a live rate-limit hold is skipped even if least-utilized.
    #[test]
    fn revalidation_skips_hard_held_account() {
        let manager = build_manager(
            config_with(vec![account("held", 0), account("free", 0)]),
            pacing_refresher(),
        );
        set_over_threshold(&manager, 0, 0.950, "allowed_warning"); // least util …
        set_over_threshold(&manager, 1, 0.990, "allowed_warning");
        {
            let mut a = manager.accounts.write().expect("accounts lock poisoned");
            a[0].rate_limited_until_ms = Some(crate::now_ms() + 60_000); // … but held.
        }
        let now = OffsetDateTime::now_utc();
        assert_eq!(
            manager.select_revalidation(&HashSet::new(), now, None, None),
            Some(1),
            "a live-held account is a HARD block — skipped even as least-utilized"
        );
    }

    /// #4 Anti-storm: two back-to-back calls — the first serves, the second (inside
    /// `REVALIDATION_MIN_SPACING_MS`) returns `None`.
    #[test]
    fn revalidation_throttle_spacing_respected() {
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0)]),
            pacing_refresher(),
        );
        set_over_threshold(&manager, 0, 0.970, "allowed_warning");
        set_over_threshold(&manager, 1, 0.990, "allowed_warning");
        let now = OffsetDateTime::now_utc();
        assert_eq!(
            manager.select_revalidation(&HashSet::new(), now, None, None),
            Some(0),
            "first call serves"
        );
        assert_eq!(
            manager.select_revalidation(&HashSet::new(), now, None, None),
            None,
            "a second call within the spacing window is throttled to None"
        );
    }

    /// #5 When one account is UNDER threshold, normal `select` returns it and the
    /// revalidation path is never consulted — the normal path stays unchanged.
    #[test]
    fn revalidation_not_consulted_when_an_account_is_eligible() {
        let manager = build_manager(
            config_with(vec![account("hot", 0), account("cool", 0)]),
            pacing_refresher(),
        );
        set_over_threshold(&manager, 0, 0.999, "allowed_warning");
        // account 1 stays healthy (no quota set) → under threshold → eligible.
        let now = OffsetDateTime::now_utc();
        assert_eq!(
            manager.select(&HashSet::new(), now, None, None),
            Some(1),
            "an under-threshold account is served by the normal path; revalidation \
             is only consulted after select() returns None"
        );
    }

    /// #6 A Fable request skips an account whose Fable weekly (`7d_oi`) is a hard
    /// reject, but a non-Fable request serves that same account.
    #[test]
    fn revalidation_fable_skips_fable_exhausted() {
        let build = || {
            let manager = build_manager(
                config_with(vec![account("fable-full", 0), account("other", 0)]),
                pacing_refresher(),
            );
            let now = OffsetDateTime::now_utc();
            // account 0: least-utilized on shared dims but Fable weekly exhausted.
            set_over_threshold(&manager, 0, 0.950, "allowed_warning");
            set_over_threshold(&manager, 1, 0.990, "allowed_warning");
            set_fable_exhausted(&manager, 0, 0.999);
            (manager, now)
        };

        // Fable request: account 0's Fable weekly is a hard reject → skip → serve 1.
        let (manager, now) = build();
        assert_eq!(
            manager.select_revalidation(&HashSet::new(), now, Some("claude-fable-5"), None),
            Some(1),
            "a Fable request skips the Fable-exhausted account even when least-utilized"
        );

        // Non-Fable request on a fresh manager: account 0 IS served (Fable weekly
        // gates Fable traffic only; on shared dims account 0 is least-utilized).
        let (manager, now) = build();
        assert_eq!(
            manager.select_revalidation(&HashSet::new(), now, Some("claude-opus-4-6"), None),
            Some(0),
            "a non-Fable request still serves the Fable-exhausted account"
        );
    }
}
