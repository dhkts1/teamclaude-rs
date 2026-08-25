//! The wire contract between the running proxy's read-only status endpoint
//! ([`crate::proxy::STATUS_PATH`]) and `tcr status`.
//!
//! # Why it exists
//!
//! `tcr status` used to build a FRESH offline [`Manager`](crate::manager::Manager)
//! and print its counters. Those counters live in the *server's* process, so the
//! offline ones are structurally zero — which made `cacheHitRatio` report a
//! confident `0.0` for every account forever. A metric that cannot fire is worse
//! than no metric: it reported "cache fine" straight through a real prompt-cache
//! catastrophe. This module is the seam that lets the CLI read the *live*
//! process's numbers instead.
//!
//! # The no-secret invariant
//!
//! The proxy holds every account's OAuth access **and** refresh token. This
//! payload is an explicit projection of [`AccountSnapshot`] — the display struct
//! the TUI paints — which by construction carries no credential material: no
//! access token, no refresh token, no proxy api-key, no `Authorization` echo.
//! Two things keep it that way:
//!
//! 1. [`StatusPayload::into_snapshot`] rebuilds `AccountSnapshot` with a struct
//!    literal that names **every** field, so adding a field to `AccountSnapshot`
//!    is a compile error here — a mechanical prompt to decide, deliberately,
//!    whether it may cross a process boundary.
//! 2. `status_endpoint_leaks_no_secrets` (in `proxy.rs`) asserts on the response
//!    **bytes**, not on a struct, so a leak introduced through any path fails a
//!    test rather than a code review.
//!
//! Deliberately NOT on the wire: the recent-request ring buffer (it carries the
//! request paths a client sent) and the live session table. `tcr status` prints
//! neither, so neither is exposed — the endpoint ships the smallest thing that
//! renders the fleet view.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::build_info::BuildInfo;
use crate::probe::ProbeStatus;
use crate::stats::{AccountSnapshot, GateReason, QuotaState, StatsSnapshot};

/// Discriminator carried by every status response, checked by the client before
/// it trusts a body.
///
/// It exists because of version skew: a tcr server built *before* this endpoint
/// existed has no `/_tcr/status` route, so the request falls through to its
/// catch-all and is forwarded to Anthropic — whose error JSON would otherwise be
/// a plausible-looking body. Requiring an exact `kind` match means only a payload
/// this code produced is ever rendered as live status; anything else falls back
/// to the offline snapshot with a visible warning. Bump the suffix if the shape
/// ever changes incompatibly.
pub const STATUS_KIND: &str = "tcr.status.v1";

