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
/// the server's account order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusPayload {
    /// Always [`STATUS_KIND`]. See its docs for why the client demands it.
    pub kind: String,
    pub accounts: Vec<AccountStatus>,
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
    pub fn from_snapshot(snapshot: &StatsSnapshot, thresholds: &[f64]) -> Self {
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
            })
            .collect();
        Self {
            kind: STATUS_KIND.to_string(),
            accounts,
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
        let wire = serde_json::to_string(&StatusPayload::from_snapshot(&snapshot, &[0.85]))
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
        assert_eq!(after.requests, before.requests);
        assert_eq!(after.input_tokens, before.input_tokens);
        assert_eq!(after.output_tokens, before.output_tokens);
        assert_eq!(after.cache_read_tokens, before.cache_read_tokens);
        assert_eq!(after.cache_creation_tokens, before.cache_creation_tokens);
        assert_eq!(after.last_used, before.last_used);
        assert_eq!(after.probe_status, before.probe_status);
        assert_eq!(after.quota_state, before.quota_state);
        assert_eq!(after.gate, before.gate);
    }

    /// The `kind` discriminator is what stops an older server's upstream-forwarded
    /// Anthropic error from being read as a status payload. Assert the exact
    /// literal, since the client compares against it.
    #[test]
    fn payload_carries_the_kind_discriminator() {
        let wire = serde_json::to_string(&StatusPayload::from_snapshot(
            &snapshot_with_counters(),
            &[0.9],
        ))
        .expect("serialize");
        assert!(
            wire.contains("\"kind\":\"tcr.status.v1\""),
            "payload names its kind: {wire}"
        );
    }
}
