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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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
}

/// Whether a live session was keyed on a stable client identity (x-api-key /
/// metadata.user_id) or fell back to a per-connection key. DISPLAY-only —
/// never a routing input.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SessionKind {
    Stable,
    Fallback,
}

/// One live session's view: a short display id, the account it's pinned to,
/// how many requests it has served, and when it was last seen.
#[derive(Debug, Clone)]
pub struct SessionSnapshot {
    pub id: String,      // short hex derived from the u64 session key (display only)
    pub account: String, // account name it is currently pinned to
    pub requests: u64,
    pub last_seen: Option<OffsetDateTime>,
    /// [`SessionKind::Stable`] when keyed on a stable client identity (x-api-key /
    /// `metadata.user_id`); [`SessionKind::Fallback`] for a per-connection key.
    /// Display provenance only — the TUI folds all fallback sessions into one dim
    /// aggregate row; routing is unchanged.
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
    /// Live per-session serving stats, most-recent-first.
    pub sessions: Vec<SessionSnapshot>,
}