/// A live fleet view as served by the proxy: one row per configured account, in
/// the server's account order, plus the build the server is running.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusPayload {
    /// Always [`STATUS_KIND`]. See its docs for why the client demands it.
    pub kind: String,
    pub accounts: Vec<AccountStatus>,
    /// Which commit the SERVING process was built from — the one fact no client
    /// can derive, since the server may have been running for days while the
    /// checkout moved on ([`crate::build_info`]).
    ///
    /// # Why this did NOT bump [`STATUS_KIND`]
    ///
    /// The client demands an EXACT `kind` match and falls back to the offline
    /// snapshot — whose serving counters are structurally zero — on any
    /// mismatch. So a bump is not a neutral version marker here: it would make
    /// every not-yet-rebuilt client reject a new server's payload and silently
    /// render zeros, which is precisely the false-zero failure this endpoint was
    /// built to end. A bump has to be reserved for a change that would otherwise
    /// be MISREAD, and this one cannot be, in either direction:
    ///
    /// * OLD client ← NEW server: serde ignores unknown fields by default, so
    ///   the extra `build` object is skipped and every field the old client
    ///   reads is unchanged. (`status_payload_tolerates_an_unknown_field` pins
    ///   that behaviour, since a later `deny_unknown_fields` would break it.)
    /// * NEW client ← OLD server: `#[serde(default)]` fills in
    ///   [`BuildInfo::default`] — every field `unknown` — and the client renders
    ///   "cannot tell whether the server is current", which is the truth.
    ///
    /// The rule this encodes: `kind` gates a change that would be MISREAD in
    /// either skew direction — never one both directions degrade through
    /// honestly. The test is behavioural, not a taxonomy of edits:
    ///
    /// * Would an OLD client reading a NEW payload render something FALSE (as
    ///   opposed to something absent)? Unknown fields are skipped by default, so
    ///   an addition passes; a field RETYPED or given new meaning under the same
    ///   name does not, because the old client parses it and believes it.
    /// * Would a NEW client reading an OLD payload render something FALSE? With
    ///   `#[serde(default)]` plus `skip_serializing_if` the field simply reads as
    ///   its default, which is the truth "the server did not report this".
    ///   WITHOUT a default it is a hard deserialize error and the client falls
    ///   back to the all-zeros offline snapshot — a fabricated healthy fleet.
    ///
    /// A field REMOVAL is therefore not automatically a bump, and an earlier
    /// wording of this rule said it was. `free_at_floor_ms` was removed without
    /// one, correctly: it was `Option` with `default` + `skip_serializing_if` on
    /// both sides and there is no `deny_unknown_fields` on this payload, so an
    /// old client sees an absent optional and a new client sees `None` — both
    /// true. Bumping there would have made every not-yet-rebuilt client reject
    /// the payload and render the structural zeros this endpoint exists to end.
    ///
    /// The real hazard is the opposite one, and it has bitten: adding a REQUIRED
    /// field. See [`AccountRow::stream_error_count`] — added without a default,
    /// it made a newer client unable to parse an older running server at all.
    #[serde(default)]
    pub build: BuildInfo,
    /// Whether the SERVING process is forcing HTTP/1.1 on its upstream
    /// clients — see [`crate::config::Config::http1_only`]. Server-wide, like
    /// `build`, and for the same reason: re-deriving it from
    /// `~/.config/teamclaude.json` on the client would report the file's
    /// state, not the already-booted process's, and the two can differ (the
    /// config was edited after boot; the flag is only read at client
    /// construction).
    ///
    /// `#[serde(default)]` for the same back-compat reason as `build`: an
    /// OLD server's payload has no such key, and absent must read as `false`
    /// (the actual default) rather than fail the parse and drop to the
    /// offline snapshot.
    #[serde(default)]
    pub http1_only: bool,
    /// The identity-bound control account's NAME (`Config::control_account`,
    /// resolved by `Manager::control_name`), or `None` when unset. Server-wide
    /// like `http1_only`, and for the same `#[serde(default,
    /// skip_serializing_if)]` reason: an OLD server's payload has no such key,
    /// and absent must read as "no control account reported" rather than fail
    /// the parse and drop to the offline snapshot. A NEW client reading an OLD
    /// server gets `None` — true, the server genuinely never reported one — and
    /// an OLD client reading a NEW server simply never looks at the extra key.
    /// Neither direction is MISREAD, so — per this struct's `build`
    /// doc-comment on when a bump is and is not warranted — this must NOT bump
    /// [`STATUS_KIND`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control: Option<String>,
    /// Every group on the fleet mapped to its resolved color
    /// (`crate::config::Config::group_colors`), server-wide like `http1_only`
    /// and for the same reason: it is resolved from the config the SERVING
    /// process booted with (`Manager::group_colors`), and a client must not
    /// re-derive it from a config file that may have moved on since boot.
    ///
    /// `#[serde(default)]` for the same forward-compat reason as `http1_only`
    /// — an OLD server's payload has no such key, and absent must read as an
    /// empty map (a NEW client talking to an old server reports no group
    /// colors, which is the truth: that server never resolved any) rather
    /// than fail the parse and drop to the offline snapshot. Per this
    /// struct's `build` doc-comment on when a bump is and is not warranted,
    /// this must NOT bump [`STATUS_KIND`] — same reasoning as `control`.
    #[serde(default)]
    pub group_colors: std::collections::BTreeMap<String, String>,
}

