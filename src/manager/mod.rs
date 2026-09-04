//! Account rotation, token freshness, and the live state the proxy/TUI share.
//!
//! Selection order:
//!   1. lowest `priority` value wins (operator-controlled; default 0);
//!   2. within a priority tier, the account in the soonest reset-urgency BUCKET
//!      goes next — the governing weekly reset floored into
//!      `resetUrgencyTierHours`-wide buckets (default 24h; `0` disables the
//!      term). Unused weekly quota is worth nothing once its window resets, so
//!      the fleet spends the account whose headroom expires soonest first. A
//!      window with no known reset sorts ahead of every bucket ("probe it").
//!   3. within a bucket, the least-recently-*selected* account goes next, so
//!      consecutive requests fan out across the fleet instead of hammering one
//!      account. (A single request barely moves a weekly bar, so ordering by
//!      quota headroom would pin one account until its bar caught up — and
//!      ordering by the RAW reset instant would do the same, which is why step 2
//!      buckets rather than compares directly.) A never-selected account sorts
//!      first; the raw weekly reset is the cold-start tiebreak below that.
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

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
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

mod pins;
mod probing;
mod refresh;
mod select;
mod snapshot;
mod state;
mod throttle;
mod usage;
mod warm;

// Re-exported so unit F's replay harness (`tests/`) can call the SAME
// production predicate rather than a reimplementation — see
// the divert-budget design notes §6. Import path: `teamclaude_rs::manager::{divert_verdict, DivertVerdict}`.
pub use select::{divert_verdict, DivertVerdict};

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

/// How many probe sweeps in a row may fail to read an account's quota before
/// keep-warm stops waiting for evidence and proceeds without it — the escape valve
/// on [`Manager::warm_targets`]'s boot gate.
///
/// The gate waits because a blank quota is *unknown*, not *known-cold*, and
/// warming an account whose 5h window is already live is wasted spend. But absence
/// of evidence is not evidence of a live window, and if the probe fails
/// persistently the wait never ends: keep-warm goes silently dark while config and
/// TUI both read as enabled. That is the worse failure — an unbounded wait is a
/// kill switch, a bounded one costs a few minutes.
///
/// Three, from the probe cadence. One failed probe proves nothing: the fleet-wide
/// false error `probing.rs` documents (a bursted sweep 429ing itself) is exactly a
/// one-sweep event, and lifting on it would hand the boot burst straight back.
///
/// **What the three count, since the probe went per-account.** The counter always
/// lived on the account and was always bumped per PROBE by
/// [`Manager::record_probe`]; when the background refresh was a fleet sweep, one
/// sweep was one probe per account, so "three sweeps" and "three of this
/// account's probes" were the same number. Only the first phrasing has stopped
/// being true. The semantics are deliberately unchanged: three consecutive failed
/// probes *of this account*, which is still exactly the "a probe failing once is a
/// hiccup, three times is a condition" test it was chosen for — and it is now
/// genuinely per-account, so one account with a dead credential no longer has its
/// failures interleaved with a fleet-wide notion of a sweep.
///
/// What DID change is the wall clock, and only because the cadence moved. Three
/// failures are three of this account's own randomly drawn intervals, so at the
/// default [`crate::probe::DEFAULT_PROBE_SECONDS`] (300s, drawn `+/-30%`) the
/// bound is 10.5-19.5 minutes rather than the old ~2.5. That is longer but still
/// bounded, still a fraction of any sane `warmupSeconds` (3600s is the usual
/// value), and it is only ever paid when the probe is genuinely broken — a
/// working probe latches `quota_known` on its first success and never reaches
/// here at all.
const PROBE_FAILURES_BEFORE_WARMING_UNPROBED: u32 = 3;

/// Bound on consecutive keep-warm requests that succeeded (200) but carried NONE
/// of the `anthropic-ratelimit-unified-5h-*` headers — a response that latches no
/// evidence (see [`Manager::update_quota`]'s doc-comment: "a response WITHOUT the
/// header latches nothing"). Left unbounded, such an account would stay a
/// [`Manager::warm_targets`] member forever — its 5h window never gets folded, so
/// `live_reset` never fires — and get re-warmed every cadence at real upstream
/// cost for nothing.
///
/// Mirrors [`PROBE_FAILURES_BEFORE_WARMING_UNPROBED`] in shape (three consecutive
/// misses is a hiccup, not a verdict) but bounds the opposite direction: that one
/// bounds a WAIT before warming starts; this one bounds a REPEAT once warming
/// itself stops producing evidence.
///
/// Crossing this excludes the account from [`Manager::warm_targets`] — but that
/// exclusion must itself be recoverable without a restart, or a transient
/// upstream condition that strips the 5h headers for a few minutes sidelines the
/// account for the life of the process. [`AccountRuntime::warm_evidence_retry_after_ms`]
/// is that recovery: a flat cooldown, not a per-response reset — an EARLIER
/// version of this fix reset the counter on every successful background probe,
/// which reopened exactly the loop this constant bounds (`probeable_indices`
/// probes an excluded account regardless of `warm_targets` membership, so a
/// probe-triggered reset re-armed a full burst of [`WARM_ATTEMPTS_WITHOUT_EVIDENCE_LIMIT`]
/// warms every probe cycle — worse than the original unbounded-warm bug).
const WARM_ATTEMPTS_WITHOUT_EVIDENCE_LIMIT: u32 = 3;

/// Flat cooldown before a keep-warm-excluded account (see
/// [`WARM_ATTEMPTS_WITHOUT_EVIDENCE_LIMIT`]) is retried. An hour: two orders of
/// magnitude cheaper than retrying every probe cycle (probe cadence defaults to
/// minutes, not hours — see [`crate::probe::DEFAULT_PROBE_SECONDS`]) while still
/// recovering well inside a restart's cost. Deliberately a flat wait, not
/// exponential backoff like [`AccountRuntime::error_backoff_ms`]: a header-less
/// warm response is not evidence the account is unhealthy (unlike a rejected
/// refresh token), so there is no reason to believe the NEXT retry is less
/// likely to work than this one, and a flat interval keeps the recovery time
/// bound simple to state.
const WARM_EVIDENCE_RETRY_COOLDOWN_MS: i64 = 3_600_000;

/// Decay window for [`AccountRuntime::stream_error_times_ms`] — how far back a
/// stream error still counts toward the operator-facing decayed count. An hour:
/// long enough to show a persistent pattern, short enough that a stale blip from
/// hours ago does not linger in the TUI forever.
const STREAM_ERROR_WINDOW_MS: i64 = 3_600_000;

/// Hard cap on [`AccountRuntime::stream_error_times_ms`] so a pathological
/// account (erroring on every request, indefinitely) cannot grow the queue
/// without bound even inside the decay window.
const STREAM_ERROR_CAP: usize = 64;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnifiedRejectionKind {
    Overall,
    FiveHour,
    SevenDay,
    FableWeekly,
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
    /// Mirrors [`config::Account::groups`], normalized to an owned `Vec` (empty
    /// when the config carried no `groups` key) so [`Manager::eligible`] can test
    /// membership without an `Option` at every call site.
    pub groups: Vec<String>,
    pub switch_threshold: Option<f64>,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at_ms: Option<i64>,
    pub status: AccountStatus,
    pub quota: Quota,
    /// Latched `true` the first time this account's 5h window was actually READ —
    /// "we have evidence about this account's window", which is the question
    /// [`Manager::warm_targets`]' boot gate is really asking. Never cleared: once
    /// we have read it we have read it, and a later probe FAILURE deliberately
    /// leaves the last-learned windows in place rather than blanking them.
    ///
    /// TWO sources latch it, because the evidence genuinely has two sources:
    ///  - [`Manager::apply_usage`], on a SUCCESSFUL probe of the usage endpoint —
    ///    including one that reports no 5h bucket at all, which is itself the
    ///    positive fact "this account has no live window";
    ///  - [`Manager::update_quota`], when a served response's rate-limit headers
    ///    carry the unified 5h window. Those headers are first-hand evidence about
    ///    that account, so an account carrying live traffic must not read as
    ///    "quota unknown" — it plainly is not.
    ///
    /// A response with NO 5h header latches nothing: that is silence, not the
    /// endpoint telling us there is no window.
    ///
    /// This, and NOT `probe_status`, is the gate. `record_probe` stamps
    /// `Error`/`Timeout`/`RateLimited` on a FAILED probe too, so
    /// `probe_status != Never` while `quota` is still `Quota::default()` — and a
    /// gate keyed on that lifts on blank quota, which is the boot burst coming
    /// straight back (a real fleet-wide false-error sweep is documented in
    /// `probing.rs`). It is equally NOT `quota.five_hour.is_some()`: `apply_bucket`
    /// early-returns when the endpoint omits the bucket, so an account whose
    /// responses never carry a 5h bucket would become permanently warm-INELIGIBLE —
    /// a dark feature that reads as enabled.
    pub quota_known: bool,
    /// Probes of THIS account that have failed CONSECUTIVELY without reading its
    /// quota. Bumped by [`Manager::record_probe`] on every terminal failure and
    /// reset to `0` by a success.
    ///
    /// It exists to bound the wait: `quota_known` alone can never latch when the
    /// probe fails forever, so gating on it unconditionally makes keep-warm
    /// structurally dark. See [`PROBE_FAILURES_BEFORE_WARMING_UNPROBED`].
    pub consecutive_probe_failures: u32,
    /// Consecutive keep-warm requests ([`Manager::warm_account`]) that succeeded
    /// but carried none of the unified 5h rate-limit headers — a 200 that latched
    /// no evidence. Reset to `0` only by a response (warm or served) whose
    /// headers DO carry the 5h window — see [`Manager::update_quota`].
    /// Deliberately NOT reset by a successful background probe
    /// ([`Manager::apply_usage`]): see that function's doc-comment for why a
    /// probe read is not the evidence this counter is bounding. Bounds
    /// [`Manager::warm_targets`]'s otherwise-unbounded repeat; see
    /// [`WARM_ATTEMPTS_WITHOUT_EVIDENCE_LIMIT`]. The exclusion this drives is
    /// recovered by [`Self::warm_evidence_retry_after_ms`] instead, once its
    /// cooldown elapses.
    pub consecutive_warms_without_evidence: u32,
    /// Set by [`Manager::record_warm_without_evidence`] once
    /// `consecutive_warms_without_evidence` reaches
    /// [`WARM_ATTEMPTS_WITHOUT_EVIDENCE_LIMIT`]: the wall-clock instant (epoch
    /// ms) at or after which [`Manager::warm_targets`] treats this account as
    /// eligible again despite the counter still being at or over the limit —
    /// one bounded retry per [`WARM_EVIDENCE_RETRY_COOLDOWN_MS`], not one per
    /// probe cycle. `None` when the account is not currently excluded, or once
    /// a header-bearing response clears it (see [`Manager::update_quota`]).
    pub warm_evidence_retry_after_ms: Option<i64>,
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
    pub overall_rejected_until_ms: Option<i64>,
    pub five_hour_rejected_until_ms: Option<i64>,
    pub seven_day_rejected_until_ms: Option<i64>,
    pub fable_rejected_until_ms: Option<i64>,
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
    /// The next permitted usage probe time, from an upstream `retry-after` header.
    pub probe_retry_after_ms: Option<i64>,
    /// Wall-clock ms of each stream failure observed on a stream this account
    /// served (see [`Manager::record_stream_error`]): an in-band SSE `error`
    /// event, or a stream that hit EOF without Anthropic's `message_stop`
    /// terminator — recorded with the fixed kind `"truncated"`, which covers a
    /// mid-stream transport death and the malformed/utf8 break alike. An
    /// `error` event takes precedence: EOF without `message_stop` is only
    /// counted as `"truncated"` when no more specific `error` was already
    /// recorded. Pruned to [`STREAM_ERROR_WINDOW_MS`] and hard-capped at
    /// [`STREAM_ERROR_CAP`] entries on BOTH insert and read, so it cannot grow
    /// unbounded. OBSERVABILITY ONLY — nothing in `select.rs` reads this
    /// field; see that module's gates for why.
    pub stream_error_times_ms: VecDeque<i64>,
    /// The most recent stream failure's kind: an Anthropic `error.type` (e.g.
    /// `"overloaded_error"`) from an in-band SSE `error` event, or the fixed
    /// string `"truncated"` when the stream hit EOF without `message_stop` and
    /// no more specific `error` was already recorded, alongside the decayed
    /// count above.
    pub last_stream_error: Option<String>,
    /// Coalescing gate for THIS account's OAuth refresh. Lives on the account rather
    /// than in a parallel Vec indexed by position, so an account added after startup
    /// cannot be missing one — the desync it replaces made `ensure_fresh` a silent
    /// no-op (no log, no status change) until the initial token expired.
    pub refresh_lock: Arc<AsyncMutex<()>>,
    /// This account's OWN upstream-forwarding client — never shared with any
    /// other account. `hyper-util`'s pool keys a connection on `(scheme,
    /// authority)` alone (`PoolKey`, `client/legacy/client.rs`), with nothing in
    /// that key touching the Bearer token or account identity — so a single
    /// `reqwest::Client` reused across the fleet collapses every account's
    /// traffic onto ONE pooled connection, and that connection's death (an h2
    /// `PROTOCOL_ERROR` reset, observed live spanning 2-3 distinct accounts at
    /// once) takes every account down with it, defeating rotation and failover
    /// at the connection layer no matter how healthy any individual account is.
    /// Built once per account by [`build_serving_client`] (never rebuilt on
    /// every request — that would throw away the warm pool this exists to
    /// keep) and wrapped in `Arc` so [`Manager::http_client`] can hand out
    /// cheap clones while still letting a test prove two accounts' clients are
    /// genuinely distinct instances via `Arc::ptr_eq`.
    ///
    /// REBUILT periodically, though — see [`MAX_SERVES_PER_CONNECTION`] and
    /// [`Manager::recycle_client`]. "Never on every request" is not
    /// "never": an h2 connection held open indefinitely ages into the reset
    /// zone this field's own history describes.
    pub http: Arc<reqwest::Client>,
    /// Upstream sends dispatched on the CURRENT [`Self::http`] since it was
    /// built, and the trigger for retiring it (see
    /// [`MAX_SERVES_PER_CONNECTION`]). Reset to 0 by the recycle, so it means
    /// "age of this client in serves", never a lifetime total —
    /// [`Self::requests`] is the lifetime counter and is deliberately NOT
    /// reused here: it is a display statistic incremented on a different path
    /// (`snapshot.rs`), and hanging connection lifetime off it would couple a
    /// transport decision to a stats counter that is free to change meaning.
    pub serves_since_client_build: u32,
}

// Test-only fault injection for `build_serving_client`: set via
// `fail_next_client_build`, consumed (and cleared) by the next call on this
// thread. Exists to prove — with a real panic, not a hypothetical one — two
// properties of `Manager::add_or_update_account`: its Added path builds the
// runtime with NEITHER `self.accounts` nor `config_write` held, so a panic
// here poisons neither lock — every other `.expect(...)` call site on either
// one in this module would otherwise go down with it — and its Updated path
// never reaches this function at all. See
// `add_or_update_account_added_path_does_not_poison_either_lock_on_a_client_build_panic`
// and `add_or_update_account_updated_path_never_builds_a_client`.
#[cfg(test)]
thread_local! {
    static FAIL_NEXT_CLIENT_BUILD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Arm [`FAIL_NEXT_CLIENT_BUILD`] so the next call to `build_serving_client` on
/// this thread panics instead of building. Test-only.
#[cfg(test)]
pub(crate) fn fail_next_client_build() {
    FAIL_NEXT_CLIENT_BUILD.with(|f| f.set(true));
}

/// Upstream sends after which an account's HTTP client is retired and rebuilt,
/// dropping its pooled h2 connection so the next send opens a fresh one.
///
/// Anthropic's edge eventually drains a long-lived h2 connection, and the
/// settings in [`build_serving_client`] — `http2_keep_alive_while_idle` plus a
/// 300s pool idle timeout — are deliberately chosen to keep ONE connection warm
/// forever, so nothing retired it. Measured over one day's live log (2026-08-27,
/// n=2136 `Reset(_, PROTOCOL_ERROR, Remote)` transport failures), the resets are
/// concentrated by connection age, not by concurrency:
///
/// | stream id at reset | share |
/// |---|---|
/// | 1-99 (fresh connection) | 6.8% |
/// | 100-999 | 21.5% |
/// | 1000-4999 | 5.9% |
/// | 5000-9999 | 21.9% |
/// | 10000+ | 43.9% |
///
/// Median 9699, max 17449 — roughly 8700 requests multiplexed onto one socket.
/// h2 client-initiated stream ids advance by 2, so N serves reaches stream id
/// ~2N: 500 keeps a connection under ~1000, below the p25 of the observed reset
/// distribution, while still amortising the handshake over 500 requests.
///
/// Retiring a connection is CHEAP and must not be confused with rotating an
/// account. It costs one TCP+TLS handshake (~100-300ms, the cost
/// [`build_serving_client`]'s keep-alive settings exist to avoid); it does NOT
/// cost a prompt-cache miss, because Anthropic's prompt cache is keyed on the
/// account and prefix server-side and has nothing to do with which socket the
/// bytes arrived on. Account rotation is the expensive one (measured 5-10x the
/// quota of a warm serve), and it is exactly what this constant exists to make
/// rarer.
const MAX_SERVES_PER_CONNECTION: u32 = 500;

/// Build one upstream-forwarding HTTP client. Called once per account (see
/// [`AccountRuntime::http`]) so every account's client is configured
/// identically and differs from every other account's only in the connection
/// pool it privately owns.
///
/// `http1_only` mirrors [`config::Config::http1_only`] — see its doc-comment
/// for why this exists and why it defaults to `false`. When `true`, every
/// stream this client opens is capped at HTTP/1.1, so the
/// `http2_keep_alive_*` settings below become inert (h1 has no PING frame to
/// send); they are left in place because they are still correct for the
/// default h2 path.
pub(crate) fn build_serving_client(http1_only: bool) -> Arc<reqwest::Client> {
    #[cfg(test)]
    if FAIL_NEXT_CLIENT_BUILD.with(|f| f.replace(false)) {
        panic!("build reqwest client (test-injected failure)");
    }
    // no_proxy(): reqwest honors HTTPS_PROXY/HTTP_PROXY by default. We ARE the
    // proxy — routing our upstream through an ambient proxy (e.g. the JS
    // teamclaude on :3456) loops us through the thing we replace and every
    // request dies as "upstream unreachable". Always reach Anthropic directly.
    let mut builder = reqwest::Client::builder()
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
        .pool_idle_timeout(std::time::Duration::from_secs(300));
    if http1_only {
        // Structural blast-radius cap: h1 does not multiplex, so a
        // connection-level fault (GOAWAY / framing error) kills at most the
        // one request on that connection instead of every stream sharing it.
        builder = builder.http1_only();
    }
    Arc::new(builder.build().expect("build reqwest client"))
}

/// A rotation slot answers a user's account query by the same three fields a
/// config record does, so [`Manager::set_disabled_by_query`] can run the CLI's
/// own resolution rule over the LIVE fleet.
impl crate::identity::Queryable for AccountRuntime {
    fn query_name(&self) -> &str {
        &self.name
    }
    fn query_org_name(&self) -> Option<&str> {
        self.org_name.as_deref()
    }
    fn query_org_uuid(&self) -> Option<&str> {
        self.org_uuid.as_deref()
    }
}

/// What [`Manager::set_disabled_by_query`] did.
///
/// `Applied` carries the RESOLVED name plus the durable half's fate, which the
/// caller must surface — see [`DisablePersist::warning`].
///
/// The name is resolved, not echoed, because the query may have been the account's
/// bare EMAIL where its stored name carries an org suffix (`me@example.com
/// (Acme)`) — so the answer has to say what was actually parked. It is NOT a
/// substring or case-insensitive match: [`crate::identity::match_accounts`] tries
/// exact name, then exact email, byte-for-byte and untrimmed. Do not "fix"
/// resolution to match a looser description; a widened rule is how a query
/// silently parks an account nobody named, which is exactly what the `Ambiguous`
/// arm below exists to refuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetDisabledOutcome {
    Applied {
        name: String,
        persist: DisablePersist,
    },
    /// The query named no account in the live rotation.
    NoMatch,
    /// The query named more than one, listed here so the caller can tell the user
    /// what to pass `--org` for.
    Ambiguous(Vec<String>),
}

/// What [`Manager::add_or_update_account`] did.
///
/// Same shape as [`SetDisabledOutcome`] — resolve by identity against the LIVE
/// rotation, then report what happened plus the durable half's fate — but with
/// one difference `SetDisabledOutcome` never needs: a disable/enable query only
/// ever names an account that must already exist, so a miss is failure
/// (`NoMatch`). An account-add legitimately names one nobody has seen before, so
/// `Match::None` here is the SUCCESS arm (`Added`), not a rejection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddAccountOutcome {
    /// No account in the live rotation carried this identity — appended at the
    /// END via [`Manager::add_account`]. Pins are in-memory `(session_key,
    /// account_index)`, so appending is the only insertion that cannot re-key a
    /// live session onto the wrong account. An account with no explicit
    /// `priority` is assigned `max(existing priorities) + 1` — deliberate,
    /// not incidental: it joins the back of the fleet rather than reading as
    /// 0 (the primary tier) — see the doc-comment on the Added arm inside
    /// [`Manager::add_or_update_account`].
    Added {
        idx: usize,
        name: String,
        persist: AddPersist,
    },
    /// Exactly one account already carried this identity — its credentials were
    /// replaced IN PLACE at `idx` (see [`Manager::add_or_update_account`]). Its
    /// `account_type` is always stamped to the submitted value and any identity
    /// field (`account_uuid`/`org_uuid`/`org_name`) it was missing is backfilled
    /// — never a value it already carried. Routing state — priority, disabled
    /// flag, switch threshold, learned quota, counters — is untouched, and
    /// `idx` never moves.
    Updated {
        idx: usize,
        name: String,
        persist: AddPersist,
    },
    /// The submitted identity matches more than one account in the live
    /// rotation; these are their names, so the caller can tell the operator
    /// what to disambiguate with. Never guessed.
    Ambiguous(Vec<String>),
}

impl AccountRuntime {
    fn from_config(account: &config::Account, http1_only: bool) -> Self {
        Self {
            name: account.name.clone(),
            account_type: account.account_type.clone(),
            account_uuid: account.account_uuid.clone(),
            org_uuid: account.org_uuid.clone(),
            org_name: account.org_name.clone(),
            priority: account.priority.unwrap_or(0),
            disabled: account.disabled.unwrap_or(false),
            groups: account.groups.clone().unwrap_or_default(),
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
            consecutive_probe_failures: 0,
            consecutive_warms_without_evidence: 0,
            warm_evidence_retry_after_ms: None,
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
            overall_rejected_until_ms: None,
            five_hour_rejected_until_ms: None,
            seven_day_rejected_until_ms: None,
            fable_rejected_until_ms: None,
            refresh_retry_after_ms: None,
            error_retry_after_ms: None,
            error_backoff_ms: 0,
            probe_status: ProbeStatus::Never,
            last_probe_ms: None,
            probe_error: None,
            probe_retry_after_ms: None,
            stream_error_times_ms: VecDeque::new(),
            last_stream_error: None,
            refresh_lock: Arc::new(AsyncMutex::new(())),
            http: build_serving_client(http1_only),
            serves_since_client_build: 0,
        }
    }
}

/// What [`Manager::set_disabled`] achieved about DURABILITY — whether the bench
/// will still be there after a restart.
///
/// Returned rather than only logged because `tracing` is redirected to a log file
/// in TUI mode, so a failed write was invisible to the one person who could act on
/// it: the TUI kept rendering the account as disabled while the flag had provably
/// not reached disk. Every non-`None` [`Self::warning`] is a state where pressing
/// `d` looked like it worked and did not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisablePersist {
    /// The config file carries the flag — written just now, or already correct.
    Persisted,
    /// No config file behind this manager (`tcr demo`, `tcr status --probe`, the
    /// tests). Memory-only BY DESIGN: nothing to persist to, nothing wrong.
    NoConfigFile,
    /// The index named no account, so nothing changed in memory either.
    NoSuchAccount,
    /// Nothing on disk carries this account's identity — the entry was deleted or
    /// renamed while the proxy ran.
    NoEntry,
    /// More than one entry carries this identity; the write was refused rather
    /// than landed on a guess. See [`config::DisabledWrite::Ambiguous`].
    Ambiguous,
    /// The write itself failed (unreadable, malformed, or unwritable file).
    WriteFailed,
}

impl DisablePersist {
    /// The one line to put in front of the user, or `None` when the outcome needs
    /// no explanation. Short enough for a single terminal row and phrased so the
    /// headline (`NOT SAVED`) survives truncation on a narrow pane.
    ///
    /// `disabled` is the DIRECTION the keypress asked for, and it is required
    /// because the consequence of a failed write is its exact opposite in the two
    /// directions. Nothing was written either way, so the file still says whatever
    /// it said: after a failed **disable** the account comes back IN ROTATION,
    /// after a failed **enable** it comes back BENCHED. A single direction-blind
    /// line told the user "it returns to rotation on restart" for both — so on a
    /// failed enable they restarted on that advice and got the opposite.
    pub fn warning(self, disabled: bool) -> Option<&'static str> {
        match self {
            // The flag is durable, or was never meant to be (demo/status have no
            // config file at all) — nothing to say.
            Self::Persisted | Self::NoConfigFile => None,
            Self::NoSuchAccount => Some("NOT SAVED: that account row no longer exists"),
            Self::NoEntry => Some(if disabled {
                "NOT SAVED: no config entry matches this account — it returns to rotation on restart"
            } else {
                "NOT SAVED: no config entry matches this account — it stays benched after a restart"
            }),
            // Says nothing about a restart, so it needs no direction — only an
            // action that actually helps.
            Self::Ambiguous => Some(
                "NOT SAVED: two config entries share this account's identity — give each its own orgUuid",
            ),
            Self::WriteFailed => Some(if disabled {
                "NOT SAVED: writing the config failed — it returns to rotation on restart"
            } else {
                "NOT SAVED: writing the config failed — it stays benched after a restart"
            }),
        }
    }
}

/// What [`Manager::set_control_by_query`] did. Same shape as
/// [`SetDisabledOutcome`], with one difference: a `None` query is a legitimate
/// request (clear the control account), not a missing argument, so `Applied`'s
/// `name` is itself an `Option` rather than `Applied` only ever meaning "matched
/// something".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetControlOutcome {
    /// `name` is the resolved account name, or `None` when this call cleared
    /// the control account.
    Applied {
        name: Option<String>,
        persist: ControlPersist,
    },
    /// The query named no account in the live rotation.
    NoMatch,
    /// The query named more than one, listed so the caller can tell the user
    /// what to pass `--org` for.
    Ambiguous(Vec<String>),
}

/// What [`Manager::set_control`] achieved about DURABILITY — whether the
/// control account will still be set (or cleared) after a restart. Simpler
/// than [`DisablePersist`]: `controlAccount` is a single top-level key
/// resolved by NAME, not a per-account flag located by identity, so there is
/// no `NoEntry`/`Ambiguous` arm on the durable side — see
/// [`config::ControlWrite`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlPersist {
    /// The config file carries the control account (or its absence) — written
    /// just now, or already correct.
    Persisted,
    /// No config file behind this manager (`tcr demo`, `tcr status --probe`,
    /// tests). Memory-only BY DESIGN.
    NoConfigFile,
    /// The write itself failed (unreadable, malformed, or unwritable file).
    WriteFailed,
}

impl ControlPersist {
    /// The one line to put in front of the user, or `None` when the outcome
    /// needs no explanation. Mirrors [`DisablePersist::warning`]; there is no
    /// direction-dependent wording here because clearing and setting fail the
    /// same way — a failed write just leaves the file saying whatever it said.
    pub fn warning(self) -> Option<&'static str> {
        match self {
            Self::Persisted | Self::NoConfigFile => None,
            Self::WriteFailed => {
                Some("NOT SAVED: writing the config failed — this will not survive a restart")
            }
        }
    }
}

/// What [`Manager::add_or_update_account`] achieved about DURABILITY. Same role
/// as [`DisablePersist`], deliberately simpler: an upsert either lands or it does
/// not, and there is no "no such account" / "no config entry" arm here — a MISS
/// on the durable side is FILLED IN by [`config::save_account`], never refused
/// (see that function's doc-comment for why it differs from
/// [`config::save_disabled`] on exactly this point).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddPersist {
    /// The config file carries the new or replaced credentials — written just
    /// now.
    Persisted,
    /// No config file behind this manager (`tcr demo`, `tcr status --probe`,
    /// tests). Memory-only BY DESIGN.
    NoConfigFile,
    /// More than one entry on disk carries this identity; the write was refused
    /// rather than landed on a guess. See [`config::AccountWrite::Ambiguous`].
    Ambiguous,
    /// The write itself failed: unreadable, malformed or unwritable file, or a
    /// document whose `accounts` key exists but is not a JSON array.
    WriteFailed,
}

