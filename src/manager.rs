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
use tokio::sync::Mutex as AsyncMutex;

use crate::config::{self, Config, PacingConfig};
use crate::oauth::{self, LiveRefresher, TokenRefresher, Tokens};
use crate::probe::{LiveUsageProber, ProbeStatus, Usage, UsageProber};
use crate::quota::Quota;
use crate::stats::{AccountSnapshot, RequestLogEntry, SessionKind, SessionSnapshot, StatsSnapshot};
use crate::warmer::{AccountWarmer, LiveWarmer};

const REQUEST_LOG_CAPACITY: usize = 200;

/// Upper bound on a single rate-limit hold. A 429 `retry-after` larger than this
/// is clamped so an account is always revalidated within the window rather than
/// pinned out for hours with no live request to clear the hold (finding #5).
const MAX_RATE_LIMIT_HOLD_SECONDS: i64 = 3600;

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
    pub input_tokens: u64,
    pub output_tokens: u64,
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
            input_tokens: 0,
            output_tokens: 0,
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
    /// (x-api-key / `metadata.user_id`); [`SessionKind::Fallback`] when it fell back to
    /// the per-connection key. DISPLAY provenance only — the routing pin is unaffected.
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
        let refresh_locks = accounts
            .iter()
            .map(|_| Arc::new(AsyncMutex::new(())))
            .collect();
        let upstream = config.upstream.clone();
        let proxy_api_key = config.proxy.api_key.clone();
        let global_threshold = config.switch_threshold;
        let pacing = config.pacing.clone();

        Arc::new(Self {
            accounts: RwLock::new(accounts),
            refresh_locks,
            refresher,
            prober,
            warmer,
            warm_in_flight: AtomicBool::new(false),
            // no_proxy(): reqwest honors HTTPS_PROXY/HTTP_PROXY by default. We ARE the
            // proxy — routing our upstream through an ambient proxy (e.g. the JS
            // teamclaude on :3456) loops us through the thing we replace and every
            // request dies as "upstream unreachable". Always reach Anthropic directly.
            http: reqwest::Client::builder()
                .no_proxy()
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
            log: Mutex::new(VecDeque::with_capacity(REQUEST_LOG_CAPACITY)),
            current: Mutex::new(None),
            select_seq: AtomicU64::new(1),
            affinity: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            session_seq: AtomicU64::new(1),
        })
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

    /// Configured probe cadence in seconds, read from the config's unmodelled
    /// `quotaProbeSeconds` (default [`crate::probe::DEFAULT_PROBE_SECONDS`]). A
    /// value `<= 0` disables probing.
    pub fn probe_interval_seconds(&self) -> u64 {
        self.config
            .lock()
            .expect("config lock poisoned")
            .extra
            .get("quotaProbeSeconds")
            .and_then(|v| v.as_i64())
            .map(|v| v.max(0) as u64)
            .unwrap_or(crate::probe::DEFAULT_PROBE_SECONDS)
    }

    /// Configured keep-warm cadence in seconds, read from the config's unmodelled
    /// `warmupSeconds`. **Default 0 = OFF** (unlike the probe's 75) — keep-warm
    /// spends real quota, so it ships dark and is only ever running when explicitly
    /// enabled. A value `<= 0` disables it (no warm task is spawned).
    pub fn warmup_interval_seconds(&self) -> u64 {
        self.config
            .lock()
            .expect("config lock poisoned")
            .extra
            .get("warmupSeconds")
            .and_then(|v| v.as_i64())
            .map(|v| v.max(0) as u64)
            .unwrap_or(0)
    }

    /// Whether session affinity is enabled, read from the config's unmodelled
    /// top-level `sessionAffinity` (default `false` — off unless explicitly
    /// enabled). Same read pattern as [`Self::probe_interval_seconds`]. When
    /// `false`, the hybrid server injects no `SessionKey` extension, so `select`
    /// always receives `affinity = None` and the disabled path is inert.
    pub fn session_affinity_enabled(&self) -> bool {
        self.config
            .lock()
            .expect("config lock poisoned")
            .extra
            .get("sessionAffinity")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    /// Mint the next session key: a strictly-increasing, unique `u64` starting at
    /// 1. Called once per connection by the hybrid server when affinity is on.
    pub fn next_session_key(&self) -> u64 {
        self.session_seq.fetch_add(1, Ordering::Relaxed)
    }

    pub fn account_count(&self) -> usize {
        self.accounts.read().expect("accounts lock poisoned").len()
    }

    pub fn upstream(&self) -> &str {
        &self.upstream
    }

    pub fn proxy_api_key(&self) -> Option<&str> {
        self.proxy_api_key.as_deref()
    }

    pub fn http_client(&self) -> reqwest::Client {
        self.http.clone()
    }

    /// The access token to inject for account `idx` (a clone — the request
    /// outlives the lock).
    pub fn access_token(&self, idx: usize) -> Option<String> {
        self.accounts
            .read()
            .expect("accounts lock poisoned")
            .get(idx)
            .map(|a| a.access_token.clone())
    }

    /// Display name of account `idx`, for the request log.
    pub fn account_name(&self, idx: usize) -> Option<String> {
        self.accounts
            .read()
            .expect("accounts lock poisoned")
            .get(idx)
            .map(|a| a.name.clone())
    }

    /// The pooled `account_uuid` to inject for account `idx` (a clone — the
    /// request outlives the lock). `None` when the account has no configured
    /// UUID, in which case the proxy leaves the body unchanged.
    pub fn account_uuid(&self, idx: usize) -> Option<String> {
        self.accounts
            .read()
            .expect("accounts lock poisoned")
            .get(idx)
            .and_then(|a| a.account_uuid.clone())
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
    /// exhausted: the soonest future hold/reset across all accounts, clamped to
    /// at least 1s, defaulting to 60s when nothing is known.
    pub fn retry_after_hint(&self, now: OffsetDateTime) -> i64 {
        let now_ms = odt_to_ms(now);
        let accounts = self.accounts.read().expect("accounts lock poisoned");
        let mut soonest = i64::MAX;
        for account in accounts.iter() {
            let candidates = [
                account.rate_limited_until_ms,
                account
                    .quota
                    .five_hour
                    .and_then(|w| w.live_reset(now))
                    .map(odt_to_ms),
                account
                    .quota
                    .seven_day
                    .and_then(|w| w.live_reset(now))
                    .map(odt_to_ms),
                // Fable requests gate on the model-scoped weekly (`7d_oi`) bucket, so
                // an all-Fable-exhausted fleet has its real reset here — without it the
                // hint falls through to the 60s default while the true reset is days out.
                account
                    .quota
                    .seven_day_oi
                    .and_then(|w| w.live_reset(now))
                    .map(odt_to_ms),
            ];
            for candidate in candidates.into_iter().flatten() {
                if candidate > now_ms {
                    soonest = soonest.min(candidate);
                }
            }
        }
        if soonest == i64::MAX {
            60
        } else {
            ((soonest - now_ms + 999) / 1000).max(1)
        }
    }

    /// Pick the best eligible account not in `tried`, spreading load across the
    /// fleet, or `None` if all are exhausted/held/disabled.
    ///
    /// Within a priority tier we pick the **least-recently-selected** account
    /// (lowest `last_selected_seq`; a never-selected account sorts first) so
    /// consecutive requests fan out instead of hammering one account. Ordering by
    /// quota headroom was rejected deliberately: a single request barely moves a
    /// weekly bar, so "most headroom first" would deterministically pin one
    /// account until its bar caught up — the exact overload this fixes. The
    /// winner is stamped with the next monotonic tick *before returning*, so even
    /// a burst of concurrent selects rotates (each sees the previous stamp). The
    /// soonest weekly reset is the final cold-start tiebreak (all-unseen startup).
    ///
    /// This mutates rotation state (the stamp), so it takes the write lock.
    ///
    /// `model` is the request's target model (if known). When it names a Fable
    /// model, an account whose model-scoped weekly (`7d_oi`) bucket is exhausted
    /// is skipped — while that same account still serves every non-Fable model.
    ///
    /// `affinity` is the caller's session key (opt-in; `None` when the feature is
    /// off). With `None` this is byte-for-byte the pre-affinity behaviour. With
    /// `Some(key)`: if the session is already pinned to an account that is not in
    /// `tried` and still passes [`Self::eligible`], that pinned account is
    /// returned — still stamped with a fresh select tick so *other* sessions' LRU
    /// steers away from a busy pinned account. Otherwise a normal LRU/priority
    /// pick runs and its winner is recorded as the session's pin (the initial pin
    /// or a re-pin when the old pin became ineligible / was already tried).
    /// Affinity never overrides priority — the pin is always a normal pick.
    pub fn select(
        &self,
        tried: &HashSet<usize>,
        now: OffsetDateTime,
        model: Option<&str>,
        affinity: Option<u64>,
    ) -> Option<usize> {
        let now_ms = odt_to_ms(now);
        // Compute the Fable classification ONCE, not per-account.
        let is_fable = model.is_some_and(crate::model::is_fable_model);

        // Affinity fast-path: honour an existing pin when it is still usable. Read
        // the pin under the affinity lock, then DROP that lock before taking the
        // accounts lock (never nest the two — that is the documented deadlock).
        if let Some(key) = affinity {
            // (1) UNDER THE AFFINITY LOCK ONLY: read this session's pin `X` and, in
            // the same critical section, tally the per-account pinned-session counts
            // that the load-balancing decision needs. Drop this lock before taking
            // the accounts lock — the two are NEVER held simultaneously (the
            // documented deadlock).
            let (pinned, counts) = {
                let pins = self.affinity.lock().expect("affinity lock poisoned");
                let pinned = pins.get(&key).map(|&(idx, _)| idx);
                let mut counts: HashMap<usize, usize> = HashMap::new();
                for &(idx, _) in pins.values() {
                    *counts.entry(idx).or_insert(0) += 1;
                }
                (pinned, counts)
            };
            if let Some(idx) = pinned {
                if !tried.contains(&idx) {
                    let count_x = counts.get(&idx).copied().unwrap_or(0);
                    // (2) UNDER THE ACCOUNTS LOCK ONLY: confirm the pin `X` is still
                    // usable, then — and only when >=2 sessions stack on `X` — look
                    // for the least-loaded ELIGIBLE account `Y` that strictly improves
                    // balance (`count(Y)+1 < count(X)`). Stamp whichever we settle on.
                    // `None` means `X` is ineligible → fall through to the normal
                    // pick/re-pin path (which already handles a dead pin).
                    let decision: Option<(usize, Option<(String, String)>)> = {
                        let mut accounts = self.accounts.write().expect("accounts lock poisoned");
                        // Respect pacing on the pin: a saturated pinned account
                        // temporarily YIELDS here and the LRU pick below (with its
                        // soft fallback) steers to a cooler account, so a session's
                        // concurrent burst spreads instead of all slamming one pin.
                        let x_usable = accounts.get(idx).is_some_and(|a| {
                            Self::eligible(
                                a,
                                self.global_threshold,
                                &self.pacing,
                                true,
                                now,
                                now_ms,
                                is_fable,
                            )
                        });
                        if !x_usable {
                            None
                        } else {
                            // Default: honour the pin. When `count_x < 2` (a LONE
                            // session) `target` stays `idx` and nothing below runs, so
                            // this path is byte-identical to the pre-migration
                            // behaviour — a lone session's warm cache is never moved.
                            let mut target = idx;
                            let mut migrate_names: Option<(String, String)> = None;
                            if count_x >= 2 {
                                // Least-loaded eligible target, ordered by
                                // (pinned-session-count asc, in_flight asc,
                                // last_selected_seq asc / LRU). A candidate qualifies
                                // ONLY if it strictly improves balance — this guard
                                // prevents thrash and equal-swaps.
                                let mut best: Option<usize> = None;
                                let mut best_key: Option<(usize, u32, u64)> = None;
                                for (cand, account) in accounts.iter().enumerate() {
                                    if cand == idx {
                                        continue;
                                    }
                                    let count_y = counts.get(&cand).copied().unwrap_or(0);
                                    if count_y + 1 >= count_x {
                                        continue;
                                    }
                                    // Only migrate onto an ELIGIBLE account — the same
                                    // gate the pin honours (disabled/error/rate-limit/
                                    // quota/pacing), so a throttled `Y` is never chosen.
                                    if !Self::eligible(
                                        account,
                                        self.global_threshold,
                                        &self.pacing,
                                        true,
                                        now,
                                        now_ms,
                                        is_fable,
                                    ) {
                                        continue;
                                    }
                                    let cand_key =
                                        (count_y, account.in_flight, account.last_selected_seq);
                                    if best_key.is_none_or(|b| cand_key < b) {
                                        best = Some(cand);
                                        best_key = Some(cand_key);
                                    }
                                }
                                if let Some(y) = best {
                                    let x_name = accounts
                                        .get(idx)
                                        .map(|a| a.name.clone())
                                        .unwrap_or_default();
                                    let y_name =
                                        accounts.get(y).map(|a| a.name.clone()).unwrap_or_default();
                                    migrate_names = Some((x_name, y_name));
                                    target = y;
                                }
                            }
                            // Stamp the chosen account so a second session's LRU steers
                            // away from an account already busy under a pin.
                            let tick = self.select_seq.fetch_add(1, Ordering::Relaxed);
                            if let Some(account) = accounts.get_mut(target) {
                                account.last_selected_seq = tick;
                            }
                            Some((target, migrate_names))
                        }
                    };
                    if let Some((target, migrate_names)) = decision {
                        // (3) UNDER THE AFFINITY LOCK ONLY: commit the pin. Accounts
                        // lock already dropped — never nest.
                        //
                        // TOCTOU close: the `counts` driving the migration decision
                        // were read in section (1) and the lock was then dropped, so a
                        // concurrent select on another session stacked on the same X
                        // could have decided the SAME idle Y in parallel. Committing
                        // both blindly would OVER-migrate (Y over-stacks, X empties —
                        // the inverse of the goal, and it can oscillate). So for a
                        // MIGRATION we RE-VALIDATE against FRESH counts re-tallied from
                        // the live map under this same lock that mutates it: the pin
                        // must still be X AND `count(target)+1 < count(X)` must still
                        // hold. If not, ABORT the move and keep the existing pin. No
                        // accounts lock is taken here (the re-check needs only the
                        // affinity-map counts + the already-chosen target; the next
                        // select re-checks eligibility anyway).
                        let mut pins = self.affinity.lock().expect("affinity lock poisoned");
                        let mut committed = target;
                        if target != idx {
                            let still_pinned_x = pins.get(&key).map(|&(i, _)| i) == Some(idx);
                            let mut count_x_now = 0usize;
                            let mut count_t_now = 0usize;
                            for &(i, _) in pins.values() {
                                if i == idx {
                                    count_x_now += 1;
                                }
                                if i == target {
                                    count_t_now += 1;
                                }
                            }
                            // Strictly-improves-balance guard, re-checked on fresh state.
                            if still_pinned_x && count_t_now + 1 < count_x_now {
                                if let Some((x_name, y_name)) = migrate_names {
                                    tracing::info!(
                                        "affinity: migrate session off {} (n={}) -> {}",
                                        x_name,
                                        count_x_now,
                                        y_name
                                    );
                                }
                            } else {
                                // The decision went stale between sections (a sibling
                                // select already rebalanced): keep the current pin.
                                committed = idx;
                            }
                        }
                        // Re-pin the session (migration target, or the honoured/kept X)
                        // and refresh its last-touch for LRU eviction.
                        pins.insert(key, (committed, now_ms));
                        return Some(committed);
                    }
                }
            }
        }

        // Normal LRU/priority pick (identical to the pre-affinity path). The
        // accounts lock is scoped so it is released before we touch the affinity
        // lock again for the re-pin below.
        let best = {
            let mut accounts = self.accounts.write().expect("accounts lock poisoned");

            // First pass: honour pacing (skip accounts at the concurrency cap or
            // inside the min-spacing window). With pacing OFF this is byte-identical
            // to the pre-pacing pick.
            let mut best = self.pick_eligible(&accounts, tried, now, now_ms, is_fable, true);

            // Soft fallback (CRITICAL — pacing must never DROP a servable request):
            // if pacing gated EVERY account out but at least one is servable ignoring
            // pacing, serve the least-loaded (lowest in_flight, then the normal LRU
            // key). With pacing OFF the first pass and this pass use identical
            // eligibility, so a None first pass ⟹ None here too — default-OFF stays
            // byte-identical (no spurious fallback, no log).
            if best.is_none() {
                if let Some(idx) = self.pick_least_loaded(&accounts, tried, now, now_ms, is_fable) {
                    if let Some(account) = accounts.get(idx) {
                        tracing::info!(
                            account = %account.name,
                            in_flight = account.in_flight,
                            "pacing: all accounts paced, serving least-loaded"
                        );
                    }
                    best = Some(idx);
                }
            }

            // Stamp the chosen account so the next select prefers a different one.
            if let Some(idx) = best {
                let tick = self.select_seq.fetch_add(1, Ordering::Relaxed);
                if let Some(account) = accounts.get_mut(idx) {
                    account.last_selected_seq = tick;
                }
            }
            best
        };

        // Record the pick as this session's pin (initial pin, or re-pin on
        // migration). Skipped entirely when affinity is off, so the map stays
        // empty on the disabled path.
        if let (Some(key), Some(idx)) = (affinity, best) {
            let mut pins = self.affinity.lock().expect("affinity lock poisoned");
            pins.insert(key, (idx, now_ms));
            // Bound the map by size + LRU-by-last-touch: once over AFFINITY_CAP, evict
            // the single oldest-last-touch entry (not the one we just inserted). Stable
            // pins survive reconnects, so this size cap — not a disconnect hook — is
            // what keeps a long-lived proxy from growing the map without limit.
            const AFFINITY_CAP: usize = 1024;
            if pins.len() > AFFINITY_CAP {
                if let Some((&oldest, _)) = pins.iter().min_by_key(|(_, &(_, touch))| touch) {
                    if oldest != key {
                        pins.remove(&oldest);
                    }
                }
            }
        }
        best
    }

    fn eligible(
        account: &AccountRuntime,
        global_threshold: f64,
        pacing: &PacingConfig,
        respect_pacing: bool,
        now: OffsetDateTime,
        now_ms: i64,
        is_fable: bool,
    ) -> bool {
        if account.disabled || account.status == AccountStatus::Error {
            return false;
        }
        // A rate-limit hold blocks the account only while it is still in the
        // future — a past hold is treated as expired (live), no mutation needed.
        if let Some(until) = account.rate_limited_until_ms {
            if now_ms < until {
                return false;
            }
        }
        let threshold = account.switch_threshold.unwrap_or(global_threshold);
        if account.quota.is_near(threshold, now) {
            return false;
        }
        // Per-model routing: only a Fable request gates on the model-scoped weekly
        // (`7d_oi`) bucket — every non-Fable model still serves from this account.
        if is_fable && account.quota.model_weekly_exhausted(threshold, now) {
            return false;
        }
        // SOFT pacing gate, evaluated LAST so it only ever narrows an already-healthy
        // account. When `respect_pacing` is false (the fallback pass) or pacing is
        // unconfigured, this is inert — so a default-OFF build is byte-identical here
        // and the fallback pass can always still find a servable account.
        if respect_pacing && pacing.is_active() {
            if let Some(cap) = pacing.effective_max_in_flight() {
                if account.in_flight >= cap {
                    return false;
                }
            }
            if let Some(gap) = pacing.min_spacing_ms {
                if now_ms.saturating_sub(account.last_served_ms) < gap as i64 {
                    return false;
                }
            }
        }
        true
    }

    /// The best pacing-respecting eligible account not in `tried`, by ascending
    /// `(priority, last_selected_seq, soonest weekly reset)` — the pre-pacing LRU
    /// order, now additionally skipping any account the soft pacing gate holds out.
    /// Read-only (no stamp); the caller stamps the winner. Emits one INFO line per
    /// account skipped *specifically because of pacing* (healthy but capped/spaced)
    /// so the knobs are tunable live.
    fn pick_eligible(
        &self,
        accounts: &[AccountRuntime],
        tried: &HashSet<usize>,
        now: OffsetDateTime,
        now_ms: i64,
        is_fable: bool,
        respect_pacing: bool,
    ) -> Option<usize> {
        let mut best: Option<usize> = None;
        let mut best_key: Option<(i64, u64, i128)> = None;
        for (idx, account) in accounts.iter().enumerate() {
            if tried.contains(&idx) {
                continue;
            }
            if !Self::eligible(
                account,
                self.global_threshold,
                &self.pacing,
                respect_pacing,
                now,
                now_ms,
                is_fable,
            ) {
                // Distinguish a pacing skip (healthy but capped/spaced) from a real
                // ineligibility (disabled/error/quota) so the log names only the former.
                if respect_pacing
                    && self.pacing.is_active()
                    && Self::eligible(
                        account,
                        self.global_threshold,
                        &self.pacing,
                        false,
                        now,
                        now_ms,
                        is_fable,
                    )
                {
                    tracing::info!(
                        account = %account.name,
                        in_flight = account.in_flight,
                        "pacing: skip in selection"
                    );
                }
                continue;
            }
            // Unknown weekly reset sorts FIRST (probe it) — treat as the minimum.
            let reset = account
                .quota
                .governing_weekly_reset(now)
                .map(|r| r.unix_timestamp() as i128)
                .unwrap_or(i128::MIN);
            let key = (account.priority, account.last_selected_seq, reset);
            if best_key.is_none_or(|b| key < b) {
                best = Some(idx);
                best_key = Some(key);
            }
        }
        best
    }

    /// The least-loaded servable account not in `tried`, IGNORING pacing (the soft
    /// fallback pass). Sort key ascending: `(in_flight, priority, last_selected_seq,
    /// weekly reset)` — least concurrent load first, then the normal LRU order. All
    /// non-pacing eligibility (disabled/error/rate-limit/quota) still applies, so a
    /// genuinely exhausted fleet still yields `None` (a real 429), while a merely
    /// all-paced fleet always yields the coolest account. Read-only.
    fn pick_least_loaded(
        &self,
        accounts: &[AccountRuntime],
        tried: &HashSet<usize>,
        now: OffsetDateTime,
        now_ms: i64,
        is_fable: bool,
    ) -> Option<usize> {
        let mut best: Option<usize> = None;
        let mut best_key: Option<(u32, i64, u64, i128)> = None;
        for (idx, account) in accounts.iter().enumerate() {
            if tried.contains(&idx) {
                continue;
            }
            if !Self::eligible(
                account,
                self.global_threshold,
                &self.pacing,
                false,
                now,
                now_ms,
                is_fable,
            ) {
                continue;
            }
            let reset = account
                .quota
                .governing_weekly_reset(now)
                .map(|r| r.unix_timestamp() as i128)
                .unwrap_or(i128::MIN);
            let key = (
                account.in_flight,
                account.priority,
                account.last_selected_seq,
                reset,
            );
            if best_key.is_none_or(|b| key < b) {
                best = Some(idx);
                best_key = Some(key);
            }
        }
        best
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

    /// Refresh account `idx`'s token if it is hard-expired OR within the proactive
    /// [`oauth::EXPIRING_SOON_MS`] buffer of expiry, coalescing concurrent callers
    /// into a single upstream refresh. Refreshing ahead of the hard boundary keeps
    /// a batch of in-flight requests from all carrying a just-dead token into an
    /// upstream 401 storm.
    pub async fn ensure_fresh(&self, idx: usize) {
        self.ensure_fresh_inner(idx, false).await;
    }

    /// Force a refresh regardless of expiry (used after a 401), still coalesced.
    /// Returns `true` iff a new token was actually applied — a `false` return means
    /// the force was coalesced, cooldown-suppressed, or the upstream refresh failed,
    /// so the caller (the proxy 401 arm) rotates instead of retrying a dead token.
    pub async fn ensure_fresh_force(&self, idx: usize) -> bool {
        self.ensure_fresh_inner(idx, true).await
    }

    /// Returns `true` iff [`Self::apply_refresh`] ran (a new token was applied).
    async fn ensure_fresh_inner(&self, idx: usize, force: bool) -> bool {
        // Decide whether to refresh and snapshot the access token we intend to
        // replace — all before taking the (async) coalescing lock.
        let Some((refresh_token, access_before)) = self.refresh_plan(idx, force, crate::now_ms())
        else {
            return false;
        };

        let lock = match self.refresh_locks.get(idx) {
            Some(lock) => lock.clone(),
            None => return false,
        };
        let _guard = lock.lock().await;

        // Coalesce ALL concurrent callers, the force path included (bug #10): a
        // leader that already refreshed while we queued on the lock changed the
        // access token, so a follower sees a different token here and skips the
        // upstream refresh entirely — and so never sends the now-rotated stale
        // refresh token (which would 401 and wrongly sideline a healthy account).
        if self.access_token(idx).as_deref() != Some(access_before.as_str()) {
            return false;
        }
        // A *transient* leader failure leaves the access token unchanged, so the
        // guard above can't catch us — but the leader stamped a short cooldown
        // before releasing the lock. Honor it: skip re-POSTing the same
        // single-use refresh token while the cooldown is still live. It
        // self-clears by time, so a later sequential retry is not suppressed.
        {
            let accounts = self.accounts.read().expect("accounts lock poisoned");
            if let Some(until) = accounts.get(idx).and_then(|a| a.refresh_retry_after_ms) {
                if crate::now_ms() < until {
                    return false;
                }
            }
        }
        let (name, account_uuid, org_uuid, org_name) = {
            let accounts = self.accounts.read().expect("accounts lock poisoned");
            match accounts.get(idx) {
                Some(a) => (
                    a.name.clone(),
                    a.account_uuid.clone(),
                    a.org_uuid.clone(),
                    a.org_name.clone(),
                ),
                None => return false,
            }
        };

        tracing::info!(account = %name, "refreshing OAuth token");
        match self.refresher.refresh(refresh_token).await {
            Ok(tokens) => {
                // A *forced* success re-arms the refresh cooldown, collapsing a herd
                // of concurrent 401-driven force-refreshes to ONE rotation instead of
                // N back-to-back. Passing the deadline INTO apply_refresh folds the
                // re-arm and the success-path clear into one write, so the throttle
                // can never be clobbered by an ordering slip between two lock
                // sections — the policy (whether/what) stays here, the single atomic
                // write lives in apply_refresh.
                let cooldown_after = force.then(Self::cooldown_deadline);
                self.apply_refresh(idx, &tokens, cooldown_after);
                self.persist_tokens(&name, account_uuid, org_uuid, org_name, &tokens);
                tracing::info!(account = %name, "token refreshed");
                true
            }
            Err(oauth::OAuthError::AuthRejected { status }) => {
                tracing::error!(account = %name, status, "refresh token rejected — re-login needed");
                if let Some(a) = self
                    .accounts
                    .write()
                    .expect("accounts lock poisoned")
                    .get_mut(idx)
                {
                    a.status = AccountStatus::Error;
                }
                // A REJECTED refresh is the one signal that this credential may truly
                // be dead — back off before the next recovery attempt (first rejection
                // arms the base delay; each further one doubles it). A *transient*
                // failure deliberately does NOT back off: it proves nothing, so the
                // next probe tick may retry promptly.
                self.grow_error_backoff(idx);
                false
            }
            Err(oauth::OAuthError::Transient(msg)) => {
                // Keep the current token; this one request fails over instead.
                // The token is UNCHANGED, so the access-token guard can't stop a
                // follower already queued on the lock from re-POSTing the same
                // single-use refresh token. Stamp a short self-clearing cooldown
                // (still under this lock, so queued followers see it) to coalesce
                // the concurrent batch; a later sequential retry still refreshes.
                if let Some(a) = self
                    .accounts
                    .write()
                    .expect("accounts lock poisoned")
                    .get_mut(idx)
                {
                    a.refresh_retry_after_ms = Some(Self::cooldown_deadline());
                }
                tracing::warn!(account = %name, error = %msg, "token refresh transient failure");
                false
            }
        }
    }

    /// If account `idx` should be refreshed now, return `(refresh_token,
    /// access_before)` — the refresh token to send and the access token we mean
    /// to replace. `access_before` lets [`Self::ensure_fresh_inner`] detect, under
    /// the coalescing guard, that another caller already refreshed (the access
    /// token changed), coalescing even the force path (bug #10).
    fn refresh_plan(&self, idx: usize, force: bool, now_ms: i64) -> Option<(String, String)> {
        let accounts = self.accounts.read().expect("accounts lock poisoned");
        let account = accounts.get(idx)?;
        if account.account_type != "oauth" {
            return None;
        }
        let refresh_token = account.refresh_token.clone()?;
        let is_expired = oauth::is_expired(account.expires_at_ms, now_ms);
        // Proactive refresh: a token within the EXPIRING_SOON buffer is refreshed
        // before it hard-expires, so a batch of in-flight requests at the boundary
        // never all carry a just-dead token into an upstream 401 storm.
        let is_expiring_soon = oauth::is_expiring_soon(account.expires_at_ms, now_ms);
        if !(force || is_expired || is_expiring_soon) {
            return None;
        }
        // A transient failure stamped a short cooldown to coalesce the batch —
        // don't START a new refresh during it. A hard-expired token under
        // `force` still overrides, so a dead-in-hand token is never pinned
        // un-refreshable by the cooldown (the cooldown also self-clears by time).
        let in_cooldown = account
            .refresh_retry_after_ms
            .is_some_and(|until| now_ms < until);
        if in_cooldown && !(force && is_expired) {
            return None;
        }
        Some((refresh_token, account.access_token.clone()))
    }

    /// The wall-clock deadline before which no new refresh of an account may
    /// START. Single source of the cooldown formula so the transient-failure arm
    /// and the forced-success re-arm can never drift apart.
    fn cooldown_deadline() -> i64 {
        crate::now_ms() + REFRESH_RETRY_COOLDOWN_MS
    }

    /// Install a freshly refreshed token pair. `cooldown_after` is the refresh
    /// floor set atomically with the install: `None` clears any transient-failure
    /// cooldown (a good refresh ends it); `Some(deadline)` re-arms it for a
    /// *forced* success so a herd of concurrent 401 force-refreshes collapses to
    /// one rotation. Folding it in here makes the clear-vs-re-arm a single write —
    /// no ordering hazard between two separate lock sections.
    fn apply_refresh(&self, idx: usize, tokens: &Tokens, cooldown_after: Option<i64>) {
        let mut accounts = self.accounts.write().expect("accounts lock poisoned");
        if let Some(account) = accounts.get_mut(idx) {
            account.access_token = tokens.access_token.clone();
            account.refresh_token = Some(tokens.refresh_token.clone());
            account.expires_at_ms = Some(tokens.expires_at_ms);
            account.refresh_retry_after_ms = cooldown_after;
            if account.status == AccountStatus::Error {
                // RECOVERY: a refresh just succeeded, which a genuinely dead
                // credential could never do — so this row was never dead, and it is
                // now holding a fresh valid token. Clear the error backoff with it.
                account.status = AccountStatus::Active;
            }
            account.error_retry_after_ms = None;
            account.error_backoff_ms = 0;
        }
    }

    /// Arm (or double) the recovery backoff for an errored row. First rejection
    /// arms [`ERROR_REPROBE_BASE_MS`]; each subsequent one doubles up to
    /// [`ERROR_REPROBE_CAP_MS`], so a permanently dead credential is retried at most
    /// ~twice an hour instead of hammering the OAuth endpoint (which would risk
    /// rate-limiting the whole fleet).
    fn grow_error_backoff(&self, idx: usize) {
        let mut accounts = self.accounts.write().expect("accounts lock poisoned");
        if let Some(account) = accounts.get_mut(idx) {
            account.error_backoff_ms = if account.error_backoff_ms == 0 {
                ERROR_REPROBE_BASE_MS
            } else {
                account
                    .error_backoff_ms
                    .saturating_mul(2)
                    .min(ERROR_REPROBE_CAP_MS)
            };
            account.error_retry_after_ms = Some(crate::now_ms() + account.error_backoff_ms);
        }
    }

    /// Flush the current in-memory config (with any refreshed tokens) to disk.
    /// Token refreshes already persist incrementally via [`Self::persist_tokens`],
    /// so this is the belt-and-suspenders final flush on shutdown (DESIGN §main).
    /// A missing `config_path` (tests, corrupt-source boot) is a silent no-op so
    /// a corrupt user file is never clobbered with defaults.
    pub fn persist_now(&self) {
        let Some(path) = &self.config_path else {
            return;
        };
        // Save UNDER the lock (not clone-then-save-unlocked) so a shutdown flush
        // can't race a concurrent persist_tokens and clobber a just-rotated token.
        let config = self.config.lock().expect("config lock poisoned");
        if let Err(err) = config::save(path, &config) {
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
        // through the save serializes writes so every rotation lands on disk.
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
        if let Err(err) = config::save(path, &config) {
            tracing::error!(error = %err, "failed to persist refreshed token to config");
        }
    }

    /// Fold a response's rate-limit headers into account `idx`'s quota. This runs
    /// on **every** upstream response (including 429s that trigger a rotation), so
    /// it deliberately does NOT touch the request counter — that would double-count
    /// a client request that was retried across accounts (bug #4). Request counting
    /// happens once, in [`Manager::record_served`].
    pub fn update_quota(&self, idx: usize, headers: &reqwest::header::HeaderMap) {
        let mut accounts = self.accounts.write().expect("accounts lock poisoned");
        if let Some(account) = accounts.get_mut(idx) {
            account.quota.update_from_headers(headers);
        }
    }

    /// Count exactly one served client request against account `idx` (the true
    /// serving account) and stamp its last-used time. Called once per client
    /// request at the terminal outcome — never per upstream response — so retries
    /// that rotate across accounts do not inflate the counter (bug #4).
    pub fn record_served(
        &self,
        idx: usize,
        now: OffsetDateTime,
        session_key: Option<u64>,
        kind: SessionKind,
    ) {
        // Short display id for the log line, computed before the accounts lock so
        // the borrow ordering in `tracing::info!` stays simple.
        let sid = session_key.map(short_session_id);
        {
            let mut accounts = self.accounts.write().expect("accounts lock poisoned");
            if let Some(account) = accounts.get_mut(idx) {
                account.requests += 1;
                account.last_used_ms = Some(odt_to_ms(now));
                // One line per forwarded request naming the account that served it,
                // so the load spread is observable in the headless log (and auditable
                // against the reported "everything piles onto one account" symptom).
                tracing::info!(account = %account.name, index = idx, session = sid, "serving request");
            }
        }
        // Upsert per-session stats (only when a session key exists) so a session
        // pinned by affinity is observable live. Independent of the `affinity`
        // pin map, so routing is unaffected.
        if let Some(key) = session_key {
            let now_ms = odt_to_ms(now);
            let mut sessions = self.sessions.lock().expect("sessions lock poisoned");
            let stat = sessions.entry(key).or_insert(SessionStat {
                account_idx: idx,
                requests: 0,
                last_seen_ms: now_ms,
                kind,
            });
            stat.account_idx = idx;
            stat.requests += 1;
            stat.last_seen_ms = now_ms;
            stat.kind = kind;
            // Bound the map so a long-lived proxy can't grow it without limit: once over
            // SESSION_CAP, evict the single oldest-last-seen entry (not the one we just
            // touched). Personal use has a handful of sessions; this is a backstop.
            const SESSION_CAP: usize = 128;
            if sessions.len() > SESSION_CAP {
                if let Some((&oldest, _)) = sessions.iter().min_by_key(|(_, s)| s.last_seen_ms) {
                    if oldest != key {
                        sessions.remove(&oldest);
                    }
                }
            }
        }
        self.set_current(idx);
    }

    /// Add token usage to account `idx` (the true serving account — bug #3).
    /// `input_tokens` already includes `cache_creation_input_tokens` +
    /// `cache_read_input_tokens` (summed by the caller — bug #4).
    pub fn update_usage(&self, idx: usize, input_tokens: u64, output_tokens: u64) {
        let mut accounts = self.accounts.write().expect("accounts lock poisoned");
        if let Some(account) = accounts.get_mut(idx) {
            account.input_tokens += input_tokens;
            account.output_tokens += output_tokens;
        }
    }

    /// Fold a background probe's usage into account `idx`'s quota windows.
    pub fn apply_usage(&self, idx: usize, usage: &Usage) {
        let mut accounts = self.accounts.write().expect("accounts lock poisoned");
        if let Some(account) = accounts.get_mut(idx) {
            account.quota.apply_usage(usage);
        }
    }

    /// Record the health of account `idx`'s most recent probe. A failing probe
    /// stamps a visible status + message (never a silently-frozen bar), while an
    /// `Ok` probe clears any prior error.
    pub fn record_probe(&self, idx: usize, status: ProbeStatus, error: Option<String>) {
        let mut accounts = self.accounts.write().expect("accounts lock poisoned");
        if let Some(account) = accounts.get_mut(idx) {
            account.probe_status = status;
            account.probe_error = error;
            // "Running" is a transient in-flight marker; only a terminal outcome
            // updates the last-probe timestamp the TUI ages against.
            if status != ProbeStatus::Never {
                account.last_probe_ms = Some(crate::now_ms());
            }
        }
    }

    /// The OAuth account indices eligible for a usage probe: an OAuth account with a
    /// refresh token that is not disabled, plus any `Error` row whose RECOVERY
    /// backoff has elapsed.
    ///
    /// Errored rows were previously excluded outright, on the reasoning that "a probe's
    /// successful refresh would flip an `Error` row back to `Active`, silently
    /// re-inserting it into rotation". That reasoning no longer holds, and its premise
    /// has since been removed on both sides:
    /// - `Error` now arises ONLY from a REJECTED refresh (the `AuthRejected` arm). A
    ///   request-level 401 no longer condemns a row (that was rotation churn falsely
    ///   sidelining healthy accounts — fixed 2026-07-17), and the port-takeover
    ///   singleton removed token-wars, the main source of *transient* rejections.
    /// - Recovery requires a **successful refresh**, which a genuinely dead
    ///   credential cannot produce — so a re-probe can only ever revive a row that was
    ///   never actually dead. The feared "silently re-insert a dead account" is
    ///   unreachable: a dead token re-rejects and the backoff doubles.
    ///
    /// Excluding them outright made `Error` a life sentence: nothing probes or selects
    /// an errored row, and only a refresh clears it — so one transient rejection
    /// sidelined a healthy account until restart (observed live 2026-07-17).
    fn probeable_indices(&self) -> Vec<usize> {
        let now_ms = crate::now_ms();
        let accounts = self.accounts.read().expect("accounts lock poisoned");
        accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| {
                a.account_type == "oauth"
                    && a.refresh_token.is_some()
                    && !a.disabled
                    && (a.status != AccountStatus::Error
                        || a.error_retry_after_ms.is_some_and(|until| now_ms >= until))
            })
            .map(|(idx, _)| idx)
            .collect()
    }

    /// Probe **every** OAuth account's quota concurrently. Refreshes all rows,
    /// not just the serving one — that is the whole point of the background probe.
    pub async fn probe_all(&self) {
        let idxs = self.probeable_indices();
        let last = idxs.len().saturating_sub(1);
        for (i, idx) in idxs.into_iter().enumerate() {
            self.probe_account(idx).await;
            // Space the calls so a fleet sweep never bursts the usage endpoint
            // into its own 429 — the burst was what painted a false "error" on
            // every row. Sequential + spaced stays well inside the cadence.
            if i < last {
                tokio::time::sleep(crate::probe::PROBE_SPACING).await;
            }
        }
    }

    /// Probe one account: ensure its token is fresh, read usage, and on a `401`
    /// force-refresh and retry exactly once. Success folds the usage into the
    /// account's windows; any failure is recorded as visible probe health.
    pub async fn probe_account(&self, idx: usize) {
        // An `Error` row only reaches here once its recovery backoff elapsed (see
        // `probeable_indices`). `Error` means "the refresh token was rejected", so the
        // only meaningful recovery test is to attempt a refresh: success runs
        // `apply_refresh`, which clears Error→Active and the backoff — proving the row
        // was never dead. Anything else leaves it errored; a rejection already grew the
        // backoff (a transient failure deliberately did not), so just stand down.
        // Short-circuit: a healthy row never pays for the forced refresh.
        if self.account_status(idx) == Some(AccountStatus::Error)
            && !self.ensure_fresh_force(idx).await
        {
            return;
        }
        self.ensure_fresh(idx).await;
        let Some(token) = self.access_token(idx) else {
            return;
        };

        let result = self.prober.probe(token).await;
        let result = match result {
            Err(err) if err.status == Some(401) => {
                // Token rejected: force a refresh and retry once with the new one.
                self.ensure_fresh_force(idx).await;
                match self.access_token(idx) {
                    Some(fresh) => self.prober.probe(fresh).await,
                    None => Err(err),
                }
            }
            other => other,
        };

        match result {
            Ok(usage) => {
                self.apply_usage(idx, &usage);
                self.record_probe(idx, ProbeStatus::Ok, None);
            }
            Err(err) => {
                let msg = err.message.to_lowercase();
                let is_timeout = msg.contains("timed out") || msg.contains("timeout");
                // A failing probe never blanks the last-learned bar (no apply_usage
                // on this path), so the only question is how loud to be. Soften only
                // ENDPOINT-side failures where the credential is provably fine: a 429
                // (the usage endpoint throttling the probe itself) or a transient 5xx
                // read as a benign, self-clearing `RateLimited`, never the false
                // fleet-wide "error" a bursted sweep used to paint. But a TRANSPORT
                // failure (no HTTP status, not a timeout — connection refused, DNS/TLS,
                // a proxy-env regression) stays a visible `Error`: a persistent one is
                // a real connectivity problem and must NOT hide behind a benign label
                // (probe health is first-class — masking it defeats the point).
                let status = match err.status {
                    Some(429) => ProbeStatus::RateLimited,
                    Some(s) if (500..=599).contains(&s) => ProbeStatus::RateLimited,
                    Some(_) => ProbeStatus::Error,
                    None if is_timeout => ProbeStatus::Timeout,
                    None => ProbeStatus::Error,
                };
                self.record_probe(idx, status, Some(err.message));
            }
        }
    }

    /// The account indices eligible for a keep-warm request. Starts from the same
    /// base as [`Self::probeable_indices`] (an OAuth account with a refresh token
    /// that is neither disabled nor errored) and additionally skips:
    ///  - a `Throttled` account — warming it would just 429;
    ///  - an account whose 5h window is **live** (a future reset) — already warm,
    ///    so warming again only burns quota for nothing;
    ///  - a near/over-threshold account — warming an exhausted account is pointless.
    ///
    /// A cold account (no 5h data) or one whose 5h reset has already passed IS a
    /// target — those are exactly the accounts whose window we want to (re)start.
    pub fn warm_targets(&self) -> Vec<usize> {
        let now = OffsetDateTime::now_utc();
        let accounts = self.accounts.read().expect("accounts lock poisoned");
        accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| {
                a.account_type == "oauth"
                    && a.refresh_token.is_some()
                    && !a.disabled
                    && a.status != AccountStatus::Error
                    && a.status != AccountStatus::Throttled
                    // A live future 5h reset means the session window is already running.
                    && a.quota.five_hour.and_then(|w| w.live_reset(now)).is_none()
                    // Warming an at/over-threshold account is wasted spend.
                    && !a
                        .quota
                        .is_near(a.switch_threshold.unwrap_or(self.global_threshold), now)
            })
            .map(|(idx, _)| idx)
            .collect()
    }

    /// Warm one idle account: ensure its token is fresh, fire a minimal upstream
    /// request to (re)start its 5h window, and on success fold the response's
    /// rate-limit headers so the just-started window is immediately visible (which
    /// suppresses a re-warm on the next sweep). A warm FAILURE is non-fatal —
    /// logged at `warn` and stepped over, never a crash of the loop.
    pub async fn warm_account(&self, idx: usize) {
        self.ensure_fresh(idx).await;
        let Some(token) = self.access_token(idx) else {
            return;
        };
        let upstream = self.upstream.clone();
        let name = self.account_name(idx).unwrap_or_default();
        match self.warmer.warm(token, upstream).await {
            Ok(headers) => {
                // Fold the now-live 5h window into the account's quota so the next
                // sweep sees it as already warm (REQUIRED — suppresses re-warm).
                self.update_quota(idx, &headers);
                tracing::info!(account = %name, index = idx, "keep-warm: started 5h window");
            }
            Err(err) => {
                tracing::warn!(
                    account = %name,
                    index = idx,
                    error = %err,
                    "keep-warm request failed (non-fatal)"
                );
            }
        }
    }

    /// Warm every eligible idle account once, SEQUENTIALLY with [`crate::probe::PROBE_SPACING`]
    /// between calls (mirrors the spaced probe sweep). Overlapping sweeps are
    /// skipped via [`Self::warm_in_flight`] so two timers never double-warm.
    pub async fn warm_all(&self) {
        // Skip if a sweep is already running (mirrors the JS `_running` guard).
        if self.warm_in_flight.swap(true, Ordering::SeqCst) {
            return;
        }
        // Reset the flag even if a warm unexpectedly unwinds.
        let _guard = WarmInFlightGuard(&self.warm_in_flight);

        let idxs = self.warm_targets();
        let last = idxs.len().saturating_sub(1);
        for (i, idx) in idxs.into_iter().enumerate() {
            self.warm_account(idx).await;
            if i < last {
                tokio::time::sleep(crate::probe::PROBE_SPACING).await;
            }
        }
    }

    /// Hold account `idx` out of rotation for `seconds` (a 429 quota rejection).
    /// The hold is clamped to at most [`MAX_RATE_LIMIT_HOLD_SECONDS`] so a huge
    /// `retry-after` (Anthropic weekly caps report hours) can never pin a healthy
    /// account out for that long with no revalidation path: it is re-selected
    /// after the bounded hold, and either serves or is re-held. Durable
    /// exhaustion is separately kept out of rotation by the
    /// learned quota utilization, not by this short-term hold.
    pub fn mark_rate_limited(&self, idx: usize, seconds: i64) {
        let hold = seconds.clamp(0, MAX_RATE_LIMIT_HOLD_SECONDS);
        let until = crate::now_ms() + hold * 1000;
        let mut accounts = self.accounts.write().expect("accounts lock poisoned");
        if let Some(account) = accounts.get_mut(idx) {
            account.status = AccountStatus::Throttled;
            account.rate_limited_until_ms = Some(until);
            tracing::info!(account = %account.name, hold_seconds = hold, "rate limited");
        }
    }

    /// Clear a rate-limit hold after live proof it no longer binds (any non-429).
    pub fn clear_rate_limited(&self, idx: usize) {
        let mut accounts = self.accounts.write().expect("accounts lock poisoned");
        if let Some(account) = accounts.get_mut(idx) {
            if account.status == AccountStatus::Throttled {
                account.status = AccountStatus::Active;
                account.rate_limited_until_ms = None;
            }
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
    pub fn set_disabled(&self, idx: usize, disabled: bool) {
        let mut accounts = self.accounts.write().expect("accounts lock poisoned");
        if let Some(account) = accounts.get_mut(idx) {
            account.disabled = disabled;
            if !disabled && account.status == AccountStatus::Error {
                account.status = AccountStatus::Active;
                account.rate_limited_until_ms = None;
            }
        }
    }

    /// Record which account actually served the most recent request.
    pub fn set_current(&self, idx: usize) {
        *self.current.lock().expect("current lock poisoned") = Some(idx);
    }

    /// Append a served-request entry to the ring buffer (most-recent-last).
    pub fn push_log(&self, entry: RequestLogEntry) {
        let mut log = self.log.lock().expect("log lock poisoned");
        if log.len() >= REQUEST_LOG_CAPACITY {
            log.pop_front();
        }
        log.push_back(entry);
    }

    /// Compute the live snapshot the TUI renders. Every quota figure is evaluated
    /// at `now` so the display can never show a past-reset window as still full.
    pub fn snapshot(&self, now: OffsetDateTime) -> StatsSnapshot {
        let accounts = self.accounts.read().expect("accounts lock poisoned");
        let account_snaps = accounts
            .iter()
            .map(|a| {
                let five_hour = a.quota.five_hour.map(|w| w.effective(now));
                let seven_day = a.quota.seven_day.map(|w| w.effective(now));
                // Honest quota state vs this account's OWN threshold (same gating
                // dims `eligible` uses): the most-spent of the 5-hour and weekly
                // windows decides near-limit vs exhausted. Status stays "active" —
                // being quota-parked is not a dead credential.
                let threshold = a.switch_threshold.unwrap_or(self.global_threshold);
                let gating = [five_hour, seven_day]
                    .into_iter()
                    .flatten()
                    .reduce(f64::max);
                let quota_state = match gating {
                    Some(u) if u >= 1.0 => crate::stats::QuotaState::Exhausted,
                    Some(u) if u >= threshold => crate::stats::QuotaState::NearLimit,
                    _ => crate::stats::QuotaState::Normal,
                };
                AccountSnapshot {
                    name: a.name.clone(),
                    priority: a.priority,
                    // `Throttled` is cleared from the enum only when the account
                    // next serves a non-429 (proxy.rs), so a naturally-expired hold
                    // would linger as a stale "throttled" label. Derive the DISPLAYED
                    // status from the live deadline — exactly as `eligible`,
                    // `rate_limited_until` and the quota bars already do — so the
                    // snapshot never shows a status the routing no longer honours.
                    status: {
                        let displayed = match a.status {
                            AccountStatus::Throttled
                                if a.rate_limited_until_ms
                                    .is_none_or(|until| until <= odt_to_ms(now)) =>
                            {
                                AccountStatus::Active
                            }
                            other => other,
                        };
                        displayed.as_str().to_string()
                    },
                    disabled: a.disabled,
                    five_hour,
                    five_hour_reset: a.quota.five_hour.and_then(|w| w.live_reset(now)),
                    seven_day,
                    seven_day_reset: a.quota.seven_day.and_then(|w| w.live_reset(now)),
                    seven_day_oi: a.quota.seven_day_oi.map(|w| w.effective(now)),
                    requests: a.requests,
                    input_tokens: a.input_tokens,
                    output_tokens: a.output_tokens,
                    last_used: a.last_used_ms.and_then(ms_to_odt),
                    rate_limited_until: a
                        .rate_limited_until_ms
                        .filter(|&until| until > odt_to_ms(now))
                        .and_then(ms_to_odt),
                    probe_status: a.probe_status,
                    last_probe: a.last_probe_ms.and_then(ms_to_odt),
                    probe_error: a.probe_error.clone(),
                    quota_state,
                }
            })
            .collect();

        let recent = self
            .log
            .lock()
            .expect("log lock poisoned")
            .iter()
            .rev()
            .cloned()
            .collect();

        // Resolve each session's account_idx → name from the accounts guard we
        // already hold, sorted most-recent-first for the TUI sessions pane.
        let sessions = {
            let map = self.sessions.lock().expect("sessions lock poisoned");
            let mut v: Vec<SessionSnapshot> = map
                .iter()
                .map(|(k, s)| SessionSnapshot {
                    id: short_session_id(*k),
                    account: accounts
                        .get(s.account_idx)
                        .map(|a| a.name.clone())
                        .unwrap_or_default(),
                    requests: s.requests,
                    last_seen: ms_to_odt(s.last_seen_ms),
                    kind: s.kind,
                })
                .collect();
            v.sort_by_key(|s| std::cmp::Reverse(s.last_seen));
            v
        };

        StatsSnapshot {
            accounts: account_snaps,
            current: *self.current.lock().expect("current lock poisoned"),
            recent,
            sessions,
        }
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

    /// Cache tokens count: `update_usage` accumulates whatever the caller sums,
    /// which for the proxy includes cache-creation + cache-read input tokens.
    #[test]
    fn update_usage_accumulates_input_and_output() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(config_with(vec![account("a", 0)]), refresher);
        // e.g. input_tokens=10 + cache_creation=100 + cache_read=1000 = 1110.
        manager.update_usage(0, 1110, 42);
        manager.update_usage(0, 0, 8);
        let snap = manager.snapshot(OffsetDateTime::now_utc());
        assert_eq!(snap.accounts[0].input_tokens, 1110);
        assert_eq!(snap.accounts[0].output_tokens, 50);
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
    /// limited), the next same-key select re-pins to a DIFFERENT eligible account
    /// and then sticks to that one.
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
        manager.mark_rate_limited(pinned, 300);
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

    /// If the pinned account is in `tried` (this request already failed over it),
    /// affinity falls through to a normal pick and re-pins to the new account.
    #[test]
    fn affinity_falls_through_when_pinned_in_tried() {
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

    /// Migration #2 — two sessions stacked on acct 0 with acct 1 idle+eligible:
    /// selecting for one of them migrates it to acct 1, and the affinity map records
    /// the new pin.
    #[test]
    fn stacked_session_migrates_to_idle() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0)]),
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

    /// Migration #3 — convergence, no thrash: after #2's migration the layout is
    /// balanced (one session each), so further selects for BOTH sessions are stable —
    /// neither bounces back.
    #[test]
    fn migration_converges_no_thrash() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0)]),
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

    /// Migration #5 — three sessions stacked on acct 0 with accts 1,2 idle spread to
    /// one-each after a round of selects (convergence across three accounts).
    #[test]
    fn three_sessions_spread_across_three_idle() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0), account("c", 0)]),
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
        let hint = manager.retry_after_hint(now);
        assert!(
            hint > 60,
            "the 7d_oi reset must drive the hint past the 60s default, got {hint}"
        );
        let expected = ((reset_ms - odt_to_ms(now) + 999) / 1000).max(1);
        assert_eq!(
            hint, expected,
            "hint equals the 7d_oi reset delta in seconds"
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

    /// TOCTOU regression (concurrency) — the migration feature's OWN target workload.
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
            config_with(vec![
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
}
