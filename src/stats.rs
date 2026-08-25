//! The shared, live snapshot the TUI renders.
//!
//! [`Manager::snapshot`](crate::manager::Manager::snapshot) computes one of these
//! from the current account state at a caller-supplied `now`. Every quota figure
//! is evaluated live at that instant (see [`crate::quota::QuotaWindow::effective`]),
//! so a bar the TUI draws is never a stale cached value — that is the display half
//! of bug #2's fix.

use time::OffsetDateTime;

use crate::probe::ProbeStatus;

/// One row of the recent-request ring buffer.
#[derive(Debug, Clone)]
pub struct RequestLogEntry {
    pub time: OffsetDateTime,
    pub method: String,
    pub path: String,
    pub status: u16,
    pub account: String,
}

/// How close an account is to its own switch threshold, evaluated live. Drives
/// an honest "near limit" / "exhausted" label: an account parked out of rotation
/// because it is near/over its weekly (or 5-hour) cap is still operationally
/// **active** — never the red `error` reserved for a dead credential.
///
/// Serde-derived so it crosses the status endpoint's wire ([`crate::status`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QuotaState {
    /// Comfortably under the switch threshold — in normal rotation.
    #[default]
    Normal,
    /// At/over the switch threshold but under 100%: held out of rotation until
    /// the window resets, though the credential itself is fine.
    NearLimit,
    /// At/over 100% on a gating window — fully spent until it resets.
    Exhausted,
}

impl QuotaState {
    /// The one place the `>=1.0 => Exhausted`, `>=threshold => NearLimit`, else
    /// `Normal` rule lives. Takes an already-`Option`-flattened utilization —
    /// `None` (window has no reading yet) maps to `Normal`, matching the
    /// pre-existing gating computation in
    /// [`crate::manager::Manager::snapshot`], which folds a `None` window out of
    /// the `reduce(f64::max)` before ever reaching this match. Shared by the
    /// combined gating state (`snapshot.rs`) and the per-window state now
    /// surfaced in the accounts JSON (`cli.rs`), so the threshold logic exists
    /// exactly once.
    pub fn from_utilization(util: Option<f64>, threshold: f64) -> Self {
        match util {
            Some(u) if u >= 1.0 => QuotaState::Exhausted,
            Some(u) if u >= threshold => QuotaState::NearLimit,
            _ => QuotaState::Normal,
        }
    }
}

/// Why an account is currently out of rotation, mirroring the hard gates
/// [`crate::manager::Manager::eligible`] applies — the single source of truth is
/// [`crate::manager::Manager::account_gate`], which computes this alongside a
/// [`AccountSnapshot::free_at`] clear-instant. Soft pacing is deliberately NOT a
/// reason here: it only ever narrows an already-healthy account, never holds one
/// out, so a paced account still reads [`GateReason::Ok`].
///
/// Serde-derived so it crosses the status endpoint's wire ([`crate::status`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GateReason {
    /// In rotation — no hard gate is active.
    Ok,
    /// A rate-limit hold (429 `retry-after`) that has not yet lifted.
    Hold,
    /// The shared 5-hour bucket is at/over threshold.
    FiveHour,
    /// The shared weekly bucket is at/over threshold.
    SevenDay,
    /// The model-scoped weekly (Fable `7d_oi`) bucket is at/over threshold — only
    /// ever surfaced for a Fable-scoped evaluation (`is_fable = true`).
    FableWeekly,
    /// A standard (API-key) token/request limit is at/over threshold — mirrors the
    /// standard branch of [`crate::quota::Quota::is_near`], so `account_gate`
    /// agrees with `eligible` on API-key accounts. Never fires for OAuth accounts
    /// (all standard fields `None`).
    Standard,
    /// A dead credential (`AccountStatus::Error`) — needs a re-login, never self-frees.
    Login,
    /// Anthropic answered `anthropic-ratelimit-unified-status: rejected` for this
    /// account. Unlike a window it carries no reset to wait on, so it never
    /// self-frees. Long held out of rotation by
    /// [`crate::manager::Manager::account_hard_ok`], it had no reason of its own
    /// here and so rendered as [`GateReason::Ok`].
    Rejected,
    /// Operator-disabled — held out until re-enabled, never self-frees.
    Disabled,
    /// This account belongs to a group marked `reserved` (`tcr group reserve`)
    /// and the request that would have selected it did not ask for one of
    /// that account's groups. Unlike the other gates this one is a fact about
    /// the REQUEST's group ask, not purely about the account — see
    /// [`crate::manager::Manager::account_gate`]'s `group` parameter. Never
    /// self-frees on its own (only `tcr group unreserve` clears it), so it
    /// carries no clear-instant, same as [`Self::Disabled`].
    Reserved,
}

