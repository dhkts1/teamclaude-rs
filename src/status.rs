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
//! request paths a client sent) and the affinity-hash session table
//! ([`StatsSnapshot::sessions`], which exists for the TUI's pin display). `tcr status` prints
//! neither, so neither is exposed.
//!
//! [`StatsSnapshot::wire_sessions`] — the Claude Code `session_id`-keyed table with tool-call
//! timing (F1, `docs/design/panel-tabs.md`) — IS on the wire, as [`StatusPayload::sessions`]:
//! it carries no credential material either, and it is the whole point of the feature.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

pub mod network_fact;

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
    /// One row per Claude Code session the proxy has seen in the last hour (F1,
    /// `docs/design/panel-tabs.md`) — see [`crate::stats::StatsSnapshot::wire_sessions`] for
    /// where this comes from and [`tcr_status_wire::SessionRow`] for the shape.
    ///
    /// `#[serde(default)]` for the same forward/back-compat reason as `group_colors` above: an
    /// OLD server's payload has no such key, and a NEW client reads an empty array — the
    /// truth, "this server never reported any sessions" — rather than failing the parse and
    /// dropping to the all-zeros offline snapshot. An OLD client reading a NEW server simply
    /// never looks at the extra key. Neither direction is MISREAD, so this must NOT bump
    /// [`STATUS_KIND`], per this struct's `build` doc-comment.
    #[serde(default)]
    pub sessions: Vec<tcr_status_wire::SessionRow>,
    /// Fleet-wide tool-call totals summed across [`Self::sessions`] server-side — see
    /// [`tcr_status_wire::SessionsSummary`]'s doc-comment. `#[serde(default)]` for the same
    /// forward/back-compat reason as `sessions` itself: an OLD server's payload has no such
    /// key, and a NEW client reads the all-zero default — the truth, "this server never
    /// reported one" — rather than failing the parse and dropping to the offline snapshot.
    /// Neither skew direction is MISREAD, so this must NOT bump [`STATUS_KIND`].
    #[serde(default)]
    pub sessions_summary: tcr_status_wire::SessionsSummary,
    /// One row per pinned Mac, in the vocabulary the panel already decodes,
    /// see [`PeerStatusRow`], and [`peer_status_rows`] for the derivation.
    ///
    /// # Why the peers ride the STATUS payload at all
    ///
    /// The Peers tab used to answer its live half from `tcr peer ls --json`,
    /// which is a projection of two FILES. Thirteen of the sixteen fields the
    /// panel decodes had no Rust writer at all, so the tab drew placeholders
    /// while both suites stayed green. A file cannot answer "how fast is this
    /// path right now"; the serving process can, and this is the seam that
    /// lets it.
    ///
    /// `#[serde(default, skip_serializing_if)]` for the same forward/back-compat
    /// reason as [`Self::sessions`], and, per this struct's `build`
    /// doc-comment on when a bump is and is not warranted, this must NOT bump
    /// [`STATUS_KIND`]: an OLD client skips the unknown key, and a NEW client
    /// reading an OLD server reads an empty list, which is the truth ("that
    /// server never reported any peers") rather than a fabricated mesh. A bump
    /// would make every not-yet-rebuilt client reject the payload and render
    /// the structural zeros this endpoint exists to end.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub peers: Vec<PeerStatusRow>,
    /// Why [`Self::peers`] is empty when the peers file itself could not be
    /// read (a bad edit, a permission mismatch) rather than because this Mac
    /// has never paired with anything. `None` on the ordinary paths: no
    /// peers file at all, or one that parsed fine and simply lists nobody.
    ///
    /// Follows [`AccountStatus::probe_error`]'s shape and, per this struct's
    /// `build` doc-comment on when a bump is and is not warranted, this must
    /// NOT bump [`STATUS_KIND`]: an OLD client skips the unknown key, and a
    /// NEW client reading an OLD server reads `None`, which is the truth
    /// ("that server never reported a read failure") rather than a
    /// fabricated one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peers_error: Option<String>,
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
    /// The parked subset of [`Self::groups`], mirroring
    /// [`AccountSnapshot::parked_groups`]. `#[serde(default)]` for the same
    /// forward-compat reason as `reserved_groups` above — an older server
    /// predates parking entirely.
    #[serde(default)]
    pub parked_groups: Vec<String>,
    /// The opted-in subset of [`Self::groups`] — see
    /// [`AccountSnapshot::control_allowed_groups`]. `#[serde(default)]` for the
    /// same forward-compat reason as the two fields above: a server that
    /// predates the opt-in omits it entirely.
    #[serde(default)]
    pub control_allowed_groups: Vec<String>,
    /// The account's plan as the profile endpoint reported it, verbatim, and the
    /// two fields that refine it — mirroring [`AccountSnapshot`]'s three.
    ///
    /// On this wire (server → CLI) rather than derived at either end, because
    /// only the server has them: they are read from the config the SERVING
    /// process loaded, and the CLI's live path has no config of its own to
    /// consult. The customer-facing label is still derived once, one layer out,
    /// by `render_accounts`/`render_accounts_json`.
    ///
    /// `#[serde(default)]` for the same forward-compat reason as `groups` above,
    /// and it degrades honestly in both directions: an older server omits the
    /// keys, a newer client reads `None`, and every surface renders "plan
    /// unknown" — which is the truth about a server that cannot report one.
    #[serde(default)]
    pub organization_type: Option<String>,
    #[serde(default)]
    pub rate_limit_tier: Option<String>,
    #[serde(default)]
    pub seat_tier: Option<String>,
    /// The org this account is scoped to — see [`AccountSnapshot::org_uuid`]:
    /// a fact for a client to show, never an address. `#[serde(default)]`
    /// for the same forward-compat reason as the fields above.
    #[serde(default)]
    pub org_uuid: Option<String>,
    #[serde(default)]
    pub org_name: Option<String>,
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
                parked_groups: a.parked_groups.clone(),
                control_allowed_groups: a.control_allowed_groups.clone(),
                organization_type: a.organization_type.clone(),
                rate_limit_tier: a.rate_limit_tier.clone(),
                seat_tier: a.seat_tier.clone(),
                org_uuid: a.org_uuid.clone(),
                org_name: a.org_name.clone(),
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
            sessions: snapshot.wire_sessions.clone(),
            sessions_summary: snapshot.wire_sessions_summary.clone(),
            // EMPTY here, and filled by the caller that holds the peers file,
            // the peer state file and the lease ledger, none of which is in a
            // [`StatsSnapshot`], which is the accounts view and nothing else.
            // Deriving them here would mean this function reading two files off
            // disk on every status request, and it takes no paths. The serving
            // process assigns [`Self::peers`] from [`peer_status_rows`] after
            // this call; an empty list is the honest answer for every other
            // caller (the CLI's own round-trip tests, `into_snapshot`).
            peers: Vec::new(),
            peers_error: None,
        }
    }

    /// Rebuild the snapshot the CLI renderers take, plus the server's thresholds.
    ///
    /// The `recent` log and the affinity-hash `sessions` table (`StatsSnapshot::sessions`,
    /// the TUI pin display) come back EMPTY and `current` `None`: they are not on the wire
    /// (see the module docs) and no `tcr status` renderer reads them.
    /// `StatsSnapshot::wire_sessions`, by contrast, IS reconstructed from
    /// [`Self::sessions`] — it genuinely round-trips.
    /// Every `AccountSnapshot` field is reconstructed too — the struct literal below names
    /// all of them, so a new field forces an explicit decision here instead of silently
    /// rendering a default.
    pub fn into_snapshot(self) -> (StatsSnapshot, Vec<f64>) {
        let mut thresholds = Vec::with_capacity(self.accounts.len());
        let accounts = self
            .accounts
            .into_iter()
            .map(|a| {
                thresholds.push(a.threshold);
                AccountSnapshot {
                    name: a.name,
                    organization_type: a.organization_type,
                    rate_limit_tier: a.rate_limit_tier,
                    seat_tier: a.seat_tier,
                    org_uuid: a.org_uuid,
                    org_name: a.org_name,
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
                    parked_groups: a.parked_groups,
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
                wire_sessions: self.sessions,
                wire_sessions_summary: self.sessions_summary,
            },
            thresholds,
        )
    }
}

