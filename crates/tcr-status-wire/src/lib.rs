//! Serde types for the `tcr status --json` wire contract.
//!
//! This is the type that `render_accounts_json` (`src/cli.rs` in the main crate)
//! builds and serializes, and that the macOS app's `FleetStatus.swift` decodes.
//! Before this crate existed, the Rust side had no type at all for this shape —
//! only an ad-hoc `serde_json::json!` literal — so the two sides could drift
//! with nothing to catch it beyond the committed fixture
//! (`tests/fixtures/status-contract.json`) that both sides read.
//!
//! Serde-only: no `unsafe`, no macOS dependency. `cargo test --all` / `cargo
//! clippy --all-targets --locked` (`.github/workflows/ci.yml`) build this crate
//! on the ubuntu runner, so it must stay Linux-clean forever.
//!
//! # Row-at-a-time decode
//!
//! The wire is a bare JSON array, one object per account, and it must stay
//! decodable one element at a time: `FleetStatus.swift` decodes row-by-row so a
//! single malformed row (its doc-comment names an actual `"quota": null`
//! incident) cannot take down the whole fleet's view. [`AccountStatusRow`]
//! keeps that property expressible on the Rust side too — decode with
//! [`AccountStatusRow::from_value`] against one already-parsed `serde_json::Value`
//! at a time, never `serde_json::from_slice::<Vec<AccountStatusRow>>` against the
//! whole payload in one shot, which fails the entire batch on one bad row.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One bucket's token and cost totals. Every field is a plain sum over the
/// requests in the bucket, so a client can add two of these together.
///
/// `costUsd` is the API **list-price equivalent** — what this traffic would
/// have cost on the API — not a bill: the accounts behind this proxy are
/// subscriptions. It is `null`, never `0.0`, when the bucket served requests
/// and none of their models could be priced; `unpricedRequests` says how many
/// of `requests` are missing from the figure, so a partial total is never
/// mistaken for a complete one.
///
/// A bucket with NO requests reports `costUsd: 0.0`. Nothing served is a
/// measured zero, and `null` is reserved for the one case above — an idle
/// account used to report `today.costUsd: null` beside `lastHour.costUsd: 0`
/// for the very same absence of traffic.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageTotals {
    #[serde(default)]
    pub requests: u64,
    /// Base (non-cached) input tokens. Deliberately NOT the row-level
    /// `inputTokens`, which is the QUOTA counter and folds cache creation and
    /// cache reads into one number — those are separate billing dimensions and
    /// have to stay apart to be priced.
    #[serde(default)]
    pub input_tokens: u64,
    /// ALL cache-creation tokens, both TTLs — the same quantity the row-level
    /// [`AccountStatusRow::cache_creation_tokens`] carries, deliberately, so
    /// one key never means two things in one row. The 5-minute part is this
    /// minus [`Self::cache_creation_1h_tokens`].
    #[serde(default)]
    pub cache_creation_tokens: u64,
    /// The SUBSET of [`Self::cache_creation_tokens`] written under the extended
    /// 1-hour TTL, which bills at twice base input rather than 1.25x.
    #[serde(default)]
    pub cache_creation_1h_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    /// `null` when nothing in this bucket could be priced — see the struct docs.
    #[serde(default)]
    pub cost_usd: Option<f64>,
    #[serde(default)]
    pub unpriced_requests: u64,
}

/// [`UsageTotals`] for a bounded window, plus the instant the window opened.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageWindow {
    /// Unix milliseconds: the start of this account's current 5-hour window
    /// (`fiveHourResetAtMs - 5h`), as read from Anthropic's own headers.
    pub since: i64,
    #[serde(flatten)]
    pub totals: UsageTotals,
}

/// One account's usage, aggregated by the proxy at request time.
///
/// Absent (`null`) means "not measured": the row came from a server built
/// before this existed, or from the offline path, which has no serving
/// counters at all. It never means zero usage.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageRow {
    /// The local calendar day of the machine the SERVER runs on.
    #[serde(default)]
    pub today: UsageTotals,
    /// This account's current 5-hour window. `null` when the server has not
    /// learned the window's reset, so its start cannot be named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<UsageWindow>,
    /// The trailing 60 minutes — burn rate is `lastHour.costUsd` per hour, by
    /// definition.
    #[serde(default)]
    pub last_hour: UsageTotals,
    /// `today`, split by model id.
    #[serde(default)]
    pub today_by_model: BTreeMap<String, UsageTotals>,
}