/// One account's live row. Field-for-field the serializable half of
/// [`AccountSnapshot`], plus the server's own `threshold` for that account.
///
/// The threshold rides along deliberately: it decides which windows read as
/// "held", and re-deriving it on the client from `~/.config/teamclaude.json`
/// would silently use a file that may have been edited since the server booted —
/// and, worse, would zip a client-ordered threshold list against a
/// server-ordered account list. The server is the authority on its own state.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountStatus {
    pub name: String,
    pub priority: i64,
    pub status: String,
    pub disabled: bool,
    pub five_hour: Option<f64>,
    /// Timestamps cross the wire as Unix milliseconds — the same unit the config's
    /// `expiresAt` and every internal deadline already use.
    pub five_hour_reset_ms: Option<i64>,
    pub seven_day: Option<f64>,
    pub seven_day_reset_ms: Option<i64>,
    pub seven_day_oi: Option<f64>,
    /// The Fable weekly window's reset, mirroring [`Self::seven_day_reset_ms`].
    /// `#[serde(default, skip_serializing_if = "Option::is_none")]` for the same
    /// forward/back-compat reason as `usage` below: an OLDER server's payload
    /// has no such key (reads as `None`, the truth — "this server does not
    /// report it"), and an OLDER client simply never looks at the extra key on
    /// a NEWER server's payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seven_day_oi_reset_ms: Option<i64>,
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub last_used_ms: Option<i64>,
    pub rate_limited_until_ms: Option<i64>,
    pub probe_status: ProbeStatus,
    pub last_probe_ms: Option<i64>,
    pub probe_error: Option<String>,
    pub quota_state: QuotaState,
    pub gate: GateReason,
    pub free_at_ms: Option<i64>,
    /// The account's effective switch threshold on the SERVER (its own
    /// `switchThreshold`, else the global one).
    pub threshold: f64,
    /// Decayed count of stream failures — an in-band SSE `error` event, or a
    /// stream that hit EOF without Anthropic's `message_stop` terminator
    /// (recorded as `"truncated"`); see [`AccountSnapshot::stream_error_count`].
    /// Put ON the wire deliberately —
    /// it carries no credential material and `tcr status --json` is how an
    /// operator sees the fleet (`tcr status --json | jq '.[].streamErrorCount'`);
    /// see this module's doc comment on the no-secret invariant.
    ///
    /// `#[serde(default)]` is load-bearing, not tidiness. This field was ADDED,
    /// and a newer client routinely talks to an older still-running server: the
    /// binary on disk is rebuilt on merge while the live process keeps serving
    /// until someone restarts it, which is the normal state of this system, not
    /// an edge case. Without a default, that skew is a hard deserialize failure
    /// ("missing field `streamErrorCount`") and `tcr status` falls back to an
    /// OFFLINE snapshot whose serving counters are all structural zeros — the
    /// operator is shown a fabricated healthy fleet at exactly the moment the
    /// real one is on fire. Observed live 2026-08-04: client bd60839 against
    /// server 325df03 reported all 13 accounts `active` while the log carried
    /// 52 rate-limit events and 8 accounts sat on hour-long holds.
    ///
    /// Absent → 0, which reads identically to "no stream errors seen". That is
    /// the honest degradation: `source` on the same payload already tells the
    /// operator the reading came from a server that could not report it.
    #[serde(default)]
    pub stream_error_count: usize,
    /// The most recent stream error's type, alongside the count above. Same
    /// on-the-wire decision as `stream_error_count`; rendered as `lastStreamError`.
    pub last_stream_error: Option<String>,
    /// Group labels for this account, mirroring [`AccountSnapshot::groups`].
    /// `#[serde(default)]` for the same forward-compat reason as
    /// `stream_error_count`: an older server that predates groups omits the
    /// field, and a newer client must still deserialize its payload rather than
    /// falling back to a fabricated offline snapshot.
    #[serde(default)]
    pub groups: Vec<String>,
    /// The reserved subset of [`Self::groups`], mirroring
    /// [`AccountSnapshot::reserved_groups`]. `#[serde(default)]` for the same
    /// forward-compat reason as `groups` — an older server predates
    /// reservation entirely.
    #[serde(default)]
    pub reserved_groups: Vec<String>,
    /// The opted-in subset of [`Self::groups`] — see
    /// [`AccountSnapshot::control_allowed_groups`]. `#[serde(default)]` for the
    /// same forward-compat reason as the two fields above: a server that
    /// predates the opt-in omits it entirely.
    #[serde(default)]
    pub control_allowed_groups: Vec<String>,
    /// Proxy-computed usage and cost — see [`AccountSnapshot::usage`].
    ///
    /// Both skew directions degrade HONESTLY, which is exactly what
    /// [`StatusPayload::build`]'s doc-comment reserves a [`STATUS_KIND`] bump
    /// for NOT doing — so this must not bump it:
    ///
    /// * OLD client ← NEW server: serde skips the unknown key; every field the
    ///   old client reads is untouched.
    /// * NEW client ← OLD server: the key is absent and reads as `None`, and
    ///   the client renders "this server does not report usage" — the truth.
    ///
    /// **The `Option` is what carries that second direction, not the
    /// `#[serde(default)]`.** serde's `missing_field` hands the field a
    /// `MissingFieldDeserializer` whose `deserialize_option` returns
    /// `visit_none` (serde 1.0.228, `src/private/de.rs:45-50`), so an absent
    /// `Option<T>` is `None` with or without the attribute. Verified by
    /// mutation: removing `default` here leaves every test green.
    ///
    /// That is NOT true of a non-`Option` field, which is where the real hazard
    /// lives — see [`Self::stream_error_count`], a bare `usize` whose `default`
    /// genuinely is load-bearing and whose absence took down a whole fleet view
    /// on 2026-08-04. `default` is kept here for consistency with the optional
    /// fields beside it; `skip_serializing_if` is doing real work, keeping the
    /// key off the wire entirely rather than emitting `"usage": null`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<tcr_status_wire::UsageRow>,
}