/// Which kind of route a [`PathStatus`] describes.
///
/// A typed discriminant rather than a bool or a string, because the panel
/// renders the two differently and a third kind (a relay, phase 6) is already
/// on the roadmap. `direct` and `via` on the wire, which is what the panel's
/// path sub-line decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PathKind {
    /// A socket address this Mac dials itself.
    Direct,
    /// Reached through another pinned Mac that forwards for us.
    Via,
    /// Reached over a socket this Mac opened to another and parked there.
    ///
    /// Its own word rather than `via`, because the two differ in the fact a
    /// reader needs: a `via` path is one this Mac dials when it wants it, and
    /// a `reverse` one exists only while the far Mac keeps a socket parked, so
    /// "it stopped working" means different things and has different remedies.
    Reverse,
}

/// One way to reach one pinned Mac, and what is known about it.
///
/// # What is measured and what is honestly absent
///
/// [`Self::endpoint`] and [`Self::kind`] come off the peers file. Every other
/// field is a MEASUREMENT, and each one is `None` until something measures it
/// rather than `0`. The distinction is the whole reason this block exists, a
/// `0 ms` round trip beside a `0 %` loss reads as a perfect path, and that
/// sentence is exactly what a structurally-unwritten field used to print.
///
/// The two traffic fields have their writer now, the per-path meter, summed
/// into the state file's `pathTraffic` section, so for them the distinction is
/// live in both directions: an unmeasured path is `None` and a path that
/// carried nothing this hour is `Some(0)`. The per-endpoint round trip and the
/// loss fraction are the prober's and are still written by nobody into this
/// shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PathStatus {
    /// The socket address for a [`PathKind::Direct`] path, or the forwarding
    /// Mac's peer id for a [`PathKind::Via`] one.
    pub endpoint: String,
    pub kind: PathKind,
    /// Round trip over this path, milliseconds. `None` until the prober writes
    /// it; never `0`, which would read as instantaneous.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtt_ms: Option<f64>,
    /// Fraction of probes lost over this path, 0 to 1. `None` until the prober
    /// writes it; a measured zero is a real `0.0` and means something
    /// different.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loss_pct: Option<f64>,
    /// Bytes carried over THIS path in the last rolling hour, out of the state
    /// file's `pathTraffic` section
    /// ([`crate::peer::state::PathTraffic`]).
    ///
    /// The byte budget ([`crate::peer::tunnel::TunnelBudget`]) still charges a
    /// PEER and still cannot answer this: a cap keyed on anything but the peer
    /// whose handshake was checked is not a cap. The second meter beside it
    /// ([`crate::peer::tunnel::PathMeter`]) is keyed on the locator the traffic
    /// went over, and this is its figure.
    ///
    /// `None` and `Some(0)` are different answers and the distinction is the
    /// point: `None` is "nothing here measures this peer's paths", `Some(0)` is
    /// "measured, and this path carried nothing this hour". A path with no row
    /// of its own on a peer that HAS rows is a measured zero, which is what
    /// makes "it all went the other way" a readable fact rather than an
    /// absence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_per_hour: Option<u64>,
    /// Tokens drawn over THIS path in the last hour, from the same section and
    /// on the same `None`/`Some(0)` rule.
    ///
    /// It is a WINDOW and not a rate, which is what separates it from
    /// [`PeerStatusRow::tokens_per_hour`]: that one divides a lease's `spent`
    /// by the hours since it was granted and answers "how fast is this Mac
    /// drawing", this one sums what was actually debited inside the hour and
    /// answers "how much went this way". Two questions, and a reader that added
    /// the paths up expecting the row's figure would be adding a rate to a
    /// count.
    ///
    /// Zero on every path whose leases are measured in fractions of a window,
    /// for the reason [`crate::peer::lease::Ledger::note_lease_path`]'s
    /// neighbour gives: a fraction carries no token count, and this build
    /// refuses to relay a tokens-unit lease at all
    /// ([`tcr_peer_wire::LeaseRefusal::Unsupported`]), so a non-zero
    /// figure here needs that refusal lifted first.
    ///
    /// Which is to say: **this field is structurally zero in this build, on
    /// every path, and reading it as a measurement is reading a gap.** A
    /// tokens-unit lease cannot even be minted, because
    /// [`crate::peer::lease::clamp_to_grant`] takes its budget from
    /// `fraction_budget`, which is `None` for
    /// [`tcr_peer_wire::LeaseUnit::Tokens`] and answers `Unsupported` before a
    /// lease id is issued; and if one somehow existed,
    /// [`crate::peer::lease::Ledger::may_relay`] refuses it at the same
    /// predicate on every borrow. `Ledger::tokens_of`, the only caller that
    /// ever feeds [`crate::peer::tunnel::PathMeter::charge_tokens`], returns
    /// `None` for any other unit, so nothing reaches the meter.
    ///
    /// The hand-mode path does not fill the gap either, and it is the one that
    /// looks like it should: a borrower's `UsageHint` is applied by
    /// `Ledger::apply_usage_hint`, which raises the lease's `spent` and never
    /// touches the per-path meter, because that figure is a share of the
    /// owner's window and not a token count. So the live numbers today are
    /// [`Self::bytes_per_hour`] for both modes and `spent` on the lease row;
    /// a per-path token figure is a tokens lease unit away, which is a change
    /// to the mint, the relay check and the wire, not a display fix here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_per_hour: Option<u64>,
    /// Last time this Mac is known to have reached the peer over this path,
    /// Unix milliseconds.
    ///
    /// Derived, and deliberately narrow: `peer-state.json` records one
    /// `last_seen` PER PEER, so it can only be attributed to a path when the
    /// row has exactly one. With two endpoints and one timestamp, which path
    /// answered is unknown, and this stays `None` on both rather than claiming
    /// the same success twice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ok_ms: Option<i64>,
}

