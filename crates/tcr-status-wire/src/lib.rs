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

/// One tool call still awaiting its `tool_result`, on the wire.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunningToolRow {
    pub tool: String,
    pub started_ms: i64,
    /// First 120 characters of a Bash tool's `input.command` — `None` for any other tool, or
    /// when the running call carries no command. Held in memory only on the server; never
    /// written to a log (see `src/session_wire.rs`'s module doc in the main crate).
    pub command_head: Option<String>,
}

/// One completed tool call, for a session's "ten slowest" list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlowToolRow {
    pub tool: String,
    pub seconds: f64,
    pub command_head: Option<String>,
    pub ended_ms: i64,
}

/// A session's tool-call aggregates.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionToolsRow {
    #[serde(default)]
    pub calls: u64,
    #[serde(default)]
    pub errors: u64,
    #[serde(default)]
    pub timeouts: u64,
    /// Tool calls still awaiting a `tool_result`, capped at 64 per session.
    #[serde(default)]
    pub running: Vec<RunningToolRow>,
    /// The ten slowest completed tool calls, descending by `seconds`.
    #[serde(default)]
    pub slowest: Vec<SlowToolRow>,
}

/// One live session on the `tcr status --json` wire's `sessions` array (F1,
/// `docs/design/panel-tabs.md`). One entry per session the proxy has seen in the last hour —
/// see `src/session_wire.rs` in the main crate for how it is built and bounded.
///
/// `#[serde(default)]` on every field but `sessionId` so a payload from a server built before
/// this row existed simply omits `sessions` entirely (the array field itself is
/// `#[serde(default)]` on the enclosing status payload, in `status.rs`) — never a hard parse
/// failure that drops a client back to a fabricated all-zeros offline snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRow {
    pub session_id: String,
    /// The account this session's most recent request served against, or `None` when it has
    /// never been attributed to one (e.g. the request carried no stable identity).
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    pub first_seen_ms: i64,
    pub last_seen_ms: i64,
    #[serde(default)]
    pub requests: u64,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub tools: SessionToolsRow,
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
    /// `"disabled"`, `"reserved"`, or `"parked"`.
    pub gate: String,
    pub five_hour: Option<f64>,
    pub seven_day: Option<f64>,
    pub seven_day_oi: Option<f64>,
    pub five_hour_state: Option<String>,
    pub seven_day_state: Option<String>,
    /// Per-window state for the Fable weekly (`seven_day_oi`), mirroring
    /// [`Self::seven_day_state`] and gating Fable requests only — see
    /// `GateReason::FableWeekly`. `null` when that window has no reading yet,
    /// same "not measured" idiom as the fields beside it.
    pub seven_day_oi_state: Option<String>,
    pub five_hour_reset_at_ms: Option<i64>,
    pub seven_day_reset_at_ms: Option<i64>,
    /// The Fable weekly window's reset, mirroring [`Self::seven_day_reset_at_ms`]:
    /// unconditional on threshold, `null` when the window's reset has already
    /// elapsed with nothing learned since or was never learned at all.
    pub seven_day_oi_reset_at_ms: Option<i64>,
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
    /// The subset of [`Self::groups`] marked `parked`
    /// (`groupSettings.<g>.parked`) — a non-empty list means this account is
    /// held out of rotation entirely, the same way `disabled` does. Rides
    /// beside `reserved_groups` and for the same reason: the panel names the
    /// group that parked a row, which it cannot do from membership alone.
    /// `#[serde(default)]` so a row from a server predating the field decodes
    /// as "nothing parked" rather than failing.
    #[serde(default)]
    pub parked_groups: Vec<String>,
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
    /// The organization's plan as the profile endpoint reported it, VERBATIM
    /// (`claude_max`, `claude_team`, `claude_pro`, `claude_enterprise`) — for a
    /// script that wants the provider's own word rather than our label. `None`
    /// when this account has never been profiled.
    #[serde(default)]
    pub organization_type: Option<String>,
    /// The organization's rate-limit tier, verbatim
    /// (`default_claude_max_20x`, `default_raven`) — the field that separates
    /// Max 20x from Max 5x, on the wire for the same script-facing reason as
    /// [`Self::organization_type`].
    #[serde(default)]
    pub rate_limit_tier: Option<String>,
    /// The organization's seat tier, verbatim (`organization.seat_tier`) —
    /// `team_standard` and `team_tier_1` both observed on one live Team org.
    /// `None` on Max and Pro rows, which have no seats: the field is a property
    /// of a seated org, not of every account.
    #[serde(default)]
    pub seat_tier: Option<String>,
    /// The customer-facing plan label ("Max 20x", "Team Premium", ...) derived
    /// from the three raw fields above by [`plan_label`] — the ONE place that mapping
    /// exists, so the CLI's plain text, the TUI and the macOS panel cannot
    /// disagree about what a row's plan is called.
    ///
    /// `None` when nothing is known, and that is load-bearing: a fabricated
    /// label is worse than a blank, because the whole point of this field is
    /// telling two rows with the SAME NAME apart.
    #[serde(default)]
    pub plan: Option<String>,
    /// The org this account is scoped to. A FACT about the row, for a client to
    /// show — not an address: `name` is unique and is the whole address every
    /// `tcr` verb takes.
    ///
    /// It used to be how a client narrowed a duplicated email, back when two
    /// rows could share a name and the panel's "Copy Access Token" failed with
    /// "matches 2 accounts". Naming the org is still worth doing on a row whose
    /// name does not already carry it.
    #[serde(default)]
    pub org_uuid: Option<String>,
    #[serde(default)]
    pub org_name: Option<String>,
}