/// Unix milliseconds for an instant, matching [`crate::now_ms`]'s unit.
fn to_ms(t: OffsetDateTime) -> i64 {
    (t.unix_timestamp_nanos() / 1_000_000) as i64
}

/// Inverse of [`to_ms`]. `None` on an out-of-range value rather than a panic — a
/// nonsense timestamp from the wire degrades one rendered field, never the run.
fn from_ms(ms: i64) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000).ok()
}

impl StatusPayload {
    /// Project a live snapshot onto the wire. `thresholds` is the server's own
    /// per-account list (see [`crate::manager::Manager::thresholds`]); a short
    /// list falls back to `1.0` per account, which can only fail CLOSED — at 1.0
    /// nothing but a fully-exhausted window reads as held, never a false hold.
    pub fn from_snapshot(
        snapshot: &StatsSnapshot,
        thresholds: &[f64],
        http1_only: bool,
        control: Option<String>,
        group_colors: std::collections::BTreeMap<String, String>,
    ) -> Self {
        let accounts = snapshot
            .accounts
            .iter()
            .enumerate()
            .map(|(i, a)| AccountStatus {
                name: a.name.clone(),
                priority: a.priority,
                status: a.status.clone(),
                disabled: a.disabled,
                five_hour: a.five_hour,
                five_hour_reset_ms: a.five_hour_reset.map(to_ms),
                seven_day: a.seven_day,
                seven_day_reset_ms: a.seven_day_reset.map(to_ms),
                seven_day_oi: a.seven_day_oi,
                seven_day_oi_reset_ms: a.seven_day_oi_reset.map(to_ms),
                requests: a.requests,
                input_tokens: a.input_tokens,
                output_tokens: a.output_tokens,
                cache_read_tokens: a.cache_read_tokens,
                cache_creation_tokens: a.cache_creation_tokens,
                last_used_ms: a.last_used.map(to_ms),
                rate_limited_until_ms: a.rate_limited_until.map(to_ms),
                probe_status: a.probe_status,
                last_probe_ms: a.last_probe.map(to_ms),
                probe_error: a.probe_error.clone(),
                quota_state: a.quota_state,
                gate: a.gate,
                free_at_ms: a.free_at.map(to_ms),
                threshold: thresholds.get(i).copied().unwrap_or(1.0),
                stream_error_count: a.stream_error_count,
                last_stream_error: a.last_stream_error.clone(),
                groups: a.groups.clone(),
                reserved_groups: a.reserved_groups.clone(),
                control_allowed_groups: a.control_allowed_groups.clone(),
                usage: a.usage.clone(),
            })
            .collect();
        Self {
            kind: STATUS_KIND.to_string(),
            accounts,
            // Compile-time constants of the SERVING binary — not passed in,
            // because the only honest answer is the one baked into this process.
            build: BuildInfo::current(),
            http1_only,
            control,
            group_colors,
        }
    }