/// One pinned Mac as the SERVING process sees it: the sixteen fields the panel
/// already decodes (`TcrBarCore/PeerListDocument.swift`'s `PeerEntry`), plus
/// the per-path block and the one rate the ledger can answer.
///
/// # One vocabulary, and which side moved
///
/// The panel's field names win here, key for key (`lastSeenMs`, `leaseSpent`,
/// `byteCapPerHour`), and the Rust side does the renaming. The other direction
/// was tried by accident for three waves: the CLI emitted `node`, `label` and
/// `addrs` while the panel decoded `id`, `name` and `address`, every Swift
/// field is `decodeIfPresent`, and so the mismatch rendered an EMPTY row
/// instead of failing a test.
///
/// # Every field is either derived from a file or honestly absent
///
/// Nothing here probes, dials, or asks a peer anything: [`peer_status_rows`]
/// takes the two files' already-parsed contents and a clock. The fields that
/// need a live measurement ([`Self::in_flight`], [`Self::no_headroom`],
/// [`Self::bytes_per_hour`]) are `Option` and stay `None` on this path, so a
/// reader can tell "not measured" from "measured zero".
///
/// No `PartialEq`: [`crate::peer::config::LendGrant`] has none, and adding one
/// there is a change to another file. Tests compare the serialized JSON,
/// which is the contract that actually matters here anyway.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerStatusRow {
    /// The pinned key in its WIRE form, 52 Crockford characters, which is the
    /// same form `tcr peer ls --json` writes as a row's `node`.
    ///
    /// It used to be the `tcr-…` display form, and that made the two reads
    /// unjoinable: the panel merges the file's rows with the serving process's
    /// rows on this key ([`PeerListDocument.mergingLive`]), the halves carried
    /// two different spellings of the same Mac, and every live row was appended
    /// as a second row rather than merged. It is also the only form
    /// [`tcr_peer_wire::PeerId::parse`] accepts, so a command built from a row
    /// the panel is drawing (`--revoke`, `--relend`) now names something the
    /// CLI can read back.
    ///
    /// [`Self::display`] carries the short form for the screen.
    pub id: String,
    /// The same key in the `tcr-…` display form: about 50 bits, for a person to
    /// read one row aloud. Nothing parses it back.
    pub display: String,
    /// The operator's label, through the same sanitizer `tcr peer ls` uses,
    /// see [`masked_label`]. This repository is public and this payload is read
    /// by a GUI: a label that is an email or a uuid comes out `[masked]`.
    pub name: String,
    /// The newest endpoint, for the panel's single-address line. The full set
    /// is [`Self::paths`]; this is the one a row draws when it has no room for
    /// a sub-line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// Always true on this list: it is built from the peers FILE, and every row
    /// in that file is pinned. A found-not-trusted Mac is a discovery row and
    /// does not appear here at all.
    pub trusted: bool,
    /// Last time this peer answered, Unix milliseconds. Freshness, not
    /// liveness: a sleeping laptop is the normal state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_ms: Option<i64>,
    /// We may ask this Mac to carry us out ([`crate::peer::config::Allow::carry`]).
    pub carries: bool,
    /// This Mac may open a serve to us, which means it reads our requests in
    /// full ([`crate::peer::config::Allow::allow_disclose`]).
    pub serves: bool,
    /// Requests this Mac is serving right now. `None` here: in-flight lives in
    /// the running [`crate::peer::lease::Ledger`] and not in either file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_flight: Option<u32>,
    /// Fraction of the newest live lease's window already spent, 0 to 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_spent: Option<f64>,
    /// Seconds left on the newest live lease before the borrower must ask
    /// again. Clamped at zero, never negative.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_ttl_seconds: Option<i64>,
    /// Bytes carried for this Mac in the last rolling hour. `None` here for the
    /// same reason as [`PathStatus::bytes_per_hour`]: the budget lives in the
    /// serving process's [`crate::peer::tunnel::TunnelBudget`], not in a file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_per_hour: Option<u64>,
    /// The ceiling those bytes are measured against, the operator's own
    /// `maxTunnelBytesPerHour`, or
    /// [`crate::peer::tunnel::DEFAULT_MAX_TUNNEL_BYTES_PER_HOUR`], and only on
    /// a row this Mac actually carries for
    /// ([`crate::peer::config::Allow::gateway`]). A cap on a row that carries
    /// nothing is a meter with no meaning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_cap_per_hour: Option<u64>,
    /// Tokens this Mac's leases have actually drawn per hour, from the lease
    /// ledger in `peer-state.json`.
    ///
    /// Derived, and only where the ledger can answer it: a lease measured in
    /// [`tcr_peer_wire::LeaseUnit::Tokens`] knows both its granted amount and
    /// the fraction spent, so `amount * spent` over the hours since it was
    /// granted is an OBSERVED rate. A lease measured as a utilization fraction
    /// carries no token count anywhere in this tree, so those rows are `None`
    /// rather than a fraction dressed up as tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_per_hour: Option<u64>,
    /// The lender says it has nothing spare. `None` on this path: it is a
    /// refusal that arrives on the wire, and neither file records one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_headroom: Option<bool>,
    /// The row-level end, Unix SECONDS: the soonest end among the
    /// live leases this row is party to while any is running, and the latest
    /// end once they have all ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<u64>,
    /// True only when this row has at least one lease and every one of them is
    /// past its end. A row with one live lease and one ended lease is not an
    /// ended row.
    pub ended: bool,
    /// The lender's own grants for this row, one per lease, each with its
    /// scope: the per-Mac sheet's "Lend from" list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lend: Vec<crate::peer::config::LendGrant>,
    /// Every way this Mac knows to reach that one, newest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<PathStatus>,
}

/// A peer label, through the shared sanitizer, or `[masked]`.
///
/// The same rule `tcr peer ls` applies on the way out, and for the same reason:
/// this repository is public, the peers file is hand-editable JSON, and a label
/// that is an email, an org name or a uuid must not reach a screenshot, a log
/// or a bug report. A plain label is returned untouched.
pub fn masked_label(label: &str) -> String {
    tcr_peer_wire::sanitize_label(label).unwrap_or_else(|_| "[masked]".to_string())
}

/// Derive the status payload's peers block from the two files, read-only.
///
/// `rows` is the peers file's own list ([`crate::peer::config::PeerStore::peers`]),
/// `state` the peer state file ([`crate::peer::state::load`]), and `now_ms` the
/// clock every derived field is answered against, taken ONCE by the caller, so
/// two rows cannot disagree about whether the same 18:00 has arrived.
///
/// Nothing here opens a socket, and nothing here writes: a status request must
/// not be able to change the mesh, and the prober that WILL measure paths is a
/// separate writer feeding the same shape.
pub fn peer_status_rows(
    rows: &[crate::peer::config::PeerRow],
    state: &crate::peer::state::PeerState,
    now_ms: i64,
) -> Vec<PeerStatusRow> {
    let now_s = u64::try_from(now_ms.max(0) / 1_000).unwrap_or(0);
    rows.iter()
        .map(|row| {
            let last_seen_ms = state
                .last_seen
                .iter()
                .find(|(peer, _)| *peer == row.node)
                .map(|(_, at)| *at);
            let paths = peer_paths(row, last_seen_ms, state, now_ms);
            let (until, ended) = row_ends(row, state, now_s);
            let live = newest_live_lease(row, state, now_ms);
            PeerStatusRow {
                id: row.node.to_wire(),
                display: row.node.display(),
                name: masked_label(&row.label),
                // NEWEST first, which is the order `paths` is built in, so the
                // one-line address and the sub-line's first entry cannot
                // disagree about which endpoint is current.
                address: paths.first().map(|path| path.endpoint.clone()),
                trusted: true,
                last_seen_ms,
                carries: row.allow.carry,
                serves: row.allow.allow_disclose,
                in_flight: None,
                lease_spent: live.map(|lease| lease.spent),
                lease_ttl_seconds: live
                    .map(|lease| ((lease.expires_at_ms - now_ms).max(0)) / 1_000),
                bytes_per_hour: None,
                byte_cap_per_hour: row
                    .allow
                    .gateway
                    .then_some(crate::peer::tunnel::DEFAULT_MAX_TUNNEL_BYTES_PER_HOUR),
                tokens_per_hour: tokens_per_hour(row, state, now_ms),
                no_headroom: None,
                until,
                ended,
                lend: row.lend.clone(),
                paths,
            }
        })
        .collect()
}

/// The peers block for the SERVING process, read off the two files the mesh
/// keeps, at `now_ms`.
///
/// [`peer_status_rows`] is the derivation and takes already-parsed contents;
/// this is the one place that opens the files, so the proxy's status handler
/// has a single call and the derivation stays testable without a filesystem.
///
/// # Why an unreadable file is an empty block and not a 500
///
/// `tcr status` is how an operator finds out what this process thinks, very
/// much including when something is wrong with it. Refusing the whole payload
/// because the peers file is missing would take the accounts view away too,
/// and a missing peers file is the ORDINARY state of a Mac that has never
/// paired with anything. So a read that fails is logged at warn with the path
/// and the error, which is a surfaced error and not a swallowed one, and the
/// block is empty: no peers file, no peers. [`Self::peers_error`] on
/// [`StatusPayload`] is how that empty block is told apart from the ordinary
/// "no peers file at all" case; see [`peers_block_with_error`], which this
/// calls.
pub fn peers_block(peers_path: &std::path::Path, now_ms: i64) -> Vec<PeerStatusRow> {
    peers_block_with_error(peers_path, now_ms).0
}