/// A single account's live-computed view.
#[derive(Debug, Clone)]
pub struct AccountSnapshot {
    pub name: String,
    pub priority: i64,
    pub status: String,
    pub disabled: bool,
    /// 5-hour utilization (0.0–1.0), evaluated live — `None` if never learned.
    pub five_hour: Option<f64>,
    pub five_hour_reset: Option<OffsetDateTime>,
    /// Weekly utilization (0.0–1.0), evaluated live.
    pub seven_day: Option<f64>,
    pub seven_day_reset: Option<OffsetDateTime>,
    /// Model-scoped weekly utilization (Fable), evaluated live.
    pub seven_day_oi: Option<f64>,
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Cache-read input tokens (a subset of `input_tokens`). The prompt-cache
    /// hit ratio the TUI and `tcr status` surface is `cache_read_tokens /
    /// input_tokens` (guarded when `input_tokens == 0`).
    pub cache_read_tokens: u64,
    /// Cache-creation input tokens (also a subset of `input_tokens`).
    pub cache_creation_tokens: u64,
    pub last_used: Option<OffsetDateTime>,
    /// If throttled, when the hold lifts (only while still in the future).
    pub rate_limited_until: Option<OffsetDateTime>,
    /// Health of the most recent background quota probe for this account.
    pub probe_status: ProbeStatus,
    /// When the most recent probe finished — the TUI shows its age so a stalled
    /// probe is visible rather than silently freezing the quota bars.
    pub last_probe: Option<OffsetDateTime>,
    /// The most recent probe's error message, if it failed.
    pub probe_error: Option<String>,
    /// Live quota state vs this account's switch threshold — for an honest
    /// near-limit/exhausted label that never masquerades as an error.
    pub quota_state: QuotaState,
    /// Why this account is out of rotation right now (the latest-clearing hard
    /// gate), computed live via [`crate::manager::Manager::account_gate`] for the
    /// general (non-Fable) view. [`GateReason::Ok`] when in rotation.
    pub gate: GateReason,
    /// When [`Self::gate`] clears — the instant ALL active hard gates have lifted.
    /// `None` when in rotation (`Ok`), when the gate never self-frees
    /// (`Login`/`Disabled`), or when a gating window has no known reset (unknown —
    /// we cannot promise a time).
    pub free_at: Option<OffsetDateTime>,
    /// Count of stream failures this account's streams carried within the decay
    /// window (see `STREAM_ERROR_WINDOW_MS` in `manager/mod.rs`): an in-band SSE
    /// `error` event, or a stream that hit EOF without Anthropic's `message_stop`
    /// terminator — recorded with the fixed kind `"truncated"`, which covers a
    /// mid-stream transport death and the malformed/utf8 break alike. An `error`
    /// event takes precedence: EOF without `message_stop` is only counted as
    /// `"truncated"` when no more specific `error` was already recorded. Before
    /// that check existed, a truncated 200 with no in-band `error` event was
    /// booked as a clean serve and never reached this field at all.
    /// OBSERVABILITY ONLY: nothing in `select.rs` reads it, so it never gates
    /// rotation. On the wire deliberately (see `status.rs`'s module doc): it
    /// carries no credential material, and `tcr status --json` is how an
    /// operator sees the fleet — `tcr status --json | jq '.[].streamErrorCount'`,
    /// rendered by `cli::render_accounts_json`, with the plain-text `tcr status`
    /// carrying the same number as a `stream_errors=` token.
    ///
    /// That last sentence was FALSE when this field was introduced: the renderers
    /// did not exist, so the value reached the wire and stopped there. Both were
    /// added afterwards, which is what makes the claim true now. It is load-bearing
    /// — if a future change drops either renderer, delete the claim with it rather
    /// than leaving a rationale the code no longer earns.
    pub stream_error_count: usize,
    /// The most recent stream failure's kind: an Anthropic `error.type` (e.g.
    /// `"overloaded_error"`) from an in-band SSE `error` event, or the fixed
    /// string `"truncated"` when the stream hit EOF without `message_stop` and
    /// no more specific `error` was already recorded. Same on-the-wire decision
    /// as `stream_error_count`, and surfaced by the same two renderers
    /// (`lastStreamError` in JSON, the `last_stream_error=` token in text).
    pub last_stream_error: Option<String>,
    /// Group labels for this account, mirroring `AccountRuntime::groups` —
    /// empty (never `None`) when the config carried no `groups` key.
    pub groups: Vec<String>,
    /// The subset of [`Self::groups`] currently marked `reserved`
    /// (`tcr group reserve`), sorted. Always an array, `[]` when none — a
    /// config fact, never `null`. See [`GateReason::Reserved`].
    pub reserved_groups: Vec<String>,
    /// The subset of [`Self::groups`] that have opted in to letting an explicit
    /// `--group` ask select the control account (`tcr group allow-control`),
    /// sorted. Always an array, `[]` when none — a config fact, never `null`,
    /// same contract as [`Self::reserved_groups`] beside it.
    pub control_allowed_groups: Vec<String>,
    /// Proxy-computed usage and cost for this account: today, the current 5-hour
    /// window, the trailing hour, and today split by model.
    ///
    /// `None` means NOT MEASURED, never "served nothing". It is `None` on any
    /// snapshot built outside the serving process — a fresh offline `Manager`
    /// has no traffic to aggregate and no ledger attached — and on a row
    /// reconstructed from a server whose build predates the field. The same
    /// honest-null discipline `cacheHitRatio` follows: an unmeasured zero read
    /// as a measurement is how a real prompt-cache catastrophe went unseen.
    pub usage: Option<tcr_status_wire::UsageRow>,
}