    /// Rebuild the snapshot the CLI renderers take, plus the server's thresholds.
    ///
    /// The `recent` log and `sessions` table come back EMPTY and `current` `None`:
    /// they are not on the wire (see the module docs) and no `tcr status` renderer
    /// reads them. Every `AccountSnapshot` field, by contrast, is reconstructed —
    /// the struct literal below names all of them, so a new field forces an
    /// explicit decision here instead of silently rendering a default.
    pub fn into_snapshot(self) -> (StatsSnapshot, Vec<f64>) {
        let mut thresholds = Vec::with_capacity(self.accounts.len());
        let accounts = self
            .accounts
            .into_iter()
            .map(|a| {
                thresholds.push(a.threshold);
                AccountSnapshot {
                    name: a.name,
                    priority: a.priority,
                    status: a.status,
                    disabled: a.disabled,
                    five_hour: a.five_hour,
                    five_hour_reset: a.five_hour_reset_ms.and_then(from_ms),
                    seven_day: a.seven_day,
                    seven_day_reset: a.seven_day_reset_ms.and_then(from_ms),
                    seven_day_oi: a.seven_day_oi,
                    seven_day_oi_reset: a.seven_day_oi_reset_ms.and_then(from_ms),
                    requests: a.requests,
                    input_tokens: a.input_tokens,
                    output_tokens: a.output_tokens,
                    cache_read_tokens: a.cache_read_tokens,
                    cache_creation_tokens: a.cache_creation_tokens,
                    last_used: a.last_used_ms.and_then(from_ms),
                    rate_limited_until: a.rate_limited_until_ms.and_then(from_ms),
                    probe_status: a.probe_status,
                    last_probe: a.last_probe_ms.and_then(from_ms),
                    probe_error: a.probe_error,
                    quota_state: a.quota_state,
                    gate: a.gate,
                    free_at: a.free_at_ms.and_then(from_ms),
                    stream_error_count: a.stream_error_count,
                    last_stream_error: a.last_stream_error,
                    groups: a.groups,
                    reserved_groups: a.reserved_groups,
                    control_allowed_groups: a.control_allowed_groups,
                    usage: a.usage,
                }
            })
            .collect();
        (
            StatsSnapshot {
                accounts,
                current: None,
                recent: Vec::new(),
                sessions: Vec::new(),
            },
            thresholds,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot_with_counters() -> StatsSnapshot {
        StatsSnapshot {
            accounts: vec![AccountSnapshot {
                name: "alice@example.com".to_string(),
                priority: 0,
                status: "active".to_string(),
                disabled: false,
                five_hour: Some(0.42),
                five_hour_reset: from_ms(crate::now_ms() + 3_600_000),
                seven_day: None,
                seven_day_reset: None,
                seven_day_oi: Some(0.11),
                seven_day_oi_reset: from_ms(crate::now_ms() + 3_600_000),
                requests: 7,
                input_tokens: 1_000,
                output_tokens: 200,
                cache_read_tokens: 750,
                cache_creation_tokens: 50,
                last_used: from_ms(crate::now_ms()),
                rate_limited_until: None,
                probe_status: ProbeStatus::Ok,
                last_probe: from_ms(crate::now_ms()),
                probe_error: None,
                quota_state: QuotaState::Normal,
                gate: GateReason::Ok,
                free_at: None,
                stream_error_count: 0,
                last_stream_error: None,
                groups: vec!["codereview".to_string()],
                reserved_groups: vec!["codereview".to_string()],
                control_allowed_groups: vec!["codereview".to_string()],
                usage: None,
            }],
            current: Some(0),
            recent: Vec::new(),
            sessions: Vec::new(),
        }
    }

    /// A snapshot survives serialize → wire → deserialize → snapshot with every
    /// rendered field intact. This is the contract `tcr status --json` depends on:
    /// the live path must render byte-identically to the offline path given the
    /// same numbers, differing only in the `source` label.
    #[test]
    fn payload_round_trips_every_rendered_field() {
        let snapshot = snapshot_with_counters();
        let wire = serde_json::to_string(&StatusPayload::from_snapshot(
            &snapshot,
            &[0.85],
            false,
            None,
            Default::default(),
        ))
        .expect("serialize");
        let back: StatusPayload = serde_json::from_str(&wire).expect("deserialize");
        assert_eq!(back.kind, STATUS_KIND);
        let (rebuilt, thresholds) = back.into_snapshot();
        assert_eq!(thresholds, vec![0.85], "the server's threshold rides along");

        let (before, after) = (&snapshot.accounts[0], &rebuilt.accounts[0]);
        assert_eq!(after.name, before.name);
        assert_eq!(after.priority, before.priority);
        assert_eq!(after.status, before.status);
        assert_eq!(after.five_hour, before.five_hour);
        assert_eq!(after.five_hour_reset, before.five_hour_reset);
        assert_eq!(after.seven_day_oi, before.seven_day_oi);
        assert_eq!(
            after.seven_day_oi_reset, before.seven_day_oi_reset,
            "sevenDayOiResetAtMs rides the wire intact"
        );
        assert_eq!(after.requests, before.requests);
        assert_eq!(after.input_tokens, before.input_tokens);
        assert_eq!(after.output_tokens, before.output_tokens);
        assert_eq!(after.cache_read_tokens, before.cache_read_tokens);
        assert_eq!(after.cache_creation_tokens, before.cache_creation_tokens);
        assert_eq!(after.last_used, before.last_used);
        assert_eq!(after.probe_status, before.probe_status);
        assert_eq!(after.quota_state, before.quota_state);
        assert_eq!(after.gate, before.gate);
        assert_eq!(after.groups, before.groups, "groups rides the wire intact");
        assert_eq!(
            after.reserved_groups, before.reserved_groups,
            "reservedGroups rides the wire intact"
        );
    }

    /// A payload from an older server that predates `reservedGroups` still
    /// deserializes, with the field defaulting to empty — same forward-compat
    /// contract as `groups` and `stream_error_count`.
    #[test]
    fn payload_without_reserved_groups_field_still_deserializes() {
        let wire = serde_json::to_string(&StatusPayload::from_snapshot(
            &snapshot_with_counters(),
            &[0.85],
            false,
            None,
            Default::default(),
        ))
        .expect("serialize");
        let mut value: serde_json::Value = serde_json::from_str(&wire).expect("parse");
        for account in value["accounts"].as_array_mut().expect("accounts array") {
            account
                .as_object_mut()
                .expect("account object")
                .remove("reservedGroups");
        }
        let stripped = serde_json::to_string(&value).expect("re-serialize");
        let back: StatusPayload =
            serde_json::from_str(&stripped).expect("deserialize without reservedGroups field");
        assert_eq!(
            back.accounts[0].reserved_groups,
            Vec::<String>::new(),
            "missing reservedGroups field on the wire defaults to empty, not a decode error"
        );
    }

    /// A payload from an older server that predates `groups` still deserializes,
    /// with the field defaulting to empty — the same forward-compat contract
    /// `stream_error_count` already relies on.
    #[test]
    fn payload_without_groups_field_still_deserializes() {
        let wire = serde_json::to_string(&StatusPayload::from_snapshot(
            &snapshot_with_counters(),
            &[0.85],
            false,
            None,
            Default::default(),
        ))
        .expect("serialize");
        // Simulate an older server: strip every `"groups":[...]` occurrence from
        // the account object rather than hand-writing a whole payload, so this
        // test tracks the real field name if it ever changes.
        let mut value: serde_json::Value = serde_json::from_str(&wire).expect("parse");
        for account in value["accounts"].as_array_mut().expect("accounts array") {
            account
                .as_object_mut()
                .expect("account object")
                .remove("groups");
        }
        let stripped = serde_json::to_string(&value).expect("re-serialize");
        let back: StatusPayload =
            serde_json::from_str(&stripped).expect("deserialize without groups field");
        assert_eq!(
            back.accounts[0].groups,
            Vec::<String>::new(),
            "missing groups field on the wire defaults to empty, not a decode error"
        );
    }

    /// A payload from an older server that predates `sevenDayOiResetMs` still
    /// deserializes, defaulting to `None` — same forward-compat contract as
    /// `reservedGroups` and `groups`.
    #[test]
    fn payload_without_seven_day_oi_reset_field_still_deserializes() {
        let mut snapshot = snapshot_with_counters();
        snapshot.accounts[0].seven_day_oi_reset = from_ms(crate::now_ms() + 3_600_000);
        let wire = serde_json::to_string(&StatusPayload::from_snapshot(
            &snapshot,
            &[0.85],
            false,
            None,
            Default::default(),
        ))
        .expect("serialize");
        let mut value: serde_json::Value = serde_json::from_str(&wire).expect("parse");
        for account in value["accounts"].as_array_mut().expect("accounts array") {
            account
                .as_object_mut()
                .expect("account object")
                .remove("sevenDayOiResetMs");
        }
        let stripped = serde_json::to_string(&value).expect("re-serialize");
        let back: StatusPayload =
            serde_json::from_str(&stripped).expect("deserialize without sevenDayOiResetMs field");
        assert_eq!(
            back.accounts[0].seven_day_oi_reset_ms, None,
            "missing sevenDayOiResetMs field on the wire defaults to None, not a decode error"
        );
    }

    /// The server's build stamp rides along and survives the wire.
    #[test]
    fn payload_carries_the_servers_build() {
        let wire = serde_json::to_string(&StatusPayload::from_snapshot(
            &snapshot_with_counters(),
            &[0.85],
            false,
            None,
            Default::default(),
        ))
        .expect("serialize");
        let back: StatusPayload = serde_json::from_str(&wire).expect("deserialize");
        assert_eq!(
            back.build,
            BuildInfo::current(),
            "the payload reports the SERVING binary's build"
        );
        assert!(
            wire.contains("\"builtAt\""),
            "the build object is camelCase on the wire like every other field: {wire}"
        );
    }

    /// BACK-COMPAT, direction 1 — a NEW client reading an OLD server, whose
    /// payload predates the `build` field entirely.
    ///
    /// It must parse (a hard error would drop the client to the offline snapshot
    /// and its structurally-zero counters, the exact regression this endpoint
    /// exists to prevent) and it must report `unknown` — never a default that
    /// reads as a real sha, and never anything a comparison could call in-sync.
    #[test]
    fn payload_without_a_build_field_parses_as_unknown() {
        let legacy = r#"{"kind":"tcr.status.v1","accounts":[]}"#;
        let payload: StatusPayload =
            serde_json::from_str(legacy).expect("an older server's payload still parses");
        assert_eq!(payload.kind, STATUS_KIND);
        assert_eq!(payload.build, BuildInfo::default());
        assert_eq!(payload.build.sha, crate::build_info::UNKNOWN);

        // A partial build object degrades per-field rather than failing the parse.
        let partial = r#"{"kind":"tcr.status.v1","accounts":[],"build":{"sha":"cd146ce"}}"#;
        let payload: StatusPayload = serde_json::from_str(partial).expect("partial build parses");
        assert_eq!(payload.build.sha, "cd146ce");
        assert_eq!(payload.build.dirty, None);
        assert_eq!(payload.build.built_at, crate::build_info::UNKNOWN);
    }

    /// BACK-COMPAT, direction 2 — an OLD client reading a NEW server. The old
    /// binary's `StatusPayload` has no `build` field, so what keeps it working is
    /// serde's default of IGNORING unknown fields. This test stands in for that
    /// old struct: it would fail the day someone adds `deny_unknown_fields`,
    /// which is what would silently break every un-rebuilt client.
    #[test]
    fn status_payload_tolerates_an_unknown_field() {
        let future = r#"{"kind":"tcr.status.v1","accounts":[],"build":{"sha":"cd146ce","dirty":false,"builtAt":"2026-07-26T00:00:00Z"},"somethingAddedLater":{"n":1}}"#;
        let payload: StatusPayload =
            serde_json::from_str(future).expect("an unknown field is skipped, not fatal");
        assert_eq!(payload.kind, STATUS_KIND, "and the kind never had to move");
        assert_eq!(payload.build.sha, "cd146ce");
    }

    /// THE SKEW THAT ACTUALLY HAPPENS HERE: a NEW client reading an OLD
    /// server's payload — the binary on disk is rebuilt on merge while the live
    /// process keeps serving until someone restarts it.
    ///
    /// An account row with no `usage` key at all must deserialize, and read as
    /// `None` — "this server does not report usage", which is true. Anything
    /// that makes the absence a hard deserialize error instead drops
    /// `tcr status` to the offline snapshot's structural zeros and shows a
    /// fabricated healthy fleet; `stream_error_count` did exactly that on
    /// 2026-08-04.
    ///
    /// What this test actually catches is `usage` ceasing to be an `Option`
    /// (verified by mutation — making it a required field turns this red).
    /// Removing the field's `#[serde(default)]` does NOT, because serde already
    /// reads an absent `Option` as `None`; the field's own doc-comment records
    /// why, and which of its attributes is therefore load-bearing.
    #[test]
    fn a_payload_without_usage_reads_as_not_measured() {
        let old_server = r#"{"kind":"tcr.status.v1","accounts":[{"name":"alice@example.com","priority":0,"status":"active","disabled":false,"fiveHour":null,"fiveHourResetMs":null,"sevenDay":null,"sevenDayResetMs":null,"sevenDayOi":null,"requests":7,"inputTokens":1000,"outputTokens":200,"cacheReadTokens":750,"cacheCreationTokens":50,"lastUsedMs":null,"rateLimitedUntilMs":null,"probeStatus":"ok","lastProbeMs":null,"probeError":null,"quotaState":"normal","gate":"ok","freeAtMs":null,"threshold":0.85,"lastStreamError":null}]}"#;
        let payload: StatusPayload = serde_json::from_str(old_server)
            .expect("a payload with no usage key must still deserialize");
        let account = payload
            .accounts
            .first()
            .expect("the payload carries one account");
        assert_eq!(
            account.usage, None,
            "absent usage is NOT MEASURED, never a fabricated zero"
        );
        // And every counter the old server DID report survives untouched.
        assert_eq!(account.requests, 7);
        assert_eq!(account.cache_creation_tokens, 50);

        // The reconstructed snapshot carries the same absence through, so the
        // renderer one layer out has the same fact to work with.
        let (snapshot, _) = payload.into_snapshot();
        assert_eq!(snapshot.accounts[0].usage, None);
    }

    /// The other direction: a usage object on the wire survives the round trip
    /// with every dimension and the cost intact, so nothing is quietly dropped
    /// between the server and the renderer.
    #[test]
    fn a_usage_row_round_trips_through_the_payload() {
        let mut snapshot = snapshot_with_counters();
        let totals = tcr_status_wire::UsageTotals {
            requests: 3,
            input_tokens: 1_000,
            cache_creation_tokens: 2_000,
            cache_creation_1h_tokens: 500,
            cache_read_tokens: 9_000,
            output_tokens: 400,
            cost_usd: Some(0.0425),
            unpriced_requests: 1,
        };
        snapshot.accounts[0].usage = Some(tcr_status_wire::UsageRow {
            today: totals,
            window: Some(tcr_status_wire::UsageWindow {
                since: 1_767_207_600_000,
                totals,
            }),
            last_hour: totals,
            today_by_model: [("claude-opus-5".to_string(), totals)]
                .into_iter()
                .collect(),
        });
        let wire = serde_json::to_string(&StatusPayload::from_snapshot(
            &snapshot,
            &[0.85],
            false,
            None,
            Default::default(),
        ))
        .expect("payload serializes");
        let (back, _) = serde_json::from_str::<StatusPayload>(&wire)
            .expect("payload deserializes")
            .into_snapshot();
        assert_eq!(
            back.accounts[0].usage, snapshot.accounts[0].usage,
            "usage crosses the wire unchanged"
        );
        // `since` rides inside the window object via `flatten`; prove the flatten
        // actually round-trips rather than trusting the derive.
        assert_eq!(
            back.accounts[0]
                .usage
                .as_ref()
                .and_then(|u| u.window)
                .map(|w| w.since),
            Some(1_767_207_600_000)
        );
    }

    /// The MIRROR of the test above, for the direction that actually broke: a
    /// NEW client reading an OLD server's payload.
    ///
    /// This is the normal state of the system, not an edge case — the on-disk
    /// binary is rebuilt on merge while the live process keeps serving the old
    /// one until someone restarts it, so `tcr status` routinely runs newer code
    /// than the server it queries. On 2026-08-04 a client at bd60839 queried a
    /// server at 325df03, could not parse the reply for want of
    /// `streamErrorCount`, fell back to the offline snapshot, and reported all
    /// 13 accounts `active` while the log carried 52 rate-limit events and 8
    /// accounts sat on hour-long holds. A REQUIRED added field is the hazard;
    /// `#[serde(default)]` is the fix, and this test is what keeps it.
    ///
    /// The row below is deliberately spelled the way the OLD server emits it —
    /// no `streamErrorCount`, no `lastStreamError`, no `freeAtFloorMs`.
    #[test]
    fn status_payload_parses_an_older_servers_row() {
        let old = r#"{"kind":"tcr.status.v1","accounts":[{"name":"a@example.com","priority":0,
            "status":"throttled","disabled":false,"requests":7,"inputTokens":1,"outputTokens":2,
            "cacheReadTokens":0,"cacheCreationTokens":0,"probeStatus":"ok","quotaState":"normal",
            "gate":"hold","threshold":0.9}]}"#;
        let payload: StatusPayload = serde_json::from_str(old)
            .expect("a newer client MUST parse an older server's row, not fall back to offline");
        let row = &payload.accounts[0];
        assert_eq!(row.stream_error_count, 0, "absent reads as 'not reported'");
        assert_eq!(row.last_stream_error, None);
        // The fields the old server DID send must survive intact — the point is
        // graceful degradation, not a payload parsed into defaults wholesale.
        assert_eq!(row.status, "throttled");
        assert_eq!(row.requests, 7);
    }

    /// BACK-COMPAT for `control` specifically: a NEW client reading an OLD
    /// server's payload, which predates the `controlAccount` feature entirely
    /// and carries no such key. Must parse — a hard error here would drop the
    /// client to the offline snapshot, the exact regression `#[serde(default)]`
    /// on every added field exists to prevent — and `control` must read as
    /// `None`, the honest "the server never reported one", not a fabricated
    /// name.
    #[test]
    fn status_payload_without_control_parses() {
        let old = r#"{"kind":"tcr.status.v1","accounts":[]}"#;
        let payload: StatusPayload =
            serde_json::from_str(old).expect("an older server's payload still parses");
        assert_eq!(payload.control, None);
    }

    /// `control` round-trips over the wire like every other field, and
    /// `skip_serializing_if` means an UNSET control account does not even
    /// appear on the wire (never a `null` literal) — the same "clear removes
    /// the key" contract [`crate::config::save_control_account`] uses on disk.
    #[test]
    fn control_round_trips_and_absent_serializes_no_key() {
        let with_control = StatusPayload::from_snapshot(
            &snapshot_with_counters(),
            &[0.85],
            false,
            Some("alice@example.com".to_string()),
            Default::default(),
        );
        let wire = serde_json::to_string(&with_control).expect("serialize");
        assert!(wire.contains("\"control\":\"alice@example.com\""), "{wire}");
        let back: StatusPayload = serde_json::from_str(&wire).expect("deserialize");
        assert_eq!(back.control, Some("alice@example.com".to_string()));

        let without_control = StatusPayload::from_snapshot(
            &snapshot_with_counters(),
            &[0.85],
            false,
            None,
            Default::default(),
        );
        let wire = serde_json::to_string(&without_control).expect("serialize");
        assert!(
            !wire.contains("\"control\""),
            "an absent control account must not serialize a null: {wire}"
        );
    }

    /// The `kind` discriminator is what stops an older server's upstream-forwarded
    /// Anthropic error from being read as a status payload. Assert the exact
    /// literal, since the client compares against it.
    #[test]
    fn payload_carries_the_kind_discriminator() {
        let wire = serde_json::to_string(&StatusPayload::from_snapshot(
            &snapshot_with_counters(),
            &[0.9],
            false,
            None,
            Default::default(),
        ))
        .expect("serialize");
        assert!(
            wire.contains("\"kind\":\"tcr.status.v1\""),
            "payload names its kind: {wire}"
        );
    }
}