/// [`peers_block`], plus the parse error when the peers file itself could not
/// be read, for [`StatusPayload::peers_error`]. A separate function rather
/// than changing `peers_block`'s own return type: every existing caller of
/// `peers_block` wants the rows only, and `payload.peers = peers_block(..)`
/// reads as what it does.
pub fn peers_block_with_error(
    peers_path: &std::path::Path,
    now_ms: i64,
) -> (Vec<PeerStatusRow>, Option<String>) {
    // `PeerStore` and not a bare `read_or_default`: it is the reader every
    // other surface goes through, and it is what derives each grant's `ended`
    // against the clock. A second reader here would be a second answer to
    // "has this lease ended".
    let store = match crate::peer::config::PeerStore::open(peers_path) {
        Ok(store) => store,
        Err(err) => {
            tracing::warn!(
                path = %peers_path.display(),
                error = %err,
                "status: the peers file could not be read, so the status payload's peers \
                 block is empty; the accounts it reports are unaffected"
            );
            return (Vec::new(), Some(err.to_string()));
        }
    };
    let state_path = crate::peer::serve::peer_state_path(peers_path);
    let state = match crate::peer::state::load(&state_path, now_ms) {
        Ok(state) => state,
        Err(err) => {
            // A cold start rather than a refusal, the contract this file
            // already documents: it costs the rows their `lastSeen` and their
            // lease figures, and nothing else.
            tracing::warn!(
                path = %state_path.display(),
                error = %err,
                "status: no peer runtime state, so the peers block reports what the peers \
                 file alone can say"
            );
            crate::peer::state::PeerState::default()
        }
    };
    (peer_status_rows(&store.peers(), &state, now_ms), None)
}

/// [`peer_graph`], derived off the two files on disk rather than values a
/// caller already holds: the same split [`peers_block`] draws from
/// [`peer_status_rows`], for the same reason. `tcr peer graph` and the CLI's
/// own tests both need the derivation, and only one of them should know where
/// the state file lives.
///
/// Nothing here opens a socket: an unreadable peers file reports a graph with
/// only `this` as a node, never a refusal, so asking for a picture of a mesh
/// this Mac has not yet joined is not an error.
pub fn peer_graph_block(
    peers_path: &std::path::Path,
    this: &tcr_peer_wire::PeerId,
    this_label: &str,
    now_ms: i64,
) -> PeerGraph {
    let rows = match crate::peer::config::PeerStore::open(peers_path) {
        Ok(store) => store.peers(),
        Err(err) => {
            tracing::warn!(
                path = %peers_path.display(),
                error = %err,
                "status: the peers file could not be read, so the graph carries only this Mac"
            );
            Vec::new()
        }
    };
    let state_path = crate::peer::serve::peer_state_path(peers_path);
    let state = match crate::peer::state::load(&state_path, now_ms) {
        Ok(state) => state,
        Err(err) => {
            tracing::warn!(
                path = %state_path.display(),
                error = %err,
                "status: no peer runtime state, so the graph carries no lease or freshness edges"
            );
            crate::peer::state::PeerState::default()
        }
    };
    peer_graph(this, this_label, &rows, &state, now_ms)
}

/// The whole mesh as THIS Mac sees it: one node per Mac, one edge per way to
/// reach one, one edge per live lease.
///
/// # What a graph is for, and why it is not the peers block
///
/// [`PeerStatusRow`] answers "what is my link to that Mac", one row at a time,
/// in the panel's vocabulary. A graph answers a question no row can: which Macs
/// are reachable only THROUGH another one, and which way the borrowing flows.
/// Both are derived from the same two files by the same clock, so they cannot
/// disagree: the graph is a second projection, never a second source.
///
/// # Its own kind, and why it is not [`STATUS_KIND`]
///
/// A graph is a different document with a different shape, so it carries its
/// own discriminator: a reader handed this where a status payload was expected
/// must refuse it, and the version this shape moves at has nothing to do with
/// the status payload's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerGraph {
    /// Always [`PEER_GRAPH_KIND`]. Checked by the reader before it trusts a
    /// body, for the reason [`STATUS_KIND`] gives at length.
    pub kind: String,
    /// The clock every edge was derived against, Unix milliseconds. Taken ONCE
    /// so two edges cannot disagree about whether the same lease has ended.
    pub generated_at_ms: i64,
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

/// The discriminator on every [`PeerGraph`].
pub const PEER_GRAPH_KIND: &str = "tcr.peer.graph.v1";

/// Whether a node is the Mac that answered, or one it has pinned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GraphRole {
    /// The Mac this graph was asked of. Exactly one node carries it.
    #[serde(rename = "self")]
    ThisMac,
    /// A pinned Mac. One per row in the peers file, and nothing else: a found
    /// but untrusted Mac has no identity yet (it is learned in the handshake),
    /// so it cannot be a node in an identity-addressed graph.
    #[serde(rename = "peer")]
    Peer,
}

/// One Mac.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphNode {
    /// The pinned key in its `tcr-…` display form, which is the only name a
    /// Mac has here: an address is a locator and belongs on an edge.
    pub id: String,
    /// The operator's label through [`masked_label`], this graph can be served
    /// as a page, so a label that is an email or a uuid comes out `[masked]`.
    pub name: String,
    pub role: GraphRole,
    /// Last time that Mac answered, Unix milliseconds. Absent on the node that
    /// answered (it does not see itself) and on a Mac never yet reached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_ms: Option<i64>,
}

/// One directed fact between two Macs.
///
/// The common half is `from` and `to`; what KIND of fact it is, and the fields
/// that only that kind has, are [`GraphEdgeDetail`], one tagged value rather
/// than an edge carrying every field of both kinds with half of them null. A
/// reader that matches on `kind` gets exactly the fields that kind defines.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphEdge {
    /// The node the fact points FROM, as a [`GraphNode::id`].
    pub from: String,
    pub to: String,
    #[serde(flatten)]
    pub detail: GraphEdgeDetail,
}

/// Which kind of fact an edge is, and the fields only that kind has.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
// `rename_all` on an enum renames the VARIANTS, never the fields inside
// them, so it alone let `path_kind` and `expires_at_ms` onto the wire while
// the variant tags looked perfectly right. `rename_all_fields` is the half
// that reaches the fields, and the schema test is what caught the absence.
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum GraphEdgeDetail {
    /// A way to reach the Mac at `to`. Every figure is the prober's and absent
    /// until it lands, never a zero, which would read as a perfect path.
    Path {
        /// The socket address dialled, or the forwarding Mac's id on a
        /// [`PathKind::Via`] edge.
        endpoint: String,
        path_kind: PathKind,
        /// The Mac the bytes pass THROUGH, on a via edge only. It is what makes
        /// the graph a graph: the reader can see that reaching `to` costs a
        /// third Mac's willingness to forward.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        through: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rtt_ms: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        loss_pct: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_ok_ms: Option<i64>,
    },
    /// A live lease. `from` is the LENDER and `to` is the borrower, always, so
    /// the arrow points the way the quota flows and a reader never has to
    /// consult a direction field to know which Mac is paying.
    Lease {
        /// The ledger's own id, in the 32-character hex form every CLI surface
        /// prints and `--revoke` reads back
        /// ([`crate::peer::config::lease_id_string`]). Zero-padded, because
        /// unpadded is a DIFFERENT string for any id with a leading zero
        /// nibble, and a reader lining this edge up against a lease list or
        /// building a command from it would match nothing.
        lease_id: String,
        /// Fraction of the window drawn so far, 0 to 1.
        spent: f64,
        /// The renewal deadline, Unix milliseconds: the lease dies here unless
        /// it is renewed.
        expires_at_ms: i64,
        /// The operator's own "lend until", Unix SECONDS, when one was set. A
        /// different clock from `expires_at_ms` and deliberately kept apart.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        until: Option<u64>,
    },
}