/// The customer-facing plan label for one account, derived from the three raw
/// profile fields — `organization.organization_type`,
/// `organization.rate_limit_tier` and `organization.seat_tier`.
///
/// The single place this mapping exists. It lives in the wire crate rather than
/// the binary because both consumers need it: the server fills the `plan` field
/// from here, and so does the CLI's OFFLINE path, which has no server to ask.
///
/// `organization_type` is the PLAN WORD — Max, Pro, Team, Enterprise. The other
/// two supply at most one SUFFIX after it, and the rate-limit multiplier wins
/// when both could:
///
/// | plan word | suffix from | example |
/// |---|---|---|
/// | any | `rate_limit_tier` ending `_20x` / `_5x` | `Max 20x`, `Team 5x` |
/// | Team / Enterprise, no multiplier | `seat_tier` | `Team Standard`, `Team Tier 2` |
/// | Max / Pro, no multiplier | nothing | `Max`, `Pro` |
///
/// The multiplier outranks the seat because it is the stronger statement about
/// what the account can actually do. Measured on a live fleet: a premium Team
/// seat reads `seat_tier=team_tier_1` with `rate_limit_tier=default_claude_max_5x`,
/// while a standard one reads `seat_tier=team_standard` with
/// `rate_limit_tier=default_raven`. Labelling the first by its seat would print
/// "Team Tier 1", which tells a reader nothing; "Team 5x" tells them the size of
/// the account they are about to route traffic to.
///
/// `account.has_claude_max` is deliberately not an input — measured on the same
/// fleet it reads `true` on Team rows too, so a label derived from it would call
/// a Team account "Max".
///
/// Anything this function does not recognize survives VERBATIM rather than being
/// bucketed into the nearest known value: an unknown `organization_type` is
/// returned as-is with no suffix, and an unknown seat is appended raw (`Team
/// team_trial`) rather than dropped. A new plan or seat name is a thing we have
/// not seen yet; showing it unchanged is honest, mapping it onto "Max" or
/// silently discarding it is a claim nobody made. `None` in gives `None` out for
/// the same reason: an unprofiled account's plan is unknown, and "unknown" is
/// not "Pro".
pub fn plan_label(
    organization_type: Option<&str>,
    rate_limit_tier: Option<&str>,
    seat_tier: Option<&str>,
) -> Option<String> {
    /// The rate-limit multiplier a tier names, if it names one.
    fn multiplier(rate_limit_tier: Option<&str>) -> Option<&'static str> {
        let tier = rate_limit_tier?;
        if tier.ends_with("_20x") {
            Some("20x")
        } else if tier.ends_with("_5x") {
            Some("5x")
        } else {
            // A tier that names no multiplier (`default_raven`) is not a size
            // we failed to read — it is a tier from a different vocabulary, and
            // guessing a multiplier off it would be inventing a number.
            None
        }
    }

    /// What kind of seat this is, in words. `team_tier_1` → `Tier 1`; an
    /// unrecognized seat rides along verbatim rather than being dropped, since
    /// it is a real difference between two otherwise identical rows.
    fn seat_words(seat_tier: &str) -> String {
        if seat_tier.ends_with("_standard") {
            return "Standard".to_string();
        }
        match seat_tier.split("_tier_").nth(1) {
            Some(n) if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) => {
                format!("Tier {n}")
            }
            _ => seat_tier.to_string(),
        }
    }

    let org_type = organization_type?;
    let (plan, seated) = match org_type {
        "claude_max" => ("Max", false),
        "claude_pro" => ("Pro", false),
        "claude_team" => ("Team", true),
        "claude_enterprise" => ("Enterprise", true),
        // An unknown plan gets no suffix at all: we cannot know whether it is
        // seated, so neither refinement is safe to attach to it.
        other => return Some(other.to_string()),
    };

    Some(match multiplier(rate_limit_tier) {
        Some(mult) => format!("{plan} {mult}"),
        None => match seat_tier.filter(|_| seated) {
            Some(seat) => format!("{plan} {}", seat_words(seat)),
            None => plan.to_string(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three labels the live fleet actually produces, from the exact field
    /// triples measured on it — not synthetic combinations.
    #[test]
    fn plan_label_names_the_three_rows_the_live_fleet_produces() {
        assert_eq!(
            plan_label(Some("claude_max"), Some("default_claude_max_20x"), None),
            Some("Max 20x".to_string())
        );
        assert_eq!(
            plan_label(
                Some("claude_team"),
                Some("default_claude_max_5x"),
                Some("team_tier_1")
            ),
            Some("Team 5x".to_string()),
            "a premium Team seat carries a 5x multiplier, and the multiplier is \
             the useful half: 'Team Tier 1' names a seat nobody can size"
        );
        assert_eq!(
            plan_label(
                Some("claude_team"),
                Some("default_raven"),
                Some("team_standard")
            ),
            Some("Team Standard".to_string()),
            "a standard seat's tier names no multiplier, so the seat is what is \
             left to say"
        );
    }

    #[test]
    fn plan_label_falls_back_to_the_seat_only_when_no_multiplier_is_named() {
        assert_eq!(
            plan_label(Some("claude_team"), None, Some("team_tier_2")),
            Some("Team Tier 2".to_string()),
            "a seat tier with no multiplier beside it is spelled out"
        );
        assert_eq!(
            plan_label(Some("claude_team"), Some("default_raven"), None),
            Some("Team".to_string()),
            "no multiplier and no seat means no suffix — never a guessed Standard"
        );
        assert_eq!(
            plan_label(Some("claude_enterprise"), None, Some("enterprise_standard")),
            Some("Enterprise Standard".to_string())
        );
        assert_eq!(
            plan_label(Some("claude_enterprise"), Some("something_20x"), None),
            Some("Enterprise 20x".to_string())
        );
    }

    #[test]
    fn plan_label_ignores_a_seat_on_an_unseated_plan() {
        assert_eq!(
            plan_label(Some("claude_max"), None, Some("team_standard")),
            Some("Max".to_string()),
            "Max has no seats, so a stray seat value must not become part of its name"
        );
        assert_eq!(
            plan_label(Some("claude_pro"), None, Some("team_tier_1")),
            Some("Pro".to_string())
        );
        assert_eq!(
            plan_label(Some("claude_max"), Some("default_claude_max_5x"), None),
            Some("Max 5x".to_string())
        );
        assert_eq!(
            plan_label(Some("claude_max"), Some("something_else"), None),
            Some("Max".to_string()),
            "a tier naming no multiplier leaves the size off rather than guessing one"
        );
    }

    #[test]
    fn plan_label_returns_an_unknown_organization_type_verbatim() {
        assert_eq!(
            plan_label(
                Some("claude_something_new"),
                Some("default_claude_max_20x"),
                Some("team_standard")
            ),
            Some("claude_something_new".to_string()),
            "an unrecognized plan must survive verbatim, never be bucketed into a \
             known one — and it takes no suffix, because we cannot know whether it \
             is seated or what its tier vocabulary means"
        );
    }

    #[test]
    fn plan_label_returns_an_unknown_seat_tier_verbatim() {
        assert_eq!(
            plan_label(Some("claude_team"), None, Some("team_trial")),
            Some("Team team_trial".to_string()),
            "an unrecognized seat is a real difference between two rows — it rides \
             along verbatim rather than being dropped or read as standard"
        );
        assert_eq!(
            plan_label(Some("claude_team"), None, Some("team_tier_x")),
            Some("Team team_tier_x".to_string()),
            "a tier suffix that is not a number is not a tier number"
        );
    }

    #[test]
    fn plan_label_is_none_when_the_plan_is_unknown() {
        assert_eq!(
            plan_label(None, None, None),
            None,
            "an unprofiled account has no label — never a fabricated default"
        );
        assert_eq!(
            plan_label(None, Some("default_claude_max_20x"), Some("team_standard")),
            None,
            "neither refinement ever names a plan on its own: non-Max orgs carry \
             tiers, and a seat without a plan is not a plan"
        );
    }
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