/// Whether a live session was keyed on a stable client identity (x-api-key /
/// metadata.user_id), on a hash of its cacheable prefix (`system` + `tools`)
/// because it had no stronger identity, or had neither and served unpinned.
/// DISPLAY-only — never a routing input.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SessionKind {
    Stable,
    /// Pinned via `proxy::prefix_session_key` (private, see its doc-comment): no `x-api-key` /
    /// `metadata.user_id`, but the request had a `system` and/or `tools` field
    /// whose hash pins it (and byte-identical requests after it) to one
    /// account. Weaker than `Stable` — it identifies the REQUEST's cacheable
    /// prefix, not the client — but still a real pin, distinct from `Fallback`.
    Prefix,
    Fallback,
}

/// One live session's view: a short display id, the account it is PINNED to, the
/// account that actually served its most recent request, how many requests it has
/// served, and when it was last seen.
///
/// The two account fields are deliberately separate because they genuinely differ:
/// a session's pin is HELD while a single request is diverted elsewhere (a Fable
/// title call whose model-scoped weekly is spent, one request during a hold that
/// clears while the cache is still warm — see
/// [`crate::manager::Manager::select`]). Collapsing them made every such divert
/// look like the session had moved account, which is how a fleet measured at a
/// 1.70% switch rate read as "sessions keep jumping" in the TUI.
#[derive(Debug, Clone)]
pub struct SessionSnapshot {
    pub id: String, // short hex derived from the u64 session key (display only)
    /// Account name this session is currently PINNED to, read from the manager's
    /// affinity map — the sole authority on the pin. A session with no pin (an
    /// identity-less fallback serve, a seeded demo row) has no home, so this falls
    /// back to [`Self::last_served_account`].
    pub account: String,
    /// Account name that served this session's most recent request. Equal to
    /// [`Self::account`] on a normal serve; DIFFERENT exactly when that one request
    /// was diverted while the pin stayed put — so the divert stays observable
    /// instead of being hidden.
    pub last_served_account: String,
    pub requests: u64,
    pub last_seen: Option<OffsetDateTime>,
    /// [`SessionKind::Stable`] when keyed on a stable client identity (x-api-key /
    /// `metadata.user_id`); [`SessionKind::Prefix`] when there was none but the
    /// request's `system`/`tools` hash pinned it anyway; [`SessionKind::Fallback`]
    /// when there was neither and the request served unpinned. Display provenance
    /// only. `Prefix` sessions have a real pin and group under their account in
    /// the TUI like `Stable` ones do; `Fallback` sessions would fold into one dim
    /// aggregate row, but as of this writing no `SessionSnapshot` ever actually
    /// carries `Fallback` — see `TreeRow::Unpinned`'s doc-comment in `tui.rs` for
    /// why. Routing is unchanged either way.
    pub kind: SessionKind,
}

/// The whole picture the TUI paints each tick.
#[derive(Debug, Clone)]
pub struct StatsSnapshot {
    pub accounts: Vec<AccountSnapshot>,
    /// Index of the account that most recently served a request, if any.
    pub current: Option<usize>,
    /// Most-recent-first request log.
    pub recent: Vec<RequestLogEntry>,
    /// Live per-session serving stats in a STABLE order (pinned account, then
    /// session id) — deliberately NOT recency, so a row holds its place instead of
    /// jumping to the top of the pane on every request.
    pub sessions: Vec<SessionSnapshot>,
}