/// Derive [`PeerGraph`] from the two files, read-only.
///
/// `this` and `this_label` are the answering Mac's own identity: it is not in
/// the peers file, which holds every Mac EXCEPT this one, so a graph that
/// derived its nodes from the file alone would have no centre and no edges.
///
/// Nothing here opens a socket or writes anything, the same contract
/// [`peer_status_rows`] carries: asking for a picture of the mesh must not be
/// able to change it.
pub fn peer_graph(
    this: &tcr_peer_wire::PeerId,
    this_label: &str,
    rows: &[crate::peer::config::PeerRow],
    state: &crate::peer::state::PeerState,
    now_ms: i64,
) -> PeerGraph {
    let me = this.display();
    let mut nodes = vec![GraphNode {
        id: me.clone(),
        name: masked_label(this_label),
        role: GraphRole::ThisMac,
        last_seen_ms: None,
    }];
    let mut edges: Vec<GraphEdge> = Vec::new();

    for row in rows {
        let peer = row.node.display();
        let last_seen_ms = state
            .last_seen
            .iter()
            .find(|(node, _)| *node == row.node)
            .map(|(_, at)| *at);
        nodes.push(GraphNode {
            id: peer.clone(),
            name: masked_label(&row.label),
            role: GraphRole::Peer,
            last_seen_ms,
        });

        // The paths, from the one derivation the peers block uses too, so a
        // path drawn on the graph and the same path drawn on the tab cannot
        // report different figures.
        for path in peer_paths(row, last_seen_ms, state, now_ms) {
            // `path.endpoint` is now the WIRE id on a Via/Reverse path (the
            // fix `PeerStatusRow`'s own endpoint needed, so the panel can
            // resolve it against `id`). This graph's node ids are all the
            // DISPLAY form instead (`me`, `peer`, above), and nothing here
            // re-resolves `through` against `nodes`, so carrying the wire id
            // through unchanged would put one id space in `to` and a
            // different one in `through` for the same Mac. Parsed back to
            // display for this edge alone; a parse failure (an id this build
            // cannot read) falls back to the wire form rather than losing
            // the field.
            let endpoint = if matches!(path.kind, PathKind::Via | PathKind::Reverse) {
                tcr_peer_wire::PeerId::parse(&path.endpoint)
                    .map_or_else(|_| path.endpoint.clone(), |id| id.display())
            } else {
                path.endpoint.clone()
            };
            let through = matches!(path.kind, PathKind::Via).then(|| endpoint.clone());
            edges.push(GraphEdge {
                from: me.clone(),
                to: peer.clone(),
                detail: GraphEdgeDetail::Path {
                    endpoint,
                    path_kind: path.kind,
                    through,
                    rtt_ms: path.rtt_ms,
                    loss_pct: path.loss_pct,
                    last_ok_ms: path.last_ok_ms,
                },
            });
        }

        // Leases this Mac has LENT that peer: this Mac's quota, drawn there.
        for held in state.leases.iter().filter(|held| held.peer == row.node) {
            if held.lease.expires_at_ms <= now_ms {
                continue;
            }
            edges.push(lease_edge(&me, &peer, &held.lease));
        }
        // And the ones it has BORROWED from that peer: the arrow turns around,
        // because the lender is always `from`.
        for borrowed in state
            .borrowed
            .iter()
            .filter(|borrowed| borrowed.lender == row.node)
        {
            if borrowed.lease.expires_at_ms <= now_ms {
                continue;
            }
            edges.push(lease_edge(&peer, &me, &borrowed.lease));
        }
    }

    PeerGraph {
        kind: PEER_GRAPH_KIND.to_string(),
        generated_at_ms: now_ms,
        nodes,
        edges,
    }
}

/// One lease edge, lender to borrower.
fn lease_edge(from: &str, to: &str, lease: &tcr_peer_wire::Lease) -> GraphEdge {
    GraphEdge {
        from: from.to_string(),
        to: to.to_string(),
        detail: GraphEdgeDetail::Lease {
            lease_id: crate::peer::config::lease_id_string(lease.lease_id),
            spent: lease.spent,
            expires_at_ms: lease.expires_at_ms,
            until: lease.until,
        },
    }
}

/// The endpoints of one row as [`PathStatus`] values, newest first.
///
/// Both kinds are derived now. The row's own list carries the locator
/// ([`crate::peer::config::Locator`]), so a `Via` endpoint, a Mac that
/// forwards for us, comes through as [`PathKind::Via`] with the forwarding
/// Mac's peer id where a direct path carries a socket address. An earlier
/// version of this comment said every path was direct because the peers file
/// "holds socket addresses and nothing else"; that stopped being true when the
/// row started holding endpoints.
///
/// `observed_at_ms` is deliberately NOT copied onto the path. It says when this
/// Mac last LEARNED the endpoint, which is a different fact from when it last
/// reached the peer over it, and [`PathStatus::last_ok_ms`] is the second one.
fn peer_paths(
    row: &crate::peer::config::PeerRow,
    last_seen_ms: Option<i64>,
    state: &crate::peer::state::PeerState,
    now_ms: i64,
) -> Vec<PathStatus> {
    use crate::peer::config::Locator;

    // Whether this peer's paths are measured AT ALL, decided once for the row:
    // a peer with no traffic row has never been metered and every one of its
    // paths answers `None`, a peer with any row has, and a path of its own with
    // no row carried nothing. Deciding it per path instead would make the two
    // answers depend on which endpoint happened to be looked at first.
    let metered = state
        .path_traffic
        .iter()
        .any(|traffic| traffic.peer == row.node);

    // One endpoint means one candidate for the peer-level `last_seen`; two mean
    // the timestamp cannot be attributed and both paths say so. See
    // `PathStatus::last_ok_ms`.
    let attributable = row.endpoints.len() == 1;
    row.endpoints
        .iter()
        .map(|endpoint| {
            // The wire id, not `display()`: `PeerStatusRow::id` (the key the
            // panel resolves names by, `mergingLive` and `peerNames` both key
            // on it) is the wire form, and a `Via`/`Reverse` endpoint that
            // carried the display form instead could never be looked up in
            // that map, so the tab drew the raw id where a name belonged.
            let (address, kind) = match endpoint.locator {
                Locator::Direct { addr } => (addr.to_string(), PathKind::Direct),
                Locator::Via { node } => (node.to_wire(), PathKind::Via),
                Locator::Reverse { node } => (node.to_wire(), PathKind::Reverse),
            };
            let traffic = state
                .path_traffic
                .iter()
                .find(|traffic| traffic.peer == row.node && traffic.locator == endpoint.locator);
            PathStatus {
                endpoint: address,
                kind,
                rtt_ms: None,
                loss_pct: None,
                bytes_per_hour: metered.then(|| {
                    traffic.map_or(0, |row| {
                        rolled(row.bytes_last_hour, row.updated_at_ms, now_ms)
                    })
                }),
                tokens_per_hour: metered.then(|| {
                    traffic.map_or(0, |row| {
                        rolled(row.tokens_last_hour, row.updated_at_ms, now_ms)
                    })
                }),
                last_ok_ms: attributable.then_some(last_seen_ms).flatten(),
            }
        })
        .collect()
}

/// One written total, read back against the clock: the figure it stood at, or
/// `0` once the hour it described has entirely rolled off.
///
/// A [`crate::peer::state::PathTraffic`] row is a SUM taken at an instant, not
/// the window itself, so the only two honest readings of it are "this is what
/// the hour held then" and "then is longer ago than the window, so it holds
/// nothing now". A partial roll-off is not derivable from a total and is not
/// attempted: the meter's next write is what moves the figure, and a status
/// reader that scaled it by elapsed time would be inventing a decay the traffic
/// never had.
///
/// The boundary is [`crate::peer::tunnel::TunnelBudget::trim`]'s, to the
/// millisecond: a row exactly one window old is still inside its window on both
/// sides, so "may carry this hour" and "carried this hour" cannot be read
/// against two different hours.
fn rolled(total: u64, updated_at_ms: i64, now_ms: i64) -> u64 {
    if now_ms.saturating_sub(updated_at_ms) > crate::peer::tunnel::BUDGET_WINDOW_MS {
        0
    } else {
        total
    }
}