/// A gating window (`5h` or `7d`) currently holding an account out of rotation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeldWindowRow {
    /// `"5h"` or `"7d"`.
    pub window: String,
    pub reset_at_ms: i64,
    pub minutes_until_reset: i64,
}

/// One account's row on the `tcr status --json` wire.
///
/// Field-for-field mirror of what `render_accounts_json` emits today — see that
/// function's doc-comments in `src/cli.rs` for why each field is shaped the way
/// it is (in particular, why several are `Option` rather than a fabricated `0`
/// or `false` on the offline path). This type does not change the contract; it
/// gives the existing contract a name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountStatusRow {
    /// `"live"` or `"offline"` — which process's numbers this row carries.
    pub source: String,
    /// Short git sha of the serving build, `None` when there is no serving
    /// process to report one (offline path).
    pub server_sha: Option<String>,
    pub server_dirty: Option<bool>,
    pub http1_only: bool,
    pub name: String,
    pub priority: i64,
    pub status: String,
    pub disabled: bool,
    pub control: bool,
    pub quota: Option<f64>,
    /// `"ok"`, `"near"`, or `"spent"` — see `quota_state_token`.
    pub quota_state: String,
    /// Kebab-case `GateReason` token: `"ok"`, `"hold"`, `"five-hour"`,
    /// `"seven-day"`, `"fable-weekly"`, `"standard"`, `"login"`, `"rejected"`,
    /// `"disabled"`, or `"reserved"`.
    pub gate: String,
    pub five_hour: Option<f64>,
    pub seven_day: Option<f64>,
    pub seven_day_oi: Option<f64>,
    pub five_hour_state: Option<String>,
    pub seven_day_state: Option<String>,
    pub five_hour_reset_at_ms: Option<i64>,
    pub seven_day_reset_at_ms: Option<i64>,
    /// `None` on the offline path — a structural "not measured", never `0`.
    pub requests: Option<u64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_hit_ratio: Option<f64>,
    pub last_probe_ms: Option<i64>,
    pub probe_status: String,
    pub probe_error: Option<String>,
    pub stream_error_count: Option<u64>,
    pub last_stream_error: Option<String>,
    pub held: Vec<HeldWindowRow>,
    pub free_at_ms: Option<i64>,
    pub seconds_until_free: Option<i64>,
    pub rate_limited_until_ms: Option<i64>,
    pub groups: Vec<String>,
    pub reserved_groups: Vec<String>,
    /// The subset of [`Self::groups`] that have opted in to letting an explicit
    /// `--group` ask select the control account (`groupSettings.<g>.allowControlAccount`).
    /// Rides beside `reserved_groups` and for the same reason: the panel decides
    /// whether a group can route at all, and it cannot answer that from
    /// membership alone once the opt-in exists.
    #[serde(default)]
    pub control_allowed_groups: Vec<String>,
    /// Every group on the fleet mapped to its resolved color, repeated per row.
    pub group_colors: BTreeMap<String, String>,
    /// Cache-creation input tokens (a subset of `inputTokens`, like
    /// `cacheReadTokens` beside it). Tracked since cache accounting landed and
    /// never emitted here until now, which left `cacheReadTokens` on the wire
    /// with no companion to say how much of the input was spent WRITING the
    /// cache. `None` on the offline path, same "not measured" idiom as the
    /// other serving counters.
    #[serde(default)]
    pub cache_creation_tokens: Option<u64>,
    /// Proxy-computed usage and cost for this account. `None` when the serving
    /// build predates it or the row came from the offline path — see
    /// [`UsageRow`]'s doc-comment: absent is "not measured", never zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageRow>,
}

impl AccountStatusRow {
    /// Decode one already-parsed row. This is the row-at-a-time entry point —
    /// call it per-element over the array (`serde_json::Value::Array` iterated
    /// one item at a time, or one line of a JSONL rendering), never
    /// `serde_json::from_slice::<Vec<AccountStatusRow>>` against the whole
    /// payload, which lets one malformed row fail every row alongside it.
    pub fn from_value(value: serde_json::Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(value)
    }
}