impl AddPersist {
    /// The one line to put in front of the operator, or `None` when the outcome
    /// needs no explanation.
    pub fn warning(self) -> Option<&'static str> {
        match self {
            Self::Persisted | Self::NoConfigFile => None,
            Self::Ambiguous => Some(
                "NOT SAVED: another config entry shares this account's identity — give it its own orgUuid",
            ),
            Self::WriteFailed => {
                Some("NOT SAVED: writing the config failed — this will not survive a restart")
            }
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
    /// (x-api-key / `metadata.user_id`); [`SessionKind::Prefix`] when there was none
    /// but a `system`/`tools` hash pinned it anyway; [`SessionKind::Fallback`] when
    /// there was neither and the request served unpinned. DISPLAY provenance
    /// only — never routing.
    kind: SessionKind,
}

/// One session's divert fan-out inside ONE hold episode. Keyed on the EPISODE,
/// not on wall-clock: a session that diverts three times across three separate
/// holds hours apart is healthy and must start each one with a full budget.
/// See the divert-budget design notes §4.1.
/// `pub` (not `pub(super)`) and every field `pub`, deliberately: unit F's
/// replay harness (the divert-budget design notes §6) reconstructs episodes
/// from retained logs and must be able to build a `DivertEpisode` directly to
/// feed [`select::divert_verdict`], the SAME predicate production calls —
/// "not a reimplementation" is the whole point of the gate. See that
/// function's re-export at the bottom of this file for the exact import path.
#[derive(Debug, Clone, Copy)]
pub struct DivertEpisode {
    /// The pinned account the episode is about.
    pub pin: usize,
    /// The pin's `rate_limited_until_ms` at the time of the first divert. This
    /// value IS the episode identity — every divert inside one hold observes the
    /// same deadline, and a new hold necessarily carries a different one. A
    /// mismatch against the live value on a later divert (different pin, or the
    /// same pin with a different `until_ms`) means the ledger entry is stale and
    /// is overwritten from scratch — no timer, no sweeper.
    ///
    /// **Considered and accepted: two genuinely distinct holds on the same pin
    /// CAN collide on this value** (a second 429 arriving with the identical
    /// millisecond deadline as the first, or two holds both clamped to the
    /// same `MAX_RATE_LIMIT_HOLD_SECONDS` ceiling). When that happens the mask
    /// does NOT reset — the two holds are treated as one episode. This is
    /// deliberately left as-is rather than defended against: a same-deadline
    /// collision means the session is still inside a hold window with the
    /// identical clear-instant in every way that matters to the sticky
    /// policy, so reusing the prior destination is still the right call, not
    /// a bug. No counter or nonce was added to disambiguate same-`until_ms`
    /// holds — that would widen a persisted-adjacent structure to defend
    /// against a hypothetical with no observed real case.
    pub until_ms: i64,
    /// Bitmask of destination account indices already served for this session in
    /// this episode. `count_ones()` is the distinct-destination count; capped at
    /// 64 accounts, which the fleet is nowhere near.
    pub destinations: u64,
    /// The first destination, so the sticky overlay has a single preferred index
    /// without scanning the mask.
    pub sticky: usize,
}

/// Owns all rotation state and the machinery to refresh tokens and reach upstream.
pub struct Manager {
    accounts: RwLock<Vec<AccountRuntime>>,
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
    /// The persisted config, kept so token refreshes can be written back with
    /// every unmodelled field intact.
    config: Mutex<Config>,
    /// Serializes the whole read-modify-write of the CONFIG FILE, and nothing else
    /// (**INV1**). Every writer — [`Self::persist_now`], [`Self::persist_tokens`],
    /// [`Self::persist_disabled`] — holds this across its whole read, modify,
    /// `sync_all` and rename, so two persists can never interleave and a stale
    /// document can never clobber a just-rotated single-use refresh token.
    ///
    /// It exists so `self.config` no longer has to do that job (**INV2**). The
    /// config mutex is taken on the PER-CONNECTION path —
    /// [`Self::session_affinity_enabled`], from `mitm.rs`'s serve path — so holding
    /// it across an fsync let one TUI keypress stall connection setup. Splitting the
    /// two gives serialization without putting file I/O under a lock that request
    /// handling waits on.
    ///
    /// Lock order is `config_write` → `config`, never the reverse: nothing that
    /// holds `config` ever reaches for this. A `()` payload — it guards an external
    /// resource (the file), not data.
    config_write: Mutex<()>,
    config_path: Option<PathBuf>,
    upstream: String,
    proxy_api_key: Option<String>,
    global_threshold: f64,
    /// The set of group labels marked `reserved` (`tcr group reserve`).
    /// Seeded from the config at construction and **hot-reloaded** from the
    /// config file's `groupSettings` thereafter — see
    /// [`Self::reload_groups_if_changed`] for when and how. `RwLock` (not a
    /// plain field) precisely because it is no longer construction-only: a
    /// reader takes a short read lock and clones ([`Self::reserved_groups`]),
    /// never held across the accounts/affinity locks. See
    /// [`Manager::eligible`]'s `reserved_group` doc for the rule this drives.
    reserved_groups: RwLock<HashSet<String>>,
    /// The set of group labels opted in to `allowControlAccount` (`tcr group
    /// allow-control`). Seeded from the config at construction and
    /// **hot-reloaded** thereafter, same cadence and same `RwLock` reasoning
    /// as [`Self::reserved_groups`] — see
    /// [`Self::reload_groups_if_changed`]. See `select.rs`'s
    /// `control_excluded` handling for the rule this drives.
    control_allowed_groups: RwLock<HashSet<String>>,
    /// Every group on the fleet mapped to its resolved color
    /// (`config::Config::group_colors`). Seeded at construction and
    /// hot-reloaded alongside [`Self::reserved_groups`] — same
    /// [`Self::reload_groups_if_changed`] cadence, same `RwLock` reasoning.
    /// `BTreeMap` for deterministic wire order, matching
    /// [`config::Config::group_colors`]'s own return type.
    group_colors: RwLock<BTreeMap<String, String>>,
    /// The config file's mtime as of the last successful (or unmodified-since)
    /// [`Self::reload_groups_if_changed`] check. `None` before the first check.
    /// Guarded by `config_write` at every access — see that lock's doc and
    /// `reload_groups_if_changed`'s for why: this and the file read it gates
    /// must move together, or two racing reload calls could both see a stale
    /// `None`/old value and redundantly re-parse (harmless) or, worse, one
    /// could advance it past a write the other hasn't applied yet.
    groups_reload_mtime: Mutex<Option<std::time::SystemTime>>,
    /// Per-account request pacing knobs, snapshotted from the config at
    /// construction. Default (all `None`) → inert → selection is byte-identical to
    /// the no-pacing build. See [`config::PacingConfig`].
    pacing: PacingConfig,
    /// Per-ORGANIZATION outbound throttle knobs, snapshotted from config at
    /// construction (all-`None` → inert). This is the primary limiter: Anthropic
    /// sets limits per organization, so this is the bucket that matches the thing
    /// actually being limited.
    account_throttle: ThrottleConfig,
    /// Fleet-wide ceiling knobs, snapshotted from config at construction
    /// (all-`None` → inert). Insurance against a shared-identity limit nobody has
    /// measured; set far looser than [`Self::account_throttle`] so it does not bind
    /// in normal use.
    fleet_throttle: ThrottleConfig,
    /// Per-account usage buckets, pricing and the append-only ledger — the one
    /// place a served request's tokens are aggregated for display. Present on
    /// every `Manager`, but only the SERVING process attaches a ledger to it
    /// (`Self::attach_usage_ledger`), so an offline manager's tracker is empty
    /// and its rows read as "not measured" rather than as zero traffic.
    ///
    /// Lock order: this is a **leaf** lock. Nothing taken while it is held, and
    /// it is always innermost — `Self::record_usage` takes the accounts lock,
    /// drops it, and only then records here; `Self::snapshot` holds the accounts
    /// READ lock across its `usage_row` calls, which is safe precisely because
    /// no path ever runs the other way round.
    usage: crate::usage::UsageTracker,
    /// Resolved hard-lock target (index of the account named by `config.lockAccount`),
    /// or `None` when unlocked / the name did not match. When `Some(i)`, [`Self::select`]
    /// returns `i` unconditionally (bypassing rotation/affinity/migration) — no failover.
    locked_idx: Option<usize>,
    /// The resolved identity-bound control account (index of the account named
    /// by `config.controlAccount`), or `None` when unset / the name did not
    /// match. Deliberately **not** `locked_idx`'s immutable `Option<usize>` —
    /// this one is set at runtime via `tcr control` / `POST
    /// /_tcr/accounts/control`, mirroring `set_disabled_by_query`, so it needs
    /// interior mutability. Read by [`Self::probeable_indices`] to keep usage
    /// tracking on this account even while it stays `disabled` (out of the
    /// inference rotation on purpose — see the module's `probing.rs` doc).
    /// Selection itself does NOT consult this field (part 1's invariant);
    /// routing is added on top in part 2.
    ///
    /// Lock order: taken ALONE, never nested under `accounts` or `affinity` —
    /// same discipline as `select.rs:253-256`.
    control_idx: RwLock<Option<usize>>,
    /// GCRA theoretical-arrival-times (epoch ms) for the PER-ORGANIZATION throttle,
    /// keyed by [`Self::throttle_bucket_key`].
    ///
    /// One mutex over the whole map rather than a mutex per key: the guard is held
    /// only for an O(1) lookup-and-update and never across the sleep, so contention
    /// is identical to the single scalar this replaced. A map (not a `Vec` indexed
    /// by account) because the key is an ORG, and several accounts may share one.
    ///
    /// Entries are created on demand, so an account added at runtime needs no
    /// coordination at the `accounts.push` sites.
    ///
    /// Lock order: taken ALONE. [`Self::throttle_send`] releases this guard before
    /// touching [`Self::fleet_tat_ms`] — the two are never held together.
    org_tat_ms: AsyncMutex<HashMap<String, i64>>,
    /// GCRA theoretical-arrival-time (epoch ms) for the fleet-wide ceiling. Same
    /// discipline as [`Self::org_tat_ms`]: held only for the O(1) update, released
    /// before any sleep.
    fleet_tat_ms: AsyncMutex<i64>,
    log: Mutex<VecDeque<RequestLogEntry>>,
    current: Mutex<Option<usize>>,
    /// Monotonic counter handed out one tick at a time by [`Manager::select`] to
    /// stamp the account it picks, so the next select prefers a different one
    /// (load spread). Starts at 1 so 0 reads unambiguously as "never selected".
    select_seq: AtomicU64,
    /// Session affinity (default ON, opt-out via `"sessionAffinity": false`): a
    /// stable identity hash → `(account index it is pinned to, last-touch ms)`.
    /// Populated only when `sessionAffinity` is enabled and a `SessionKey`
    /// extension flows in; empty (and never consulted) otherwise, so the disabled
    /// path is provably inert. Bounded by a size cap
    /// (`AFFINITY_CAP`) + LRU-by-last-touch eviction in [`Manager::select`] — stable
    /// pins intentionally SURVIVE reconnects (that is the point of a stable key), so
    /// there is no disconnect-release. Kept a plain `std::sync::Mutex` and **never**
    /// held while the accounts lock is taken (read the pin, drop this lock, then do
    /// eligibility) so the two can never deadlock.
    affinity: Mutex<HashMap<u64, (usize, i64)>>,
    /// Session keys whose MOST RECENT request carried Anthropic's extended
    /// `"cache_control": {"ttl": "1h"}` (see [`crate::cache_ttl`]), so
    /// [`crate::manager::pins::affinity_pin_snapshot`] knows which persisted
    /// pins get [`crate::affinity::EXTENDED_PIN_TTL_MS`] instead of the
    /// default at the next restart. Deliberately a side map rather than a third
    /// tuple field on `affinity` itself: nothing on the selection path
    /// (`select.rs`) needs to read it, only the snapshot/restore bridge does, so
    /// widening the hot map's value type for every read site there would be
    /// blast radius this doesn't need. Written directly from the request path
    /// (`crate::proxy`, alongside where `session_key` is computed) rather than
    /// through `select()`, guarded and pruned the same way `affinity` is —
    /// never held while the accounts lock is taken.
    affinity_extended: Mutex<HashSet<u64>>,
    /// Set whenever `affinity` is mutated, cleared by the flusher task that
    /// writes the map to disk (see [`Manager::mark_affinity_dirty`] and
    /// [`crate::affinity`]). A relaxed atomic rather than a channel because the
    /// setter sits on the request path and the reader is a 5-second timer: the
    /// only ordering that matters is "a change eventually causes a write", and a
    /// tick that observes the flag late simply writes on the next one.
    affinity_dirty: AtomicBool,
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
    /// Connection-scoped affinity for NOISE traffic (`/api/event_logging`,
    /// `/mcp-registry` — see [`select::classify_request`]): connection key
    /// ([`crate::proxy::SessionKey`]'s wrapped `u64`, minted per-connection by
    /// [`Manager::next_session_key`]) → `(account index, last-touch ms)`.
    ///
    /// Deliberately **separate** from [`Self::affinity`] and **never** written to
    /// `session-affinity.json` — the whole reason [`crate::proxy::SessionKey`]'s
    /// own doc-comment objects to using it as a routing key ("a pin keyed on it
    /// dies with the connection and leaves a ghost entry") does not apply here:
    /// that objection is about the PERSISTED map surviving past the connection
    /// it was minted for. This map is memory-only, LRU-capped
    /// (`CONN_AFFINITY_CAP`) with a short idle TTL (`CONN_AFFINITY_TTL_MS`), so a
    /// dead connection's entry ages out rather than persisting as a ghost.
    /// Guarded the same way as `affinity` — never held while the accounts lock
    /// is taken.
    conn_affinity: Mutex<HashMap<u64, (usize, i64)>>,
    /// One [`DivertEpisode`] per session currently mid-hold, session key ->
    /// episode. See the divert-budget design notes §4.1/§4.2.
    ///
    /// Guarded the same way as `affinity` and `conn_affinity` — **read before any
    /// accounts lock is taken, written only after the affinity guard has
    /// dropped, never nested inside either.** Not persisted (deliberately: after
    /// a restart the pins are restored but no hold is, so every session
    /// legitimately starts a fresh episode) and bounded the same way `affinity`
    /// is — a size cap + LRU-by-last-touch eviction, keyed on `until_ms` as the
    /// episode's own last-touch (see [`select`]'s `AFFINITY_CAP` eviction for the
    /// pattern this mirrors).
    pub(super) divert_ledger: Mutex<HashMap<u64, DivertEpisode>>,
    /// Per-account quota headroom reserved for the control account (§3, part 2
    /// of the control-account feature) — `switchThreshold`/`global_threshold`
    /// minus this, applied ONLY when a general (non-control-preferred) pick
    /// evaluates the control account as a candidate. See
    /// [`select::effective_threshold`]. Resolved from `config.control_reserve`,
    /// clamped to `[0.0, 0.5]`.
    ///
    /// Consulted only once the control account is a CANDIDATE for a general
    /// pick, which is [`Self::control_pooled`] or a `--group` ask that opted in
    /// via `allowControlAccount`. Otherwise `select_with_group` force-adds the
    /// control index to `tried` and this is never reached — and it is inert in
    /// every case while the control account is `disabled`, because
    /// [`Self::eligible`]'s terminal gate drops it first.
    control_reserve: f64,
    /// Whether the control account takes GENERAL inference traffic, guarded by
    /// [`Self::control_reserve`] alone. Boot-time snapshot of
    /// `config.control_pooled`; default `false`, which preserves the rule that
    /// an inference request never selects the control account.
    ///
    /// Read [`crate::config::Config::control_pooled`] before changing this: the
    /// reserve is a floor over LAGGING headers, so `true` accepts that inference
    /// can burn the control account into `rejected` and cost the identity plane
    /// its anchor until the weekly window resets.
    control_pooled: bool,
    /// Width in MILLISECONDS of the reset-urgency bucket that rotation ranks by
    /// within a priority tier, resolved once from
    /// [`crate::config::Config::reset_urgency_tier_hours`] at construction
    /// (boot-time snapshot — an edit needs a restart). `0` disables the term,
    /// restoring the pre-tier `(priority, last_selected_seq, reset)` ordering.
    ///
    /// Held in ms rather than hours so [`select::Manager::reset_urgency_tier`]
    /// divides in the same unit the resets arrive in, with no repeated
    /// hour-conversion at the top of every selection loop.
    reset_urgency_tier_ms: i64,
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

/// What [`Manager::exhaustion_hint`] learned about an exhausted fleet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExhaustionHint {
    /// Whole seconds until the soonest account frees, at least 1; the `60`
    /// sentinel when `free_at` is `None`.
    pub retry_after: i64,
    /// The instant that account frees, when any account advertises one.
    pub free_at: Option<OffsetDateTime>,
    /// The gate holding that account — what `retry_after` is timed against.
    pub binding: Option<GateReason>,
    /// Every account out of rotation, counted by the gate holding it.
    pub gated: BTreeMap<GateReason, usize>,
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

/// Whether a wall-clock cooldown deadline (epoch ms) has elapsed as of `now_ms`.
/// `None` (no cooldown armed) reads as "not held back by this gate" — the same
/// shape [`Manager::probeable_indices`]'s `error_retry_after_ms` check and
/// [`Manager::warm_targets`]'s `warm_evidence_retry_after_ms` check both need,
/// factored out so the two don't drift on the `>=` vs `>` boundary.
fn cooldown_elapsed(deadline_ms: Option<i64>, now_ms: i64) -> bool {
    deadline_ms.is_some_and(|until| now_ms >= until)
}

/// A sorted `Vec` of `set`'s members, for a deterministic (and readable)
/// before/after in [`Manager::apply_group_reload`]'s change log — `HashSet`'s
/// own iteration order is unspecified and would make the same reload log
/// differently run to run.
fn sorted_groups(set: &HashSet<String>) -> Vec<String> {
    let mut v: Vec<String> = set.iter().cloned().collect();
    v.sort();
    v
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
            .map(|a| AccountRuntime::from_config(a, config.http1_only))
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
        let upstream = config.upstream.clone();
        let proxy_api_key = config.proxy.api_key.clone();
        let global_threshold = config.switch_threshold;
        let reserved_groups = config.reserved_group_names();
        let control_allowed_groups = config.control_allowed_group_names();
        // Snapshotted at construction, same restart-to-take-effect contract as
        // `reserved_groups` above and `config::Account::groups` itself — a
        // `tcr group color` write needs a restart before the running server's
        // wire reflects it.
        let group_colors = config.group_colors();
        let pacing = config.pacing.clone();
        let account_throttle = config.account_throttle.clone();
        let fleet_throttle = config.fleet_throttle.clone();

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

        // Clamp defensively even though `config::default_control_reserve` and
        // the field's own doc already promise `[0.0, 0.5]` — a hand-edited
        // config file is not bound by either.
        let control_reserve = config.control_reserve.clamp(0.0, 0.5);
        // Read before `config` is moved into the struct below, same reason as
        // `locked_name` above.
        let control_pooled = config.control_pooled;

        // Hours → ms once, at construction. `i64::from` then a checked multiply:
        // a hand-edited `resetUrgencyTierHours` of, say, 100_000_000 would
        // overflow the product and wrap to a negative width, which would invert
        // the bucket ordering rather than fail loudly. Saturating at i64::MAX
        // instead degrades to "one bucket for everyone" — the same shape as the
        // `0` disable, which is the safe direction to fall.
        let reset_urgency_tier_ms =
            i64::from(config.reset_urgency_tier_hours).saturating_mul(3_600_000);

        let control_idx = config.control_account.as_ref().and_then(|name| {
            let idx = accounts.iter().position(|a| a.name == *name);
            if idx.is_none() {
                let names: Vec<&str> = accounts.iter().map(|a| a.name.as_str()).collect();
                tracing::error!(
                    control_account = %name, available = ?names,
                    "controlAccount name did not match any account — running with NO control account"
                );
            }
            idx
        });

        let usage = crate::usage::UsageTracker::new(
            accounts.len(),
            crate::pricing::PricingTable::new(config.pricing.clone().into_iter().collect()),
        );

        let manager = Arc::new(Self {
            accounts: RwLock::new(accounts),
            usage,
            refresher,
            prober,
            warmer,
            warm_in_flight: AtomicBool::new(false),
            warm_wake: Notify::new(),
            config: Mutex::new(config),
            config_write: Mutex::new(()),
            config_path,
            upstream,
            proxy_api_key,
            global_threshold,
            reserved_groups: RwLock::new(reserved_groups),
            control_allowed_groups: RwLock::new(control_allowed_groups),
            group_colors: RwLock::new(group_colors),
            groups_reload_mtime: Mutex::new(None),
            pacing,
            account_throttle,
            fleet_throttle,
            locked_idx,
            control_idx: RwLock::new(control_idx),
            org_tat_ms: AsyncMutex::new(HashMap::new()),
            fleet_tat_ms: AsyncMutex::new(0),
            log: Mutex::new(VecDeque::with_capacity(REQUEST_LOG_CAPACITY)),
            current: Mutex::new(None),
            select_seq: AtomicU64::new(1),
            affinity: Mutex::new(HashMap::new()),
            affinity_extended: Mutex::new(HashSet::new()),
            affinity_dirty: AtomicBool::new(false),
            sessions: Mutex::new(HashMap::new()),
            session_seq: AtomicU64::new(1),
            next_revalidation_at_ms: std::sync::atomic::AtomicI64::new(0),
            conn_affinity: Mutex::new(HashMap::new()),
            divert_ledger: Mutex::new(HashMap::new()),
            control_reserve,
            control_pooled,
            reset_urgency_tier_ms,
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
    /// The seconds half of [`Self::exhaustion_hint`]; see it for the derivation.
    pub fn retry_after_hint(&self, now: OffsetDateTime, is_fable: bool) -> i64 {
        self.exhaustion_hint(now, is_fable, None).retry_after
    }

    /// [`Self::retry_after_hint`], scoped to the members of one group.
    ///
    /// Exists for STRICT (reserved) groups. A strict `--group g` request can only
    /// ever be served by a member of `g`, so sizing its wait with the fleet-wide
    /// hint above times it against accounts the request will never be allowed to
    /// use: any unrelated account un-gating sooner wins that `min`, the caller's
    /// one-shot soft-wait is spent waking early, and the request answers a 429
    /// while its own member is still seconds from free. The fleet-wide hint stays
    /// exactly as it is — ungrouped traffic genuinely can use any account, and
    /// that promise must keep holding for it.
    ///
    /// Returns the same `60` default when no member advertises a recovery instant,
    /// so a strict group whose members are all `Error`/disabled degrades to the
    /// same honest "try again in a minute" as an exhausted fleet.
    pub fn retry_after_hint_for_group(
        &self,
        now: OffsetDateTime,
        is_fable: bool,
        group: &str,
    ) -> i64 {
        self.exhaustion_hint(now, is_fable, Some(group)).retry_after
    }

    /// Everything a synthetic exhausted 429 can honestly tell the client, derived
    /// once from the same per-account gate view selection uses.
    ///
    /// `retry_after` is the soonest instant at which SOME account genuinely
    /// re-enters rotation, in whole seconds, clamped to at least 1. Honest by
    /// construction: it minimises over each account's [`Self::account_gate`]
    /// `free_at` — the instant ALL of *that* account's active gates clear —
    /// instead of the raw min over every window's reset. The raw-min was
    /// bug-shaped: it counted a 5-hour reset of an account that stays gated on
    /// its weekly bucket, and the reset of an `Error`/disabled account that never
    /// self-frees at all, so it promised a recovery that would not happen.
    /// Accounts that contribute nothing (`Ok`, `Login`, `Disabled`, or a gating
    /// window with no known reset) are skipped.
    ///
    /// When NO account advertises a recovery instant, `retry_after` is the `60`
    /// sentinel and `free_at` is `None`. The two are reported separately on
    /// purpose: a caller sizing a wait keeps reading the number, and a caller
    /// writing a message can say "no reset time is known" instead of printing a
    /// fixed 60 that a reader would take for a measurement. `proxy.rs` relies on
    /// the sentinel staying above its soft-wait cap — see the compile-time
    /// assertion beside `EXHAUSTION_SOFT_WAIT_MAX_SECS` there.
    ///
    /// `binding` is the gate that holds the soonest-freeing account — the one
    /// whose clearing `retry_after` is timed against — so the 429 can carry the
    /// matching `anthropic-ratelimit-unified-*` claim when that gate is a quota
    /// window, and stay a plain 429 when it is not. `gated` counts every account
    /// out of rotation by its gate, for the message.
    ///
    /// `is_fable` scopes the evaluation exactly as selection does: only a Fable
    /// request is gated by the model-scoped weekly (`7d_oi`) bucket, so an
    /// all-Fable-exhausted fleet reports that bucket's reset while non-Fable
    /// traffic ignores it.
    ///
    /// `group` narrows the fleet to one group's members and, reaching
    /// [`Self::account_gate`] as `Some(group)` on purpose, reports each member's
    /// REAL gate rather than the reservation: an explicit ask is never
    /// reserved-blocked by its own group ([`Self::reserved_blocks`]). `None` is
    /// the "unrequested traffic" view [`Self::snapshot`] reports — a promise this
    /// hint makes must hold for the traffic that would actually hit the 429.
    pub fn exhaustion_hint(
        &self,
        now: OffsetDateTime,
        is_fable: bool,
        group: Option<&str>,
    ) -> ExhaustionHint {
        let now_ms = odt_to_ms(now);
        let reserved_groups = self.reserved_groups();
        let accounts = self.accounts.read().expect("accounts lock poisoned");
        let mut gated: BTreeMap<GateReason, usize> = BTreeMap::new();
        let mut soonest: Option<(i64, GateReason)> = None;
        for account in accounts
            .iter()
            .filter(|account| group.is_none_or(|g| account.groups.iter().any(|m| m == g)))
        {
            let threshold = account.switch_threshold.unwrap_or(self.global_threshold);
            let (reason, free_at) = Self::account_gate(
                account,
                threshold,
                now,
                now_ms,
                is_fable,
                group,
                &reserved_groups,
            );
            if reason != GateReason::Ok {
                *gated.entry(reason).or_insert(0) += 1;
            }
            let Some(at) = free_at.map(odt_to_ms).filter(|&at| at > now_ms) else {
                continue;
            };
            if soonest.is_none_or(|(best, _)| at < best) {
                soonest = Some((at, reason));
            }
        }
        match soonest {
            Some((at, reason)) => ExhaustionHint {
                retry_after: ((at - now_ms + 999) / 1000).max(1),
                free_at: ms_to_odt(at),
                binding: Some(reason),
                gated,
            },
            None => ExhaustionHint {
                retry_after: 60,
                free_at: None,
                binding: None,
                gated,
            },
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
    ///
    /// Also ages the account's HTTP client: this is the one call taken exactly
    /// once per upstream send, so it is where [`MAX_SERVES_PER_CONNECTION`] is
    /// counted and where a due recycle is claimed. The accounts write-lock is
    /// taken here and then AGAIN by [`Self::recycle_client`] — never nested, and
    /// never held across the client build. It used to be a single uncontested
    /// section; that is why the claim resets the counter rather than letting
    /// `recycle_client` re-read it, which would race between the two sections.
    pub fn enter_in_flight(self: &Arc<Self>, idx: usize) -> InFlightGuard {
        let claimed_recycle = {
            let mut accounts = self.accounts.write().expect("accounts lock poisoned");
            match accounts.get_mut(idx) {
                Some(account) => {
                    account.in_flight = account.in_flight.saturating_add(1);
                    account.last_served_ms = crate::now_ms();
                    account.serves_since_client_build =
                        account.serves_since_client_build.saturating_add(1);
                    // Claim the recycle under the SAME write lock that observes the
                    // threshold, and reset the counter as the act of claiming it:
                    // that is what makes the claim exclusive, so two sends crossing
                    // the threshold concurrently cannot both rebuild — the loser
                    // reads 0 and returns false.
                    if account.serves_since_client_build >= MAX_SERVES_PER_CONNECTION {
                        account.serves_since_client_build = 0;
                        true
                    } else {
                        false
                    }
                }
                None => false,
            }
        };
        if claimed_recycle {
            self.recycle_client(idx);
        }
        InFlightGuard {
            manager: Arc::clone(self),
            idx,
        }
    }

    /// Retire account `idx`'s HTTP client and give it a fresh one, dropping the
    /// pooled h2 connection that has now carried [`MAX_SERVES_PER_CONNECTION`]
    /// sends before Anthropic's edge drains it out from under us.
    ///
    /// Requests already in flight are untouched. Each holds its own
    /// `Arc<reqwest::Client>` clone, taken by [`Self::http_client`] before the
    /// send, so the OLD client — and the connection carrying their streams —
    /// stays alive until the last of them finishes and drops the final `Arc`.
    /// Only sends selected after this point get the new pool. That is the whole
    /// reason this swaps an `Arc` instead of reaching into the existing client.
    ///
    /// The replacement is built with NEITHER lock held, matching
    /// [`Self::add_or_update_account`]'s Added path and for the same reason: a
    /// panic in [`build_serving_client`] (which tests inject) must not poison
    /// `self.accounts`.
    fn recycle_client(&self, idx: usize) {
        let fresh = build_serving_client(self.http1_only());
        let mut accounts = self.accounts.write().expect("accounts lock poisoned");
        let Some(account) = accounts.get_mut(idx) else {
            // Stale index: accounts are appended, never removed, so this is not
            // reachable on the live path — same rationale as `http_client`.
            return;
        };
        account.http = fresh;
        tracing::info!(
            account_index = idx,
            account = %account.name,
            serves = MAX_SERVES_PER_CONNECTION,
            "retiring the upstream connection after its serve budget — \
             a fresh h2 connection costs a handshake, not a prompt-cache miss"
        );
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
        // INV1 — the write lock, held across the whole read-modify-write below, is
        // what stops a shutdown flush racing a concurrent persist_tokens and
        // clobbering a just-rotated token. It replaces the config lock in that role;
        // dropping it here while persist_tokens no longer holds `config` across its
        // save would leave BOTH writers unserialized.
        let _writing = self
            .config_write
            .lock()
            .expect("config write lock poisoned");
        // INV2 — clone inside a short critical section so the config lock is
        // released on this line, before any file I/O below runs.
        let snapshot = self.config.lock().expect("config lock poisoned").clone();
        if let Err(err) = config::save_tokens(path, &snapshot) {
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
        // INV1 — the modify and the save must be ONE atomic step against the file.
        // Without that, two concurrent refreshes race: a stale save clobbers the
        // other account's just-rotated refresh token, which then 400s
        // ("invalid_grant") on its next refresh and is dead until re-authed by hand.
        // This lock — not the config lock — is what provides that serialization now,
        // and it covers save_tokens' whole read-modify-write of the file.
        let _writing = self
            .config_write
            .lock()
            .expect("config write lock poisoned");
        // INV2 — the config lock is taken ONLY for this block: apply the rotation to
        // the snapshot and clone it out. The guard drops at the closing brace, so
        // `save_tokens` below runs with `self.config` free and a per-connection
        // `session_affinity_enabled()` never waits on an fsync. Cloning is safe here
        // in a way it was NOT before: INV1 already serializes the writers, so no
        // second writer can slip a stale document in between.
        //
        // Deliberately NOT INV3 (mutate only after a successful write): the mutation
        // IS the write's content — it has to precede it — and memory is the recovery
        // buffer for a single-use token. If this save fails, the freshly rotated
        // token still sits in the snapshot for `persist_now` to flush at shutdown;
        // rolling memory back on failure would lose it for good.
        let (snapshot, placement) = {
            let mut config = self.config.lock().expect("config lock poisoned");
            // Resolved, never first-match: `same_identity` treats an unknown org as
            // a match, so with the legacy two-org shape in the file a first-match
            // search lands THIS account's rotated credential on the OTHER account's
            // record — overwriting a single-use refresh token that is then dead.
            // An unbreakable tie writes nothing and says so.
            let placement = crate::identity::resolve(config.accounts.iter().enumerate(), &probe);
            if let crate::identity::Resolved::One(position) = placement {
                if let Some(account) = config.accounts.get_mut(position) {
                    account.access_token = tokens.access_token.clone();
                    // THE TRAP: same invariant as `apply_refresh` in
                    // `manager/refresh.rs` — `tokens.refresh_token` must be
                    // `Some(...)` on every reachable refresh success.
                    account.refresh_token = tokens.refresh_token.clone();
                    account.expires_at = Some(tokens.expires_at_ms);
                }
            }
            (config.clone(), placement)
        };
        // Logged with the config lock released (INV2) — a `warn!` is cheap but the
        // critical section above is on the per-connection path's lock.
        match placement {
            crate::identity::Resolved::One(_) => {}
            crate::identity::Resolved::None => tracing::warn!(
                account = %name,
                "no loaded config account carries this identity; the rotated token is not in the snapshot and will not reach disk"
            ),
            crate::identity::Resolved::Many => tracing::warn!(
                account = %name,
                "more than one loaded config account carries this identity; refusing to guess which one just rotated, so the rotated token will not reach disk and this account may need `tcr login`"
            ),
        }
        // Tokens only: the in-memory config is a boot-time snapshot, so writing
        // it whole would stamp stale settings over the user's live file.
        if let Err(err) = config::save_tokens(path, &snapshot) {
            tracing::error!(error = %err, "failed to persist refreshed token to config");
        }
    }

    /// A cloned snapshot of the currently-reserved group names. Clones rather
    /// than returning a guard so a caller never holds this lock across a
    /// second lock acquisition (`accounts`, `affinity`) — the set is a
    /// handful of short strings, so the clone costs nothing worth avoiding.
    /// See the field's own doc for the hot-reload contract this serves.
    fn reserved_groups(&self) -> HashSet<String> {
        self.reserved_groups
            .read()
            .expect("reserved_groups lock poisoned")
            .clone()
    }

    /// Whether `group` is currently reserved — and therefore STRICT: its own
    /// traffic never spills to a non-member (see
    /// [`Self::select_with_group`]'s strict arm).
    ///
    /// A membership test rather than [`Self::reserved_groups`] + `contains`,
    /// because the request path asks this per request and only ever needs the
    /// one answer; cloning the set to look at a single key is waste on a hot
    /// path. Reads the same hot-reloaded cache, so `tcr group unreserve` takes
    /// effect on the next request with no restart.
    pub fn is_group_reserved(&self, group: &str) -> bool {
        self.reserved_groups
            .read()
            .expect("reserved_groups lock poisoned")
            .contains(group)
    }

    /// A cloned snapshot of the currently control-account-allowed group
    /// names. Same clone-not-guard reasoning as [`Self::reserved_groups`].
    fn control_allowed_groups(&self) -> HashSet<String> {
        self.control_allowed_groups
            .read()
            .expect("control_allowed_groups lock poisoned")
            .clone()
    }

    /// Re-read [`config::Account::groups`] and `groupSettings` (`reserved`,
    /// `allowControlAccount`, `color`) from `self.config_path` when the file's mtime has moved since
    /// the last check — the fix for group edits appearing to do nothing
    /// (`docs/plans/live-reload-bridge.md`, problem 1). Called from a natural
    /// cadence point ([`Self::select_with_group`], [`Self::select_revalidation`],
    /// [`Self::snapshot`]) rather than a dedicated watcher thread — one more
    /// background task is one more thing to keep alive, and every one of
    /// those call sites already runs on every request or every TUI tick.
    ///
    /// Touches ONLY the fields named above. Every other field of `self.config`
    /// — accounts, credentials, tokens, every unmodelled top-level key — is
    /// left exactly as booted: the in-memory snapshot stays authoritative for
    /// those, or this would fight [`Self::persist_now`] / [`Self::persist_tokens`]
    /// / [`Self::persist_disabled`]'s read-modify-write of the SAME file.
    ///
    /// Takes `config_write` (INV1, see that field's doc) across the mtime
    /// check AND the file read/parse — the same discipline every writer of
    /// this file already uses, so a reload can never straddle a concurrent
    /// persist's write. [`config::write_atomic`]'s temp-file-then-rename
    /// already rules out a torn read on its own; this keeps reload from being
    /// the one reader that does not bother.
    ///
    /// A file that cannot be stat'd, read, or parsed **keeps every current
    /// in-memory value** and logs a warning — never blanks the fleet's groups
    /// — and, unlike a successful reload, does NOT advance the remembered
    /// mtime, so a transient or malformed edit self-heals on the very next
    /// natural-cadence check rather than waiting for the file to change
    /// again.
    pub(super) fn reload_groups_if_changed(&self) {
        let Some(path) = &self.config_path else {
            return;
        };
        let _writing = self
            .config_write
            .lock()
            .expect("config write lock poisoned");
        let mtime = match std::fs::metadata(path).and_then(|m| m.modified()) {
            Ok(m) => m,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    path = %path.display(),
                    "group reload: could not stat config file, keeping current groups"
                );
                return;
            }
        };
        {
            let last = self
                .groups_reload_mtime
                .lock()
                .expect("groups reload mtime lock poisoned");
            if *last == Some(mtime) {
                return;
            }
        }
        match config::load(path) {
            Ok(fresh) => {
                self.apply_group_reload(&fresh);
                *self
                    .groups_reload_mtime
                    .lock()
                    .expect("groups reload mtime lock poisoned") = Some(mtime);
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    path = %path.display(),
                    "group reload: config is unreadable/malformed, keeping current groups"
                );
            }
        }
    }

    /// Apply `fresh`'s group membership/settings onto the running server and
    /// log exactly what changed — the line an operator greps for after
    /// editing groups to ask "did it pick up my edit". Silent when nothing
    /// actually differs (the common case: most cadence ticks see an unchanged
    /// file, or a mtime bump from an unrelated write). Called only from
    /// [`Self::reload_groups_if_changed`], under `config_write`.
    ///
    /// Reserved-group membership is applied BEFORE per-account `groups`, so
    /// that by the time this returns and the caller re-tests a pinned
    /// session's hard gate, a session on an account whose group just became
    /// reserved is judged against the fresh reservation — the existing
    /// pin-reclaim-on-reservation test (`pin_reclaim_when_the_pinned_accounts_group_is_reserved`)
    /// extends to the reload path for free, because [`Self::select_with_group`]
    /// re-tests [`Self::account_hard_ok`] against a fresh [`Self::reserved_groups`]
    /// snapshot on every call — reservation was never cached PAST one call, only
    /// ACROSS them until this reload existed.
    ///
    /// Matches each running [`AccountRuntime`] to `fresh.accounts` by
    /// [`crate::identity::same_identity`] — the SAME identity rule
    /// [`Self::persist_tokens`] uses — rather than by name, so a config
    /// mid-way through a login churn (a display name changed, an org backfilled)
    /// still reloads the right row instead of silently dropping its groups.
    fn apply_group_reload(&self, fresh: &Config) {
        let new_reserved = fresh.reserved_group_names();
        let new_control_allowed = fresh.control_allowed_group_names();
        let new_colors = fresh.group_colors();
        let mut changed: Vec<String> = Vec::new();

        {
            let mut reserved = self
                .reserved_groups
                .write()
                .expect("reserved_groups lock poisoned");
            if *reserved != new_reserved {
                changed.push(format!(
                    "reserved groups: {:?} -> {:?}",
                    sorted_groups(&reserved),
                    sorted_groups(&new_reserved)
                ));
                *reserved = new_reserved;
            }
        }
        {
            let mut control_allowed = self
                .control_allowed_groups
                .write()
                .expect("control_allowed_groups lock poisoned");
            if *control_allowed != new_control_allowed {
                changed.push(format!(
                    "control-allowed groups: {:?} -> {:?}",
                    sorted_groups(&control_allowed),
                    sorted_groups(&new_control_allowed)
                ));
                *control_allowed = new_control_allowed;
            }
        }
        {
            let mut colors = self
                .group_colors
                .write()
                .expect("group_colors lock poisoned");
            if *colors != new_colors {
                changed.push(format!("group colors: {:?} -> {:?}", *colors, new_colors));
                *colors = new_colors;
            }
        }
        {
            let mut accounts = self.accounts.write().expect("accounts lock poisoned");
            for account in accounts.iter_mut() {
                let probe = crate::identity::probe(
                    &account.name,
                    account.account_uuid.clone(),
                    account.org_uuid.clone(),
                    account.org_name.clone(),
                );
                let Some(matched) = fresh
                    .accounts
                    .iter()
                    .find(|a| crate::identity::same_identity(&probe, a))
                else {
                    continue;
                };
                let new_groups = matched.groups.clone().unwrap_or_default();
                if new_groups != account.groups {
                    changed.push(format!(
                        "{}: groups {:?} -> {:?}",
                        account.name, account.groups, new_groups
                    ));
                    account.groups = new_groups;
                }
            }
        }

        if !changed.is_empty() {
            tracing::info!(changes = ?changed, "config reload: picked up a group edit");
        }
    }

    /// Flush account `idx`'s `disabled` flag to the config file, so an account
    /// deliberately benched from the TUI is still benched after a restart.
    ///
    /// Upholds the same three invariants as [`Self::persist_tokens`], with the line
    /// carrying each marked below:
    ///  - **INV1** — the `config_write` guard spans the whole read-modify-write, so
    ///    this write and a concurrent token rotation never interleave. That matters
    ///    more here than the flag does: an interleaved write can clobber a refresh
    ///    token that rotated in between, and a refresh token is single-use, so the
    ///    clobbered account 400s (`invalid_grant`) on its next refresh and is dead
    ///    until re-authed by hand.
    ///  - **INV2** — `save_disabled` runs with `self.config` NOT held. That lock is
    ///    on the per-connection path (`session_affinity_enabled`), so holding it
    ///    across a read + `sync_all` + rename let one keypress stall connection
    ///    setup.
    ///  - **INV3** — the in-memory snapshot is mutated only once the file is known
    ///    to carry the flag. Mutating first (and regardless of the outcome) left
    ///    memory and disk permanently diverged on `NoEntry`/`Ambiguous`/`Err`, with
    ///    nothing to reconcile or retry them — the exact opposite of the guarantee.
    ///
    /// Writes the flag ONLY, via [`config::save_disabled`], never the whole config:
    /// the in-memory `Config` is a boot-time snapshot, so flushing it whole would
    /// revert every setting the user edited while the proxy was running.
    ///
    /// A missing `config_path` (tests, `tcr demo`, `tcr status --probe`) is a
    /// SILENT no-op — those managers must never touch a real config file.
    fn persist_disabled(
        &self,
        idx: usize,
        target: &config::Account,
        disabled: bool,
    ) -> DisablePersist {
        let Some(path) = &self.config_path else {
            return DisablePersist::NoConfigFile;
        };
        // INV1 — held across `save_disabled`'s whole read + modify + sync + rename.
        let _writing = self
            .config_write
            .lock()
            .expect("config write lock poisoned");
        // INV2 — no `self.config` guard is alive on this line, so the file I/O
        // cannot block a per-connection config read.
        let outcome = config::save_disabled(path, target, disabled);
        // INV3 — memory is touched only on the arms where the FILE now carries the
        // desired state. `Unchanged` qualifies: it means the document already said
        // exactly this, so nothing was written precisely because nothing needed to
        // be. Every other arm leaves the snapshot alone.
        if matches!(
            outcome,
            Ok(config::DisabledWrite::Updated) | Ok(config::DisabledWrite::Unchanged)
        ) {
            let mut config = self.config.lock().expect("config lock poisoned");
            // Resolved by identity, exactly as `persist_tokens` and `save_disabled`
            // resolve, so all three land on the same record. An unbreakable tie
            // writes nothing: `save_disabled` refuses one too, and memory must not
            // drift onto a record the file was not allowed to touch.
            if let crate::identity::Resolved::One(position) =
                crate::identity::resolve(config.accounts.iter().enumerate(), target)
            {
                if let Some(account) = config.accounts.get_mut(position) {
                    account.disabled = if disabled { Some(true) } else { None };
                }
            }
        }
        match outcome {
            Ok(config::DisabledWrite::Updated) => {
                tracing::info!(
                    account = %target.name,
                    index = idx,
                    disabled,
                    "persisted account disabled flag to config"
                );
                DisablePersist::Persisted
            }
            Ok(config::DisabledWrite::Unchanged) => {
                tracing::debug!(
                    account = %target.name,
                    index = idx,
                    disabled,
                    "config already carries this disabled state; nothing written"
                );
                DisablePersist::Persisted
            }
            Ok(config::DisabledWrite::NoEntry) => {
                tracing::warn!(
                    account = %target.name,
                    index = idx,
                    path = %path.display(),
                    "no config entry carries this account's identity; the disabled flag will NOT survive a restart"
                );
                DisablePersist::NoEntry
            }
            Ok(config::DisabledWrite::Ambiguous) => {
                tracing::warn!(
                    account = %target.name,
                    index = idx,
                    path = %path.display(),
                    "more than one config entry carries this account's identity; refusing to guess which one to flag, so the disabled flag will NOT survive a restart"
                );
                DisablePersist::Ambiguous
            }
            Err(err) => {
                tracing::error!(
                    error = %err,
                    account = %target.name,
                    index = idx,
                    path = %path.display(),
                    "failed to persist the disabled flag to config"
                );
                DisablePersist::WriteFailed
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
    ///
    /// The flag is also PERSISTED (see [`Self::persist_disabled`]) — memory-only
    /// was the bug: a restart silently returned a deliberately benched account to
    /// rotation, because the server writes nothing but credentials back.
    ///
    /// Returns what became of the DURABLE half, because in TUI mode `tracing` is
    /// redirected to a log file: a failed write reported only there leaves the TUI
    /// rendering the account as benched while the bench provably will not survive a
    /// restart. The caller is expected to put [`DisablePersist::warning`] in front of
    /// the person who pressed the key.
    pub fn set_disabled(&self, idx: usize, disabled: bool) -> DisablePersist {
        // Take the identity out under the accounts lock and RELEASE that lock
        // before persisting. The persist path takes the config lock, and holding
        // both at once here would invert the order `warm_targets` reads them in
        // (config, then accounts) — the shape a deadlock is made of.
        let target = {
            let mut accounts = self.accounts.write().expect("accounts lock poisoned");
            let Some(account) = accounts.get_mut(idx) else {
                return DisablePersist::NoSuchAccount;
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
        self.persist_disabled(idx, &target, disabled)
    }

    /// Resolve a user-supplied `(query, org)` against the LIVE rotation and apply
    /// `disabled` to whatever it names — the whole operation the control endpoint
    /// performs, kept here so no caller outside this module has to know that a
    /// rotation index is what [`Self::set_disabled`] takes.
    ///
    /// Resolution runs over `self.accounts` — the rotation slots themselves — and
    /// NOT over the boot-time config snapshot, so the index handed to
    /// `set_disabled` is a slot by construction. Matching the config's vector and
    /// then indexing the runtime one would be correct only while the two stay the
    /// same length in the same order forever, which is not a property anything
    /// here enforces.
    ///
    /// The read guard is released before `set_disabled` takes its write guard —
    /// `RwLock` is not reentrant, so holding it across the call would deadlock.
    pub fn set_disabled_by_query(
        &self,
        query: &str,
        org: Option<&str>,
        disabled: bool,
    ) -> SetDisabledOutcome {
        let (idx, name) = {
            let accounts = self.accounts.read().expect("accounts lock poisoned");
            match crate::identity::match_one(&accounts[..], query, org) {
                crate::identity::Match::One(idx) => (idx, accounts[idx].name.clone()),
                crate::identity::Match::None => return SetDisabledOutcome::NoMatch,
                crate::identity::Match::Ambiguous(names) => {
                    return SetDisabledOutcome::Ambiguous(names)
                }
            }
        };
        SetDisabledOutcome::Applied {
            name,
            persist: self.set_disabled(idx, disabled),
        }
    }

    /// The current control account's rotation index, or `None` when unset.
    pub fn control(&self) -> Option<usize> {
        *self.control_idx.read().expect("control lock poisoned")
    }

    /// The current control account's name, or `None` when unset / the index no
    /// longer resolves (an account row cannot disappear — `add_account` is
    /// append-only — so the only way this reads `None` with `control()` some
    /// is a bug, not a live scenario; resolving defensively here rather than
    /// indexing avoids a panic either way).
    pub fn control_name(&self) -> Option<String> {
        let idx = self.control()?;
        self.accounts
            .read()
            .expect("accounts lock poisoned")
            .get(idx)
            .map(|a| a.name.clone())
    }

    /// Whether the control account also takes general inference traffic — the
    /// boot-time `controlPooled` snapshot. Surfaced on the `server started` line
    /// because it is the one routing flag whose cost is an OUTAGE rather than a
    /// slowdown, and an operator reading that line after a restart needs to see
    /// it without opening the config.
    pub fn control_pooled(&self) -> bool {
        self.control_pooled
    }

    /// The resolved `lockAccount` name, or `None` when unlocked / the name did
    /// not match any account. `locked_idx` is fixed at construction (see
    /// [`Self::assemble`]), so this is a plain resolve against the live
    /// `accounts` vec — no lock ordering concerns beyond the read itself.
    /// Read by the boot-line log in `server.rs` so a hard lock is visible from
    /// outside the process, the same reasoning as `http1_only` on that line.
    pub fn locked_account_name(&self) -> Option<String> {
        let idx = self.locked_idx?;
        self.accounts
            .read()
            .expect("accounts lock poisoned")
            .get(idx)
            .map(|a| a.name.clone())
    }

    /// Whether per-account request pacing does anything (see
    /// [`config::PacingConfig::is_active`]). Read by the boot-line log in
    /// `server.rs`, not the request path — [`Self::eligible`] reads
    /// `self.pacing` directly.
    pub fn pacing_active(&self) -> bool {
        self.pacing.is_active()
    }

    /// Whether EITHER outbound throttle does anything (see
    /// [`config::ThrottleConfig::is_active`]). Read by the boot-line log in
    /// `server.rs`, not the request path — [`Self::throttle_send`] reads
    /// `self.account_throttle` / `self.fleet_throttle` directly.
    ///
    /// `true` when either bucket is live, because "throttling is on" is what the
    /// boot line is telling the operator; a build with only the fleet ceiling armed
    /// is still throttled.
    pub fn throttle_active(&self) -> bool {
        self.account_throttle.is_active() || self.fleet_throttle.is_active()
    }

    /// Persist ONLY the top-level `controlAccount` key, via
    /// [`config::save_control_account`] — never the whole config (see
    /// [`Self::persist_disabled`]'s doc-comment for why: the in-memory
    /// `Config` is a boot-time snapshot, and flushing it whole would revert
    /// every setting the user edited while the proxy ran — the exact clobber
    /// fixed in `1d978ce`).
    ///
    /// Same three invariants as [`Self::persist_disabled`]:
    ///  - **INV1** — `config_write` spans the whole read-modify-rename, so this
    ///    write and a concurrent token rotation or disabled-flag write can
    ///    never interleave.
    ///  - **INV2** — `save_control_account` runs with `self.config` NOT held,
    ///    so the file I/O never blocks the per-connection config read.
    ///  - **INV3** — the in-memory snapshot (`config.control_account`) is
    ///    mutated only once the file is known to carry the desired state
    ///    (`Updated` or `Unchanged`); a failed write leaves memory and disk in
    ///    the same (divergent-from-the-request) state rather than only disk.
    ///
    /// A missing `config_path` (tests, `tcr demo`, `tcr status --probe`) is a
    /// SILENT no-op — those managers must never touch a real config file.
    fn persist_control(&self, name: Option<String>) -> ControlPersist {
        let Some(path) = &self.config_path else {
            return ControlPersist::NoConfigFile;
        };
        // INV1 — held across `save_control_account`'s whole read + modify +
        // sync + rename.
        let _writing = self
            .config_write
            .lock()
            .expect("config write lock poisoned");
        // INV2 — no `self.config` guard is alive on this line.
        let outcome = config::save_control_account(path, name.as_deref());
        // INV3 — memory is touched only on the arms where the FILE now carries
        // the desired state.
        if matches!(
            outcome,
            Ok(config::ControlWrite::Updated) | Ok(config::ControlWrite::Unchanged)
        ) {
            let mut config = self.config.lock().expect("config lock poisoned");
            config.control_account = name.clone();
        }
        match outcome {
            Ok(config::ControlWrite::Updated) => {
                tracing::info!(
                    control_account = ?name,
                    "persisted control account to config"
                );
                ControlPersist::Persisted
            }
            Ok(config::ControlWrite::Unchanged) => {
                tracing::debug!(
                    control_account = ?name,
                    "config already names this control account; nothing written"
                );
                ControlPersist::Persisted
            }
            Err(err) => {
                tracing::error!(
                    error = %err,
                    control_account = ?name,
                    path = %path.display(),
                    "failed to persist the control account to config"
                );
                ControlPersist::WriteFailed
            }
        }
    }

    /// Set (`idx = Some(_)`) or clear (`idx = None`) the control account by
    /// rotation index, and persist it. Mirrors [`Self::set_disabled`]'s
    /// resolve-name-then-persist shape: the name is read out under a released
    /// `accounts` READ lock before `control_idx` is taken, so the two never
    /// nest (same discipline `set_disabled` documents for `accounts`/`config`).
    fn set_control(&self, idx: Option<usize>) -> ControlPersist {
        let name = idx.and_then(|i| {
            self.accounts
                .read()
                .expect("accounts lock poisoned")
                .get(i)
                .map(|a| a.name.clone())
        });
        {
            let mut control = self.control_idx.write().expect("control lock poisoned");
            *control = idx;
        }
        self.persist_control(name)
    }

    /// Resolve a user-supplied `(query, org)` against the LIVE rotation and set
    /// the control account to whatever it names — the whole operation the
    /// control endpoint performs. `query = None` CLEARS the control account
    /// (there is nothing to resolve, so this never reaches `match_one`).
    ///
    /// Resolution runs over `self.accounts` — never the boot-time config
    /// snapshot — for the same reason [`Self::set_disabled_by_query`] does.
    pub fn set_control_by_query(
        &self,
        query: Option<&str>,
        org: Option<&str>,
    ) -> SetControlOutcome {
        let Some(query) = query else {
            return SetControlOutcome::Applied {
                name: None,
                persist: self.set_control(None),
            };
        };
        let (idx, name) = {
            let accounts = self.accounts.read().expect("accounts lock poisoned");
            match crate::identity::match_one(&accounts[..], query, org) {
                crate::identity::Match::One(idx) => (idx, accounts[idx].name.clone()),
                crate::identity::Match::None => return SetControlOutcome::NoMatch,
                crate::identity::Match::Ambiguous(names) => {
                    return SetControlOutcome::Ambiguous(names)
                }
            }
        };
        SetControlOutcome::Applied {
            name: Some(name),
            persist: self.set_control(Some(idx)),
        }
    }

    /// Record which account actually served the most recent request.
    pub fn set_current(&self, idx: usize) {
        *self.current.lock().expect("current lock poisoned") = Some(idx);
    }

    /// Append a new account to the live rotation and return its index.
    ///
    /// APPEND-ONLY — this is a hard requirement, not a style choice. Pins are
    /// in-memory `(session_key, account_index)`; appending keeps every existing
    /// index valid, whereas inserting or reordering would re-key live sessions
    /// and cold-start their prompt cache, which is the exact cost this whole
    /// feature exists to avoid. Never insert at a position other than the end,
    /// and never reorder either vec.
    ///
    /// Updates both `self.accounts` (the live rotation `AccountRuntime`s) and
    /// `self.config`'s accounts vec (so later identity-resolution in
    /// `persist_tokens` / `persist_disabled` finds the row instead of logging
    /// the "no loaded config account carries this identity" WARN). The two
    /// locks are taken SEQUENTIALLY, never nested — the same discipline
    /// `set_disabled` documents: holding both at once risks inverting the lock
    /// order `warm_targets` reads them in.
    ///
    /// Derives the runtime row via [`AccountRuntime::from_config`], which is
    /// exactly how every other account's runtime state is built at startup, so
    /// an appended account gets its own `refresh_lock` for free — there is no
    /// longer a parallel structure to keep in sync.
    pub fn add_account(&self, account: config::Account) -> usize {
        let http1_only = self.config.lock().expect("config lock poisoned").http1_only;
        let runtime = AccountRuntime::from_config(&account, http1_only);
        let idx = {
            let mut accounts = self.accounts.write().expect("accounts lock poisoned");
            accounts.push(runtime);
            accounts.len() - 1
        };
        {
            let mut config = self.config.lock().expect("config lock poisoned");
            config.accounts.push(account);
        }
        idx
    }

    /// Resolve `account` against the LIVE rotation by identity and either append
    /// it (nothing matched) or replace an existing account's credentials in
    /// place (exactly one matched) — the whole operation `POST /_tcr/accounts`
    /// performs, kept here for the same reason [`Self::set_disabled_by_query`]
    /// is: no caller outside this module needs to know that a rotation index is
    /// what the mutating primitives take.
    ///
    /// Resolution uses [`crate::identity::resolve`]/[`crate::identity::same_identity`]
    /// — the SAME rule the durable half ([`config::save_account`]) and `tcr
    /// login` (`oauth::upsert_account`) already run — never
    /// [`crate::identity::match_one`]. `match_one` goes through
    /// [`crate::identity::Queryable`], which exposes only name and org: it
    /// cannot compare `account_uuid`, so it was structurally unable to tell two
    /// different people sharing a display name apart. Unifying on `resolve`
    /// fixes that (a differing `account_uuid` never matches, so a same-named
    /// different person is APPENDED, never overwritten) and it fixes the
    /// live/durable split brain the old rule caused: `resolve` tolerates an
    /// unknown org on either side exactly as the durable write does, so a
    /// legacy no-org row and a submission carrying its own org now agree on
    /// which one account that is, on both halves, every time — not only when
    /// there happen to be two orgs in play.
    ///
    /// One `match_one`-style fallback survives, deliberately narrow: when
    /// `account` carries NO identity fields at all (a bare name — the ordinary
    /// single-account case), `resolve`'s exact name-equality miss is retried
    /// through `match_one` so a bare email still finds a stored display name
    /// carrying an org suffix (`email (Org)`), the way the CLI's own query
    /// resolution always has. It is gated on "no identity fields submitted"
    /// and nothing looser: widening it to a submission that DOES carry an
    /// `account_uuid` would resurrect the exact bug this rewrite fixes, by
    /// matching on name/email again after the uuid comparison already said
    /// "different person".
    ///
    /// Resolve AND mutate under ONE write-lock acquisition of `self.accounts` —
    /// closing a TOCTOU the old two-lock version had: it resolved under a READ
    /// lock, released it, then took a separate WRITE lock to append. Two
    /// concurrent submissions of the same brand-new identity could both resolve
    /// "no match" before either had appended, and both would append —
    /// duplicating the live row. Holding one write lock across resolve-then-
    /// mutate means the second caller's resolve runs after the first caller's
    /// append is already visible, so it finds the row and updates it instead.
    ///
    /// `config_write` is held across the WHOLE function — resolve, mutate, and
    /// persist — not just the file I/O at the end. The `self.accounts` fix
    /// above only serializes the LIVE half: it guarantees the second of two
    /// concurrent same-identity submissions sees the first one's row and takes
    /// the Updated path instead of duplicating it. It said nothing about the
    /// DURABLE half, which used to take its own `config_write` lock only
    /// inside `persist_added`/`persist_replaced`, well after `self.accounts`
    /// was released — so two callers' persists could still race onto
    /// `config_write` in the OPPOSITE order from the one their live resolves
    /// agreed on, each writing its OWN submitted credentials. The loser's
    /// `persist_replaced` (seeing nothing on disk yet) would append a fresh
    /// entry with ITS tokens; the winner's `persist_added` would then find
    /// that entry and merge ITS OWN tokens over it — leaving the file holding
    /// whichever call happened to reach `config_write` first, independent of
    /// which one actually won the live race. Measured 2/200 over 200 rounds of
    /// two concurrent adds of one identity. Holding this lock across both
    /// halves makes resolve-mutate-persist one atomic unit, so the durable
    /// write order always agrees with the live resolve order.
    ///
    /// This is the one place in `Manager` that holds `config_write` and
    /// `self.accounts` at once — everywhere else they are taken sequentially,
    /// never nested (see `set_disabled`'s and `warm_targets`'s comments on
    /// that discipline). It is safe: `self.accounts` is held only for the
    /// brief in-memory resolve-and-mutate inside this span, never across the
    /// file I/O below, and no other code path holds `self.accounts` and then
    /// reaches for `config_write` or `config` — so this adds one new,
    /// one-directional edge (`config_write` → `self.accounts`) to the lock
    /// order, not a cycle. `persist_added`/`persist_replaced` no longer take
    /// `config_write` themselves — they assume the caller already holds it.
    ///
    /// A resolution that lands on Added needs a freshly-built
    /// [`AccountRuntime`] (a `reqwest::Client`) to push, and building one must
    /// never happen while EITHER lock is held — a panic mid-build would
    /// poison it for every other `.expect(...)` site on that lock,
    /// `config_write` included (`persist_tokens`, `persist_now`'s shutdown
    /// flush, this function's own next call). So `config_write` is held
    /// across each ATTEMPT's resolve/mutate/persist, not necessarily across
    /// the whole call: an attempt that resolves Added with no runtime ready
    /// yet mutates NOTHING, drops both locks, builds, and retries — see the
    /// loop inside the function body for why at most one retry is ever
    /// needed and why it cannot reopen the TOCTOU above.
    pub fn add_or_update_account(&self, mut account: config::Account) -> AddAccountOutcome {
        let target = crate::identity::probe(
            &account.name,
            account.account_uuid.clone(),
            account.org_uuid.clone(),
            account.org_name.clone(),
        );
        let bare_identity = account.account_uuid.is_none()
            && account.org_uuid.is_none()
            && account.org_name.is_none();

        /// What the locked resolve-and-mutate section below produced, so the
        /// durable write can run after `self.accounts`'s lock is released —
        /// `persist_added`/`persist_replaced` do their own file I/O and must
        /// never run under `self.accounts`'s lock (though both still run
        /// under `config_write`, held since this attempt's start — see the N2
        /// doc-comment on `add_or_update_account`).
        enum Resolution {
            Added {
                idx: usize,
            },
            Updated {
                idx: usize,
                name: String,
                /// The row's OWN identity (post-backfill) plus its REAL routing
                /// state (priority/switch_threshold/disabled) — never
                /// `identity::probe`'s placeholders, see [`Self::persist_replaced`]
                /// and the F5 note there. Boxed: `Added` is a bare `usize`, and
                /// clippy flags the size gap between variants otherwise.
                target: Box<config::Account>,
            },
            /// The resolve below landed on Added, but there is no
            /// already-built [`AccountRuntime`] to push yet — see the loop
            /// below, which built nothing here on purpose.
            NeedsBuild,
        }

        // Built on demand, never while a lock is held — see the doc-comment
        // above `add_or_update_account`. `None` until the first attempt
        // below discovers it actually needs one.
        let mut speculative_runtime: Option<AccountRuntime> = None;

        loop {
            // N2 — see above: held across THIS ATTEMPT's resolve, mutate and
            // persist so it never interleaves with another concurrent call's.
            // Dropped early, before any mutation, on an attempt that turns out
            // to need a client it doesn't have yet (`NeedsBuild` below).
            let _writing = self
                .config_write
                .lock()
                .expect("config write lock poisoned");

            let resolution = {
                let mut accounts = self.accounts.write().expect("accounts lock poisoned");

                let probes: Vec<(usize, config::Account)> = accounts
                    .iter()
                    .enumerate()
                    .map(|(i, a)| {
                        (
                            i,
                            crate::identity::probe(
                                &a.name,
                                a.account_uuid.clone(),
                                a.org_uuid.clone(),
                                a.org_name.clone(),
                            ),
                        )
                    })
                    .collect();

                let matched =
                    match crate::identity::resolve(probes.iter().map(|(i, p)| (*i, p)), &target) {
                        crate::identity::Resolved::One(idx) => Some(idx),
                        crate::identity::Resolved::Many => {
                            // Recompute the loose set for its names: `Resolved::Many`
                            // itself carries none, and this is exactly the set
                            // `resolve` drew from (same predicate, same candidates).
                            let names = accounts
                                .iter()
                                .filter(|a| {
                                    crate::identity::same_identity(
                                        &target,
                                        &crate::identity::probe(
                                            &a.name,
                                            a.account_uuid.clone(),
                                            a.org_uuid.clone(),
                                            a.org_name.clone(),
                                        ),
                                    )
                                })
                                .map(|a| a.name.clone())
                                .collect();
                            return AddAccountOutcome::Ambiguous(names);
                        }
                        crate::identity::Resolved::None if bare_identity => {
                            match crate::identity::match_one(&accounts[..], &account.name, None) {
                                crate::identity::Match::One(idx) => Some(idx),
                                crate::identity::Match::None => None,
                                crate::identity::Match::Ambiguous(names) => {
                                    return AddAccountOutcome::Ambiguous(names);
                                }
                            }
                        }
                        crate::identity::Resolved::None => None,
                    };

                match matched {
                    Some(idx) => {
                        let row = accounts
                            .get_mut(idx)
                            .expect("idx was just resolved against this same vec");
                        row.access_token = account.access_token.clone();
                        row.refresh_token = account.refresh_token.clone();
                        row.expires_at_ms = account.expires_at.map(oauth::normalize_expires_at);
                        // F4: fresh credentials clear a stuck error — `eligible`
                        // hard-gates on `status == Error` — but do NOT clear a
                        // rate-limit hold. That hold expires on its own
                        // (`MAX_RATE_LIMIT_HOLD_SECONDS`) and the next 429 re-arms
                        // it; clearing it here bought nothing but the ability to
                        // race a genuinely-still-limited account back into
                        // rotation early.
                        if row.status == AccountStatus::Error {
                            row.status = AccountStatus::Active;
                        }
                        // F6: the submitted type always wins — this route only
                        // ever carries a real credential, so a row whose stored
                        // type was stale (e.g. `"api"`) is corrected, or
                        // `refresh_plan` silently never refreshes it again.
                        // Identity fields are BACKFILLED — filled in only where
                        // the row does not already carry a value — mirroring
                        // `oauth::upsert_account`. Backfilling (rather than
                        // ignoring) is what permanently closes the split-brain
                        // trigger: once a legacy no-org row's org is filled in,
                        // it stops being loosely matched by every org variant of
                        // that person and starts requiring an exact org match,
                        // like any other fully-known account.
                        row.account_type = account.account_type.clone();
                        if row.account_uuid.is_none() {
                            row.account_uuid = account.account_uuid.clone();
                        }
                        if row.org_uuid.is_none() {
                            row.org_uuid = account.org_uuid.clone();
                        }
                        if row.org_name.is_none() {
                            row.org_name = account.org_name.clone();
                        }
                        // F5: carry the row's REAL routing state, not
                        // `identity::probe`'s None placeholders — see
                        // `persist_replaced`'s doc-comment for the restart-un-bench
                        // this replaces.
                        let target = config::Account {
                            name: row.name.clone(),
                            account_type: row.account_type.clone(),
                            account_uuid: row.account_uuid.clone(),
                            org_uuid: row.org_uuid.clone(),
                            org_name: row.org_name.clone(),
                            access_token: String::new(),
                            refresh_token: None,
                            expires_at: None,
                            priority: Some(row.priority),
                            switch_threshold: row.switch_threshold,
                            disabled: row.disabled.then_some(true),
                            // Same "carry the row's REAL routing state" rule as
                            // priority/switch_threshold/disabled above: group labels
                            // are configured routing state, not identity, so a
                            // credential re-add/refresh must not silently clear them.
                            groups: (!row.groups.is_empty()).then(|| row.groups.clone()),
                            extra: serde_json::Map::new(),
                        };
                        Resolution::Updated {
                            idx,
                            name: row.name.clone(),
                            target: Box::new(target),
                        }
                    }
                    None => match speculative_runtime.take() {
                        // No runtime ready yet — mutate nothing, report it, and
                        // let the loop below build one with no lock held before
                        // retrying. Safe to abandon this attempt outright: it
                        // has not touched `accounts`, `self.config`, or disk.
                        None => Resolution::NeedsBuild,
                        Some(mut runtime) => {
                            // A follow-up regression on this arm (distinct from the
                            // TOCTOU fix this function is named for above): an
                            // appended account with no explicit priority must join
                            // the BACK of the fleet — `max(existing priorities) + 1`
                            // — not the 0 that `AccountRuntime::from_config`'s
                            // `unwrap_or(0)` reads from an absent priority, which
                            // would silently promote it to the PRIMARY tier ahead of
                            // the established fleet. Computed from `accounts` (the
                            // LIVE rotation, not the config snapshot) because this
                            // section already holds its write lock and it is
                            // authoritative for what is routing right now. Skipped
                            // when the caller submitted an explicit priority (the
                            // documented new-account case — see `AddAccountRequest`'s
                            // doc-comment in `proxy.rs`), which is never overridden.
                            // Mirrors the identical fix in `config::merge_account`'s
                            // Added arm.
                            if account.priority.is_none() {
                                let next_priority = accounts
                                    .iter()
                                    .map(|a| a.priority)
                                    .max()
                                    .map_or(0, |max| max + 1);
                                account.priority = Some(next_priority);
                                // Built on the PRIOR attempt, before `next_priority`
                                // was known — patch it in rather than rebuilding
                                // (which would call `build_serving_client()` again,
                                // under this same lock, resurrecting exactly the bug
                                // the loop above exists to avoid).
                                runtime.priority = next_priority;
                            }
                            accounts.push(runtime);
                            Resolution::Added {
                                idx: accounts.len() - 1,
                            }
                        }
                    },
                }
            };

            match resolution {
                Resolution::NeedsBuild => {
                    // Both locks are released here (self.accounts already went
                    // out of scope above; config_write is dropped explicitly)
                    // before the one call that can panic — see the doc-comment
                    // above `add_or_update_account`. Nothing was mutated on this
                    // attempt, so retrying from scratch is safe; the next
                    // attempt is guaranteed to have a runtime ready, so it
                    // cannot hit this arm again.
                    drop(_writing);
                    let http1_only = self.config.lock().expect("config lock poisoned").http1_only;
                    speculative_runtime = Some(AccountRuntime::from_config(&account, http1_only));
                    continue;
                }
                Resolution::Added { idx } => {
                    // Still sequential, never nested, with `self.accounts` (which
                    // is already released by this point) — the `self.config` lock
                    // here is a separate, short critical section, same discipline
                    // `add_account` documents. It runs nested inside
                    // `config_write` (held since this attempt's start, see the N2
                    // doc-comment above), which is what makes this whole arm
                    // atomic with respect to another concurrent call.
                    {
                        let mut config = self.config.lock().expect("config lock poisoned");
                        config.accounts.push(account.clone());
                    }
                    let name = account.name.clone();
                    let persist = self.persist_added(&account);
                    return AddAccountOutcome::Added { idx, name, persist };
                }
                Resolution::Updated { idx, name, target } => {
                    let persist = self.persist_replaced(idx, &target, &account);
                    return AddAccountOutcome::Updated { idx, name, persist };
                }
            }
        }
    }

    /// Durably persist a newly-appended account (see [`Self::add_account`]) to
    /// the config file. Same INV1 discipline as [`Self::persist_disabled`]: the
    /// whole `config::save_account` read-modify-write runs under
    /// `config_write`, never `self.config` — nothing here needs the in-memory
    /// snapshot, because [`Self::add_account`] already pushed `account` onto it.
    ///
    /// PRECONDITION: unlike every other `config_write` writer in this module,
    /// this one does NOT take the lock itself — its only caller,
    /// [`Self::add_or_update_account`], already holds it across the whole
    /// resolve-mutate-persist sequence (see that function's N2 doc-comment),
    /// and `Mutex` is not reentrant, so locking again here would deadlock.
    /// Never call this without `config_write` already held.
    fn persist_added(&self, account: &config::Account) -> AddPersist {
        let Some(path) = &self.config_path else {
            return AddPersist::NoConfigFile;
        };
        match config::save_account(path, account) {
            Ok(config::AccountWrite::Added) => {
                tracing::info!(account = %account.name, "persisted newly added account to config");
                AddPersist::Persisted
            }
            Ok(config::AccountWrite::Updated) => {
                // The on-disk document already carried this identity even
                // though the live rotation did not — an entry added to the
                // file by hand while the proxy ran, or a stale boot snapshot.
                // The fresh credentials still landed, just as an update to
                // that row rather than a new one.
                tracing::info!(
                    account = %account.name,
                    "an on-disk entry already carried this identity; its credentials were updated instead of appending a duplicate"
                );
                AddPersist::Persisted
            }
            Ok(config::AccountWrite::Ambiguous) => {
                tracing::warn!(
                    account = %account.name,
                    path = %path.display(),
                    "more than one config entry carries this account's identity; refusing to guess, so the new account will NOT survive a restart"
                );
                AddPersist::Ambiguous
            }
            Ok(config::AccountWrite::Unwritable) => {
                tracing::error!(
                    account = %account.name,
                    path = %path.display(),
                    "the config document's accounts key is not a JSON array; refusing to touch it"
                );
                AddPersist::WriteFailed
            }
            Err(err) => {
                tracing::error!(
                    error = %err,
                    account = %account.name,
                    path = %path.display(),
                    "failed to persist the newly added account to config"
                );
                AddPersist::WriteFailed
            }
        }
    }

    /// Durably persist replaced credentials for the account at live index `idx`,
    /// identified by `target` — its OWN stored identity (post-backfill), never
    /// the submitted request's (see [`Self::add_or_update_account`] for why).
    ///
    /// `target` also carries the row's REAL `priority`/`switch_threshold`/
    /// `disabled`, not `identity::probe`'s `None` placeholders. That matters
    /// only on the rare path below where no in-memory or on-disk entry carries
    /// this identity and one gets APPENDED from `target`: a placeholder-routing
    /// `target` would append a fresh, un-benched entry for an account that was
    /// deliberately disabled, so it would come back into rotation on the very
    /// next restart — the exact bug `persist_disabled` exists to prevent.
    ///
    /// PRECONDITION: same as [`Self::persist_added`] — does NOT take
    /// `config_write` itself; its only caller, [`Self::add_or_update_account`],
    /// already holds it. Never call this without `config_write` already held.
    fn persist_replaced(
        &self,
        idx: usize,
        target: &config::Account,
        fresh: &config::Account,
    ) -> AddPersist {
        let Some(path) = &self.config_path else {
            return AddPersist::NoConfigFile;
        };
        let mut for_disk = target.clone();
        for_disk.access_token = fresh.access_token.clone();
        for_disk.refresh_token = fresh.refresh_token.clone();
        for_disk.expires_at = fresh.expires_at;

        let outcome = config::save_account(path, &for_disk);
        // INV3, mirroring `persist_disabled`: touch the in-memory snapshot only
        // once the FILE actually carries the new credentials. Without this, the
        // snapshot `persist_tokens`/`persist_now` later clone and write back on
        // shutdown would still hold the OLD (just-replaced) token and stamp it
        // back over the file this call just fixed.
        if matches!(
            outcome,
            Ok(config::AccountWrite::Updated) | Ok(config::AccountWrite::Added)
        ) {
            let mut config = self.config.lock().expect("config lock poisoned");
            match crate::identity::resolve(config.accounts.iter().enumerate(), target) {
                crate::identity::Resolved::One(position) => {
                    if let Some(entry) = config.accounts.get_mut(position) {
                        entry.access_token = for_disk.access_token.clone();
                        entry.refresh_token = for_disk.refresh_token.clone();
                        entry.expires_at = for_disk.expires_at;
                    }
                }
                // No in-memory entry carries this identity either — the disk
                // row `save_account` just appended has no counterpart here.
                // Mirror the append so a later persist can still find it.
                crate::identity::Resolved::None => config.accounts.push(for_disk.clone()),
                // A tie the in-memory snapshot cannot break either; leave it
                // alone rather than guess which entry to overwrite.
                crate::identity::Resolved::Many => {}
            }
        }

        match outcome {
            Ok(config::AccountWrite::Updated) => {
                tracing::info!(account = %target.name, index = idx, "persisted replaced credentials to config");
                AddPersist::Persisted
            }
            Ok(config::AccountWrite::Added) => {
                tracing::warn!(
                    account = %target.name,
                    index = idx,
                    path = %path.display(),
                    "no config entry carried this account's identity; a fresh entry was appended"
                );
                AddPersist::Persisted
            }
            Ok(config::AccountWrite::Ambiguous) => {
                tracing::warn!(
                    account = %target.name,
                    index = idx,
                    path = %path.display(),
                    "more than one config entry carries this account's identity; refusing to guess, so the replaced credentials will NOT survive a restart"
                );
                AddPersist::Ambiguous
            }
            Ok(config::AccountWrite::Unwritable) => {
                tracing::error!(
                    account = %target.name,
                    index = idx,
                    path = %path.display(),
                    "the config document's accounts key is not a JSON array; refusing to touch it"
                );
                AddPersist::WriteFailed
            }
            Err(err) => {
                tracing::error!(
                    error = %err,
                    account = %target.name,
                    index = idx,
                    path = %path.display(),
                    "failed to persist replaced credentials to config"
                );
                AddPersist::WriteFailed
            }
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

    /// A rate-limit hold that clears while the pinned account's prompt cache is
    /// still warm — the SOFT case. The live value of a no-guidance transient park
    /// (`proxy::NO_GUIDANCE_HOLD_SECS`), which is the hold this fleet arms most.
    const SHORT_HOLD_SECS: i64 = 15;

    /// A hold that OUTLIVES the prompt cache — the only kind that is ACCOUNT-level
    /// death and may re-key a session. Derived from the threshold rather than
    /// written as a literal so that raising [`CACHE_WARM_HOLD_SECS`] can never
    /// silently turn a re-key test into a divert test.
    const LONG_HOLD_SECS: i64 = CACHE_WARM_HOLD_SECS + 60;

    /// `build_serving_client` must not panic on either branch of `http1_only` —
    /// h1 support is part of the reqwest/rustls TLS backend this crate already
    /// pulls in, but a builder method rejecting a combination it doesn't support
    /// is exactly the kind of thing that only shows up at `.build()`.
    ///
    /// This cannot assert the built client actually NEGOTIATES h1 on the wire:
    /// reqwest exposes no introspection on a built `Client` for its ALPN/HTTP
    /// version policy, and asserting anything less than that would only prove
    /// the bool reached this function's own argument — not that it reaches
    /// reqwest's TLS layer. See the PR report for the explicit statement of
    /// that gap.
    #[test]
    fn build_serving_client_builds_under_both_http1_only_settings() {
        let _off = build_serving_client(false);
        let _on = build_serving_client(true);
    }

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
            groups: None,
            extra: serde_json::Map::new(),
        }
    }

    /// Like [`account`], carrying group labels — for the `--group` prefer-routing
    /// tests.
    fn account_in_groups(name: &str, priority: i64, groups: &[&str]) -> Account {
        Account {
            groups: Some(groups.iter().map(|g| g.to_string()).collect()),
            ..account(name, priority)
        }
    }

    fn config_with(accounts: Vec<Account>) -> Config {
        Config {
            quarantined_accounts: Vec::new(),
            migrated_legacy_throttle: false,
            proxy: ProxyConfig::default(),
            upstream: "https://api.anthropic.com".to_string(),
            switch_threshold: 0.90,
            pacing: PacingConfig::default(),
            account_throttle: ThrottleConfig::default(),
            fleet_throttle: ThrottleConfig::default(),
            lock_account: None,
            control_account: None,
            control_reserve: 0.05,
            control_pooled: false,
            reset_urgency_tier_hours: 24,
            http1_only: false,
            accounts,
            group_settings: HashMap::new(),
            pricing: Default::default(),
            usage_retention_days: 90,
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
                    refresh_token: Some("fresh-refresh".to_string()),
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
                        refresh_token: Some("fresh-refresh".to_string()),
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
                    retry_after_secs: None,
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
                        retry_after_secs: None,
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

    struct RetryAfterProber {
        calls: Arc<AtomicUsize>,
    }

    impl UsageProber for RetryAfterProber {
        fn probe(&self, _access_token: String) -> ProbeFuture {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Err(ProbeError {
                    status: Some(429),
                    message: "Too Many Requests".into(),
                    retry_after_secs: Some(3600),
                })
            })
        }
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
                        retry_after_secs: None,
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

    /// A warmer that succeeds (never a `WarmError`) but returns EMPTY headers — no
    /// `anthropic-ratelimit-unified-5h-*` at all. Models a 200 response that
    /// carries none of the unified rate-limit headers (an intermediary that
    /// strips them, or a response shape the warm endpoint does not always emit):
    /// the "silence" case [`Manager::update_quota`]'s doc-comment distinguishes
    /// from evidence — a warm that latches nothing.
    struct HeaderlessWarmer;
    impl AccountWarmer for HeaderlessWarmer {
        fn warm(&self, _access_token: String, _upstream: String) -> WarmFuture {
            Box::pin(async { Ok(reqwest::header::HeaderMap::new()) })
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
    /// `quotaProbeSeconds` at its default (nonzero) — so without this, every keep-warm test
    /// below would be measuring the boot gate instead of the predicate it is
    /// actually about.
    ///
    /// `set_5h` now latches `quota_known` by itself (a response's 5h header is
    /// evidence about the window, and `update_quota` treats it as such), so calling
    /// this stays necessary only for the accounts a test does NOT drive with
    /// `set_5h` — including every account driven by `set_7d`, which reports no 5h
    /// window and so latches nothing. `probe_status` is still only ever set by a
    /// probe.
    fn mark_all_probed(manager: &Manager) {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        for account in accounts.iter_mut() {
            account.probe_status = ProbeStatus::Ok;
            account.quota_known = true;
        }
    }

    // ---- `--group` prefer-routing (Phase 1: PREFER, never `--only`) -----------

    /// The whole feature: with exactly one account carrying the requested group,
    /// an unpinned select with that group MUST land on it — even though the
    /// OTHER account would win the ordinary (no-preference) LRU/priority pick.
    /// Then, group-out the grouped account (via `disabled`, the simplest hard
    /// gate) and confirm the SAME select call FALLS BACK to the ungrouped
    /// account rather than returning `None` — the prefer-semantics that make
    /// Phase 1 different from the restricting `--only` of Phase 2.
    #[test]
    fn group_preference_prefers_then_falls_back_to_the_pool() {
        let grouped = account_in_groups("grouped", 5, &["codereview"]);
        let plain = account("plain", 0); // lower priority number sorts FIRST ordinarily
        let manager = build_manager(config_with(vec![grouped, plain]), lock_refresher());
        let now = OffsetDateTime::now_utc();

        // Sanity control: with NO group requested, priority ordering picks the
        // UNGROUPED account (index 1) — proves the group assertion below is
        // actually exercising the preference, not just priority order.
        assert_eq!(
            manager.select_with_group(&HashSet::new(), now, None, None, "/v1/messages", None, None),
            Some(1),
            "no group requested: ordinary priority order picks the ungrouped account"
        );

        // The whole feature: `--group codereview` prefers index 0 despite its
        // worse priority.
        assert_eq!(
            manager.select_with_group(
                &HashSet::new(),
                now,
                None,
                None,
                "/v1/messages",
                None,
                Some("codereview"),
            ),
            Some(0),
            "a group with capacity must be preferred over a better-priority ungrouped account"
        );

        // Gate the grouped account out entirely (disabled = hard-ineligible).
        {
            let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
            accounts[0].disabled = true;
        }

        // The prefer-semantics that distinguish Phase 1 from Phase 2's `--only`:
        // the group has no capacity, so the SAME request falls back to the whole
        // pool rather than failing.
        assert_eq!(
            manager.select_with_group(
                &HashSet::new(),
                now,
                None,
                None,
                "/v1/messages",
                None,
                Some("codereview"),
            ),
            Some(1),
            "group exhausted: must fall back to the whole pool, not return None"
        );
    }

    // ---- reserved groups (a reservation is not a preference) ------------------

    /// Like [`config_with`] but with the named groups marked `reserved` in
    /// `groupSettings` — mirrors [`config_with_lock`]/[`config_with_control`].
    fn config_with_reserved(accounts: Vec<Account>, reserved: &[&str]) -> Config {
        let mut config = config_with(accounts);
        for group in reserved {
            config.group_settings.insert(
                (*group).to_string(),
                crate::config::GroupSettings {
                    reserved: true,
                    allow_control_account: false,
                    color: None,
                    extra: serde_json::Map::new(),
                },
            );
        }
        config
    }

    /// Semantics test #1 (both directions): unrequested traffic cannot select
    /// an account in a reserved group, even when its priority would otherwise
    /// win — and the SAME account is reachable by unrequested traffic again
    /// once its group is not reserved.
    #[test]
    fn reserved_group_blocks_unrequested_traffic_both_directions() {
        let reserved_acct = account_in_groups("reserved-acct", 0, &["codereview"]);
        let plain = account("plain", 5); // worse priority — must lose ordinarily
        let manager = build_manager(
            config_with_reserved(vec![reserved_acct, plain], &["codereview"]),
            lock_refresher(),
        );
        let now = OffsetDateTime::now_utc();
        assert_eq!(
            manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
            Some(1),
            "reserved account excluded from unrequested traffic despite better priority"
        );

        // Direction 2: same accounts, group NOT reserved — priority wins again.
        let unreserved = build_manager(
            config_with(vec![
                account_in_groups("reserved-acct", 0, &["codereview"]),
                account("plain", 5),
            ]),
            lock_refresher(),
        );
        assert_eq!(
            unreserved.select(&HashSet::new(), now, None, None, "/v1/messages", None),
            Some(0),
            "same account, group unreserved: unrequested traffic can select it again"
        );
    }

    /// Semantics test #2: `--group codereview` can still select a reserved
    /// `codereview` account — reservation narrows UNREQUESTED traffic, never
    /// the group's own members.
    #[test]
    fn reserved_group_still_selectable_by_its_own_group_ask() {
        let reserved_acct = account_in_groups("reserved-acct", 0, &["codereview"]);
        let plain = account("plain", 5);
        let manager = build_manager(
            config_with_reserved(vec![reserved_acct, plain], &["codereview"]),
            lock_refresher(),
        );
        let now = OffsetDateTime::now_utc();
        assert_eq!(
            manager.select_with_group(
                &HashSet::new(),
                now,
                None,
                None,
                "/v1/messages",
                None,
                Some("codereview"),
            ),
            Some(0),
            "--group codereview still selects its own reserved account"
        );
    }

    /// Semantics test #2b: a RESERVED group is strict — when its members are
    /// all gated, the ask returns `None` instead of spilling into the pool.
    ///
    /// This is the second half of what `reserved` means. Reservation already
    /// keeps unrequested traffic OUT of the group
    /// ([`reserved_group_blocks_unrequested_traffic_both_directions`]); this
    /// keeps the group's own traffic IN. Without it, a reserved group's request
    /// silently lands on an account the operator deliberately walled off from
    /// it — measured on the live fleet 2026-09-01, 33 times in one day, every
    /// one `reason="all-members-unavailable"`, after pool traffic had
    /// rate-limited the group's only member.
    ///
    /// The contrast case is deliberately left to
    /// [`group_preference_prefers_then_falls_back_to_the_pool`]: an
    /// UNRESERVED group keeps the prefer-and-spill behaviour unchanged, so
    /// strictness rides entirely on `reserved` and needs no second flag.
    #[test]
    fn a_reserved_group_never_falls_back_to_the_pool() {
        let reserved_acct = account_in_groups("reserved-acct", 5, &["codereview"]);
        let plain = account("plain", 0); // better priority — would win any pool pick
        let manager = build_manager(
            config_with_reserved(vec![reserved_acct, plain], &["codereview"]),
            lock_refresher(),
        );
        let now = OffsetDateTime::now_utc();

        // Gate the group's only member out entirely (disabled = hard-ineligible),
        // exactly as `group_preference_prefers_then_falls_back_to_the_pool` does.
        {
            let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
            accounts[0].disabled = true;
        }

        // POSITIVE CONTROL: the pool itself is still perfectly servable. Without
        // this, the `None` below would also be satisfied by a fleet where nothing
        // at all could be picked — proving the assertion about strictness rather
        // than about an empty pool.
        assert_eq!(
            manager.select_with_group(&HashSet::new(), now, None, None, "/v1/messages", None, None),
            Some(1),
            "control: the ungrouped pool is servable, so a None below is strictness, not exhaustion"
        );

        assert_eq!(
            manager.select_with_group(
                &HashSet::new(),
                now,
                None,
                None,
                "/v1/messages",
                None,
                Some("codereview"),
            ),
            None,
            "a reserved group must never spill into the pool — refuse and let the \
             caller's exhaustion ladder soft-wait or answer an honest 429"
        );
    }

    /// A strict group's retry hint is sized by when one of ITS members frees,
    /// not by whichever account in the fleet un-gates first.
    ///
    /// The fleet-wide [`Manager::retry_after_hint`] minimises over every
    /// account, so an unrelated account recovering sooner wins. For a strict
    /// `--group` request that number is a promise about capacity the request is
    /// not allowed to use: the caller spends its one-shot soft-wait, wakes to
    /// find its own member still held, and answers a 429 that was avoidable.
    /// The member here is held 10x longer than the non-member precisely so the
    /// two hints cannot coincide by accident.
    #[test]
    fn a_strict_groups_retry_hint_is_sized_by_its_own_member() {
        let member = account_in_groups("member", 0, &["codereview"]);
        let outsider = account("outsider", 1);
        let manager = build_manager(
            config_with_reserved(vec![member, outsider], &["codereview"]),
            lock_refresher(),
        );
        let now = OffsetDateTime::now_utc();

        manager.mark_rate_limited(0, 600); // the group's member
        manager.mark_rate_limited(1, 60); // an unrelated account, free much sooner

        let fleet = manager.retry_after_hint(now, false);
        let scoped = manager.retry_after_hint_for_group(now, false, "codereview");

        // Control: the fleet-wide hint really is won by the outsider, so the
        // assertion below is measuring the group scoping and not a tautology.
        assert!(
            fleet <= 60,
            "control: fleet-wide hint should follow the sooner outsider, got {fleet}s"
        );
        assert!(
            scoped > 500,
            "a strict group's hint must follow its own held member (~600s), got {scoped}s"
        );
    }

    /// Semantics test #3: an account in reserved `codereview` + plain `dev` is
    /// reachable by BOTH `--group` values and NOT by unrequested traffic.
    #[test]
    fn reserved_plus_plain_group_reachable_by_either_ask_not_unrequested() {
        let multi = account_in_groups("multi", 0, &["codereview", "dev"]);
        let plain = account("plain", 5);
        let manager = build_manager(
            config_with_reserved(vec![multi, plain], &["codereview"]),
            lock_refresher(),
        );
        let now = OffsetDateTime::now_utc();
        assert_eq!(
            manager.select_with_group(
                &HashSet::new(),
                now,
                None,
                None,
                "/v1/messages",
                None,
                Some("codereview"),
            ),
            Some(0),
            "reachable via its reserved group"
        );
        assert_eq!(
            manager.select_with_group(
                &HashSet::new(),
                now,
                None,
                None,
                "/v1/messages",
                None,
                Some("dev"),
            ),
            Some(0),
            "reachable via its plain group too"
        );
        assert_eq!(
            manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
            Some(1),
            "unrequested traffic still excluded — one reserved group is enough"
        );
    }

    // ---- control-account opt-in groups (`allowControlAccount`) ----------------

    /// Like [`config_with_reserved`] but sets `allowControlAccount` instead —
    /// combined with [`config_with_control`]'s `control_account` set so the
    /// fixture is actually control-only, not just labelled.
    fn config_with_control_allowed(
        accounts: Vec<Account>,
        control: &str,
        groups: &[&str],
    ) -> Config {
        let mut config = config_with_control(accounts, control);
        for group in groups {
            config.group_settings.insert(
                (*group).to_string(),
                crate::config::GroupSettings {
                    reserved: false,
                    allow_control_account: true,
                    color: None,
                    extra: serde_json::Map::new(),
                },
            );
        }
        config
    }

    /// The whole feature, both directions: a group whose only member is the
    /// control account cannot serve `--group research` inference by default —
    /// but the SAME fixture, with `allowControlAccount` set on `research`,
    /// serves it.
    #[test]
    fn control_only_group_serves_inference_only_once_opted_in() {
        let control = account_in_groups("control-acct", 0, &["research"]);
        let other = account("other", 5);

        let blocked = build_manager(
            config_with_control(vec![control.clone(), other.clone()], "control-acct"),
            lock_refresher(),
        );
        let now = OffsetDateTime::now_utc();
        assert_eq!(
            blocked.select_with_group(
                &HashSet::new(),
                now,
                None,
                None,
                "/v1/messages",
                None,
                Some("research"),
            ),
            Some(1),
            "not opted in: falls back to the whole pool, never the control account"
        );

        let mut opted_in_config = config_with_control(vec![control, other], "control-acct");
        opted_in_config.group_settings.insert(
            "research".to_string(),
            crate::config::GroupSettings {
                reserved: false,
                allow_control_account: true,
                color: None,
                extra: serde_json::Map::new(),
            },
        );
        let opted_in = build_manager(opted_in_config, lock_refresher());
        assert_eq!(
            opted_in.select_with_group(
                &HashSet::new(),
                now,
                None,
                None,
                "/v1/messages",
                None,
                Some("research"),
            ),
            Some(0),
            "opted in: the group's own `--group` ask may now select the control account"
        );
    }

    /// The opt-in is scoped to the GROUP, not to the control account globally:
    /// an unrequested (no `--group`) inference request must still never select
    /// the control account, even when its only group has opted in.
    #[test]
    fn control_opt_in_does_not_affect_unrequested_traffic() {
        let control = account_in_groups("control-acct", 0, &["research"]);
        let other = account("other", 5);
        let manager = build_manager(
            config_with_control_allowed(vec![control, other], "control-acct", &["research"]),
            lock_refresher(),
        );
        let now = OffsetDateTime::now_utc();
        assert_eq!(
            manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
            Some(1),
            "unrequested traffic must still never select the control account"
        );
    }

    /// Semantics test #4 — THE test that proves the feature is real: a
    /// session already pinned to an account whose group becomes reserved
    /// (the on-disk-pin-predates-the-reservation case the bridge names) is
    /// re-pinned away on its next UNREQUESTED request, not left serving the
    /// reserved account forever. Seeds the pin directly into `manager.affinity`
    /// — the shape a pin restored from disk at boot takes, exactly the
    /// scenario the bridge's "reachable by a proxy restart or by a pin that
    /// predates the reservation" sentence describes.
    #[test]
    fn pin_reclaim_when_the_pinned_accounts_group_is_reserved() {
        let acct = account_in_groups("acct", 0, &["codereview"]);
        let other = account("other", 5);
        let manager = build_manager(
            config_with_reserved(vec![acct, other], &["codereview"]),
            lock_refresher(),
        );
        let now = OffsetDateTime::now_utc();
        let now_ms = odt_to_ms(now);
        let key = 4242u64;
        {
            let mut affinity = manager.affinity.lock().expect("affinity lock poisoned");
            affinity.insert(key, (0, now_ms));
        }

        let served = manager
            .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
            .expect("the plain account is still eligible");
        assert_eq!(
            served, 1,
            "a reserved account must not keep serving an unrequested pinned session"
        );

        // The pin itself must have MOVED — "reservation is not a preference":
        // an ordinary per-request divert would leave the OLD index pinned and
        // only serve elsewhere for this one request; that is NOT enough here.
        let pinned_now = manager
            .affinity
            .lock()
            .expect("affinity lock poisoned")
            .get(&key)
            .map(|&(idx, _)| idx);
        assert_eq!(
            pinned_now,
            Some(1),
            "the pin itself must move off the reserved account, not merely divert one request"
        );
    }

    // ---- live group reload (problem 1: group edits appear to do nothing) ------

    /// A `Config` with one account per `(name, priority, groups)` tuple —
    /// `groups = &[]` means no `groups` key, matching [`account`]/
    /// [`account_in_groups`]'s own split. Shared setup for every reload test
    /// below: called once to build the manager's BOOT config, and called
    /// again (saved straight to `path`, never through the manager) to
    /// simulate an out-of-band edit — a hand edit, or a sibling `tcr group`
    /// invocation — while the manager keeps running.
    fn reload_config(accounts: &[(&str, i64, &[&str])]) -> Config {
        let built: Vec<Account> = accounts
            .iter()
            .map(|&(name, priority, groups)| {
                if groups.is_empty() {
                    account(name, priority)
                } else {
                    account_in_groups(name, priority, groups)
                }
            })
            .collect();
        config_with(built)
    }

    /// THE test for problem 1: editing a temp config's group membership and
    /// touching its mtime makes a SUBSEQUENT snapshot report the new groups —
    /// without rebuilding the `Manager`. If only one test in this section
    /// survives, this is it.
    #[test]
    fn snapshot_picks_up_a_group_edit_without_rebuilding_the_manager() {
        let path = tmp_config_path("reload-feature");
        let boot = reload_config(&[("acct", 0, &["dev"])]);
        config::save(&path, &boot).expect("write initial reload config");
        let manager = build_manager_with_path(boot, path.clone());

        let before = manager.snapshot(OffsetDateTime::now_utc());
        assert_eq!(before.accounts[0].groups, vec!["dev".to_string()]);

        // A fast filesystem can produce two writes with the SAME mtime at
        // whole-second resolution; sleep past that so the edit is actually
        // observable via mtime, the same concern any mtime-triggered reload
        // design has.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let edited = reload_config(&[("acct", 0, &["ops", "burst"])]);
        config::save(&path, &edited).expect("write edited reload config");

        let after = manager.snapshot(OffsetDateTime::now_utc());
        let mut groups = after.accounts[0].groups.clone();
        groups.sort();
        assert_eq!(
            groups,
            vec!["burst".to_string(), "ops".to_string()],
            "a snapshot after the file changed must report the NEW groups, without \
             rebuilding the Manager"
        );

        std::fs::remove_file(&path).ok();
    }

    /// A reload does NOT disturb accounts, credentials or tokens — only
    /// `groups`/`groupSettings` move. Rewrites the file with the SAME account
    /// identity (name match), a DIFFERENT access token, AND a group change in
    /// one write, then asserts the in-memory access token is untouched while
    /// the group DID move — proving the scope, not just the happy path.
    #[test]
    fn reload_never_touches_accounts_credentials_or_tokens() {
        let path = tmp_config_path("reload-scope");
        let boot = reload_config(&[("acct", 0, &["dev"])]);
        config::save(&path, &boot).expect("write initial reload config");
        let manager = build_manager_with_path(boot, path.clone());

        let original_token = {
            let accounts = manager.accounts.read().expect("accounts lock poisoned");
            accounts[0].access_token.clone()
        };

        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut tampered = reload_config(&[("acct", 0, &["ops"])]);
        tampered.accounts[0].access_token = "at-DIFFERENT-token-must-not-apply".to_string();
        config::save(&path, &tampered).expect("write tampered reload config");

        manager.reload_groups_if_changed();

        let accounts = manager.accounts.read().expect("accounts lock poisoned");
        assert_eq!(
            accounts[0].access_token, original_token,
            "reload must never touch the access token — only group fields"
        );
        assert_eq!(
            accounts[0].groups,
            vec!["ops".to_string()],
            "the group change in the SAME file write must still apply"
        );
        drop(accounts);
        std::fs::remove_file(&path).ok();
    }

    /// A malformed config at reload time keeps the previous in-memory groups
    /// (never blanks the fleet) and does not advance the remembered mtime, so
    /// a subsequent FIX self-heals without the file needing to change again
    /// after that.
    #[test]
    fn reload_keeps_current_groups_on_a_malformed_config() {
        let path = tmp_config_path("reload-malformed");
        let boot = reload_config(&[("acct", 0, &["dev"])]);
        config::save(&path, &boot).expect("write initial reload config");
        let manager = build_manager_with_path(boot, path.clone());

        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, b"{ this is not valid json").expect("write malformed config");
        manager.reload_groups_if_changed();

        let accounts = manager.accounts.read().expect("accounts lock poisoned");
        assert_eq!(
            accounts[0].groups,
            vec!["dev".to_string()],
            "a malformed config at reload time must keep the previous groups, not blank them"
        );
        drop(accounts);

        // Self-heal: fixing the file (mtime necessarily advances again, since
        // this write follows the malformed one) makes the NEXT reload apply.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let fixed = reload_config(&[("acct", 0, &["fixed"])]);
        config::save(&path, &fixed).expect("write fixed reload config");
        manager.reload_groups_if_changed();
        let accounts = manager.accounts.read().expect("accounts lock poisoned");
        assert_eq!(accounts[0].groups, vec!["fixed".to_string()]);
        drop(accounts);
        std::fs::remove_file(&path).ok();
    }

    /// A session pinned to an account that becomes reserved VIA RELOAD (not
    /// construction-time, unlike
    /// [`pin_reclaim_when_the_pinned_accounts_group_is_reserved`]) is
    /// re-keyed on its next unrequested `select`.
    #[test]
    fn reload_re_keys_a_pin_when_its_group_becomes_reserved() {
        let path = tmp_config_path("reload-pin-reclaim");
        let boot = reload_config(&[("acct", 0, &["codereview"]), ("other", 5, &[])]);
        config::save(&path, &boot).expect("write initial reload config");
        let manager = build_manager_with_path(boot, path.clone());

        let now = OffsetDateTime::now_utc();
        let now_ms = odt_to_ms(now);
        let key = 9191u64;
        {
            let mut affinity = manager.affinity.lock().expect("affinity lock poisoned");
            affinity.insert(key, (0, now_ms));
        }

        // Reserve 'codereview' via a fresh file write — no construction-time
        // reservation at all, unlike the sibling test above.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let reserved_config = config_with_reserved(
            vec![
                account_in_groups("acct", 0, &["codereview"]),
                account("other", 5),
            ],
            &["codereview"],
        );
        config::save(&path, &reserved_config).expect("write reserved reload config");

        let served = manager
            .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
            .expect("the plain account is still eligible");
        assert_eq!(
            served, 1,
            "a group reserved via RELOAD must not keep serving an unrequested pinned session"
        );
        let pinned_now = manager
            .affinity
            .lock()
            .expect("affinity lock poisoned")
            .get(&key)
            .map(|&(idx, _)| idx);
        assert_eq!(
            pinned_now,
            Some(1),
            "the pin must move off the newly-reserved account, not merely divert one request"
        );

        std::fs::remove_file(&path).ok();
    }

    /// Same live-reload cadence as [`reload_re_keys_a_pin_when_its_group_becomes_reserved`],
    /// applied to `allowControlAccount`: a control-only group cannot serve its
    /// own `--group` ask at boot, and a config write that opts it in — with NO
    /// restart — makes the very same request land on the control account on
    /// its next call.
    #[test]
    fn reload_picks_up_a_group_opting_in_to_the_control_account() {
        let path = tmp_config_path("reload-control-allowed");
        let boot = config_with_control(
            vec![
                account_in_groups("control-acct", 0, &["research"]),
                account("other", 5),
            ],
            "control-acct",
        );
        config::save(&path, &boot).expect("write initial reload config");
        let manager = build_manager_with_path(boot, path.clone());

        let now = OffsetDateTime::now_utc();
        assert_eq!(
            manager.select_with_group(
                &HashSet::new(),
                now,
                None,
                None,
                "/v1/messages",
                None,
                Some("research"),
            ),
            Some(1),
            "not opted in at boot: falls back to the whole pool"
        );

        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut opted_in = config_with_control(
            vec![
                account_in_groups("control-acct", 0, &["research"]),
                account("other", 5),
            ],
            "control-acct",
        );
        opted_in.group_settings.insert(
            "research".to_string(),
            crate::config::GroupSettings {
                reserved: false,
                allow_control_account: true,
                color: None,
                extra: serde_json::Map::new(),
            },
        );
        config::save(&path, &opted_in).expect("write opted-in reload config");

        assert_eq!(
            manager.select_with_group(
                &HashSet::new(),
                now,
                None,
                None,
                "/v1/messages",
                None,
                Some("research"),
            ),
            Some(0),
            "a live opt-in write must be picked up with no restart"
        );

        std::fs::remove_file(&path).ok();
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

    fn config_with_control(accounts: Vec<Account>, control: &str) -> Config {
        let mut config = config_with(accounts);
        config.control_account = Some(control.to_string());
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
        assert_eq!(
            manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
            Some(1)
        );
        // An affinity key returns the SAME locked account (lock ignores affinity).
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(42), "/v1/messages", None),
            Some(1)
        );
        // Bias the LRU toward index 0 by pinning affinity elsewhere first — lock
        // still wins.
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(7), "/v1/messages", None),
            Some(1)
        );
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
        assert_eq!(
            manager.select(&tried, now, None, None, "/v1/messages", None),
            None
        );
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
        assert_eq!(
            manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
            Some(0)
        );
    }

    /// `assemble` resolves the configured `controlAccount` name to its account
    /// index, mirroring `assemble_resolves_lock_name`; a name that matches no
    /// account resolves to `None` (runs with no control account, not a panic).
    #[test]
    fn assemble_resolves_control_name() {
        let matched = build_manager(
            config_with_control(vec![account("a", 0), account("b", 0), account("c", 0)], "c"),
            lock_refresher(),
        );
        assert_eq!(matched.control(), Some(2));

        let unmatched = build_manager(
            config_with_control(vec![account("a", 0), account("b", 0)], "ghost"),
            lock_refresher(),
        );
        assert_eq!(unmatched.control(), None);
    }

    /// Absent `controlAccount` → `control() == None`.
    #[test]
    fn control_absent_default_leaves_control_none() {
        let manager = build_manager(config_with(vec![account("solo", 0)]), lock_refresher());
        assert_eq!(manager.control(), None);
    }

    /// PART 1's invariant, NARROWED by part 2 (deliberately — see the module
    /// doc for the three-way split): setting a control account must not change
    /// which account `select` returns **for inference traffic on the realistic
    /// deployment shape, where the control account stays `disabled`** (the
    /// documented default — see `Manager::control_idx`'s doc). Part 1 asserted
    /// this unconditionally, because part 1 shipped no routing at all and the
    /// control account was inert everywhere.
    ///
    /// That is no longer strictly true in general: part 2 permanently excludes
    /// the control account from inference candidacy REGARDLESS of `disabled`
    /// (see [`inference_never_goes_to_control_even_when_unpinned`], which
    /// deliberately leaves control ENABLED and asserts inference still skips
    /// it) — so on a tiny fleet where the enabled control account would
    /// otherwise have been the natural LRU pick, inference selection DOES
    /// change the moment `disabled` is lifted. This test is narrowed to the
    /// disabled case (`set_disabled(0, true)` below) where the exclusion is
    /// provably redundant with `eligible`'s own disabled check, so the
    /// original "byte-identical" claim keeps holding exactly where it matters
    /// operationally. [`control_preference_prefers_control_for_non_inference_paths`]
    /// covers the (expected) non-inference side changing.
    #[test]
    fn setting_a_control_account_does_not_change_selection() {
        let without_control = build_manager(
            config_with(vec![account("a", 0), account("b", 0)]),
            lock_refresher(),
        );
        let with_control = build_manager(
            config_with_control(vec![account("a", 0), account("b", 0)], "a"),
            lock_refresher(),
        );
        // Realistic deployment shape: the control account stays `disabled` —
        // see the doc-comment above for why this is what keeps the invariant
        // honestly true rather than coincidentally true. Disabled on BOTH
        // managers, isolating the effect of `controlAccount` itself: the
        // baseline already excludes account 0 the ordinary way (a plain
        // `disabled` account, unrelated to control), so any DIFFERENCE
        // between the two would be attributable to the control preference,
        // not to `disabled` doing double duty.
        without_control.set_disabled(0, true);
        with_control.set_disabled(0, true);
        assert_eq!(with_control.control(), Some(0));

        let now = OffsetDateTime::now_utc();
        for _ in 0..20 {
            let picked_without =
                without_control.select(&HashSet::new(), now, None, None, "/v1/messages", None);
            let picked_with =
                with_control.select(&HashSet::new(), now, None, None, "/v1/messages", None);
            assert_eq!(
                picked_without, picked_with,
                "a control account must not change which account select() returns for inference traffic"
            );
            if let Some(idx) = picked_without {
                without_control.set_current(idx);
                with_control.set_current(idx);
            }
        }
    }

    /// The other half of the narrowing above: an UNPINNED, non-inference,
    /// non-noise request (the identity/control plane) DOES change the pick —
    /// it now prefers the control account, byte-different from `without_control`.
    #[test]
    fn control_preference_prefers_control_for_non_inference_paths() {
        let without_control = build_manager(
            config_with(vec![account("a", 0), account("b", 0)]),
            lock_refresher(),
        );
        let with_control = build_manager(
            config_with_control(vec![account("a", 0), account("b", 0)], "b"),
            lock_refresher(),
        );
        assert_eq!(with_control.control(), Some(1));

        let now = OffsetDateTime::now_utc();
        let picked_without =
            without_control.select(&HashSet::new(), now, None, None, "/api/organizations", None);
        let picked_with =
            with_control.select(&HashSet::new(), now, None, None, "/api/organizations", None);
        assert_eq!(
            picked_without,
            Some(0),
            "no control account: ordinary LRU picks index 0"
        );
        assert_eq!(
            picked_with,
            Some(1),
            "a control account IS preferred for an unpinned identity-plane request"
        );
    }

    // ---- control-account routing (part 2 — see docs/plans/control-routing-bridge-coder.md) ----

    /// Invariant 1, the one that matters: an EXISTING pin is never re-keyed by
    /// the control preference, even after control is set at runtime.
    #[test]
    fn control_preference_never_moves_an_existing_pin() {
        let manager = build_manager(
            config_with(vec![account("pool", 0), account("ctrl", 0)]),
            lock_refresher(),
        );
        let now = OffsetDateTime::now_utc();
        let key = 555u64;
        // Pin session `key` to account 0 (pool) BEFORE any control account
        // exists — an ordinary identity-plane select, no preference in play.
        let pinned = manager
            .select(
                &HashSet::new(),
                now,
                None,
                Some(key),
                "/api/organizations",
                None,
            )
            .expect("an account is eligible");
        assert_eq!(pinned, 0);

        // Set the control account to `ctrl` (index 1) at runtime.
        manager.set_control_by_query(Some("ctrl"), None);
        assert_eq!(manager.control(), Some(1));

        // The SAME session's next identity-plane request must still return its
        // EXISTING pin (0), never the newly-preferred control account (1).
        for _ in 0..5 {
            assert_eq!(
                manager.select(
                    &HashSet::new(),
                    now,
                    None,
                    Some(key),
                    "/api/organizations",
                    None
                ),
                Some(0),
                "an existing pin must never be re-keyed by the control preference"
            );
        }
    }

    /// The `keep_pin` half of invariant 1, distinct from the test above: a pin
    /// that is only DIVERTED for one request (it already failed upstream —
    /// `tried` — but the ACCOUNT itself is still alive) must not be re-keyed to
    /// the control account either. The test above never actually reaches the
    /// control overlay (a clean, servable pin returns from the affinity
    /// fast-path first); this one forces a divert so `keep_pin` really is
    /// `Some(_)` when the overlay's `keep_pin.is_none()` gate is checked.
    #[test]
    fn control_preference_never_moves_a_diverted_pin() {
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0), account("c", 0)]),
            lock_refresher(),
        );
        let now = OffsetDateTime::now_utc();
        let key = 777u64;
        let pinned = manager
            .select(
                &HashSet::new(),
                now,
                None,
                Some(key),
                "/api/organizations",
                None,
            )
            .expect("an account is eligible");
        assert_eq!(pinned, 0, "the first pick, tie-broken by index, is \"a\"");

        // Set control to "c" (index 2) — a DIFFERENT account — after the pin
        // already exists.
        manager.set_control_by_query(Some("c"), None);
        assert_eq!(manager.control(), Some(2));

        // Bias ordinary LRU firmly toward "b" (index 1): stamp "c" (control)
        // as ALREADY selected, so its LRU key sorts strictly AFTER "b"'s
        // never-selected key. If the divert incorrectly routed straight to
        // control (the hoisting bug this test exists to catch — the overlay
        // reached ahead of the `keep_pin` gate), it would land on "c"
        // regardless of this bias, because it never consults ordinary LRU at
        // all for a control-preferred pick.
        {
            let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
            accounts[2].last_selected_seq = 999;
        }

        // This ONE request diverts — the pinned account already failed it.
        let mut tried = HashSet::new();
        tried.insert(pinned);
        let served = manager
            .select(&tried, now, None, Some(key), "/api/organizations", None)
            .expect("a divert target is eligible");
        assert_eq!(
            served, 1,
            "a diverted pinned request must go through ORDINARY rotation (picking \"b\"), \
             not straight to the control account"
        );

        // A clean retry must still return the ORIGINAL pin, never re-keyed to
        // the control account by the divert.
        let rechecked = manager
            .select(
                &HashSet::new(),
                now,
                None,
                Some(key),
                "/api/organizations",
                None,
            )
            .expect("an account is eligible");
        assert_eq!(
            rechecked, pinned,
            "a per-request divert must never re-key the session's pin to the control account"
        );
    }

    /// §1 no-op guard: the control account ships `disabled` (out of the
    /// inference rotation, by design — see the module doc), and the control
    /// preference must BYPASS that gate. If `select` used `eligible` or
    /// `account_hard_ok` here instead of `Self::control_eligible`, this test
    /// fails silently useful: it would still compile and the feature would
    /// simply never fire (the single most likely way to get this wrong).
    #[test]
    fn control_is_picked_even_though_disabled() {
        let manager = build_manager(
            config_with_control(vec![account("pool", 0), account("ctrl", 0)], "ctrl"),
            lock_refresher(),
        );
        manager.set_disabled(1, true);
        assert_eq!(manager.control(), Some(1));
        let now = OffsetDateTime::now_utc();
        assert_eq!(
            manager.select(&HashSet::new(), now, None, None, "/api/organizations", None),
            Some(1),
            "a disabled control account must still be picked for an identity-plane request"
        );
    }

    #[test]
    fn control_is_not_picked_when_errored() {
        let manager = build_manager(
            config_with_control(vec![account("pool", 0), account("ctrl", 0)], "ctrl"),
            lock_refresher(),
        );
        manager.set_disabled(1, true);
        {
            let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
            accounts[1].status = AccountStatus::Error;
        }
        let now = OffsetDateTime::now_utc();
        assert_eq!(
            manager.select(&HashSet::new(), now, None, None, "/api/organizations", None),
            Some(0),
            "an ERRORED control account must degrade to normal rotation"
        );
    }

    #[test]
    fn control_is_not_picked_when_rejected() {
        let manager = build_manager(
            config_with_control(vec![account("pool", 0), account("ctrl", 0)], "ctrl"),
            lock_refresher(),
        );
        manager.set_disabled(1, true);
        {
            let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
            accounts[1].quota.status = Some("rejected".to_string());
        }
        let now = OffsetDateTime::now_utc();
        assert_eq!(
            manager.select(&HashSet::new(), now, None, None, "/api/organizations", None),
            Some(0),
            "a REJECTED control account must degrade to normal rotation"
        );
    }

    #[test]
    fn control_is_not_picked_when_rate_limited() {
        let manager = build_manager(
            config_with_control(vec![account("pool", 0), account("ctrl", 0)], "ctrl"),
            lock_refresher(),
        );
        manager.set_disabled(1, true);
        let now = OffsetDateTime::now_utc();
        let now_ms = odt_to_ms(now);
        {
            let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
            accounts[1].rate_limited_until_ms = Some(now_ms + 60_000);
        }
        assert_eq!(
            manager.select(&HashSet::new(), now, None, None, "/api/organizations", None),
            Some(0),
            "a LIVE-held control account must degrade to normal rotation"
        );
    }

    /// Inference must NEVER select the control account — including UNPINNED
    /// and with control left ENABLED (pooled), where `eligible` alone would
    /// happily pick it. Only the dedicated pool-pick exclusion stops it.
    #[test]
    fn inference_never_goes_to_control_even_when_unpinned() {
        let manager = build_manager(
            config_with_control(vec![account("ctrl", 0), account("pool", 0)], "ctrl"),
            lock_refresher(),
        );
        assert_eq!(manager.control(), Some(0));
        let now = OffsetDateTime::now_utc();
        for _ in 0..10 {
            assert_eq!(
                manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
                Some(1),
                "inference must route to the pool, never the control account"
            );
        }
    }

    /// `controlPooled` lifts the exclusion the test above pins down: the same
    /// unpinned inference now reaches the control account, because
    /// `select_with_group` stops force-adding the control index to `tried`.
    /// Asserts REACHABILITY, not a fixed index — LRU decides the order, and
    /// pinning the order here would be testing the tiebreak, not the flag.
    #[test]
    fn pooled_control_account_takes_inference() {
        let mut config = config_with_control(vec![account("ctrl", 0), account("pool", 0)], "ctrl");
        config.control_pooled = true;
        let manager = build_manager(config, lock_refresher());
        assert_eq!(manager.control(), Some(0));
        let now = OffsetDateTime::now_utc();
        let reached_control = (0..10).any(|_| {
            manager.select(&HashSet::new(), now, None, None, "/v1/messages", None) == Some(0)
        });
        assert!(
            reached_control,
            "controlPooled must let an inference pick reach the control account"
        );
    }

    /// The guard the whole opt-in rests on, and the reason `controlPooled` is
    /// not simply "delete the exclusion": a POOLED control account is held out
    /// at `switchThreshold - controlReserve` (0.90 - 0.05 = 0.85), NOT at the
    /// full threshold every other account gets.
    ///
    /// 0.86 is chosen to sit BELOW the full 0.90 on purpose — `eligible` still
    /// passes this account, so the only thing that can hold it back is
    /// [`super::select::Manager::pool_pick_respects_control_reserve`]. Raise it
    /// to 0.90 and the test would pass for the wrong reason.
    #[test]
    fn pooled_control_account_is_held_out_by_the_reserve() {
        let mut config = config_with_control(vec![account("ctrl", 0), account("pool", 0)], "ctrl");
        config.control_pooled = true;
        let manager = build_manager(config, lock_refresher());
        let now = OffsetDateTime::now_utc();
        {
            let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
            accounts[0].quota.five_hour = Some(crate::quota::QuotaWindow {
                utilization: 0.86,
                reset: Some(now + time::Duration::seconds(300)),
            });
        }
        for _ in 0..10 {
            assert_eq!(
                manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
                Some(1),
                "a pooled control account past threshold-reserve must be held out, \
                 leaving the identity plane the headroom the reserve exists to keep"
            );
        }
    }

    /// §2: a NOISE request reuses whichever account this connection already
    /// served.
    #[test]
    fn noise_path_follows_its_connection() {
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0), account("c", 0)]),
            lock_refresher(),
        );
        let now = OffsetDateTime::now_utc();
        let conn = 42u64;
        let first = manager
            .select(
                &HashSet::new(),
                now,
                None,
                None,
                "/api/event_logging/v2/batch",
                Some(conn),
            )
            .expect("an account is eligible");
        for _ in 0..5 {
            assert_eq!(
                manager.select(
                    &HashSet::new(),
                    now,
                    None,
                    None,
                    "/api/event_logging/v2/batch",
                    Some(conn)
                ),
                Some(first),
                "noise traffic on the same connection must stay on the same account"
            );
        }
    }

    /// §2 fallback: once the connection's account is no longer eligible,
    /// normal rotation takes over AND `conn_affinity` is re-recorded to the
    /// new winner (not left stale on the held account).
    #[test]
    fn noise_path_falls_back_when_its_account_is_held() {
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0)]),
            lock_refresher(),
        );
        let now = OffsetDateTime::now_utc();
        let now_ms = odt_to_ms(now);
        let conn = 7u64;
        let first = manager
            .select(
                &HashSet::new(),
                now,
                None,
                None,
                "/mcp-registry/list",
                Some(conn),
            )
            .expect("an account is eligible");
        {
            let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
            accounts[first].rate_limited_until_ms = Some(now_ms + 60_000);
        }
        let second = manager
            .select(
                &HashSet::new(),
                now,
                None,
                None,
                "/mcp-registry/list",
                Some(conn),
            )
            .expect("the other account is still eligible");
        assert_ne!(
            second, first,
            "a held connection-pinned account must fall back to rotation"
        );
        assert_eq!(
            manager.select(
                &HashSet::new(),
                now,
                None,
                None,
                "/mcp-registry/list",
                Some(conn)
            ),
            Some(second),
            "conn_affinity must be RE-RECORDED to the new winner, not left stale"
        );
    }

    /// Proves the split splits: noise and identity traffic on the SAME
    /// connection route independently — a noise pin to the pool account must
    /// not short-circuit the identity plane's control preference.
    #[test]
    fn identity_path_still_goes_to_control_on_the_same_connection() {
        let manager = build_manager(
            config_with_control(vec![account("pool", 0), account("ctrl", 0)], "ctrl"),
            lock_refresher(),
        );
        manager.set_disabled(1, true);
        let now = OffsetDateTime::now_utc();
        let conn = 99u64;
        assert_eq!(
            manager.select(
                &HashSet::new(),
                now,
                None,
                None,
                "/api/event_logging/v2/batch",
                Some(conn)
            ),
            Some(0),
            "noise pins this connection to the pool account"
        );
        assert_eq!(
            manager.select(
                &HashSet::new(),
                now,
                None,
                None,
                "/api/organizations",
                Some(conn)
            ),
            Some(1),
            "control preference must not be short-circuited by the connection's noise pin"
        );
    }

    /// `conn_affinity` is a SEPARATE, memory-only map (invariant 3): noise
    /// traffic must never mark the persisted affinity map dirty or write into
    /// it, mirroring `Self::affinity`'s own persistence contract.
    #[test]
    fn conn_affinity_is_never_persisted() {
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0)]),
            lock_refresher(),
        );
        assert!(!manager.affinity_dirty.load(Ordering::Relaxed));
        let now = OffsetDateTime::now_utc();
        for i in 0..5u64 {
            manager.select(
                &HashSet::new(),
                now,
                None,
                None,
                "/api/event_logging/v2/batch",
                Some(i),
            );
        }
        assert!(
            !manager.affinity_dirty.load(Ordering::Relaxed),
            "noise traffic must never mark the persisted affinity map dirty"
        );
        assert!(
            manager
                .affinity
                .lock()
                .expect("affinity lock poisoned")
                .is_empty(),
            "noise traffic must never write into the persisted affinity map"
        );
    }

    /// Invariant 4: inert with no control account set — path classification
    /// changes nothing about ordinary LRU.
    #[test]
    fn control_preference_is_inert_when_unset() {
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0)]),
            lock_refresher(),
        );
        assert_eq!(manager.control(), None);
        let now = OffsetDateTime::now_utc();
        assert_eq!(
            manager.select(&HashSet::new(), now, None, None, "/api/organizations", None),
            Some(0),
            "no control account set -> ordinary LRU, unaffected by path classification"
        );
    }

    /// The hard account lock still short-circuits BEFORE path classification
    /// is even computed — control preference can never outrank it.
    #[test]
    fn control_preference_never_outranks_lock_account() {
        let mut config =
            config_with_control(vec![account("locked", 0), account("ctrl", 0)], "ctrl");
        config.lock_account = Some("locked".to_string());
        let manager = build_manager(config, lock_refresher());
        assert_eq!(manager.locked_idx, Some(0));
        assert_eq!(manager.control(), Some(1));
        let now = OffsetDateTime::now_utc();
        assert_eq!(
            manager.select(&HashSet::new(), now, None, None, "/api/organizations", None),
            Some(0),
            "the hard lock must win over control preference regardless of path"
        );
    }

    /// `set_control_by_query` resolves against the LIVE rotation (mirroring
    /// `set_disabled_by_query`), reports the resolved name, sets `control()`,
    /// and a `None` query clears it.
    #[test]
    fn set_control_by_query_resolves_sets_and_clears() {
        let manager = build_manager(
            config_with(vec![account("gil", 0), account("other", 0)]),
            lock_refresher(),
        );

        let outcome = manager.set_control_by_query(Some("gil"), None);
        assert_eq!(
            outcome,
            SetControlOutcome::Applied {
                name: Some("gil".to_string()),
                persist: ControlPersist::NoConfigFile,
            }
        );
        assert_eq!(manager.control(), Some(0));
        assert_eq!(manager.control_name(), Some("gil".to_string()));

        let cleared = manager.set_control_by_query(None, None);
        assert_eq!(
            cleared,
            SetControlOutcome::Applied {
                name: None,
                persist: ControlPersist::NoConfigFile,
            }
        );
        assert_eq!(manager.control(), None);
    }

    #[test]
    fn set_control_by_query_no_match_and_ambiguous() {
        let manager = build_manager(
            config_with(vec![account("dup", 0), account("dup", 0)]),
            lock_refresher(),
        );
        assert_eq!(
            manager.set_control_by_query(Some("ghost"), None),
            SetControlOutcome::NoMatch
        );
        assert_eq!(
            manager.set_control_by_query(Some("dup"), None),
            SetControlOutcome::Ambiguous(vec!["dup".to_string(), "dup".to_string()])
        );
        assert_eq!(
            manager.control(),
            None,
            "an ambiguous query must not set anything"
        );
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
            manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
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
            manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
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
                false,
                None,
                None,
                &HashSet::new(),
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
                false,
                None,
                None,
                &HashSet::new(),
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
            manager
                .select(&HashSet::new(), now, None, None, "/v1/messages", None)
                .is_some(),
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

    /// An account's upstream client is RETIRED once it has carried
    /// [`MAX_SERVES_PER_CONNECTION`] sends, so its pooled h2 connection is
    /// replaced before Anthropic's edge drains it (the reset distribution behind
    /// that number is on the constant).
    ///
    /// The control half matters as much as the assertion: one send short of the
    /// budget the client must be the SAME `Arc`. Without it this test would pass
    /// against a build that recycled on EVERY send — which would throw away the
    /// warm pool on every request, the exact thing `AccountRuntime::http`'s
    /// doc-comment forbids.
    #[test]
    fn the_upstream_client_is_recycled_once_its_serve_budget_is_spent() {
        let manager = build_manager(config_with(vec![account("a", 0)]), pacing_refresher());
        let original = manager.http_client(0).expect("account 0 exists");

        for _ in 0..(MAX_SERVES_PER_CONNECTION - 1) {
            drop(manager.enter_in_flight(0));
        }
        assert!(
            Arc::ptr_eq(
                &original,
                &manager.http_client(0).expect("account 0 exists")
            ),
            "the pool must survive right up to the budget — rebuilding earlier pays a \
             TCP+TLS handshake the keep-alive settings exist to avoid"
        );

        drop(manager.enter_in_flight(0));
        assert!(
            !Arc::ptr_eq(
                &original,
                &manager.http_client(0).expect("account 0 exists")
            ),
            "crossing the serve budget must hand out a NEW client, so the next send \
             opens a fresh h2 connection instead of one about to be drained"
        );
        assert_eq!(
            manager.accounts.read().expect("accounts lock poisoned")[0].serves_since_client_build,
            0,
            "the recycle resets the age counter, so the next budget starts clean"
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
                false,
                None,
                None,
                &HashSet::new(),
            ),
            "cap=0 → account stays eligible regardless of in_flight (no dark pool)"
        );
        // And selection still serves normally rather than flooding the fallback.
        drop(a);
        assert!(
            manager
                .select(&HashSet::new(), now, None, None, "/v1/messages", None)
                .is_some(),
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
        assert_eq!(
            manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
            Some(1)
        );
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
        assert_eq!(
            manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
            Some(1)
        );
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
        assert_eq!(
            manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
            Some(1)
        );
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
        assert_eq!(
            manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
            Some(1)
        );
    }

    #[test]
    fn select_returns_none_when_all_tried() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(config_with(vec![account("a", 0)]), refresher);
        let now = OffsetDateTime::now_utc();
        let tried: HashSet<usize> = [0].into_iter().collect();
        assert_eq!(
            manager.select(&tried, now, None, None, "/v1/messages", None),
            None
        );
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
                .select(&HashSet::new(), now, None, None, "/v1/messages", None)
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
                manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
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
                            refresh_token: Some(format!("new-rt-{i}")),
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

    /// The refresh coalescing lock now lives on `AccountRuntime` itself rather than
    /// in a parallel `Vec` sized at construction — an account appended after
    /// startup via [`Manager::add_account`] must still refresh. Before this fix,
    /// the appended account's index was beyond `refresh_locks.len()` and
    /// `ensure_fresh_inner` returned `false` SILENTLY (no log, no error) — this
    /// test proves refresh actually fires for it, not just that the call returns.
    #[tokio::test]
    async fn appended_account_refreshes() {
        let calls = Arc::new(AtomicUsize::new(0));
        let refresher = Arc::new(CountingRefresher {
            calls: calls.clone(),
        });
        // Seed with one pre-existing account so the appended one is NOT index 0 —
        // the desync this guards against only bites an index beyond the
        // construction-time Vec's length.
        let manager = build_manager(config_with(vec![account("seed", 0)]), refresher);

        let mut appended = account("appended", 0);
        appended.expires_at = Some(crate::now_ms() - 60_000); // already expired
        let idx = manager.add_account(appended);
        assert_eq!(idx, 1, "appended account must land at the next index");

        manager.ensure_fresh(idx).await;

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "refresh must actually fire for an account appended after construction"
        );
        assert_eq!(
            manager.access_token(idx).as_deref(),
            Some("fresh-access"),
            "the appended account's access token must be the refreshed one"
        );
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
    ///
    /// UPDATED contract (control account, part 1): "disabled → skipped" now
    /// has exactly one exception, the control account, covered separately by
    /// [`probeable_indices_still_includes_a_disabled_control_account`] and
    /// [`probeable_indices_excludes_a_disabled_non_control_account_even_with_a_control_set`]
    /// below. This test has no control account configured at all, so it keeps
    /// asserting the base case: an ordinary disabled account is skipped.
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
            "a disabled non-control account must not be probed"
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

    /// The narrow exception itself, asserted directly on `probeable_indices()`
    /// — the unit `probe_all` merely iterates: a disabled account that IS the
    /// control account stays probeable, so its usage keeps getting tracked
    /// even though it is deliberately out of the inference rotation.
    #[test]
    fn probeable_indices_still_includes_a_disabled_control_account() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(
            config_with(vec![account("ok", 0), account("gil", 0)]),
            refresher,
        );
        manager.set_control_by_query(Some("gil"), None);
        manager.set_disabled(1, true);

        assert_eq!(
            manager.probeable_indices(),
            vec![0, 1],
            "the disabled CONTROL account (idx 1) must still be probeable"
        );
    }

    /// The exception must not silently widen: with a control account set,
    /// every OTHER disabled account is still excluded from
    /// `probeable_indices()`.
    #[test]
    fn probeable_indices_excludes_a_disabled_non_control_account_even_with_a_control_set() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(
            config_with(vec![account("gil", 0), account("ok", 0), account("off", 0)]),
            refresher,
        );
        manager.set_control_by_query(Some("gil"), None);
        manager.set_disabled(0, true); // the control account itself, disabled
        manager.set_disabled(2, true); // an unrelated account, disabled

        assert_eq!(
            manager.probeable_indices(),
            vec![0, 1],
            "idx 0 (disabled control) stays probeable, idx 1 (enabled) stays \
             probeable, idx 2 (disabled, NOT control) is excluded — the \
             exception must not widen"
        );
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

    #[tokio::test]
    async fn probe_retry_after_blocks_the_next_probe_until_it_expires() {
        let calls = Arc::new(AtomicUsize::new(0));
        let manager = build_manager_with_prober(
            config_with(vec![account("a", 0)]),
            Arc::new(CountingRefresher {
                calls: Arc::new(AtomicUsize::new(0)),
            }),
            Arc::new(RetryAfterProber {
                calls: Arc::clone(&calls),
            }),
        );

        manager.probe_all().await;
        manager.probe_all().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        manager.accounts.write().unwrap()[0].probe_retry_after_ms = Some(crate::now_ms() - 1);
        manager.probe_all().await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn fable_rejection_does_not_block_other_models() {
        let manager = build_manager(config_with(vec![account("a", 0)]), pacing_refresher());
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "anthropic-ratelimit-unified-status",
            reqwest::header::HeaderValue::from_static("rejected"),
        );
        headers.insert(
            "anthropic-ratelimit-unified-5h-status",
            reqwest::header::HeaderValue::from_static("allowed"),
        );
        headers.insert(
            "anthropic-ratelimit-unified-7d-status",
            reqwest::header::HeaderValue::from_static("allowed_warning"),
        );
        headers.insert(
            "anthropic-ratelimit-unified-7d_oi-status",
            reqwest::header::HeaderValue::from_static("rejected"),
        );
        manager.update_quota_with_rejections(
            0,
            &headers,
            120,
            &[UnifiedRejectionKind::FableWeekly],
        );
        let now = OffsetDateTime::now_utc();

        assert_eq!(
            manager.select(
                &HashSet::new(),
                now,
                Some("claude-opus-4-6"),
                None,
                "/v1/messages",
                None,
            ),
            Some(0)
        );
        assert_eq!(
            manager.select(
                &HashSet::new(),
                now,
                Some("claude-fable-5"),
                None,
                "/v1/messages",
                None,
            ),
            None
        );

        manager.apply_usage(
            0,
            &Usage {
                five_hour: None,
                seven_day: None,
                seven_day_oi: Some(UsageBucket {
                    utilization: Some(0.25),
                    reset_at_ms: Some(crate::now_ms() + 3_600_000),
                }),
            },
        );
        assert_eq!(
            manager.select(
                &HashSet::new(),
                now,
                Some("claude-fable-5"),
                None,
                "/v1/messages",
                None,
            ),
            Some(0),
            "a successful probe must clear stale Fable rejection evidence"
        );
    }

    #[test]
    fn fable_only_evidence_clears_an_older_overall_rejection() {
        let manager = build_manager(config_with(vec![account("a", 0)]), pacing_refresher());
        let mut shared = reqwest::header::HeaderMap::new();
        shared.insert(
            "anthropic-ratelimit-unified-status",
            reqwest::header::HeaderValue::from_static("rejected"),
        );
        manager.update_quota_with_rejections(0, &shared, 3600, &[UnifiedRejectionKind::Overall]);

        let mut fable_only = reqwest::header::HeaderMap::new();
        for (name, value) in [
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-5h-status", "allowed"),
            ("anthropic-ratelimit-unified-7d-status", "allowed_warning"),
            ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
        ] {
            fable_only.insert(
                reqwest::header::HeaderName::from_static(name),
                reqwest::header::HeaderValue::from_static(value),
            );
        }
        manager.update_quota_with_rejections(
            0,
            &fable_only,
            120,
            &[UnifiedRejectionKind::FableWeekly],
        );

        let now = OffsetDateTime::now_utc();
        assert_eq!(
            manager.select(
                &HashSet::new(),
                now,
                Some("claude-opus-4-6"),
                None,
                "/v1/messages",
                None,
            ),
            Some(0),
            "current shared allowed evidence must release Opus"
        );
        assert_eq!(
            manager.select(
                &HashSet::new(),
                now,
                Some("claude-fable-5"),
                None,
                "/v1/messages",
                None,
            ),
            None,
            "the current Fable rejection must remain active"
        );
    }

    #[test]
    fn reset_only_rejection_uses_the_latest_reported_shared_reset() {
        let manager = build_manager(config_with(vec![account("a", 0)]), pacing_refresher());
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "anthropic-ratelimit-unified-status",
            reqwest::header::HeaderValue::from_static("rejected"),
        );
        headers.insert(
            "anthropic-ratelimit-unified-5h-status",
            reqwest::header::HeaderValue::from_static("rejected"),
        );
        headers.insert(
            "anthropic-ratelimit-unified-7d-status",
            reqwest::header::HeaderValue::from_static("rejected"),
        );
        headers.insert(
            "anthropic-ratelimit-unified-5h-reset",
            reqwest::header::HeaderValue::from_str(&(now + 900).to_string()).unwrap(),
        );
        headers.insert(
            "anthropic-ratelimit-unified-7d-reset",
            reqwest::header::HeaderValue::from_str(&(now + 7200).to_string()).unwrap(),
        );

        manager.update_quota_with_rejections(0, &headers, 120, &[UnifiedRejectionKind::Overall]);

        assert_eq!(
            manager.accounts.read().unwrap()[0].overall_rejected_until_ms,
            Some((now + 7200) * 1000)
        );
        assert_eq!(
            manager.select(
                &HashSet::new(),
                OffsetDateTime::from_unix_timestamp(now + 3600).unwrap(),
                Some("claude-opus-4-6"),
                None,
                "/v1/messages",
                None,
            ),
            None,
            "normal selection must honor the reported rejection reset"
        );
    }

    #[test]
    fn current_allowed_status_beats_a_stale_full_window() {
        let manager = build_manager(config_with(vec![account("a", 0)]), pacing_refresher());
        let now = OffsetDateTime::now_utc();
        {
            let mut accounts = manager.accounts.write().unwrap();
            accounts[0].quota.seven_day = Some(crate::quota::QuotaWindow {
                utilization: 1.0,
                reset: Some(now + Duration::hours(2)),
            });
            accounts[0].overall_rejected_until_ms =
                Some((now + Duration::days(7)).unix_timestamp() * 1000);
        }
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "anthropic-ratelimit-unified-status",
            reqwest::header::HeaderValue::from_static("rejected"),
        );
        headers.insert(
            "anthropic-ratelimit-unified-5h-status",
            reqwest::header::HeaderValue::from_static("rejected"),
        );
        headers.insert(
            "anthropic-ratelimit-unified-5h-reset",
            reqwest::header::HeaderValue::from_str(&(now.unix_timestamp() + 900).to_string())
                .unwrap(),
        );
        headers.insert(
            "anthropic-ratelimit-unified-7d-status",
            reqwest::header::HeaderValue::from_static("allowed"),
        );
        headers.insert(
            "anthropic-ratelimit-unified-7d-utilization",
            reqwest::header::HeaderValue::from_static("invalid"),
        );
        headers.insert(
            "anthropic-ratelimit-unified-7d-reset",
            reqwest::header::HeaderValue::from_str(&(now.unix_timestamp() + 7200).to_string())
                .unwrap(),
        );

        manager.update_quota_with_rejections(
            0,
            &headers,
            120,
            &[
                UnifiedRejectionKind::Overall,
                UnifiedRejectionKind::FiveHour,
            ],
        );

        {
            let accounts = manager.accounts.read().unwrap();
            assert_eq!(
                accounts[0].overall_rejected_until_ms,
                Some((now.unix_timestamp() + 900) * 1000)
            );
            assert!(
                accounts[0].quota.seven_day.is_none(),
                "current allowed evidence must remove the stale full window"
            );
        }
        assert_eq!(
            manager.select(
                &HashSet::new(),
                now + Duration::minutes(16),
                Some("claude-opus-4-6"),
                None,
                "/v1/messages",
                None,
            ),
            Some(0),
            "the current 15-minute rejection must replace the stale seven-day deadline"
        );
    }

    #[test]
    fn allowed_status_preserves_utilization_from_the_same_response() {
        let manager = build_manager(config_with(vec![account("a", 0)]), pacing_refresher());
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "anthropic-ratelimit-unified-7d-status",
            reqwest::header::HeaderValue::from_static("allowed"),
        );
        headers.insert(
            "anthropic-ratelimit-unified-7d-utilization",
            reqwest::header::HeaderValue::from_static("1.0"),
        );

        manager.update_quota_with_rejections(0, &headers, 0, &[]);

        assert_eq!(
            manager.accounts.read().unwrap()[0]
                .quota
                .seven_day
                .map(|window| window.utilization),
            Some(1.0),
            "current utilization must not be removed by its current status"
        );
    }

    #[test]
    fn partial_scope_status_preserves_the_other_scope_in_an_overall_hold() {
        let manager = build_manager(config_with(vec![account("a", 0)]), pacing_refresher());
        let now = OffsetDateTime::now_utc();
        let weekly_reset = now + Duration::days(7);
        {
            let mut accounts = manager.accounts.write().unwrap();
            accounts[0].quota.seven_day = Some(crate::quota::QuotaWindow {
                utilization: 1.0,
                reset: Some(weekly_reset),
            });
            accounts[0].overall_rejected_until_ms =
                Some((weekly_reset.unix_timestamp_nanos() / 1_000_000) as i64);
        }
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "anthropic-ratelimit-unified-status",
            reqwest::header::HeaderValue::from_static("rejected"),
        );
        headers.insert(
            "anthropic-ratelimit-unified-5h-status",
            reqwest::header::HeaderValue::from_static("rejected"),
        );
        headers.insert(
            "anthropic-ratelimit-unified-5h-reset",
            reqwest::header::HeaderValue::from_str(&(now.unix_timestamp() + 900).to_string())
                .unwrap(),
        );

        manager.update_quota_with_rejections(
            0,
            &headers,
            120,
            &[
                UnifiedRejectionKind::Overall,
                UnifiedRejectionKind::FiveHour,
            ],
        );

        assert_eq!(
            manager.accounts.read().unwrap()[0].overall_rejected_until_ms,
            Some((weekly_reset.unix_timestamp_nanos() / 1_000_000) as i64),
            "missing weekly status must retain the known weekly hold"
        );
    }

    #[test]
    fn rejection_without_reset_headers_reuses_known_window_resets() {
        let manager = build_manager(config_with(vec![account("a", 0)]), pacing_refresher());
        let now = OffsetDateTime::now_utc();
        {
            let mut accounts = manager.accounts.write().unwrap();
            accounts[0].quota.five_hour = Some(crate::quota::QuotaWindow {
                utilization: 1.0,
                reset: Some(now + Duration::minutes(15)),
            });
            accounts[0].quota.seven_day = Some(crate::quota::QuotaWindow {
                utilization: 1.0,
                reset: Some(now + Duration::hours(2)),
            });
        }
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "anthropic-ratelimit-unified-status",
            reqwest::header::HeaderValue::from_static("rejected"),
        );

        manager.update_quota_with_rejections(0, &headers, 120, &[UnifiedRejectionKind::Overall]);

        assert_eq!(
            manager.accounts.read().unwrap()[0].overall_rejected_until_ms,
            Some(((now + Duration::hours(2)).unix_timestamp_nanos() / 1_000_000) as i64)
        );
    }

    #[test]
    fn probe_retry_after_has_a_seven_day_safety_cap() {
        let manager = build_manager(config_with(vec![account("a", 0)]), pacing_refresher());
        let before = crate::now_ms();

        manager.record_probe(
            0,
            ProbeStatus::RateLimited,
            Some("rate limited".to_string()),
            Some(u64::MAX),
        );

        let deadline = manager.accounts.read().unwrap()[0]
            .probe_retry_after_ms
            .unwrap();
        assert!(deadline >= before + 7 * 24 * 60 * 60 * 1000);
        assert!(deadline <= crate::now_ms() + 7 * 24 * 60 * 60 * 1000);
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
                        retry_after_secs: None,
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
            manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
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
            manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
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
            manager.select(&HashSet::new(), now, None, Some(key), "/v1/messages", None),
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
            manager.select(&HashSet::new(), now, None, Some(key), "/v1/messages", None),
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
            manager.select(&HashSet::new(), now, None, Some(key), "/v1/messages", None),
            Some(0),
            "precondition: the session starts pinned to `a`"
        );
        manager.record_served(0, now, Some(key), SessionKind::Stable);

        // A dead credential is ACCOUNT-level death → the pin is durably re-keyed.
        manager.mark_error(0);
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(key), "/v1/messages", None),
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
            manager.select(&HashSet::new(), now, None, Some(key), "/v1/messages", None);
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

    /// FINDING 7. `input_tokens` is the QUOTA counter, and `update_usage` is
    /// `pub` on a library crate — so it must add the caller's number, verbatim,
    /// whatever relationship that number has to the components passed beside
    /// it. Decomposing with a saturating subtraction and re-summing the pieces
    /// grew the counter by 800 for an `input_tokens` of 100, which is both the
    /// quota consumption and the denominator of `cacheHitRatio`.
    #[test]
    fn update_usage_adds_the_callers_input_verbatim() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(config_with(vec![account("a", 0)]), refresher);
        // An inconsistent caller: 100 of input against 800 of components.
        manager.update_usage(0, 100, 5, 750, 50);
        let snap = manager.snapshot(OffsetDateTime::now_utc());
        assert_eq!(
            snap.accounts[0].input_tokens, 100,
            "the quota counter grows by exactly what the caller passed"
        );
        assert_eq!(snap.accounts[0].output_tokens, 5);
        assert_eq!(snap.accounts[0].cache_read_tokens, 750);
        assert_eq!(snap.accounts[0].cache_creation_tokens, 50);
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
            manager.select(
                &HashSet::new(),
                now,
                Some("claude-fable-5"),
                None,
                "/v1/messages",
                None
            ),
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
            manager.select(
                &tried,
                now,
                Some("claude-opus-4-6"),
                None,
                "/v1/messages",
                None
            ),
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
            manager.select(&tried, now, None, None, "/v1/messages", None),
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
            .select(&HashSet::new(), now, None, Some(7), "/v1/messages", None)
            .expect("an account is eligible");
        for _ in 0..5 {
            assert_eq!(
                manager.select(&HashSet::new(), now, None, Some(7), "/v1/messages", None),
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
                .select(&HashSet::new(), now, None, None, "/v1/messages", None)
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
            .select(&HashSet::new(), now, None, Some(42), "/v1/messages", None)
            .expect("an account is eligible");
        manager.mark_rate_limited(pinned, LONG_HOLD_SECS);
        let repinned = manager
            .select(&HashSet::new(), now, None, Some(42), "/v1/messages", None)
            .expect("the other account is eligible");
        assert_ne!(repinned, pinned, "must migrate off the ineligible pin");
        // And it sticks to the new pin.
        for _ in 0..3 {
            assert_eq!(
                manager.select(&HashSet::new(), now, None, Some(42), "/v1/messages", None),
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
            .select(&HashSet::new(), now, None, Some(77), "/v1/messages", None)
            .expect("an account is eligible");
        // Saturate ONLY the pinned account: at cap=1 it is soft-paced while every
        // hard gate (disabled/error/hold/quota) stays clear.
        {
            let mut a = manager.accounts.write().expect("accounts lock poisoned");
            a[pinned].in_flight = 1;
        }
        let diverted = manager
            .select(&HashSet::new(), now, None, Some(77), "/v1/messages", None)
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
                manager.select(&HashSet::new(), now, None, Some(77), "/v1/messages", None),
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
            .select(&HashSet::new(), now, None, Some(88), "/v1/messages", None)
            .expect("an account is eligible");
        {
            let mut a = manager.accounts.write().expect("accounts lock poisoned");
            a[pinned].in_flight = 1;
        }
        // A later `now` is what the divert must stamp onto the surviving pin.
        let later = now + time::Duration::seconds(30);
        let later_ms = odt_to_ms(later);
        manager
            .select(&HashSet::new(), later, None, Some(88), "/v1/messages", None)
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
            .select(&HashSet::new(), now, None, Some(9), "/v1/messages", None)
            .expect("an account is eligible");
        // The failover that put A in `tried` was durable: it armed a hold that
        // outlives A's prompt cache, so there is nothing left to come home to.
        manager.mark_rate_limited(a, LONG_HOLD_SECS);
        let tried: HashSet<usize> = [a].into_iter().collect();
        let b = manager
            .select(&tried, now, None, Some(9), "/v1/messages", None)
            .expect("the untried account is eligible");
        assert_ne!(b, a, "must fall through the tried pin to the other account");
        // The pin updated to B: a fresh same-key select with nothing tried sticks to B.
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(9), "/v1/messages", None),
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
            .select(&HashSet::new(), now, None, Some(1234), "/v1/messages", None)
            .expect("an account is eligible");
        // Over the soft threshold, but Anthropic still says `allowed_warning`:
        // every HARD gate is clear.
        set_over_threshold(&manager, pinned, 0.995, "allowed_warning");

        for _ in 0..3 {
            assert_eq!(
                manager.select(&HashSet::new(), now, None, Some(1234), "/v1/messages", None),
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
            .select(&HashSet::new(), now, None, None, "/v1/messages", None)
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
            .select(&HashSet::new(), now, None, Some(2345), "/v1/messages", None)
            .expect("an account is eligible");
        manager.mark_rate_limited(pinned, LONG_HOLD_SECS);

        let served = manager
            .select(&HashSet::new(), now, None, Some(2345), "/v1/messages", None)
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
            .select(&HashSet::new(), now, None, Some(5678), "/v1/messages", None)
            .expect("an account is eligible");
        set_over_threshold(&manager, pinned, 0.995, "allowed_warning");
        // The serve-over-threshold hit a real 429, which armed a real hold — and a
        // long one, past the point where waiting it out could still hit a warm cache.
        manager.mark_rate_limited(pinned, LONG_HOLD_SECS);

        let served = manager
            .select(&HashSet::new(), now, None, Some(5678), "/v1/messages", None)
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
            manager.select(&HashSet::new(), now, None, Some(5678), "/v1/messages", None),
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
            .select(&HashSet::new(), now, None, Some(3456), "/v1/messages", None)
            .expect("an account is eligible");
        set_over_threshold(&manager, pinned, 0.995, "rejected");

        let served = manager
            .select(&HashSet::new(), now, None, Some(3456), "/v1/messages", None)
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
            .select(&HashSet::new(), now, None, Some(4567), "/v1/messages", None)
            .expect("an account is eligible");
        let tried: HashSet<usize> = [pinned].into_iter().collect();

        let served = manager
            .select(&tried, now, None, Some(4567), "/v1/messages", None)
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
            manager.select(&HashSet::new(), now, None, Some(4567), "/v1/messages", None),
            Some(pinned),
            "the session must return to its original account"
        );

        // Now the failure proves durable (a 429 armed a hold long enough to outlive
        // the account's prompt cache) — that IS hard, so the same tried-pin select
        // re-keys.
        manager.mark_rate_limited(pinned, LONG_HOLD_SECS);
        let moved = manager
            .select(&tried, now, None, Some(4567), "/v1/messages", None)
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
            .select(
                &HashSet::new(),
                now,
                Some("claude-opus-4-6"),
                Some(key),
                "/v1/messages",
                None,
            )
            .expect("an account is eligible");
        set_fable_exhausted(&manager, home, 0.999);

        let served = manager
            .select(
                &HashSet::new(),
                now,
                Some("claude-fable-5"),
                Some(key),
                "/v1/messages",
                None,
            )
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
            manager.select(
                &HashSet::new(),
                now,
                Some("claude-opus-4-6"),
                Some(key),
                "/v1/messages",
                None
            ),
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
            .select(
                &HashSet::new(),
                now,
                Some("claude-opus-4-6"),
                Some(key),
                "/v1/messages",
                None,
            )
            .expect("an account is eligible");
        set_fable_exhausted(&manager, home, 0.999);
        // The account is not merely out of Fable — it is gone for every model class,
        // and for longer than its prompt cache survives.
        manager.mark_rate_limited(home, LONG_HOLD_SECS);

        let served = manager
            .select(
                &HashSet::new(),
                now,
                Some("claude-fable-5"),
                Some(key),
                "/v1/messages",
                None,
            )
            .expect("the un-held account serves this request");
        assert_ne!(served, home, "a held pin cannot serve");
        assert_eq!(
            pin_of(&manager, key),
            Some(served),
            "a hold outliving the prompt cache is ACCOUNT-level death — it must \
             still re-key the session"
        );
        assert_eq!(
            manager.select(
                &HashSet::new(),
                now,
                Some("claude-opus-4-6"),
                Some(key),
                "/v1/messages",
                None
            ),
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
            .select(
                &HashSet::new(),
                now,
                Some("claude-opus-4-6"),
                Some(key),
                "/v1/messages",
                None,
            )
            .expect("an account is eligible");
        let other = 1 - home;
        // The whole fleet crosses the SOFT threshold, and the pin is out of Fable.
        set_over_threshold(&manager, home, 0.99, "allowed_warning");
        set_over_threshold(&manager, other, 0.96, "allowed_warning");
        set_fable_exhausted(&manager, home, 0.999);

        assert_eq!(
            manager.select(
                &HashSet::new(),
                now,
                Some("claude-fable-5"),
                Some(key),
                "/v1/messages",
                None
            ),
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
            manager.select(
                &HashSet::new(),
                now,
                Some("claude-opus-4-6"),
                Some(key),
                "/v1/messages",
                None
            ),
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
            .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
            .expect("an account is eligible");
        manager.mark_rate_limited(home, SHORT_HOLD_SECS);

        let diverted = manager
            .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
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
                manager.select(
                    &HashSet::new(),
                    after,
                    None,
                    Some(key),
                    "/v1/messages",
                    None
                ),
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
            .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
            .expect("an account is eligible");
        manager.mark_rate_limited(home, LONG_HOLD_SECS);

        let served = manager
            .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
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
            manager.select(&HashSet::new(), now, None, Some(key), "/v1/messages", None),
            Some(served),
            "the session must not snap back to the held account"
        );
    }

    /// Read the divert episode currently recorded for a session key, if any.
    fn divert_episode_of(manager: &Manager, key: u64) -> Option<DivertEpisode> {
        manager
            .divert_ledger
            .lock()
            .expect("divert ledger lock poisoned")
            .get(&key)
            .copied()
    }

    /// Phase 1's headline claim (the divert-budget design notes §4.3, Phase 1
    /// test list): a SECOND divert inside the SAME hold episode reuses the
    /// FIRST divert's destination, rather than the normal LRU spread's own
    /// anti-churn steering it onto a *different* alternate. Three accounts
    /// (not two) so this is a real assertion: with only one alternate
    /// available, "the second divert lands on the same account" would be true
    /// with or without stickiness.
    #[test]
    fn sticky_divert_reuses_first_destination() {
        let manager = build_manager(
            config_with(vec![
                account("home", 0),
                account("alt1", 0),
                account("alt2", 0),
            ]),
            pacing_refresher(),
        );
        let now = OffsetDateTime::now_utc();
        let key = 616_161u64;
        let home = manager
            .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
            .expect("an account is eligible");
        manager.mark_rate_limited(home, SHORT_HOLD_SECS);

        let first_dest = manager
            .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
            .expect("an un-held alternate serves this request");
        assert_ne!(first_dest, home, "the held pin cannot serve THIS request");

        // Without stickiness, the normal LRU/priority pick's own anti-churn
        // (`select()`'s doc-comment: "consecutive requests fan out instead of
        // hammering one account") would steer the SECOND divert onto the
        // account that was NOT just selected. With the sticky overlay it must
        // land back on `first_dest` instead.
        let second_dest = manager
            .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
            .expect("an un-held alternate serves this request");
        assert_eq!(
            second_dest, first_dest,
            "a second divert in the same hold episode must reuse the first \
             divert's destination, not spend a second fresh account"
        );
        assert_eq!(
            pin_of(&manager, key),
            Some(home),
            "sticky reuse is a divert, not a re-key — the pin stays home"
        );

        let episode = divert_episode_of(&manager, key).expect("episode recorded");
        assert_eq!(episode.pin, home);
        assert_eq!(episode.sticky, first_dest);
        assert_eq!(
            episode.destinations.count_ones(),
            1,
            "two diverts reusing the SAME destination is one distinct \
             destination, not two"
        );
    }

    /// Episode boundary (the divert-budget design notes §4.1: "Reset is
    /// structural, not timed"): a session that diverts, recovers, and is then
    /// held AGAIN on a later, distinct hold must start the second episode
    /// with a clean mask — the earlier hold's destination must not leak in as
    /// a stale sticky pick for a hold it never happened during.
    #[test]
    fn new_hold_deadline_resets_the_episode() {
        let manager = build_manager(
            config_with(vec![
                account("home", 0),
                account("alt1", 0),
                account("alt2", 0),
            ]),
            pacing_refresher(),
        );
        // A single wall-clock read for the whole test — both hold deadlines
        // below are EXPLICIT, fixed offsets from this one `now_ms`, never
        // from `mark_rate_limited` (which takes its own, separate real-clock
        // read via `crate::now_ms()`). Mixing two independent clock reads is
        // exactly what made an earlier version of this test flaky: two reads
        // microseconds apart are USUALLY distinguishable at millisecond
        // resolution, but "usually" is not a gate. Deterministic by
        // construction now, not by luck.
        let now = OffsetDateTime::now_utc();
        let now_ms = odt_to_ms(now);
        let key = 626_262u64;
        let home = manager
            .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
            .expect("an account is eligible");
        {
            let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
            accounts[home].rate_limited_until_ms = Some(now_ms + SHORT_HOLD_SECS * 1000);
        }
        manager
            .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
            .expect("an un-held alternate serves this request");
        let episode_one = divert_episode_of(&manager, key).expect("episode recorded");

        // Let the first hold clear and the session come home.
        let after = now + Duration::seconds(SHORT_HOLD_SECS + 5);
        assert_eq!(
            manager.select(
                &HashSet::new(),
                after,
                None,
                Some(key),
                "/v1/messages",
                None
            ),
            Some(home),
            "the hold cleared — the session returns to its warm pin"
        );

        // A SECOND, later hold on the SAME account: a different
        // `rate_limited_until_ms`, therefore a different episode identity.
        // Still derived from the SAME `now_ms` capture above, at a
        // deliberately distinct explicit offset — see
        // `DivertEpisode::until_ms`'s doc comment for why an ACCIDENTAL
        // collision between two real holds would be benign; this test simply
        // does not rely on one.
        {
            let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
            accounts[home].rate_limited_until_ms =
                Some(now_ms + (SHORT_HOLD_SECS + 5) * 1000 + SHORT_HOLD_SECS * 1000);
        }
        manager
            .select(
                &HashSet::new(),
                after,
                None,
                Some(key),
                "/v1/messages",
                None,
            )
            .expect("an un-held alternate serves this request");
        let episode_two = divert_episode_of(&manager, key).expect("episode recorded");

        assert_ne!(
            episode_one.until_ms, episode_two.until_ms,
            "two separate holds on the same account carry two different deadlines"
        );
        assert_eq!(
            episode_two.destinations.count_ones(),
            1,
            "the new episode starts with a FULL budget — a clean, single-bit \
             mask, not the first episode's carried-over distinct-destination count"
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
            .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
            .expect("an account is eligible");
        {
            let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
            accounts[home].status = AccountStatus::Throttled;
            accounts[home].rate_limited_until_ms = Some(now_ms + remaining_ms);
        }
        let served = manager
            .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
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
            .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
            .expect("an account is eligible");
        manager.mark_rate_limited(home, LONG_HOLD_SECS);

        let failover = manager
            .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
            .expect("the un-held account serves this request");
        assert_ne!(
            failover, home,
            "a hold outliving the prompt cache re-keys the session"
        );
        assert_eq!(pin_of(&manager, key), Some(failover));

        // The hold expires. A past hold reads as expired live, no mutation needed.
        let after = now + Duration::hours(1);
        assert_eq!(
            manager.select(
                &HashSet::new(),
                after,
                None,
                Some(key),
                "/v1/messages",
                None
            ),
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
            .select(&HashSet::new(), now, None, Some(1), "/v1/messages", None)
            .expect("an account is eligible");
        let y = manager
            .select(&HashSet::new(), now, None, Some(2), "/v1/messages", None)
            .expect("an account is eligible");
        assert_ne!(x, y, "distinct keys' initial pins fan out across the tier");
        // Each key repeats onto its own account.
        for _ in 0..3 {
            assert_eq!(
                manager.select(&HashSet::new(), now, None, Some(1), "/v1/messages", None),
                Some(x)
            );
            assert_eq!(
                manager.select(&HashSet::new(), now, None, Some(2), "/v1/messages", None),
                Some(y)
            );
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
                    "/v1/messages",
                    None,
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
            "/v1/messages",
            None,
        );

        // One more distinct key pushes len over CAP → evict the single oldest.
        manager.select(
            &HashSet::new(),
            base + time::Duration::seconds((AFFINITY_CAP + 11) as i64),
            None,
            Some((AFFINITY_CAP + 1) as u64),
            "/v1/messages",
            None,
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
                manager.select(&HashSet::new(), now, None, Some(100), "/v1/messages", None),
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
            manager.select(&HashSet::new(), now, None, Some(10), "/v1/messages", None),
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
            manager.select(&HashSet::new(), now, None, Some(10), "/v1/messages", None),
            Some(1)
        );
        // Now 1-and-1: every further select is a lone session on its account.
        for _ in 0..5 {
            assert_eq!(
                manager.select(&HashSet::new(), now, None, Some(10), "/v1/messages", None),
                Some(1),
                "the migrated session stays put (no bounce back)"
            );
            assert_eq!(
                manager.select(&HashSet::new(), now, None, Some(11), "/v1/messages", None),
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
                manager.select(&HashSet::new(), now, None, Some(20), "/v1/messages", None),
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
            manager.select(&HashSet::new(), now, None, Some(key), "/v1/messages", None);
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
                manager.select(&HashSet::new(), now, None, Some(30), "/v1/messages", None),
                Some(0),
                "a stacked session keeps its warm account when migration is off"
            );
            assert_eq!(
                manager.select(&HashSet::new(), now, None, Some(31), "/v1/messages", None),
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
            enabled.select(&HashSet::new(), now, None, Some(30), "/v1/messages", None),
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
        AccountRuntime::from_config(&account("gate", 0), false)
    }

    #[test]
    fn account_gate_ok_when_healthy() {
        let now = OffsetDateTime::now_utc();
        let a = gate_runtime();
        assert_eq!(
            Manager::account_gate(&a, 0.90, now, odt_to_ms(now), false, None, &HashSet::new()),
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
            Manager::account_gate(
                &disabled,
                0.90,
                now,
                odt_to_ms(now),
                false,
                None,
                &HashSet::new()
            ),
            (GateReason::Disabled, None)
        );

        let mut errored = gate_runtime();
        errored.status = AccountStatus::Error;
        errored.quota.five_hour = Some(window(0.99, Some(now + Duration::seconds(300))));
        assert_eq!(
            Manager::account_gate(
                &errored,
                0.90,
                now,
                odt_to_ms(now),
                false,
                None,
                &HashSet::new()
            ),
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
            Manager::account_gate(&a, 0.90, now, odt_to_ms(now), false, None, &HashSet::new()),
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
            Manager::account_gate(&a, 0.90, now, odt_to_ms(now), false, None, &HashSet::new()),
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
            Manager::account_gate(&a, 0.90, now, odt_to_ms(now), false, None, &HashSet::new()),
            (GateReason::Ok, None),
            "the non-Fable view ignores the model-scoped weekly"
        );
        assert_eq!(
            Manager::account_gate(&a, 0.90, now, odt_to_ms(now), true, None, &HashSet::new()),
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
            Manager::account_gate(&a, 0.90, now, odt_to_ms(now), false, None, &HashSet::new()),
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
        let mut a = AccountRuntime::from_config(&account("std-hold", 0), false);
        a.switch_threshold = Some(0.90);
        a.rate_limited_until_ms = Some(now_ms + 8_000); // short Hold, +8s
        a.quota.requests_limit = Some(200);
        a.quota.requests_remaining = Some(5); // 97.5% spent
        a.quota.standard_reset = Some(reset);

        // The Standard gate (later reset) wins max_by_key over the +8s Hold.
        assert_eq!(
            Manager::account_gate(&a, 0.90, now, now_ms, false, None, &HashSet::new()),
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
            Manager::account_gate(&a, 0.90, now, odt_to_ms(now), false, None, &HashSet::new()),
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
            Manager::account_gate(&a, 0.90, now, odt_to_ms(now), false, None, &HashSet::new()),
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
            Manager::account_gate(&a, 0.90, now, now_ms, false, None, &HashSet::new()),
            (GateReason::Rejected, None)
        );
        assert!(
            !Manager::account_hard_ok(&a, now_ms, None, &HashSet::new()),
            "and it stays hard-gated, exactly as before"
        );

        // Terminal, so it dominates a live window and carries NO clear-instant —
        // `retry_after_hint` reads `free_at`, and a rejected account was never going
        // to come back at its 5h reset.
        a.quota.five_hour = Some(window(0.99, Some(now + Duration::seconds(300))));
        assert_eq!(
            Manager::account_gate(&a, 0.90, now, now_ms, false, None, &HashSet::new()),
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

        const ALL: [GateReason; 10] = [
            GateReason::Ok,
            GateReason::Hold,
            GateReason::FiveHour,
            GateReason::SevenDay,
            GateReason::FableWeekly,
            GateReason::Standard,
            GateReason::Login,
            GateReason::Disabled,
            GateReason::Rejected,
            GateReason::Reserved,
        ];

        for reason in ALL {
            // Only a Fable-scoped evaluation can ever surface the model-scoped gate.
            let is_fable = reason == GateReason::FableWeekly;

            // Per case: a label, a runtime that actually exhibits `reason`, whether
            // that block is ACCOUNT-level (`account_hard_ok == false`) or
            // request-scoped (`account_hard_ok` stays true, the pin survives), and
            // the reserved-group set the request evaluates against (empty for
            // every reason but `Reserved` itself, which is the one whose gate
            // depends on more than the runtime alone).
            let cases: Vec<(&str, AccountRuntime, bool, HashSet<String>)> = match reason {
                GateReason::Ok => vec![("healthy", gate_runtime(), true, HashSet::new())],

                // Terminal: a fact about the credential, for every model class.
                GateReason::Disabled => {
                    let mut a = gate_runtime();
                    a.disabled = true;
                    vec![("operator-disabled", a, false, HashSet::new())]
                }
                GateReason::Login => {
                    let mut a = gate_runtime();
                    a.status = AccountStatus::Error;
                    vec![("dead credential", a, false, HashSet::new())]
                }
                GateReason::Rejected => {
                    let mut a = gate_runtime();
                    a.quota.status = Some("rejected".to_string());
                    vec![("upstream rejected", a, false, HashSet::new())]
                }
                // Reservation is not a preference: unrequested traffic (`group:
                // None`, matching every other case in this loop) against an
                // account in a reserved group is ACCOUNT-level-blocked, exactly
                // like the terminal gates above — see `Self::reserved_blocks`.
                GateReason::Reserved => {
                    let mut a = gate_runtime();
                    a.groups = vec!["codereview".to_string()];
                    let reserved: HashSet<String> = ["codereview".to_string()].into();
                    vec![("reserved group, unrequested traffic", a, false, reserved)]
                }

                // The one reason that splits on DURATION: past the cache TTL a hold
                // is account death, under it a timer worth keeping the pin for.
                GateReason::Hold => {
                    let mut long = gate_runtime();
                    long.rate_limited_until_ms = Some(now_ms + (CACHE_WARM_HOLD_SECS + 60) * 1_000);
                    let mut short = gate_runtime();
                    short.rate_limited_until_ms = Some(now_ms + 30_000);
                    vec![
                        ("hold outliving the cache", long, false, HashSet::new()),
                        ("hold clearing while warm", short, true, HashSet::new()),
                    ]
                }

                // Windows are per-request facts: they gate the display and every
                // serve decision, but must never move a session's pin.
                GateReason::FiveHour => {
                    let mut a = gate_runtime();
                    a.quota.five_hour = Some(window(0.99, Some(reset)));
                    vec![("5h over threshold", a, true, HashSet::new())]
                }
                GateReason::SevenDay => {
                    let mut a = gate_runtime();
                    a.quota.seven_day = Some(window(0.99, Some(reset)));
                    vec![("7d over threshold", a, true, HashSet::new())]
                }
                GateReason::FableWeekly => {
                    let mut a = gate_runtime();
                    a.quota.seven_day_oi = Some(window(0.99, Some(reset)));
                    vec![("7d_oi over threshold", a, true, HashSet::new())]
                }
                GateReason::Standard => {
                    let mut a = gate_runtime();
                    a.quota.tokens_limit = Some(1_000);
                    a.quota.tokens_remaining = Some(10); // 99% spent
                    a.quota.standard_reset = Some(reset);
                    vec![("standard limit spent", a, true, HashSet::new())]
                }
            };

            for (label, runtime, account_level, reserved) in cases {
                let (gate, _) =
                    Manager::account_gate(&runtime, 0.90, now, now_ms, is_fable, None, &reserved);
                assert_eq!(gate, reason, "fixture `{label}` must exhibit {reason:?}");

                let hard_ok = Manager::account_hard_ok(&runtime, now_ms, None, &reserved);
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

        let accounts = manager.accounts.read().expect("accounts lock poisoned");
        assert!(
            !format!("{:?}", accounts[0].http).contains("TotalTimeout"),
            "the serving client grew a total timeout; it will truncate SSE streams"
        );
    }

    /// The connection-pool isolation this whole module exists for: two accounts
    /// must never be handed the SAME client. `hyper-util` keys its pool on
    /// `(scheme, authority)` alone — nothing about the Bearer token or account
    /// identity — so a client shared across accounts collapses every account
    /// onto one pooled connection, and that connection's death takes every
    /// account down with it regardless of how healthy any individual account is
    /// (see [`AccountRuntime::http`]'s doc comment for the measured incident).
    ///
    /// This is an IN-PROCESS identity check, not a live network measurement —
    /// `Arc::ptr_eq` proves two accounts never share the same client instance
    /// (and thus never share its connection pool), and that repeated lookups of
    /// the SAME account keep returning the SAME instance rather than a fresh
    /// one that would throw away its warm pool. It does not, and cannot, prove
    /// two real TCP connections were opened — that would need an actual
    /// upstream to observe.
    #[test]
    fn different_accounts_never_share_a_serving_client() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(
            config_with(vec![account("a", 0), account("b", 0)]),
            refresher,
        );

        let client_a = manager.http_client(0).expect("account 0 exists");
        let client_b = manager.http_client(1).expect("account 1 exists");
        assert!(
            !Arc::ptr_eq(&client_a, &client_b),
            "two different accounts were handed the same client — a dead \
             connection on one account's pool would take every other account \
             down with it"
        );

        // The SAME account, looked up again, must return the SAME instance —
        // a fresh client per lookup would defeat the whole point (a cold pool
        // on every single request), even though it would still pass the
        // distinctness assertion above.
        let client_a_again = manager.http_client(0).expect("account 0 exists");
        assert!(
            Arc::ptr_eq(&client_a, &client_a_again),
            "the same account's client must be stable across lookups, not \
             rebuilt per call"
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
        let mut a = AccountRuntime::from_config(&account("a", 0), false);
        a.switch_threshold = Some(0.90);
        a.quota.five_hour = Some(window(0.99, Some(at(200))));
        a.quota.seven_day = Some(window(0.99, Some(at(5_000))));

        // Account B — a dead credential (Error) that holds the SOONEST raw reset
        // (100s). It never self-frees, so it must contribute nothing to the hint.
        let mut b = AccountRuntime::from_config(&account("b", 0), false);
        b.switch_threshold = Some(0.90);
        b.status = AccountStatus::Error;
        b.quota.five_hour = Some(window(0.99, Some(at(100))));

        // Account C — the TRUE first recovery: 5h-gated with a later reset (900s)
        // and a healthy 7d, so it genuinely returns at 900s.
        let mut c = AccountRuntime::from_config(&account("c", 0), false);
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

    /// The full hint: the gate that TIMES the wait, and a count of every account
    /// out of rotation by its gate. Same fleet as the test above, so the binding
    /// gate is C's 5-hour window, while A's weekly and B's dead credential are
    /// counted without being allowed to drive the time.
    #[test]
    fn exhaustion_hint_names_the_binding_gate_and_counts_every_gated_account() {
        let now = OffsetDateTime::now_utc();
        let at = |secs: i64| now + Duration::seconds(secs);
        let mut a = AccountRuntime::from_config(&account("a", 0), false);
        a.switch_threshold = Some(0.90);
        a.quota.five_hour = Some(window(0.99, Some(at(200))));
        a.quota.seven_day = Some(window(0.99, Some(at(5_000))));
        let mut b = AccountRuntime::from_config(&account("b", 0), false);
        b.switch_threshold = Some(0.90);
        b.status = AccountStatus::Error;
        b.quota.five_hour = Some(window(0.99, Some(at(100))));
        let mut c = AccountRuntime::from_config(&account("c", 0), false);
        c.switch_threshold = Some(0.90);
        c.quota.five_hour = Some(window(0.99, Some(at(900))));

        let manager = Manager::from_runtimes(vec![a, b, c]);
        let hint = manager.exhaustion_hint(now, false, None);
        assert_eq!(
            hint.binding,
            Some(GateReason::FiveHour),
            "C's 5h window times the wait"
        );
        assert_eq!(
            hint.free_at.map(odt_to_ms),
            Some(odt_to_ms(at(900))),
            "free_at is C's reset instant"
        );
        assert_eq!(
            hint.retry_after,
            manager.retry_after_hint(now, false),
            "the seconds half is byte-identical to retry_after_hint"
        );
        let counts: Vec<(GateReason, usize)> = hint.gated.iter().map(|(r, n)| (*r, *n)).collect();
        assert_eq!(
            counts,
            vec![
                (GateReason::FiveHour, 1),
                (GateReason::SevenDay, 1),
                (GateReason::Login, 1)
            ],
            "every gated account is counted under the gate that binds IT"
        );
    }

    /// When no account advertises a reset, the number stays the 60 sentinel the
    /// proxy's soft-wait relies on, but `free_at`/`binding` are `None` so a
    /// message can say "no reset known" instead of printing 60 as a measurement.
    #[test]
    fn exhaustion_hint_reports_an_unknown_reset_as_unknown_not_as_sixty_seconds() {
        let now = OffsetDateTime::now_utc();
        let mut a = AccountRuntime::from_config(&account("a", 0), false);
        a.switch_threshold = Some(0.90);
        a.quota.seven_day = Some(window(0.99, None));
        let manager = Manager::from_runtimes(vec![a]);
        let hint = manager.exhaustion_hint(now, false, None);
        assert_eq!(hint.retry_after, 60);
        assert_eq!(hint.free_at, None);
        assert_eq!(hint.binding, None);
        assert_eq!(hint.gated.get(&GateReason::SevenDay), Some(&1));
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

    /// **INV3 / FIX 4.** A write that did not land must leave the in-memory snapshot
    /// alone. Mutating memory first and regardless of the outcome left the two views
    /// permanently diverged — memory saying benched, disk saying nothing — with
    /// nothing to reconcile or retry them, which is the opposite of the guarantee the
    /// comment above `persist_disabled` claimed.
    ///
    /// All three failure shapes, because each takes a different arm:
    /// `NoEntry` (identity absent from disk), `Ambiguous` (two entries share it), and
    /// `Err` (the file cannot even be parsed).
    #[test]
    fn a_failed_persist_leaves_the_in_memory_snapshot_unchanged() {
        // NoEntry — the runtime row exists, the disk entry does not.
        let path = tmp_config_path("persist-noentry");
        write_account_file(&path, &["someone-else"]);
        let manager =
            build_manager_with_path(config_with(vec![account("acct-a", 0)]), path.clone());
        assert_eq!(
            manager.set_disabled(0, true),
            DisablePersist::NoEntry,
            "no disk entry carries this identity"
        );
        assert_eq!(
            manager.config.lock().unwrap().accounts[0].disabled,
            None,
            "a write that never landed must not be reflected in memory"
        );
        // The runtime row still flips — the bench takes effect NOW, it just is not
        // durable, which is exactly what the caller is told.
        assert!(manager.snapshot(OffsetDateTime::now_utc()).accounts[0].disabled);
        std::fs::remove_file(&path).ok();

        // Ambiguous — two disk entries share the identity, so none may be chosen.
        let path = tmp_config_path("persist-ambiguous");
        write_account_file(&path, &["acct-a", "acct-a"]);
        let manager =
            build_manager_with_path(config_with(vec![account("acct-a", 0)]), path.clone());
        let before = std::fs::read_to_string(&path).unwrap();
        assert_eq!(manager.set_disabled(0, true), DisablePersist::Ambiguous);
        assert_eq!(manager.config.lock().unwrap().accounts[0].disabled, None);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "an ambiguous identity must leave the file byte-identical"
        );
        std::fs::remove_file(&path).ok();

        // Err — the file cannot be parsed, so nothing can be merged into it.
        let path = tmp_config_path("persist-malformed");
        std::fs::write(&path, "{ not json").unwrap();
        let manager =
            build_manager_with_path(config_with(vec![account("acct-a", 0)]), path.clone());
        assert_eq!(manager.set_disabled(0, true), DisablePersist::WriteFailed);
        assert_eq!(manager.config.lock().unwrap().accounts[0].disabled, None);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{ not json",
            "a malformed config must be left exactly as found"
        );
        std::fs::remove_file(&path).ok();
    }

    /// Every outcome the user could mistake for success must SAY so, and the two
    /// that are fine must stay silent. This is the mapping the TUI renders, so a
    /// silent failure arm here is a lie on screen.
    #[test]
    fn every_non_durable_persist_outcome_warns() {
        for disabled in [true, false] {
            for outcome in [DisablePersist::Persisted, DisablePersist::NoConfigFile] {
                assert_eq!(
                    outcome.warning(disabled),
                    None,
                    "{outcome:?} is not a failure — warning about it would be noise"
                );
            }
            for outcome in [
                DisablePersist::NoSuchAccount,
                DisablePersist::NoEntry,
                DisablePersist::Ambiguous,
                DisablePersist::WriteFailed,
            ] {
                let warning = outcome
                    .warning(disabled)
                    .unwrap_or_else(|| panic!("{outcome:?} must warn — it did not persist"));
                assert!(
                    warning.starts_with("NOT SAVED"),
                    "{outcome:?} must lead with the headline so it survives truncation: {warning}"
                );
            }
        }
    }

    /// **THE direction guard.** Nothing was written, so the file still says what it
    /// said — and that means the consequence of a failed write is the OPPOSITE in
    /// the two directions. A failed `d` (disable) leaves the account in rotation; a
    /// failed `e` (enable) leaves it BENCHED. One direction-blind line used to
    /// claim "it returns to rotation on restart" for both, so a user whose enable
    /// failed restarted on that advice and got the account benched anyway.
    #[test]
    fn a_failed_persist_states_the_consequence_of_its_own_direction() {
        for outcome in [DisablePersist::NoEntry, DisablePersist::WriteFailed] {
            let disabling = outcome.warning(true).expect("a failed disable must warn");
            assert!(
                disabling.ends_with("it returns to rotation on restart"),
                "a failed DISABLE leaves the account in rotation: {disabling}"
            );

            let enabling = outcome.warning(false).expect("a failed enable must warn");
            assert!(
                enabling.ends_with("it stays benched after a restart"),
                "a failed ENABLE leaves the account benched: {enabling}"
            );
            assert!(
                !enabling.contains("returns to rotation"),
                "a failed enable must never promise the opposite of what happens: {enabling}"
            );
        }
    }

    /// The ambiguity refusal must name an action that actually works. It used to
    /// say "rename one to fix", which is provably wrong whenever both entries carry
    /// an `accountUuid`: the match never consults names on that path, so renaming
    /// changes nothing and the next `d` fails identically.
    #[test]
    fn the_ambiguity_warning_names_a_remedy_that_works() {
        let warning = DisablePersist::Ambiguous
            .warning(true)
            .expect("an ambiguous refusal must warn");
        assert!(
            warning.contains("orgUuid"),
            "the remedy is giving the entries distinct org keys: {warning}"
        );
        assert!(
            !warning.contains("rename"),
            "renaming does not break a UUID-matched tie: {warning}"
        );
    }

    /// A manager with no config file behind it reports that distinctly and does NOT
    /// warn: `tcr demo` and `tcr status --probe` are memory-only by design, and a
    /// "not saved" banner on every keypress there would be noise, not honesty.
    #[test]
    fn set_disabled_without_a_config_file_is_not_a_failure() {
        let manager = build_manager(
            config_with(vec![account("acct-a", 0)]),
            Arc::new(CountingRefresher {
                calls: Arc::new(AtomicUsize::new(0)),
            }),
        );
        assert_eq!(manager.set_disabled(0, true), DisablePersist::NoConfigFile);
        assert_eq!(manager.set_disabled(9, true), DisablePersist::NoSuchAccount);
    }

    /// **INV2 / FIX 6.** The config file write must not run under the config mutex.
    /// That mutex is taken on the PER-CONNECTION path — `state.rs`'s
    /// `session_affinity_enabled()`, called from `mitm.rs`'s serve path — so holding
    /// it across `save_disabled`'s read + `sync_all` + rename let one TUI keypress
    /// stall connection setup on an fsync.
    ///
    /// Measured by holding the config mutex here and requiring the write to complete
    /// anyway. Under a persist that holds `config` across the save, the write cannot
    /// start until this guard drops, so the poll below never sees the flag and the
    /// test fails on its deadline.
    #[test]
    fn the_config_file_write_does_not_run_under_the_config_mutex() {
        let path = tmp_config_path("persist-inv2");
        write_account_file(&path, &["acct-a"]);
        let manager =
            build_manager_with_path(config_with(vec![account("acct-a", 0)]), path.clone());

        // Stand in for a request-path `session_affinity_enabled()` holding the lock.
        let held = manager.config.lock().expect("config lock poisoned");

        let writer = {
            let m = Arc::clone(&manager);
            std::thread::spawn(move || m.set_disabled(0, true))
        };

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            if read_config_json(&path)["accounts"][0]
                .get("disabled")
                .is_some()
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the config file write blocked on the config mutex — INV2 violated"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // INV3 still applies: the memory update is what waits for this guard.
        drop(held);
        assert_eq!(
            writer.join().expect("persist thread panicked"),
            DisablePersist::Persisted
        );
        assert_eq!(
            manager.config.lock().unwrap().accounts[0].disabled,
            Some(true)
        );
        std::fs::remove_file(&path).ok();
    }

    /// **INV1 / FIX 6.** Concurrent persists must not interleave their
    /// read-modify-write of the file. Each of these threads benches a DIFFERENT
    /// account, so every flag must be present at the end; an interleaved
    /// read-modify-write drops the flags written between another thread's read and
    /// its rename, which shows up here as a missing flag.
    #[test]
    fn concurrent_persists_do_not_lose_each_others_writes() {
        const ACCOUNTS: usize = 8;
        let names: Vec<String> = (0..ACCOUNTS).map(|i| format!("acct-{i}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let path = tmp_config_path("persist-inv1");
        write_account_file(&path, &refs);
        let manager = build_manager_with_path(
            config_with(names.iter().map(|n| account(n, 0)).collect()),
            path.clone(),
        );

        let threads: Vec<_> = (0..ACCOUNTS)
            .map(|idx| {
                let m = Arc::clone(&manager);
                std::thread::spawn(move || m.set_disabled(idx, true))
            })
            .collect();
        for thread in threads {
            assert_eq!(
                thread.join().expect("persist thread panicked"),
                DisablePersist::Persisted
            );
        }

        let after = read_config_json(&path);
        for idx in 0..ACCOUNTS {
            assert_eq!(
                after["accounts"][idx]["disabled"],
                serde_json::json!(true),
                "account {idx}'s flag was lost to an interleaved write: {after}"
            );
        }
        assert_eq!(
            after["warmupSeconds"],
            serde_json::json!(900),
            "an unmodelled key was dropped by the concurrent writes"
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

    // ---- live account-add identity resolution (`POST /_tcr/accounts`) ----

    /// A manager whose in-memory config AND on-disk file both start out equal
    /// to `config` — so both halves of `add_or_update_account` are observable:
    /// `manager.accounts`/`manager.config` for the live view, and reloading
    /// `path` for the durable one. Mirrors `proxy.rs`'s `control_manager`.
    fn build_manager_with_disk(config: Config, tag: &str) -> (Arc<Manager>, PathBuf) {
        let path = tmp_config_path(tag);
        config::save(&path, &config).expect("write test config");
        (build_manager_with_path(config, path.clone()), path)
    }

    /// F1 — CRITICAL regression. `{alice@example.com, uuid 1111…, rt-CORP}` is
    /// on record; a submission of `{alice@example.com, uuid 2222…, rt-PERSONAL}`
    /// is a DIFFERENT PERSON who happens to share a display name. The old rule
    /// (`identity::match_one`, which goes through `Queryable` and cannot compare
    /// `account_uuid` at all) matched it onto Corp's row and overwrote its
    /// single-use refresh token — unrecoverable without a hand re-auth. It must
    /// APPEND instead, leaving Corp's row untouched, in memory and on disk.
    #[test]
    fn add_or_update_account_never_overwrites_a_different_persons_refresh_token() {
        let corp = Account {
            account_uuid: Some("uuid-1111".to_string()),
            access_token: "at-CORP".to_string(),
            refresh_token: Some("rt-CORP".to_string()),
            ..account("alice@example.com", 0)
        };
        let (manager, path) = build_manager_with_disk(config_with(vec![corp]), "f1-uuid-mismatch");

        let submission = Account {
            account_uuid: Some("uuid-2222".to_string()),
            access_token: "at-PERSONAL".to_string(),
            refresh_token: Some("rt-PERSONAL".to_string()),
            ..account("alice@example.com", 0)
        };
        let outcome = manager.add_or_update_account(submission);

        let (idx, persist) = match outcome {
            AddAccountOutcome::Added { idx, persist, .. } => (idx, persist),
            other => panic!(
                "a different account_uuid under the same name must APPEND, not \
                 update or refuse: {other:?}"
            ),
        };
        assert_eq!(idx, 1, "appended at the end, Corp's row never moves");
        assert_eq!(persist, AddPersist::Persisted);

        // In memory: Corp's row is byte-identical.
        {
            let accounts = manager.accounts.read().expect("accounts lock poisoned");
            assert_eq!(accounts.len(), 2);
            assert_eq!(
                accounts[0].refresh_token.as_deref(),
                Some("rt-CORP"),
                "Corp's single-use refresh token was overwritten in memory"
            );
            assert_eq!(accounts[0].access_token, "at-CORP");
            assert_eq!(accounts[1].refresh_token.as_deref(), Some("rt-PERSONAL"));
        }

        // On disk: same.
        let reloaded = config::load(&path).expect("reload persisted config");
        assert_eq!(reloaded.accounts.len(), 2);
        assert_eq!(
            reloaded.accounts[0].refresh_token.as_deref(),
            Some("rt-CORP"),
            "Corp's single-use refresh token was overwritten on disk"
        );
        assert_eq!(
            reloaded.accounts[1].refresh_token.as_deref(),
            Some("rt-PERSONAL")
        );

        std::fs::remove_file(&path).ok();
    }

    /// F2 — CRITICAL regression. A legacy row with NO stored org, met by a
    /// submission carrying the row's OWN org (even just profiled for the first
    /// time), must resolve to the SAME row on both the live and durable halves
    /// — never split into a second live row backed by the same one disk entry.
    #[test]
    fn add_or_update_account_backfills_a_legacy_no_org_row_instead_of_splitting_the_fleet() {
        let legacy = Account {
            account_uuid: Some("uuid-legacy".to_string()),
            refresh_token: Some("rt-legacy".to_string()),
            ..account("alice@example.com", 0)
        };
        let (manager, path) = build_manager_with_disk(config_with(vec![legacy]), "f2-legacy-org");

        let submission = Account {
            account_uuid: Some("uuid-legacy".to_string()),
            org_uuid: Some("org-corp-uuid".to_string()),
            org_name: Some("Corp".to_string()),
            access_token: "at-fresh".to_string(),
            refresh_token: Some("rt-fresh".to_string()),
            ..account("alice@example.com", 0)
        };
        let outcome = manager.add_or_update_account(submission);

        let (idx, persist) = match outcome {
            AddAccountOutcome::Updated { idx, persist, .. } => (idx, persist),
            other => panic!(
                "a legacy no-org row meeting its own org must UPDATE in place, \
                 not split the fleet: {other:?}"
            ),
        };
        assert_eq!(idx, 0);
        assert_eq!(persist, AddPersist::Persisted);

        // LIVE: one row, now carrying the org and the fresh credentials.
        {
            let accounts = manager.accounts.read().expect("accounts lock poisoned");
            assert_eq!(
                accounts.len(),
                1,
                "a second live row appeared — the split brain this fixes"
            );
            assert_eq!(accounts[0].org_uuid.as_deref(), Some("org-corp-uuid"));
            assert_eq!(accounts[0].org_name.as_deref(), Some("Corp"));
            assert_eq!(accounts[0].refresh_token.as_deref(), Some("rt-fresh"));
        }

        // DISK: agrees — one entry, same identity, now carrying the org too, so
        // the NEXT submission (a genuinely different org) requires an exact
        // match instead of tolerating a still-unknown org key forever.
        let reloaded = config::load(&path).expect("reload persisted config");
        assert_eq!(reloaded.accounts.len(), 1, "disk split into a second entry");
        assert_eq!(
            reloaded.accounts[0].org_uuid.as_deref(),
            Some("org-corp-uuid")
        );
        assert_eq!(
            reloaded.accounts[0].refresh_token.as_deref(),
            Some("rt-fresh")
        );

        std::fs::remove_file(&path).ok();
    }

    /// F3 — HIGH regression. The old two-lock version resolved under a READ
    /// lock, released it, then took a separate WRITE lock to append — so N
    /// concurrent submissions of the same brand-new identity could all resolve
    /// "no match" before any of them had appended (measured 73 duplicate rows
    /// in 200 concurrent rounds). Fire N concurrent adds of ONE new identity and
    /// assert exactly one caller appended, on both halves.
    #[test]
    fn concurrent_adds_of_one_new_identity_never_duplicate_the_live_row() {
        use std::thread;
        let (manager, path) = build_manager_with_disk(
            config_with(vec![account("alice@example.com", 0)]),
            "f3-toctou",
        );

        let n = 16usize;
        let submission = Account {
            account_uuid: Some("uuid-new".to_string()),
            access_token: "at-new".to_string(),
            refresh_token: Some("rt-new".to_string()),
            ..account("carol@example.com", 0)
        };

        let handles: Vec<_> = (0..n)
            .map(|_| {
                let m = Arc::clone(&manager);
                let acct = submission.clone();
                thread::spawn(move || m.add_or_update_account(acct))
            })
            .collect();
        let outcomes: Vec<AddAccountOutcome> = handles
            .into_iter()
            .map(|h| h.join().expect("add_or_update_account thread panicked"))
            .collect();

        let added = outcomes
            .iter()
            .filter(|o| matches!(o, AddAccountOutcome::Added { .. }))
            .count();
        let updated = outcomes
            .iter()
            .filter(|o| matches!(o, AddAccountOutcome::Updated { .. }))
            .count();
        assert_eq!(
            added, 1,
            "exactly one concurrent caller may append a brand-new identity; \
             the rest must see it as already-added (Updated): added={added} updated={updated}"
        );
        assert_eq!(updated, n - 1);

        let accounts = manager.accounts.read().expect("accounts lock poisoned");
        assert_eq!(
            accounts.len(),
            2,
            "alice's row plus exactly one new row for carol — not a duplicate"
        );
        drop(accounts);

        let reloaded = config::load(&path).expect("reload persisted config");
        assert_eq!(
            reloaded.accounts.len(),
            2,
            "disk duplicated the new identity"
        );

        std::fs::remove_file(&path).ok();
    }

    /// A follow-up regression on the Added path (distinct from the F3 TOCTOU
    /// finding above, and from `config::save_account`'s identical fix on the
    /// durable side): appending an account with no explicit priority must
    /// join the BACK of the LIVE fleet — `max(existing priorities) + 1` — not
    /// the 0 that `AccountRuntime::from_config`'s `unwrap_or(0)` reads from an
    /// absent `priority`, which would silently promote a freshly added
    /// account to the PRIMARY tier ahead of the established fleet.
    #[test]
    fn add_or_update_account_added_path_assigns_max_plus_one_priority_when_none_submitted() {
        let (manager, path) = build_manager_with_disk(
            config_with(vec![
                account("alice@example.com", 0),
                account("bob@example.com", 1),
            ]),
            "live-default-priority",
        );

        let submission = Account {
            priority: None,
            ..account("carol@example.com", 0)
        };
        let idx = match manager.add_or_update_account(submission) {
            AddAccountOutcome::Added { idx, .. } => idx,
            other => panic!("expected Added: {other:?}"),
        };

        {
            let accounts = manager.accounts.read().expect("accounts lock poisoned");
            assert_eq!(
                accounts[idx].priority, 2,
                "an added account with no explicit priority must join the back \
                 of the live fleet, not the default-derived 0"
            );
        }

        let reloaded = config::load(&path).expect("reload persisted config");
        assert_eq!(
            reloaded.accounts[idx].priority,
            Some(2),
            "the durable half must agree with the live rotation on the assigned priority"
        );

        std::fs::remove_file(&path).ok();
    }

    /// An explicit priority submitted on the Added path is never overridden by
    /// the max+1 default — mirrors the durable-side test of the same name in
    /// `config.rs`.
    #[test]
    fn add_or_update_account_added_path_keeps_an_explicit_priority() {
        let (manager, path) = build_manager_with_disk(
            config_with(vec![account("alice@example.com", 0)]),
            "live-explicit-priority",
        );

        let submission = Account {
            priority: Some(99),
            ..account("carol@example.com", 0)
        };
        let idx = match manager.add_or_update_account(submission) {
            AddAccountOutcome::Added { idx, .. } => idx,
            other => panic!("expected Added: {other:?}"),
        };

        let accounts = manager.accounts.read().expect("accounts lock poisoned");
        assert_eq!(accounts[idx].priority, 99);
        drop(accounts);

        std::fs::remove_file(&path).ok();
    }

    /// Before the first fix, the Added path's `AccountRuntime::from_config(&account)`
    /// ran INSIDE the `self.accounts.write()` block — meaning
    /// `build_serving_client`'s `.expect("build reqwest client")` executed while
    /// the write guard was held. A panic there poisons the `RwLock` for every
    /// other caller in this module: `http_client`, `select`, `snapshot`,
    /// `record_stream_error` and ~93 more all `.expect("accounts lock
    /// poisoned")`, so one recoverable client-build failure would take down the
    /// whole proxy.
    ///
    /// A second round moved the build OUTSIDE `self.accounts`'s lock but left
    /// it inside `config_write` (held from function entry in that version) —
    /// so the SAME panic instead poisoned `config_write`, which is arguably
    /// worse: `persist_tokens` and `persist_now`'s shutdown flush both
    /// `.expect()` it, so a poisoned `config_write` stops rotated refresh
    /// tokens from ever reaching disk again, on top of every future call to
    /// this very function panicking at its own `config_write.lock()`.
    ///
    /// Uses the `#[cfg(test)]` fault-injection seam on `build_serving_client`
    /// to make that failure real rather than hypothetical, on a submission
    /// guaranteed to resolve Added (an empty fleet, so nothing can match),
    /// and asserts BOTH locks survive.
    #[test]
    fn add_or_update_account_added_path_does_not_poison_either_lock_on_a_client_build_panic() {
        let manager = build_manager(config_with(vec![]), lock_refresher());
        let submission = account("brand-new@example.com", 0);

        fail_next_client_build();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            manager.add_or_update_account(submission)
        }));
        assert!(
            result.is_err(),
            "setup: the injected client-build failure must actually panic"
        );

        assert!(
            manager.accounts.read().is_ok(),
            "a client-build panic in add_or_update_account must not poison the \
             accounts lock — every other .expect(\"accounts lock poisoned\") \
             call site would panic in turn"
        );
        assert!(
            manager.config_write.lock().is_ok(),
            "a client-build panic in add_or_update_account must not poison the \
             config_write lock either — persist_tokens and persist_now's \
             shutdown flush both .expect() it, and this function itself takes \
             it again on its very next call"
        );
    }

    /// The Added-only half of the fix above: an ordinary re-auth of an
    /// EXISTING account (the common case this route serves — every token
    /// refresh's re-login goes through `POST /_tcr/accounts`, not just a
    /// first-time add) must resolve Updated on its FIRST locked attempt and
    /// never reach `build_serving_client` at all. A version that built the
    /// runtime unconditionally before resolving (fixing the poisoning bug
    /// above but not this one) would waste a client build on every re-auth
    /// and widen the panic surface to a call that never needed one. Arms the
    /// SAME fault-injection seam on a submission guaranteed to resolve
    /// Updated (an existing row with a matching bare identity) — if the
    /// build were ever reached the injected panic would fire, so a normal
    /// return proves it was not.
    #[test]
    fn add_or_update_account_updated_path_never_builds_a_client() {
        let existing = account("alice@example.com", 0);
        let manager = build_manager(config_with(vec![existing.clone()]), lock_refresher());
        let mut resubmission = existing.clone();
        resubmission.access_token = "at-alice-rotated".to_string();

        fail_next_client_build();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            manager.add_or_update_account(resubmission)
        }));

        let outcome = result.unwrap_or_else(|_| {
            panic!(
                "add_or_update_account must never reach build_serving_client on \
                 the Updated path — the injected client-build failure should \
                 not have fired at all"
            )
        });
        assert!(
            matches!(outcome, AddAccountOutcome::Updated { .. }),
            "setup: the resubmission must resolve against the existing row, \
             not append a new one"
        );
    }

    /// The live half of a brand-new-identity race was already serialized by
    /// the single `accounts` write-lock spanning resolve+mutate (see the
    /// TOCTOU fix above) — but the two callers' DURABLE writes still queued on
    /// `config_write` independently, in whatever order the OS scheduled them,
    /// with no guarantee that order agreed with which caller actually won the
    /// live race. Two real logins of the SAME identity racing each other (a
    /// double-submit, a retried `tcr login`) carry genuinely DIFFERENT tokens,
    /// so a disagreement is observable: the file could end up holding the
    /// LOSING submission's token while the live rotation served the winner's.
    /// Measured 2/200 over 200 rounds of two simultaneous adds of one
    /// identity before `config_write` was widened to span the whole
    /// resolve-mutate-persist sequence. Runs the same 200 rounds to catch it.
    #[test]
    fn concurrent_adds_of_one_new_identity_never_disagree_with_disk_on_the_token() {
        use std::thread;
        for round in 0..200 {
            let (manager, path) = build_manager_with_disk(
                config_with(vec![account("alice@example.com", 0)]),
                &format!("n2-token-race-{round}"),
            );

            let base = Account {
                account_uuid: Some("uuid-new".to_string()),
                ..account("carol@example.com", 0)
            };
            let submission_a = Account {
                access_token: "at-A".to_string(),
                refresh_token: Some("rt-A".to_string()),
                ..base.clone()
            };
            let submission_b = Account {
                access_token: "at-B".to_string(),
                refresh_token: Some("rt-B".to_string()),
                ..base
            };

            let m1 = Arc::clone(&manager);
            let m2 = Arc::clone(&manager);
            let h1 = thread::spawn(move || m1.add_or_update_account(submission_a));
            let h2 = thread::spawn(move || m2.add_or_update_account(submission_b));
            h1.join().expect("adder A panicked");
            h2.join().expect("adder B panicked");

            let live_token = {
                let accounts = manager.accounts.read().expect("accounts lock poisoned");
                accounts
                    .iter()
                    .find(|a| a.name == "carol@example.com")
                    .expect("carol's row exists live")
                    .access_token
                    .clone()
            };
            let reloaded = config::load(&path).expect("reload persisted config");
            let disk_token = reloaded
                .accounts
                .iter()
                .find(|a| a.name == "carol@example.com")
                .expect("carol's row exists on disk")
                .access_token
                .clone();

            assert_eq!(
                live_token, disk_token,
                "round {round}: live token ({live_token}) and disk token \
                 ({disk_token}) disagree for the same identity"
            );

            std::fs::remove_file(&path).ok();
        }
    }

    /// F4 — an `Error` row given fresh credentials must clear back to `Active`
    /// (`eligible` hard-gates on `status == Error`) but its rate-limit hold must
    /// SURVIVE the credential replace — clearing it bought nothing but the
    /// ability to race a genuinely-still-limited account back into rotation
    /// early. An EXPIRED hold proves both facts at once without contradiction:
    /// the field keeps its stamped value, and the account is selectable because
    /// that value is already in the past.
    #[test]
    fn add_or_update_account_clears_error_status_but_keeps_the_rate_limit_hold() {
        let (manager, path) = build_manager_with_disk(
            config_with(vec![account("alice@example.com", 0)]),
            "f4-hold-survives",
        );
        let hold_until = crate::now_ms() - 1_000;
        {
            let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
            accounts[0].status = AccountStatus::Error;
            accounts[0].rate_limited_until_ms = Some(hold_until);
        }

        let submission = Account {
            access_token: "at-fresh".to_string(),
            refresh_token: Some("rt-fresh".to_string()),
            ..account("alice@example.com", 0)
        };
        let outcome = manager.add_or_update_account(submission);
        assert!(
            matches!(outcome, AddAccountOutcome::Updated { idx: 0, .. }),
            "{outcome:?}"
        );

        {
            let accounts = manager.accounts.read().expect("accounts lock poisoned");
            assert_eq!(
                accounts[0].status,
                AccountStatus::Active,
                "fresh credentials must clear a stuck error"
            );
            assert_eq!(
                accounts[0].rate_limited_until_ms,
                Some(hold_until),
                "the rate-limit hold must survive a credential replace"
            );
        }

        assert_eq!(
            manager.select(
                &HashSet::new(),
                OffsetDateTime::now_utc(),
                None,
                None,
                "/v1/messages",
                None
            ),
            Some(0),
            "active status + an EXPIRED hold ⇒ immediately selectable"
        );

        std::fs::remove_file(&path).ok();
    }

    /// F5 hardening. When the durable write must APPEND on `persist_replaced`'s
    /// path (no on-disk entry carries this identity — e.g. it was deleted while
    /// the proxy ran), the appended record must carry the row's REAL
    /// `priority`/`disabled`, never `identity::probe`'s `None` placeholders —
    /// otherwise a deliberately-benched account comes back into rotation on the
    /// very next restart, the exact bug `persist_disabled` exists to prevent.
    #[test]
    fn add_or_update_account_persists_real_routing_state_when_appending_via_replace() {
        let alice = Account {
            account_uuid: Some("uuid-alice".to_string()),
            priority: Some(5),
            disabled: Some(true),
            ..account("alice@example.com", 5)
        };
        let (manager, path) = build_manager_with_disk(config_with(vec![alice]), "f5-routing-state");

        // The disk entry vanishes out from under the live process — the durable
        // write must now APPEND.
        std::fs::write(&path, r#"{"accounts": []}"#).expect("rewrite disk config");

        let submission = Account {
            account_uuid: Some("uuid-alice".to_string()),
            access_token: "at-fresh".to_string(),
            refresh_token: Some("rt-fresh".to_string()),
            ..account("alice@example.com", 0)
        };
        let outcome = manager.add_or_update_account(submission);
        assert!(
            matches!(outcome, AddAccountOutcome::Updated { idx: 0, .. }),
            "{outcome:?}"
        );

        let reloaded = config::load(&path).expect("reload persisted config");
        assert_eq!(reloaded.accounts.len(), 1);
        assert_eq!(
            reloaded.accounts[0].priority,
            Some(5),
            "a benched account's priority must survive an append-via-replace"
        );
        assert_eq!(
            reloaded.accounts[0].disabled,
            Some(true),
            "…and its disabled flag — the exact restart-un-bench persist_disabled exists to prevent"
        );

        std::fs::remove_file(&path).ok();
    }

    /// F6 — an update backfills every identity field the row is missing
    /// (`account_uuid`/`org_uuid`/`org_name`) and always corrects `account_type`
    /// to the submitted value: a row left at a stale non-`"oauth"` type is a
    /// silent death, because `refresh_plan` skips any account whose type is not
    /// `"oauth"`. On both the live row and the disk entry.
    #[test]
    fn add_or_update_account_backfills_identity_and_corrects_a_stale_account_type() {
        let legacy_api_row = Account {
            account_type: "api".to_string(),
            account_uuid: None,
            org_uuid: None,
            org_name: None,
            access_token: "sk-old".to_string(),
            refresh_token: None,
            ..account("alice@example.com", 0)
        };
        let (manager, path) =
            build_manager_with_disk(config_with(vec![legacy_api_row]), "f6-backfill");

        let submission = Account {
            account_type: "oauth".to_string(),
            account_uuid: Some("uuid-alice".to_string()),
            org_uuid: Some("org-corp-uuid".to_string()),
            org_name: Some("Corp".to_string()),
            access_token: "at-fresh".to_string(),
            refresh_token: Some("rt-fresh".to_string()),
            ..account("alice@example.com", 0)
        };
        let outcome = manager.add_or_update_account(submission);
        assert!(
            matches!(outcome, AddAccountOutcome::Updated { idx: 0, .. }),
            "{outcome:?}"
        );

        {
            let accounts = manager.accounts.read().expect("accounts lock poisoned");
            assert_eq!(
                accounts[0].account_type, "oauth",
                "a stale non-oauth type must be corrected, or refresh_plan silently \
                 never refreshes this account again"
            );
            assert_eq!(accounts[0].account_uuid.as_deref(), Some("uuid-alice"));
            assert_eq!(accounts[0].org_uuid.as_deref(), Some("org-corp-uuid"));
            assert_eq!(accounts[0].org_name.as_deref(), Some("Corp"));
        }

        let reloaded = config::load(&path).expect("reload persisted config");
        assert_eq!(reloaded.accounts[0].account_type, "oauth");
        assert_eq!(
            reloaded.accounts[0].account_uuid.as_deref(),
            Some("uuid-alice")
        );
        assert_eq!(
            reloaded.accounts[0].org_uuid.as_deref(),
            Some("org-corp-uuid")
        );
        assert_eq!(reloaded.accounts[0].org_name.as_deref(), Some("Corp"));

        std::fs::remove_file(&path).ok();
    }

    /// Preservation guard: a submission with NO identity fields at all (a bare
    /// name) must still find a stored row whose display name carries an org
    /// suffix, via the CLI's own email-of fallback — `identity::resolve`'s exact
    /// name equality alone would miss it and append a duplicate. This is the
    /// one case the bare-name `match_one` fallback exists for.
    #[test]
    fn add_or_update_account_bare_email_still_matches_a_display_name_with_org_suffix() {
        let display_named = Account {
            name: "alice@example.com (Corp)".to_string(),
            refresh_token: Some("rt-old".to_string()),
            ..account("alice@example.com (Corp)", 0)
        };
        let (manager, path) =
            build_manager_with_disk(config_with(vec![display_named]), "f-bare-email-fallback");

        let submission = Account {
            access_token: "at-fresh".to_string(),
            refresh_token: Some("rt-fresh".to_string()),
            ..account("alice@example.com", 0)
        };
        let outcome = manager.add_or_update_account(submission);
        let idx = match outcome {
            AddAccountOutcome::Updated { idx, .. } => idx,
            other => panic!("a bare email must still match its display-named row: {other:?}"),
        };
        assert_eq!(idx, 0);
        assert_eq!(
            manager
                .accounts
                .read()
                .expect("accounts lock poisoned")
                .len(),
            1,
            "no duplicate appended"
        );

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

    /// **THE truncation guard.** `warm_targets`' own doc-comment calls a warm that
    /// latches no evidence "self-limiting", on the strength of `warm_account`
    /// folding the response's rate-limit headers back into the account's quota. A
    /// 200 that carries none of the `anthropic-ratelimit-unified-5h-*` headers
    /// folds NOTHING (`Quota::update_from_headers`'s own doc-comment: "a response
    /// without the header latches nothing") — so the account's 5h window stays
    /// blank, `live_reset` stays `None`, and it stays a `warm_targets` member
    /// forever, warmed again every cadence at real upstream cost. This asserts the
    /// promise in `warm_targets`' doc-comment actually holds: a warm that never
    /// latches evidence must not repeat without limit.
    #[tokio::test]
    async fn warm_account_without_evidence_headers_does_not_repeat_forever() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let warmer = Arc::new(HeaderlessWarmer);
        let manager =
            build_manager_with_warmer(config_with(vec![account("cold", 0)]), refresher, warmer);
        mark_all_probed(&manager); // isolate from the never-probed boot gate
        assert_eq!(manager.warm_targets(), vec![0]);

        // A generous number of cadence-driven warms, standing in for "forever" —
        // if the account is STILL a target after this many header-less 200s, the
        // loop has no bound at all.
        for _ in 0..50 {
            manager.warm_account(0).await;
        }

        assert!(
            !manager.warm_targets().contains(&0),
            "a warm that never latches evidence must not remain an eligible target forever"
        );
    }

    /// A follow-up regression on the bound above: it must not be defeatable by
    /// the background prober. A first cut of the recovery fix reset
    /// `consecutive_warms_without_evidence` on every successful `apply_usage`
    /// call — but `probeable_indices` (unlike `warm_targets`) does not check
    /// that counter, so the prober keeps visiting an excluded account on its
    /// OWN schedule regardless of exclusion. That reset would reopen the gate
    /// every probe cycle and hand back a full burst of
    /// `WARM_ATTEMPTS_WITHOUT_EVIDENCE_LIMIT` header-less warms each time —
    /// MORE spend than the original unbounded-warm bug, not less. Drives an
    /// account past the limit, then feeds it several successful
    /// no-5h-bucket probes (several "probe cycles") and asserts it stays
    /// excluded throughout — recovery must come from the cooldown, not from
    /// probe traffic.
    #[tokio::test]
    async fn repeated_probe_cycles_do_not_reopen_a_warm_exclusion() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let warmer = Arc::new(HeaderlessWarmer);
        let manager =
            build_manager_with_warmer(config_with(vec![account("cold", 0)]), refresher, warmer);
        mark_all_probed(&manager); // isolate from the never-probed boot gate
        assert_eq!(manager.warm_targets(), vec![0]);

        for _ in 0..50 {
            manager.warm_account(0).await;
        }
        assert!(
            !manager.warm_targets().contains(&0),
            "setup: the account must be excluded before the probe-cycle check runs"
        );

        let headerless_probe = crate::probe::Usage {
            five_hour: None,
            seven_day: None,
            seven_day_oi: None,
        };
        for cycle in 0..5 {
            manager.apply_usage(0, &headerless_probe);
            assert!(
                !manager.warm_targets().contains(&0),
                "probe cycle {cycle}: a successful probe that reads no 5h bucket \
                 must not, by itself, reopen a warm exclusion — recovery is the \
                 cooldown, not probe traffic"
            );
        }
    }

    /// The recovery half of the bound above. `consecutive_warms_without_evidence`
    /// is a ONE-WAY latch with a single writer (`record_warm_without_evidence`)
    /// and only one reset (`update_quota`, on a header-bearing served or warmed
    /// response) — but an account excluded from `warm_targets` is never picked
    /// to SERVE either, so it can never earn one of those on its own. Left that
    /// way, a transient upstream condition that strips the 5h headers for a few
    /// minutes would exclude the account from keep-warm for the life of the
    /// process, recoverable only by a restart (this repo's most expensive event
    /// — a full cold prompt-cache prefix for every live session).
    ///
    /// The fix is `warm_evidence_retry_after_ms`'s flat cooldown, set by
    /// `record_warm_without_evidence` once the account crosses the limit. This
    /// drives an account past the limit, confirms it is excluded, fast-forwards
    /// the cooldown by writing a past deadline directly (the same pattern this
    /// module's other `_until_ms`/`_after_ms` cooldown tests use — a real sleep
    /// of `WARM_EVIDENCE_RETRY_COOLDOWN_MS` is not a reasonable thing to do in a
    /// unit test), and asserts the account is a warm target again.
    #[tokio::test]
    async fn warm_evidence_retry_cooldown_recovers_an_excluded_account() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let warmer = Arc::new(HeaderlessWarmer);
        let manager =
            build_manager_with_warmer(config_with(vec![account("cold", 0)]), refresher, warmer);
        mark_all_probed(&manager); // isolate from the never-probed boot gate
        assert_eq!(manager.warm_targets(), vec![0]);

        for _ in 0..50 {
            manager.warm_account(0).await;
        }
        assert!(
            !manager.warm_targets().contains(&0),
            "setup: the account must be excluded before recovery is exercised"
        );
        {
            let accounts = manager.accounts.read().expect("accounts lock poisoned");
            assert!(
                accounts[0].warm_evidence_retry_after_ms.is_some(),
                "setup: crossing the limit must arm a retry cooldown"
            );
        }

        {
            let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
            accounts[0].warm_evidence_retry_after_ms = Some(crate::now_ms() - 1);
        }

        assert!(
            manager.warm_targets().contains(&0),
            "an elapsed retry cooldown must recover an account the warm latch \
             excluded, without a restart"
        );
    }

    /// The failing half of the recovery above. `record_warm_without_evidence`
    /// only re-arms `warm_evidence_retry_after_ms` from the Ok-but-header-less
    /// branch of `warm_account` — a warm that FAILS outright takes the `Err`
    /// arm instead, which (before this fix) never touched the deadline. Once
    /// the cooldown had elapsed, a FAILING retry left the now-past deadline in
    /// place, and `warm_targets`' `cooldown_elapsed` check re-admitted the
    /// account on every subsequent pass rather than once per cooldown — the
    /// "one bounded retry burst per hour" property collapsing back to "every
    /// tick" on the failure path specifically, which is the unbounded-repeat
    /// bug this whole latch exists to bound. Drives an account past the limit
    /// and elapses its cooldown directly (this test is about the `Err` arm's
    /// re-arm, not about repeating `warm_account` 50 times to get there), then
    /// feeds it ONE failing warm (`NoWarmer` always returns `Err`) and asserts
    /// the deadline moved back into the future — and the account is excluded
    /// again immediately, not just eventually.
    #[tokio::test]
    async fn a_failing_retry_does_not_reset_the_warm_evidence_cooldown() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager_with_warmer(
            config_with(vec![account("cold", 0)]),
            refresher,
            Arc::new(NoWarmer),
        );
        mark_all_probed(&manager); // isolate from the never-probed boot gate

        {
            let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
            accounts[0].consecutive_warms_without_evidence = WARM_ATTEMPTS_WITHOUT_EVIDENCE_LIMIT;
            accounts[0].warm_evidence_retry_after_ms = Some(crate::now_ms() - 1);
        }
        assert!(
            manager.warm_targets().contains(&0),
            "setup: an elapsed cooldown must make the account a target again"
        );

        manager.warm_account(0).await; // NoWarmer always returns Err

        let deadline = {
            let accounts = manager.accounts.read().expect("accounts lock poisoned");
            accounts[0].warm_evidence_retry_after_ms
        };
        assert!(
            deadline.is_some_and(|until| until > crate::now_ms()),
            "a FAILING retry must re-arm the cooldown into the future too, not \
             just an Ok-but-header-less one — otherwise the account is \
             re-admitted on every subsequent tick instead of once per cooldown"
        );
        assert!(
            !manager.warm_targets().contains(&0),
            "immediately after a failing retry the account must be excluded \
             again, not left eligible on a stale past deadline"
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

    /// **State 1 — evidence from the OTHER source.** A served response's
    /// rate-limit headers are a first-hand read of that account's 5h window, so
    /// they open the gate exactly as a probe does. Without this an account carrying
    /// live traffic read as "quota unknown", which is false on its face: we have
    /// its window, we just did not get it from the probe.
    ///
    /// The header path must also WAKE the loop, for the same reason the probe path
    /// does — an eligible account may not sit out a whole `warmupSeconds`.
    #[tokio::test]
    async fn a_served_response_header_is_quota_evidence_and_opens_the_gate() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(config_with(vec![account("a", 0)]), refresher);
        let wake_window = std::time::Duration::from_millis(50);
        assert!(manager.probe_interval_seconds() > 0);
        assert!(manager.warm_targets().is_empty(), "boot: nothing read yet");

        // A response carrying the unified 5h window, with a reset already past —
        // so the window it reports is EXPIRED and the account is genuinely cold.
        set_5h(&manager, 0, "0.10", -1);

        assert!(
            manager.accounts.read().unwrap()[0].quota_known,
            "a response that reported the 5h window is evidence about that window"
        );
        assert_eq!(
            manager.warm_targets(),
            vec![0],
            "with the window read and not live, the account is a target"
        );
        tokio::time::timeout(wake_window, manager.warm_wake().notified())
            .await
            .expect("the first read wakes the warm loop whichever source it came from");
    }

    /// …but only a response that actually SAID something about the 5h window. A
    /// response without the header — an error page, a non-Anthropic upstream — is
    /// silence, and silence is not evidence. `set_7d` reports the weekly window and
    /// no 5h window, which is exactly that shape.
    #[test]
    fn a_response_without_the_five_hour_header_latches_nothing() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager(config_with(vec![account("a", 0)]), refresher);

        set_7d(&manager, 0, "0.10");

        assert!(
            !manager.accounts.read().unwrap()[0].quota_known,
            "a weekly-only response says nothing about the 5h window"
        );
        assert!(
            manager.warm_targets().is_empty(),
            "so the gate must still be waiting"
        );
    }

    /// **State 3 — the bounded wait.** `quota_known` can never latch while the
    /// probe fails, so gating on it *unconditionally* makes keep-warm structurally
    /// dark: silently doing nothing while config and TUI both read as enabled.
    /// After `PROBE_FAILURES_BEFORE_WARMING_UNPROBED` consecutive failed sweeps the
    /// gate stops waiting — absence of evidence is not evidence of a live window —
    /// and wakes the loop it kept parked.
    ///
    /// One failure must NOT be enough: the fleet-wide false error `probing.rs`
    /// documents is a one-sweep event, and lifting on it hands the boot burst back.
    #[tokio::test]
    async fn a_persistently_failing_probe_stops_blocking_keep_warm() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        // No account carries `ok_token`, so every sweep fails on every row.
        let manager = build_manager_with_prober(
            config_with(vec![account("a", 0)]),
            refresher,
            Arc::new(ScriptedProber {
                ok_token: "at-nobody".to_string(),
            }),
        );
        let wake_window = std::time::Duration::from_millis(50);
        assert!(manager.probe_interval_seconds() > 0);

        for sweep in 1..PROBE_FAILURES_BEFORE_WARMING_UNPROBED {
            manager.probe_all().await;
            assert!(
                manager.warm_targets().is_empty(),
                "sweep {sweep} of {PROBE_FAILURES_BEFORE_WARMING_UNPROBED}: one or two failures are a hiccup, not a verdict"
            );
            assert!(
                tokio::time::timeout(wake_window, manager.warm_wake().notified())
                    .await
                    .is_err(),
                "sweep {sweep}: nothing may wake the loop while the gate is still waiting"
            );
        }

        manager.probe_all().await;

        {
            let accounts = manager.accounts.read().unwrap();
            assert!(
                !accounts[0].quota_known,
                "the escape valve must NOT fake evidence — nothing was ever read"
            );
            assert_eq!(
                accounts[0].consecutive_probe_failures,
                PROBE_FAILURES_BEFORE_WARMING_UNPROBED
            );
        }
        assert_eq!(
            manager.warm_targets(),
            vec![0],
            "a probe that has failed every sweep is not going to answer; waiting forever is a kill switch"
        );
        tokio::time::timeout(wake_window, manager.warm_wake().notified())
            .await
            .expect("giving up on the wait must wake the loop, not leave it a full interval away");
    }

    /// The escape valve is armed by a RUN of failures, not a total: one success in
    /// between resets it. Otherwise an account that fails, recovers, and fails
    /// again drifts into "stop waiting" on evidence it actually has.
    #[tokio::test]
    async fn a_successful_probe_resets_the_failure_run() {
        let refresher = Arc::new(CountingRefresher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let manager = build_manager_with_prober(
            config_with(vec![account("a", 0)]),
            refresher,
            Arc::new(ScriptedProber {
                ok_token: "at-nobody".to_string(),
            }),
        );
        manager.probe_all().await;
        assert_eq!(
            manager.accounts.read().unwrap()[0].consecutive_probe_failures,
            1
        );

        manager.record_probe(0, ProbeStatus::Ok, None, None);

        assert_eq!(
            manager.accounts.read().unwrap()[0].consecutive_probe_failures,
            0,
            "a successful read ends the run"
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
                        if let Some(idx) =
                            manager.select(&empty, now, None, session_key, "/v1/messages", None)
                        {
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
                        manager.select(&empty, now, None, Some(key), "/v1/messages", None);
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
            manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
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
            manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
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