/// The newest lease this row is party to that has NOT expired, in either
/// direction: one the ledger minted for it, or one it granted this Mac.
///
/// Newest by `granted_at_ms`, because a row can hold several at once (decision
/// 12) and the panel draws one meter.
fn newest_live_lease<'a>(
    row: &crate::peer::config::PeerRow,
    state: &'a crate::peer::state::PeerState,
    now_ms: i64,
) -> Option<&'a tcr_peer_wire::Lease> {
    state
        .leases
        .iter()
        .filter(|held| held.peer == row.node)
        .map(|held| &held.lease)
        .chain(
            state
                .borrowed
                .iter()
                .filter(|borrowed| borrowed.lender == row.node)
                .map(|borrowed| &borrowed.lease),
        )
        .filter(|lease| lease.expires_at_ms > now_ms)
        .max_by_key(|lease| lease.granted_at_ms)
}

/// Observed tokens per hour for one row, from the lease ledger.
///
/// Summed over every live lease of that peer measured in
/// [`tcr_peer_wire::LeaseUnit::Tokens`]: `amount * spent` is what has actually
/// been drawn, and the hours since `granted_at_ms` is how long it took. A lease
/// granted less than a second ago is not a rate yet and contributes nothing,
/// dividing by that window would report a number in the billions.
///
/// `None`, not `Some(0)`, in BOTH cases where there is no rate to report: when
/// no lease of this row is measured in tokens at all (nothing in this tree
/// converts a utilization fraction to a token count), and when the only token
/// leases are younger than that first window. A zero would read as "this Mac
/// drew nothing", which is a claim and not an absence.
fn tokens_per_hour(
    row: &crate::peer::config::PeerRow,
    state: &crate::peer::state::PeerState,
    now_ms: i64,
) -> Option<u64> {
    /// A rate over a shorter window than this is noise, not a measurement.
    const MIN_ELAPSED_MS: i64 = 1_000;

    let mut total = 0.0_f64;
    let mut measured = false;
    for held in state.leases.iter().filter(|held| held.peer == row.node) {
        let tcr_peer_wire::LeaseUnit::Tokens(amount) = held.lease.unit else {
            continue;
        };
        let elapsed_ms = now_ms - held.lease.granted_at_ms;
        if elapsed_ms < MIN_ELAPSED_MS {
            // NOT measured. This sat above the check and marked the row
            // measured first, so a lease minted in the last second reported
            // `Some(0)`, "this Mac drew nothing", for a lease that had simply
            // not run long enough to divide by. The two answers this function
            // must keep apart are "no token lease here" and "not a rate yet",
            // and both of them are `None`; only a lease that has actually run
            // can contribute a number.
            continue;
        }
        measured = true;
        let hours = elapsed_ms as f64 / 3_600_000.0;
        // `spent` is a fraction of the granted amount, clamped: a ledger row
        // written by a newer build could carry a value outside 0..=1, and a
        // rate above what was granted is not a thing this can report. Dropping
        // the factor entirely reports the GRANT as if it had all been drawn,
        // which is the defect `a_lent_lease_in_tokens_reports_a_rate_from_the_ledger`
        // watched fail: 1 000 000 granted and half spent read as 500 000/h.
        total += (amount as f64) * held.lease.spent.clamp(0.0, 1.0) / hours;
    }
    measured.then(|| total.round().max(0.0) as u64)
}

/// The row-level `until` and `ended`, over every lease this row is
/// party to: the operator's grants, the leases the ledger minted against them,
/// and the leases that Mac has granted THIS one.
///
/// The same rule `tcr peer ls --json` answers with (`peer_ls_ends` in
/// `src/main.rs`), and the two must not drift: one row drawn by the panel from
/// this payload and the same row drawn from `peer ls` disagreeing about whether
/// a lease has ended is the exact confusion the two keys settled. Collapsing
/// them onto this one writer needs an edit in `src/main.rs`, which has not
/// happened yet.
fn row_ends(
    row: &crate::peer::config::PeerRow,
    state: &crate::peer::state::PeerState,
    now_s: u64,
) -> (Option<u64>, bool) {
    let mut ends: Vec<(Option<u64>, bool)> = row
        .lend
        .iter()
        .map(|grant| (grant.until, grant.has_ended(now_s)))
        .collect();
    ends.extend(
        state
            .leases
            .iter()
            .filter(|held| held.peer == row.node)
            .map(|held| (held.lease.until, lease_has_ended(&held.lease, now_s))),
    );
    ends.extend(
        state
            .borrowed
            .iter()
            .filter(|borrowed| borrowed.lender == row.node)
            .map(|borrowed| {
                (
                    borrowed.lease.until,
                    lease_has_ended(&borrowed.lease, now_s),
                )
            }),
    );

    if ends.is_empty() {
        return (None, false);
    }
    let all_ended = ends.iter().all(|(_, past)| *past);
    let named: Vec<u64> = ends.iter().filter_map(|(end, _)| *end).collect();
    let until = if all_ended {
        named.iter().copied().max()
    } else {
        // The SOONEST end among the live ones: that is the instant the row's
        // "ends in 1 h" counts down to. A lease with no end contributes no
        // candidate rather than an infinite one.
        ends.iter()
            .filter(|(_, past)| !*past)
            .filter_map(|(end, _)| *end)
            .min()
    };
    (until, all_ended)
}

/// Whether one wire lease's own end has passed, on the same clock.
///
/// A lease with no `until` has not ended by the clock: only its expiry ends it,
/// and that is a different field with a different meaning.
fn lease_has_ended(lease: &tcr_peer_wire::Lease, now_s: u64) -> bool {
    lease.until.is_some_and(|end| now_s >= end)
}

// ---------------------------------------------------------------------------
// The `tcr peer ls --json` document
// ---------------------------------------------------------------------------

/// One pinned Mac as `tcr peer ls --json` reports it.
///
/// # Why this names every field instead of flattening the row
///
/// It used to be `#[serde(flatten)] row: PeerRow`, which put EVERY key the
/// peers file holds on the wire, `rendezvousSecret` included: 64 hex characters
/// of the pair's derived port secret, on a payload TcrBar polls every three
/// seconds and an operator pastes into a bug report. `docs/peers.md` says that
/// value is never sent to anybody. A flattened struct makes "what crosses" a
/// property of another file's struct definition, so the next field added to
/// [`crate::peer::config::PeerRow`] crosses too, silently.
///
/// Naming the fields moves that decision here and makes it a compile-time one:
/// a new field on `PeerRow` reaches this payload only when somebody writes it
/// down. The secret stays in the peers FILE, which is where the reach code
/// reads it from; nothing on this surface needs it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerLsRow {
    /// The pinned static key, wire form (52 Crockford characters). The same
    /// form [`PeerStatusRow::id`] carries, so the panel can join the two reads.
    pub node: tcr_peer_wire::PeerId,
    /// The operator's label, through [`masked_label`].
    pub label: String,
    /// Every way this Mac knows to reach that one, newest first.
    #[serde(default)]
    pub endpoints: Vec<crate::peer::config::Endpoint>,
    /// When the pin was taken, Unix milliseconds.
    pub added_at: i64,
    /// What that Mac may do here, and what this Mac may ask of it.
    pub allow: crate::peer::config::Allow,
    /// The operator's own grants for this row, one per lease.
    #[serde(default)]
    pub lend: Vec<crate::peer::config::LendGrant>,
    /// The address that peer last said it sees THIS Mac at, and when it said
    /// so. An address is not a secret, and the panel draws it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sees_us_at: Option<(String, u64)>,
    /// The row-level end over every lease this row is party to, Unix SECONDS,
    /// `null` when nothing this row is party to has an end. While any lease is
    /// running it is the SOONEST end among the live ones, the instant a row's
    /// "ends in 1 h" counts down to; once they have all ended it is the LATEST
    /// end, the "ended <when>" a greyed row prints.
    ///
    /// Derived against the clock at print time and never read off the file, for
    /// the reason [`crate::peer::config::LendGrant::ended`] gives: a lease ends
    /// on a wall clock and the file's mtime does not move when 18:00 arrives.
    /// The per-lease `until` is still on each grant inside [`Self::lend`]; this
    /// is the ROW-level answer, so a row can be drawn without walking its
    /// leases and picking a different one than the next reader would.
    ///
    /// Derived from every lease this row is party to, whichever way it points:
    /// the operator's own grants and the leases the ledger minted against them,
    /// and the leases this Mac has been GRANTED by that row's Mac
    /// (`peer-state.json`'s `borrowed` section). A borrower's row used to read
    /// `until: null` for a Mac that was actively serving it, because the
    /// borrower's copy lived only in a running proxy's memory.
    pub until: Option<u64>,
    /// True only when this row has at least one lease and every one of them is
    /// past its `until`. A row with one live lease and one ended lease is not
    /// an ended row, and saying otherwise would grey a Mac still being served.
    pub ended: bool,
}

impl PeerLsRow {
    /// Project one peers-file row, with the two ends the caller derived against
    /// its own single clock.
    ///
    /// The label is masked here rather than at the call site, so no caller can
    /// forget: this repository is public and this payload is read by a GUI.
    pub fn from_row(row: crate::peer::config::PeerRow, until: Option<u64>, ended: bool) -> Self {
        Self {
            node: row.node,
            label: masked_label(&row.label),
            endpoints: row.endpoints,
            added_at: row.added_at,
            allow: row.allow,
            lend: row.lend,
            sees_us_at: row.sees_us_at,
            until,
            ended,
        }
    }
}

/// One account's exit lock as `tcr peer ls --json` reports it.
///
/// Four keys because they are one answer: where this account leaves from,
/// whether an unavailable route refuses or falls back, whether the route is
/// available right now, and how long it has been unavailable. A panel that read
/// the pin without the liveness would draw "exits via <Mac>" over an account
/// whose requests are all failing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerExitJson {
    /// The file's own word: `local`, or `via <peer id>`.
    pub egress: String,
    /// Whether a pin that cannot be honoured refuses rather than falling back.
    pub egress_strict: bool,
    /// Whether the pinned Mac is unreachable from here right now.
    pub peer_down: bool,
    /// How long the pinned Mac has been unreachable, in seconds. Absent when
    /// this Mac has never recorded contact with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_seconds: Option<u64>,
}

/// One Mac heard on the LAN and not yet trusted, as `tcr peer ls --json`
/// shows it: [`crate::peer::state::Found`] projected for a reader, the found
/// row's twin of [`PeerLsRow`].
///
/// No `node`, on purpose: a found row carries no key to pin, because the
/// beacon it comes from carries none, and `PeerListDocument`'s Swift decoder
/// already reads that absence as "not trusted, not pinned, nothing to dial
/// without pressing Trust first".
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerFoundRow {
    /// The name it announced, already whitelisted on arrival and masked
    /// again on the way out. `None` when it announced no name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// What to dial to answer it: `host:port`, built by
    /// [`crate::peer::state::Found::dial_address`].
    pub address: String,
    /// Always `false`. Written explicitly rather than left to a decoder's
    /// default, so no reader depends on the coincidence that an object with
    /// no `node` would default to the same answer.
    pub trusted: bool,
    /// When it was last heard, Unix milliseconds.
    pub last_seen_ms: i64,
}

/// One entry in `tcr peer ls --json`'s `peers` array: a pinned Mac or a found
/// one. Untagged, so both variants serialize as flat objects into one array
/// and the Swift decoder needs no discriminator field, only whether `node`
/// is present.
///
/// The order within the array matters to a reader who wants "what does this
/// Mac know about right now" in one glance: trusted rows first, found rows
/// after, newest first within the found group. `src/main.rs` builds the
/// array in that order; this type does not enforce it, because ordering an
/// array is a property of how it was built, not of what its elements are.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PeerLsPeer {
    /// A pinned Mac.
    Trusted(PeerLsRow),
    /// A Mac heard announcing, not pinned.
    Found(PeerFoundRow),
}

/// The caps the panel's Advanced pane draws, so it and this binary cannot
/// disagree about what they are.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerCapsJson {
    pub found_rows: usize,
    pub found_per_address: usize,
    pub pending: usize,
    pub knock_interval_ms: i64,
    pub knock_burst: u32,
    pub unauthenticated_sockets: usize,
}

/// The whole `tcr peer ls --json` document.
///
/// The panel reads this. The count-and-rows pairs are both present on purpose:
/// a count alone makes the panel ask a second question to render the row, and
/// the two calls would see two different instants.
///
/// Serialize only: `LentTo` has no `Deserialize`, and a reader of this document
/// is the Swift panel, which has its own model. A test asserting the shape does
/// it against the JSON, which is the contract that crosses.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerLsJson {
    pub supported: bool,
    /// One entry per Mac this node knows about right now: pinned, or heard
    /// announcing and not yet trusted. See [`PeerLsPeer`], which is an
    /// explicit projection and carries no secret.
    pub peers: Vec<PeerLsPeer>,
    /// Macs asking to pair.
    ///
    /// Each row's `addr` is the address to ANSWER on, `host:port` when the
    /// knock said which port its listener is bound to and the bare host when
    /// it did not. The panel hands that string to `tcr peer pair`, so it is
    /// built by `crate::peer::state::Knock::dial_address` rather than being
    /// the bare key the file coalesces knocks under.
    pub pending: Vec<crate::peer::state::Knock>,
    pub pending_count: usize,
    /// Blocked addresses, with the key where one was learned.
    pub blocked: Vec<crate::peer::state::Ban>,
    pub blocked_count: usize,
    /// Muted addresses, quiet until the deadline lifts on its own.
    pub muted: Vec<crate::peer::state::Mute>,
    pub muted_count: usize,
    /// How many found rows this Mac is holding back past
    /// `crate::peer::discovery::MAX_FOUND_ROWS`, carried from the serving
    /// process's browse (`crate::peer::state::PeerState::found_not_shown`).
    /// Zero when nothing was held back, and also zero when no serving
    /// process is browsing at all: a down proxy holds nothing back, it
    /// simply holds nothing.
    pub limited: usize,
    pub caps: PeerCapsJson,
    /// The "Lent to …" line, per account label.
    pub lent_to: std::collections::BTreeMap<String, Vec<crate::peer::lease::LentTo>>,
    /// Whether this Mac may be reached from off its own LAN: the peers file's
    /// own setting, not a reachability test.
    pub internet: bool,
    /// Whether this Mac is on a network at all right now: at least one
    /// non-loopback interface up with a usable address. See
    /// [`network_fact::network_present`] for the rule. `false` is the fact
    /// that turns "Looking" into "No network" on the Peers tab; old readers
    /// that do not know this key yet simply ignore it.
    pub network: bool,
    /// Every account that has an exit lock, keyed by the label a `--scope`
    /// names it by.
    pub exits: std::collections::BTreeMap<String, PeerExitJson>,
    /// Why every field above reads empty when the peers file itself could
    /// not be parsed, rather than because nothing is pinned. Follows
    /// [`StatusPayload::peers_error`]'s shape, `None` on the ordinary path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peers_error: Option<String>,
}

impl PeerLsJson {
    /// The whole document when the peers file itself could not be read: every
    /// count and row is the honest "nothing to report" default, and
    /// [`Self::peers_error`] is the only field that says this is a read
    /// failure and not an empty mesh. `caps` still reports the real
    /// compile-time limits: they describe this build, not the file.
    pub fn unreadable(error: String) -> Self {
        Self {
            supported: true,
            peers: Vec::new(),
            pending: Vec::new(),
            pending_count: 0,
            blocked: Vec::new(),
            blocked_count: 0,
            muted: Vec::new(),
            muted_count: 0,
            limited: 0,
            caps: PeerCapsJson {
                found_rows: crate::peer::discovery::MAX_FOUND_ROWS,
                found_per_address: crate::peer::discovery::MAX_FOUND_PER_ADDRESS,
                pending: crate::peer::state::MAX_PENDING_KNOCKS,
                knock_interval_ms: crate::peer::listener::KNOCK_INTERVAL_MS,
                knock_burst: crate::peer::listener::KNOCK_BURST,
                unauthenticated_sockets: crate::peer::listener::MAX_UNAUTHENTICATED_SOCKETS,
            },
            lent_to: std::collections::BTreeMap::new(),
            internet: false,
            network: network_fact::network_present(),
            exits: std::collections::BTreeMap::new(),
            peers_error: Some(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot_with_counters() -> StatsSnapshot {
        StatsSnapshot {
            accounts: vec![AccountSnapshot {
                name: "alice@example.com".to_string(),
                organization_type: Some("claude_team".to_string()),
                rate_limit_tier: Some("default_raven".to_string()),
                seat_tier: Some("team_standard".to_string()),
                org_uuid: Some("11111111-1111-1111-1111-111111111111".to_string()),
                org_name: Some("Example Org".to_string()),
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
                parked_groups: Vec::new(),
                control_allowed_groups: vec!["codereview".to_string()],
                usage: None,
            }],
            current: Some(0),
            recent: Vec::new(),
            sessions: Vec::new(),
            wire_sessions: vec![tcr_status_wire::SessionRow {
                session_id: "sess-1".to_string(),
                account: Some("alice@example.com".to_string()),
                model: Some("claude-fable-5".to_string()),
                first_seen_ms: crate::now_ms() - 60_000,
                last_seen_ms: crate::now_ms(),
                requests: 3,
                input_tokens: 400,
                output_tokens: 100,
                cache_read_tokens: 200,
                cache_creation_tokens: 30,
                tools: tcr_status_wire::SessionToolsRow {
                    calls: 2,
                    errors: 0,
                    timeouts: 1,
                    timeouts_by_class: [("git-net".to_string(), 1u64)].into_iter().collect(),
                    timed_out: vec![tcr_status_wire::SlowToolRow {
                        tool: "Bash".to_string(),
                        seconds: 600.0,
                        command_head: Some("git push origin main".to_string()),
                        command_class: Some("git-net".to_string()),
                        ended_ms: crate::now_ms(),
                    }],
                    running: vec![tcr_status_wire::RunningToolRow {
                        tool: "Bash".to_string(),
                        started_ms: crate::now_ms() - 5_000,
                        command_head: Some("ls -la".to_string()),
                        command_class: Some("other".to_string()),
                    }],
                    subagents_running: 0,
                    slowest: vec![tcr_status_wire::SlowToolRow {
                        tool: "Bash".to_string(),
                        seconds: 3.5,
                        command_head: Some("sleep 3".to_string()),
                        command_class: Some("wait".to_string()),
                        ended_ms: crate::now_ms(),
                    }],
                    over_one_minute: 0,
                    by_tool: vec![tcr_status_wire::ToolBucketRow {
                        tool: "Bash".to_string(),
                        calls: 2,
                        errors: 0,
                        seconds_p50: 3.5,
                        over_one_minute: 0,
                    }],
                },
                req_per_minute: vec![0; 29].into_iter().chain(std::iter::once(3)).collect(),
                cost_usd: 0.0125,
            }],
            wire_sessions_summary: tcr_status_wire::SessionsSummary {
                calls: 2,
                over_one_minute: 0,
                timeouts: 1,
                timeouts_by_class: [("git-net".to_string(), 1u64)].into_iter().collect(),
                by_tool: vec![tcr_status_wire::ToolBucketRow {
                    tool: "Bash".to_string(),
                    calls: 2,
                    errors: 0,
                    seconds_p50: 0.0,
                    over_one_minute: 0,
                }],
                cost_usd: 0.0125,
            },
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
            rebuilt.wire_sessions, snapshot.wire_sessions,
            "the sessions array rides the wire intact"
        );
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

    /// A payload from an older server that predates the `sessions` array (F1) still
    /// deserializes, defaulting to empty — same forward-compat contract as `groupColors`.
    #[test]
    fn payload_without_sessions_field_still_deserializes() {
        let wire = serde_json::to_string(&StatusPayload::from_snapshot(
            &snapshot_with_counters(),
            &[0.85],
            false,
            None,
            Default::default(),
        ))
        .expect("serialize");
        let mut value: serde_json::Value = serde_json::from_str(&wire).expect("parse");
        value
            .as_object_mut()
            .expect("payload object")
            .remove("sessions");
        let stripped = serde_json::to_string(&value).expect("re-serialize");
        let back: StatusPayload =
            serde_json::from_str(&stripped).expect("deserialize without a sessions field");
        assert_eq!(
            back.sessions,
            Vec::new(),
            "missing sessions field on the wire defaults to empty, not a decode error"
        );
    }

    /// A payload from an older server that predates `subagentsRunning` (F4) still
    /// deserializes, defaulting to zero — same forward-compat contract as `sessions` itself.
    #[test]
    fn payload_without_subagents_running_field_still_deserializes() {
        let wire = serde_json::to_string(&StatusPayload::from_snapshot(
            &snapshot_with_counters(),
            &[0.85],
            false,
            None,
            Default::default(),
        ))
        .expect("serialize");
        let mut value: serde_json::Value = serde_json::from_str(&wire).expect("parse");
        for session in value["sessions"].as_array_mut().expect("sessions array") {
            session["tools"]
                .as_object_mut()
                .expect("tools object")
                .remove("subagentsRunning");
        }
        let stripped = serde_json::to_string(&value).expect("re-serialize");
        let back: StatusPayload =
            serde_json::from_str(&stripped).expect("deserialize without subagentsRunning field");
        assert_eq!(
            back.sessions[0].tools.subagents_running, 0,
            "missing subagentsRunning field on the wire defaults to 0, not a decode error"
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

    /// **A found row on `tcr peer ls --json`'s `peers` array has no `node`
    /// key, no `id` key, and `"trusted":false`, written explicitly.**
    ///
    /// `PeerListDocument`'s Swift decoder reads a `node`-less object as a
    /// found row and reads `trusted` as `false` by default when `node` is
    /// absent, but this project's own rule is that nothing here may depend
    /// on that coincidence. Watch it fail by wrapping `trusted` in
    /// `#[serde(skip_serializing_if)]` on [`PeerFoundRow`]: the key
    /// disappears from the wire and this assertion catches it.
    #[test]
    fn a_found_peer_serializes_with_no_node_key_and_trusted_written_explicitly() {
        let found = PeerLsPeer::Found(PeerFoundRow {
            name: Some("studio-mac".to_string()),
            address: "198.51.100.7:7755".to_string(),
            trusted: false,
            last_seen_ms: 12_000,
        });
        let value = serde_json::to_value(&found).expect("serialize a found row");
        let object = value.as_object().expect("a found row is an object");
        assert!(
            !object.contains_key("node"),
            "a found row must carry no node key: {object:?}"
        );
        assert!(
            !object.contains_key("id"),
            "a found row must carry no id key: {object:?}"
        );
        assert_eq!(
            object.get("trusted"),
            Some(&serde_json::Value::Bool(false)),
            "trusted must be written explicitly as false: {object:?}"
        );
    }
}
