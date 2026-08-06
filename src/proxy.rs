//! The axum catch-all handler: authenticate, select an account, refresh its
//! token, forward the request to Anthropic, and stream the response back
//! **unchanged** while a side consumer parses SSE usage.
//!
//! Behaviours designed out of the JS proxy (see `DESIGN.md` + the sweep notes):
//! - The client's inbound `authorization` header is **stripped** before we set
//!   our own `Bearer` (never forward a client token upstream).
//! - An upstream `401` force-refreshes the serving account's token **once**. If a
//!   new token was applied it retries the SAME account; if the force was
//!   coalesced/cooldown-suppressed or the refresh failed (no new token) it rotates
//!   away WITHOUT sidelining — a healthy account is never marked `Error` for losing
//!   a race to the refresh throttle. Only a second `401` on a genuinely refreshed
//!   token sidelines it. A stale-token `401` is never passed to the client.
//! - A `429` with a `rejected` unified status is durable quota exhaustion →
//!   throttle + rotate; a transient `429` waits (bounded) and retries the same
//!   account, or rotates once the wait would be too long.
//! - Token usage is counted on the **true serving account** and the input count
//!   includes `cache_creation_input_tokens` + `cache_read_input_tokens` (the JS
//!   proxy counted only `input_tokens`). It is applied **once** per served
//!   request, so a retry that rotated accounts never double-counts.

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::extract::{Request, State};
use axum::http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HOST, USER_AGENT};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::Response;
use axum::Router;
use bytes::Bytes;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde_json::Value;
use time::OffsetDateTime;

use crate::manager::{AccountStatus, Manager};
use crate::stats::{RequestLogEntry, SessionKind};

/// Cap on a buffered request body (256 MiB) — a single-user localhost proxy has
/// no legitimate payload near this, but an unbounded read is a DoS surface. The
/// non-stream *response* body is bounded by the same cap (see [`read_capped_body`]).
const MAX_BODY_BYTES: usize = 256 * 1024 * 1024;
/// Depth of the SSE tee side-channel. A fast upstream feeding a slow parser task
/// can retain at most this many chunks; beyond it, `try_send` drops chunks for
/// the parser (usage counting becomes best-effort) rather than letting the buffer
/// grow without bound or gating the client passthrough on the parser.
const SSE_TEE_CAPACITY: usize = 256;
/// A transient `429` whose `retry-after` is within this bound is waited out
/// inline on the same account; anything longer throttles + rotates instead of
/// tying up the client connection.
const INLINE_WAIT_MAX_SECS: i64 = 15;
/// How many times one account may be inline-retried on a transient `429` before
/// we give up on it and rotate — bounds a pathological same-account loop.
const MAX_SAME_ACCOUNT_429: u32 = 2;
/// Hold applied to a transient 429 that carries NO retry-after / reset header
/// (σ5, 2026-07-18 live capture: Anthropic burst 429s carry neither). Short so a
/// cold-fan-out that trips every account recovers in seconds instead of the ~60s
/// fleet blackout the old unwrap_or(60) default caused. Rotates rather than
/// inline-retrying the limited account.
const NO_GUIDANCE_HOLD_SECS: i64 = 15;
/// Max random jitter (seconds) added to the no-guidance hold: desync the un-park
/// of accounts that tripped together, so a synchronized wave can't re-arm a
/// sliding-window limiter.
const NO_GUIDANCE_JITTER_MAX_SECS: i64 = 5;
/// Ceiling for a quota-rejection hold, mirroring `MAX_RATE_LIMIT_HOLD_SECONDS`
/// in `manager/mod.rs` (which clamps again, so this is the value that actually
/// binds at the call site).
const MAX_QUOTA_HOLD_SECS: i64 = 3600;
/// Spread applied to a CEILING-CLAMPED quota hold so accounts rejected together
/// do not free together. Sized well under the ceiling: it must desync the herd
/// without materially shortening a genuine cap. See [`jittered_quota_hold`].
const QUOTA_HOLD_JITTER_MAX_SECS: i64 = 90;
/// Ceiling for soft-waiting out a *transient* all-parked fleet at the exhausted
/// branch instead of firing the hard 429. Set to the maximum no-guidance park
/// (`NO_GUIDANCE_HOLD_SECS + NO_GUIDANCE_JITTER_MAX_SECS` = 20s): a burst park
/// falls at/under it, while a real quota rejection (mark_rate_limited clamped up
/// to 3600s) or a quota window (hours) sits far above it — so real exhaustion is
/// NEVER soft-waited and still hard-fails immediately.
const EXHAUSTION_SOFT_WAIT_MAX_SECS: i64 = NO_GUIDANCE_HOLD_SECS + NO_GUIDANCE_JITTER_MAX_SECS;
// Correctness of the soft-wait path depends on this cap staying BELOW
// `Manager::retry_after_hint`'s `None => 60` sentinel (mod.rs): a soonest-free of
// exactly 60 there means "no account has a known reset", NOT "60s away", so a cap
// >= 60 would make an unknown-reset fleet spuriously soft-wait. Enforced at compile time.
const _: () = assert!(
    EXHAUSTION_SOFT_WAIT_MAX_SECS < 60,
    "soft-wait cap must stay below retry_after_hint's None=>60 sentinel"
);

/// Anthropic's non-standard `529 Overloaded`. Not in [`StatusCode`]'s constants
/// (it is outside the IANA registry), so it is compared as a raw `u16`.
const STATUS_OVERLOADED: u16 = 529;
/// How many times one account may be retried IN PLACE on a `529 Overloaded`
/// before the status is forwarded to the client. Counts RETRIES, not attempts —
/// same semantics as [`MAX_SAME_ACCOUNT_429`] — so the budget is `1 + this`
/// upstream sends on that account.
const MAX_SAME_ACCOUNT_529_RETRIES: u32 = 2;
/// Base of the escalating 529 backoff: retry `n` waits `BASE << n` seconds
/// (1s, 2s), so the no-`retry-after` ladder adds 3s to a request at worst.
const RETRY_529_BASE_BACKOFF_SECS: i64 = 1;
/// Ceiling on ANY single 529 backoff, including a server-supplied `retry-after`.
/// With [`MAX_SAME_ACCOUNT_529_RETRIES`] this bounds the added latency at 8s PER
/// ACCOUNT — deliberately single-digit, because the in-flight guard is held across
/// the wait (see the 529 arm in [`handle`]). A request may spend that ladder on up
/// to `1 + `[`MAX_529_FAILOVERS_PER_REQUEST`] accounts, so the REQUEST-level ceiling
/// is a multiple of it (`overloaded_529_failover_worst_case_latency_is_bounded`).
const RETRY_529_MAX_BACKOFF_SECS: i64 = 4;
/// How many DIFFERENT accounts one request may fail over to after a
/// `529 Overloaded` has spent its IN-PLACE retries on an account.
///
/// The 529 arm shipped deliberately rotation-free, on the premise that a 529 means
/// the SERVER is saturated — so another account would be equally overloaded and the
/// failover would only pay a cold prompt cache. The live log falsifies that: over
/// one 8-minute window a single account answered 136 529s while its siblings served
/// 200s, and a session diverted off it (by an unrelated transport timeout) got a 200
/// from a sibling two seconds later. The overload is ACCOUNT-scoped and
/// time-varying, so one cold prefix is worth trading for a served request.
///
/// Bounded at 2 (at most 3 accounts per request) because the cost is paid in full
/// on every hop: each new account re-runs its own in-place ladder before yielding,
/// so the REQUEST's worst-case added sleep is `1 + this` times the per-account total
/// (asserted in `overloaded_529_failover_worst_case_latency_is_bounded`). Unbounded,
/// one request would walk an overloaded fleet end to end and every concurrent
/// request would do it too.
const MAX_529_FAILOVERS_PER_REQUEST: u32 = 2;

/// Backoff (seconds) before the `retried`-th in-place retry of a `529 Overloaded`.
///
/// The ladder is exponential from [`RETRY_529_BASE_BACKOFF_SECS`] — 1s, 2s — so a
/// briefly-overloaded upstream is retried almost immediately while a persistently
/// overloaded one is not hammered. A `retry-after` is HONOURED as a FLOOR (the
/// server knows how long it needs better than the ladder does) but CLAMPED to
/// [`RETRY_529_MAX_BACKOFF_SECS`]: an overloaded upstream asking for 300s must
/// never park a client connection — and the per-account in-flight slot it holds —
/// for minutes. Past the clamp the honest answer is to forward the 529 and let
/// the client decide. Pure — unit-tested.
fn backoff_529_secs(retried: u32, retry_after: Option<i64>) -> u64 {
    // `min(16)` keeps the shift defined for any counter; the clamp below makes
    // every value past the first couple of rungs collapse to the ceiling anyway.
    let ladder = RETRY_529_BASE_BACKOFF_SECS.saturating_mul(1i64 << retried.min(16));
    let secs = match retry_after {
        Some(hint) => hint.max(ladder),
        None => ladder,
    };
    secs.clamp(1, RETRY_529_MAX_BACKOFF_SECS) as u64
}

/// How a transient (non-quota-rejected) 429 should be handled.
#[derive(Debug, PartialEq, Eq)]
enum Transient429 {
    /// Wait `secs` inline on the same account, then retry it.
    InlineWait(i64),
    /// Park the account for `secs` and route THIS request elsewhere.
    ///
    /// "Rotate" is only half the story for a pinned session. The park arms
    /// `rate_limited_until_ms`, and selection reads its REMAINING duration against
    /// `CACHE_WARM_HOLD_SECS` (`src/manager/mod.rs`): a park that clears while the
    /// prompt cache is still warm diverts this one request and LEAVES THE PIN, so
    /// the session comes home to its warm prefix when the timer runs out; only a
    /// park that outlives the cache re-keys the session durably. Every park this
    /// function produces is short — `NO_GUIDANCE_HOLD_SECS` + jitter, or a
    /// `retry-after` clamped to 300s — so the pin-keeping branch is the common one
    /// here. The long parks come from the quota-rejected path instead, which calls
    /// `mark_rate_limited` with a `retry-after` clamped up to 3600s.
    Park(i64),
}

/// Decide how to handle a transient (non-quota-rejected) 429. Pure — unit-tested.
///
/// A PRESENT `retry-after` keeps the historical semantics (inline-wait a short
/// hint, else park+rotate). An ABSENT header no longer fabricates a 60s park
/// (which blacked out the whole fleet on a cold fan-out); it parks a short
/// [`NO_GUIDANCE_HOLD_SECS`] and rotates so the next probe can discover the real
/// window instead of the account inline-retrying into its own limit.
///
/// `jitter` (seconds) is added ONLY to the no-guidance hold so accounts that
/// tripped together un-park at staggered times; the present-`retry-after` path
/// The hold to arm for a QUOTA-REJECTED 429, jittered so a burst of accounts
/// rejected together does not un-park in lockstep.
///
/// Why this exists, measured 2026-08-04: a fan-out drew 52 rate-limit events in
/// 72 minutes, and **42 of them armed an identical 3600s hold** — eight accounts
/// inside a four-second window. They would then all have freed within that same
/// four seconds and re-burst as one wave. [`NO_GUIDANCE_JITTER_MAX_SECS`] already
/// documents this hazard verbatim ("desync the un-park of accounts that tripped
/// together, so a synchronized wave can't re-arm a sliding-window limiter") but
/// was only ever wired to the no-guidance path, never to this one.
///
/// The jitter is SUBTRACTED, and only from a hold that hit the ceiling. That is
/// deliberate and is the only direction that is safe here:
///
/// * Adding is useless — [`Manager::mark_rate_limited`] clamps to
///   `MAX_RATE_LIMIT_HOLD_SECONDS` (3600), so `3600 + jitter` clamps straight
///   back to 3600 and the herd stays synchronized. The 42 identical holds were
///   synchronized by OUR ceiling, not by upstream's number.
/// * Subtracting from a SHORT hold would return an account before upstream said
///   it may come back — the one thing this path must never do. So a hold under
///   the ceiling is armed verbatim, unjittered.
/// * At the ceiling we are ALREADY returning earlier than asked (upstream wanted
///   `3600s` or more and got exactly 3600), so spreading within
///   `[3600 - N, 3600]` stays inside an envelope we had already chosen, and
///   strictly improves on arming every account at one instant.
fn jittered_quota_hold(retry_after: i64, nanos: u32) -> i64 {
    let clamped = retry_after.clamp(1, MAX_QUOTA_HOLD_SECS);
    if clamped < MAX_QUOTA_HOLD_SECS {
        return clamped;
    }
    let jitter = (nanos as i64) % (QUOTA_HOLD_JITTER_MAX_SECS + 1);
    (clamped - jitter).max(1)
}

/// ignores it and stays byte-identical to the historical behavior.
fn classify_transient_429(retry_after: Option<i64>, retried: u32, jitter: i64) -> Transient429 {
    match retry_after {
        Some(secs) => {
            let wait = secs.clamp(1, 300);
            if retried < MAX_SAME_ACCOUNT_429 && wait <= INLINE_WAIT_MAX_SECS {
                Transient429::InlineWait(wait)
            } else {
                Transient429::Park(wait)
            }
        }
        None => Transient429::Park(NO_GUIDANCE_HOLD_SECS + jitter),
    }
}

/// Decide whether an all-accounts-unavailable state is a *transient* fleet-park
/// worth soft-waiting (Some(secs) to sleep) vs genuine exhaustion (None → hard 429).
/// `soonest_free_secs` is `Manager::retry_after_hint`; `already_waited` caps us to
/// ONE soft-wait per client request. Real exhaustion has soonest_free far above the
/// transient ceiling → None. Kept pure so it is unit-tested without timing.
fn soft_wait_secs(soonest_free_secs: i64, already_waited: bool) -> Option<u64> {
    if already_waited || soonest_free_secs <= 0 || soonest_free_secs > EXHAUSTION_SOFT_WAIT_MAX_SECS
    {
        None
    } else {
        Some(soonest_free_secs as u64)
    }
}

/// The rotation loop's TOTAL attempt budget for a fleet of `account_count` — two
/// sends per account plus a small constant, so the per-account retry ladders (401
/// force-refresh, transient-429 inline wait, the 529 backoff ladder and its
/// failovers) can never spin [`handle`]'s loop.
///
/// Extracted from the loop so the headroom assertion in
/// `overloaded_529_failover_worst_case_latency_is_bounded` binds THIS formula
/// instead of a copy of it that could silently drift from it.
fn max_attempts_for(account_count: usize) -> usize {
    account_count.saturating_mul(2).saturating_add(4).max(1)
}

/// The client's socket address, injected into request extensions by the hybrid
/// server ([`crate::mitm::serve_http`]) so the auth layer can exempt loopback
/// clients from the api-key gate. A localhost personal proxy must not demand its
/// own key from the local user; a non-loopback client (were the listener ever
/// bound wider than 127.0.0.1) still presents it. Mirrors the JS proxy's
/// "loopback client is exempt" rule.
#[derive(Clone, Copy)]
pub struct ClientAddr(pub SocketAddr);

/// The per-connection session key, injected into request extensions by the hybrid
/// server ([`crate::mitm::serve_http`]) **only when session affinity is enabled**.
/// Its PRESENCE is the feature flag: it lets a request derive a pin via
/// [`stable_session_key`] and pass it as [`Manager::select`]'s `affinity` arg; its
/// absence (the default) leaves selection at the per-request LRU rotation. The
/// wrapped value is deliberately NOT used as a routing key — one connection is one
/// `claude` process, but a pin keyed on it dies with the connection and leaves a
/// ghost entry behind, so a request with no stable identity routes unpinned.
#[derive(Clone, Copy)]
pub struct SessionKey(pub u64);

/// A deterministic hash of `prefix` + `value`, used to derive a stable affinity
/// key. `DefaultHasher` is deterministic within a process (unlike a randomized
/// `RandomState`), which is all affinity needs — the map lives for the process's
/// lifetime. The `prefix` namespaces the input space so an x-api-key and a
/// `user_id` with the same string never collide onto one account.
fn stable_hash(prefix: &str, value: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    prefix.hash(&mut hasher);
    value.hash(&mut hasher);
    hasher.finish()
}

/// Minimal shape reading only top-level `metadata.user_id`, ignoring everything
/// else. Mirrors [`crate::model::parse_request_model`]'s lenient peek.
#[derive(serde::Deserialize)]
struct MetadataPeek {
    metadata: Option<UserIdMeta>,
}

#[derive(serde::Deserialize)]
struct UserIdMeta {
    user_id: Option<String>,
}

/// Derive a STABLE affinity key from the client's most durable identity, so a
/// session survives reconnects on one account (warm prompt cache). Priority:
///   1. the `x-api-key` header (distinct team keys → distinct accounts) — but
///      SKIPPED when it equals the configured proxy key, since the shared proxy
///      secret is not a per-client identity (every remote client sends it), else
///   2. the body's top-level `metadata.user_id` (the loopback/personal path,
///      where there's no distinguishing x-api-key). Claude Code sends this as a
///      STRINGIFIED JSON blob `{"device_id":"…","account_uuid":"…","session_id":"…"}`.
///      Live-verified 2026-07-16 (6-request header/body capture): the embedded
///      `session_id` is LINEAGE-stable — a resumed session (`-r <id>`) keeps the
///      ORIGINAL id here, and subagent/sidechain requests carry the PARENT's id —
///      so hashing the whole string pins a conversation, its resumes, and its
///      subagents to one account (warm prompt cache). Do NOT key on the
///      `x-claude-code-session-id` HEADER instead: it forks on resume and varies
///      across sidechain requests, which would cold-start the cache. Else
///   3. `None` — the request has no stable identity and routes UNPINNED (plain
///      LRU). It deliberately does NOT fall back to the per-connection
///      [`SessionKey`]: that mints a pin no reconnect can ever reuse or reclaim.
///
/// Returns `None` on absence/parse failure, which routes the request unpinned.
fn stable_session_key(headers: &HeaderMap, body: &[u8], proxy_key: Option<&str>) -> Option<u64> {
    if let Some(key) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        // The shared proxy secret is not a client identity — skip it so remote
        // clients don't all collapse onto one account.
        if proxy_key != Some(key) {
            return Some(stable_hash("key:", key));
        }
    }
    let user_id = serde_json::from_slice::<MetadataPeek>(body)
        .ok()
        .and_then(|p| p.metadata)
        .and_then(|m| m.user_id)?;
    Some(stable_hash("uid:", &user_id))
}

/// Path of the read-only live-status endpoint [`status_handler`] serves.
///
/// The `_tcr/` prefix is what makes the route safe to add to a transparent
/// forwarder: every path a client legitimately means for Anthropic lives under
/// `/v1/…` (`/v1/messages`, `/v1/models`, `/v1/organizations/…`), so a segment
/// starting with an underscore can neither collide with a request we must forward
/// today nor be shadowed by an Anthropic route added tomorrow. Anything the proxy
/// ever answers locally belongs under this prefix.
pub const STATUS_PATH: &str = "/_tcr/status";

/// The path segment [`STATUS_PATH`] lives under. Every path beneath it belongs to
/// the PROXY, never to Anthropic, so anything under it that is not a registered
/// route is answered with a LOCAL 404 (see the guard in [`handle`]) instead of
/// being forwarded. That is not hygiene: before the guard existed a typo'd status
/// probe fell through the catch-all and was sent to `api.anthropic.com/_tcr/status`
/// carrying a pooled OAuth Bearer, which burned an account on a request no
/// upstream route could ever answer.
///
/// Stored WITHOUT a trailing slash and matched by [`path_is_under`], so the bare
/// `/_tcr` has no unguarded edge.
const LOCAL_PREFIX: &str = "/_tcr";

/// Paths whose upstream call must carry the **client's own** credential and which
/// therefore bypass account selection entirely. Matched as prefixes of the request
/// path, mirroring the JS proxy's `CLIENT_CREDENTIAL_PATHS` (`teamclaude/src/server.js`).
///
/// Do NOT "simplify" any of these back into the catch-all. They are bound to the
/// identity that authenticated the CLIENT, so a rotated pooled token is the wrong
/// credential by construction:
/// - `/v1/code/…` is the Remote Control channel, bound to the session's paired
///   claude.ai identity; a pooled token 403s its worker event stream.
/// - `/api/oauth/files/…` and `/api/oauth/file_upload` are attachment transfers. A
///   file uploaded from claude.ai belongs to the paired identity, so fetching it
///   with a pooled token 403s and **Claude Code silently drops the image from the
///   message** — nothing surfaces the failure, the turn just loses its attachment.
///
/// Measured on the live log while every one of these still went through rotation:
/// 13 of 57 sessionless requests came back 404 (22.8%), against 0 of 1556 pinned ones.
///
/// Written WITHOUT trailing slashes and matched by [`path_is_under`] — an entry
/// matches the exact path or that path followed by `/`, never a longer identifier.
/// Both edges of a raw `starts_with` were live defects: `"/api/oauth/file_upload"`
/// (no terminator) also relayed `/api/oauth/file_upload_v2`, and `"/v1/code/"`
/// (with one) missed the bare `/v1/code`.
const CLIENT_CREDENTIAL_PREFIXES: [&str; 3] =
    ["/v1/code", "/api/oauth/files", "/api/oauth/file_upload"];

/// The CLIENT's own OAuth token refresh. Relayed raw — no auth header at all,
/// because a refresh carries its credentials in the BODY. The proxy manages its own
/// tokens via [`Manager::ensure_fresh`]; rewriting a client's refresh would inject
/// the wrong identity into an exchange that is not ours.
///
/// Matched by [`path_is_under`], like the prefixes above. It used to be an EXACT
/// compare, which let the trailing-slash spelling `/v1/oauth/token/` fall through to
/// the POOLED path — putting our Bearer on a client's token exchange, precisely what
/// the paragraph above says must never happen. Relaying a hypothetical sub-path with
/// no auth is the safe direction to be wrong in; a pooled Bearer is not.
const CLIENT_TOKEN_REFRESH_PATH: &str = "/v1/oauth/token";

/// `user-agent` sent by [`RelayMode::Raw`] when the client sent none. The JS proxy
/// defaults to `'node'` here; naming ourselves is more honest about who is calling.
const RELAY_USER_AGENT: &str = concat!("tcr/", env!("CARGO_PKG_VERSION"));

/// How a request that must NOT go through account rotation reaches upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelayMode {
    /// Forward the client's own headers — including its `authorization` — and
    /// stream the response back. See [`CLIENT_CREDENTIAL_PREFIXES`].
    ClientCredential,
    /// Send `content-type` / `accept` / `user-agent` and nothing else, no auth in
    /// any form. See [`CLIENT_TOKEN_REFRESH_PATH`].
    Raw,
}

/// Does `path` name `base` itself, or something beneath it?
///
/// The ONE rule every path-prefix decision in this module goes through, so that a
/// route's boundary is its segment boundary and nothing else. A bare `starts_with`
/// is wrong in both directions: without a terminator `/api/oauth/file_upload` also
/// swallows `/api/oauth/file_upload_v2`, and with one `/v1/code/` misses the bare
/// `/v1/code`. Callers must pass a `base` with NO trailing slash. Pure — unit-tested.
fn path_is_under(path: &str, base: &str) -> bool {
    path.strip_prefix(base)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Is `segment` a WHATWG dot segment — `.` or `..`, in any percent-encoded spelling?
///
/// The URL parser folds `%2e`/`%2E` to `.` BEFORE it classifies a segment, so `%2e%2e`
/// and `.%2e` are `..` to it. It does NOT decode `%2f`, so `..%2f` stays one opaque
/// segment and is not traversal — decoding it here would reject a legitimate path.
/// Only 1 or 2 dots are special: `...` is an ordinary name.
fn is_dot_segment(segment: &str) -> bool {
    let mut rest = segment;
    let mut dots = 0usize;
    while !rest.is_empty() {
        if let Some(tail) = rest.strip_prefix('.') {
            rest = tail;
        } else if rest.len() >= 3
            && rest.as_bytes()[0] == b'%'
            && rest.as_bytes()[1] == b'2'
            && rest.as_bytes()[2].eq_ignore_ascii_case(&b'e')
        {
            rest = &rest[3..];
        } else {
            return false;
        }
        dots += 1;
        if dots > 2 {
            return false;
        }
    }
    dots == 1 || dots == 2
}

/// Would this raw request path mean something DIFFERENT to the upstream URL parser
/// than it means to the classifiers here? If so it must never be routed at all.
///
/// The path this module makes every decision on is `uri.path()` — the raw request
/// target, verbatim. The path that goes ON THE WIRE is whatever `Url::parse` makes
/// of `upstream + path_and_query`, and that parser normalizes per WHATWG. Where the
/// two disagree, every routing decision is made about a path the upstream never sees.
/// Measured against this crate's own reqwest, `/v1/code/../../v1/messages` classified
/// as a client-credential RELAY and arrived at the upstream as `/v1/messages` — a real
/// inference bypassing `select`, the in-flight slot, the throttle, `record_served`,
/// session pinning and the whole retry ladder, carrying the CLIENT's token. `/x/../_tcr/status`
/// slipped the `/_tcr/` guard and reached the upstream WITH A POOLED BEARER on it,
/// which is the exact shape that once burned an account.
///
/// Reconciling the two representations is the fragile fix — it re-derives the
/// upstream's parser here and stays correct only while both agree forever. Instead
/// REJECT the disagreement: no legitimate Anthropic client emits a dot segment or a
/// backslash, so the ambiguous shapes cost nothing to refuse.
///
/// Two disagreements exist, both measured, not theorized:
/// - a **dot segment** (see [`is_dot_segment`]) — the parser collapses it away.
/// - a literal **backslash**, which is not a valid request-target character at all
///   (RFC 3986 `pchar`) and which WHATWG treats as a path SEPARATOR for http(s).
///   `/v1/code\foo` classified as pooled here and landed on the upstream as the
///   Remote Control path `/v1/code/foo` wearing a pooled Bearer — the under-inclusive
///   half of the same defect. `%5c` is NOT decoded by the parser, so it is left alone.
fn path_is_ambiguous(path: &str) -> bool {
    path.contains('\\') || path.split('/').any(is_dot_segment)
}

/// Classify a request as one of the rotation-bypassing relays, or `None` for the
/// normal pooled-credential forwarding path. Takes the path WITHOUT its query, so
/// a `?…` can never smuggle a path past the match. Pure — unit-tested.
///
/// Sound only on a path [`path_is_ambiguous`] has already rejected: on a path that
/// still contains a dot segment, what this classifies is not what goes on the wire.
fn relay_mode(method: &Method, path: &str) -> Option<RelayMode> {
    if *method == Method::POST && path_is_under(path, CLIENT_TOKEN_REFRESH_PATH) {
        return Some(RelayMode::Raw);
    }
    if CLIENT_CREDENTIAL_PREFIXES
        .iter()
        .any(|base| path_is_under(path, base))
    {
        return Some(RelayMode::ClientCredential);
    }
    None
}

/// Build the proxy router. Every method and path funnels through the single
/// catch-all [`handle`] — EXCEPT [`STATUS_PATH`], which the proxy answers itself.
/// The [`Manager`] is shared state.
pub fn app(manager: Arc<Manager>) -> Router {
    Router::new()
        // Registered as a real route so it is matched BEFORE the catch-all: a
        // status request must never reach `handle`, which would rewrite it to
        // Anthropic with a pooled OAuth Bearer attached.
        //
        // `.fallback` on the METHOD router pins the wrong-method answer to a local
        // 405. Measured on axum 0.8: a bare `get(...)` ALREADY answers 405 rather
        // than inheriting the Router's catch-all, so this is belt-and-braces, not
        // a fix for a live bug — it buys a consistent JSON error body and, more to
        // the point, pins the behaviour explicitly so a future axum whose method
        // routers inherit the outer fallback cannot silently turn a POST on this
        // path into an upstream-forwarded request carrying a pooled OAuth Bearer.
        .route(
            STATUS_PATH,
            axum::routing::get(status_handler).fallback(status_method_not_allowed),
        )
        .fallback(handle)
        .with_state(manager)
}

/// A non-`GET` on [`STATUS_PATH`]. Answered locally with 405 so the request is
/// neither forwarded upstream nor able to mutate anything — the endpoint is a
/// pure read, and there is no verb on it that is not.
async fn status_method_not_allowed() -> Response {
    error_response(
        StatusCode::METHOD_NOT_ALLOWED,
        "invalid_request_error",
        "The tcr status endpoint is read-only: GET.",
        None,
    )
}

/// `GET /_tcr/status` — the live fleet snapshot, for `tcr status`.
///
/// This is a **new attack surface on a process holding every account's OAuth
/// access and refresh token**, so it is gated twice and reads nothing but state
/// that is already on screen in the TUI:
///
/// 1. **Origin.** The peer must be loopback, proven by the [`ClientAddr`]
///    extension the hybrid listener injects from the real socket address (the
///    same extension the auth gate in [`handle`] uses). It is not a header, so a
///    client cannot forge it. Absent — a request that did not arrive through the
///    listener — we fail CLOSED. Bind scope is not authorization: `127.0.0.1` is
///    reachable by every process and every container on this host, so "we only
///    bind loopback" is not a claim about who is calling.
/// 2. **Key.** When a proxy api-key is configured it is REQUIRED here, with no
///    loopback exemption — deliberately stricter than [`handle`], which exempts
///    loopback because `claude` authenticates with its own OAuth and never sends
///    the proxy key. Nothing on this host needs to read the fleet's state without
///    the operator's secret, so reading it costs the same secret that using the
///    proxy does. The compare is [`key_matches`] (constant-time, length-safe).
///
/// It takes only [`Parts`](axum::http::request::Parts), so the request body is
/// never read; it touches no `&mut` state, triggers no probe, no token refresh
/// and no config write; and it returns the [`crate::status`] projection, which
/// carries no token, no key and no `Authorization` echo (see that module's
/// no-secret invariant).
async fn status_handler(
    State(manager): State<Arc<Manager>>,
    parts: axum::http::request::Parts,
) -> Response {
    let client_is_loopback = parts
        .extensions
        .get::<ClientAddr>()
        .is_some_and(|a| a.0.ip().is_loopback());
    if !client_is_loopback {
        return error_response(
            StatusCode::FORBIDDEN,
            "permission_error",
            "The tcr status endpoint is loopback-only.",
            None,
        );
    }

    if let Some(expected) = manager.proxy_api_key() {
        let provided = parts.headers.get("x-api-key").and_then(|v| v.to_str().ok());
        if !key_matches(provided, expected) {
            return error_response(
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                "Missing or invalid x-api-key.",
                None,
            );
        }
    }

    let now = OffsetDateTime::now_utc();
    let payload =
        crate::status::StatusPayload::from_snapshot(&manager.snapshot(now), &manager.thresholds());
    let Ok(body) = serde_json::to_string(&payload) else {
        // Serializing plain numbers and strings cannot realistically fail, but a
        // proxy never panics on a client request — surface it as a 500 instead.
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
            "Could not serialize the status snapshot.",
            None,
        );
    };
    let mut response = Response::new(Body::from(body));
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    // Operational state that is stale the instant it is read, from a
    // credential-holding process — never store it anywhere.
    headers.insert("cache-control", HeaderValue::from_static("no-store"));
    response
}

/// The catch-all proxy handler.
async fn handle(State(manager): State<Arc<Manager>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    // A loopback peer is exempt from the api-key gate (see below). Absent the
    // extension (e.g. a direct axum test harness) we treat the client as remote
    // and enforce — fail closed.
    let client_is_loopback = parts
        .extensions
        .get::<ClientAddr>()
        .is_some_and(|a| a.0.ip().is_loopback());
    let method = parts.method;
    let path_and_query = parts
        .uri
        .path_and_query()
        .map_or_else(|| "/".to_string(), |pq| pq.as_str().to_string());
    // The path WITHOUT its query — what the local-route and relay classifiers match
    // on. Matching them against `path_and_query` would let a query string decide
    // routing, and a path is what these rules are actually about.
    let path = parts.uri.path().to_string();
    let req_headers = parts.headers;

    // 1. Auth: when a proxy key is configured, `x-api-key` must match it — EXCEPT
    //    for loopback clients, which are exempt. A localhost personal proxy must
    //    not demand its own key from the local user: `claude` authenticates with
    //    its own OAuth (which we strip and replace with a pooled token) and never
    //    sends the proxy key. The JS proxy exempts loopback for exactly this
    //    reason. tcr binds 127.0.0.1 only, so every client is loopback in
    //    practice; the gate still guards a non-loopback client if bound wider.
    if let Some(expected) = manager.proxy_api_key() {
        if !client_is_loopback {
            let provided = req_headers.get("x-api-key").and_then(|v| v.to_str().ok());
            if !key_matches(provided, expected) {
                return error_response(
                    StatusCode::UNAUTHORIZED,
                    "authentication_error",
                    "Missing or invalid x-api-key.",
                    None,
                );
            }
        }
    }

    // 1a. Path shape. FIRST of the routing guards, because every one below it — the
    //     `/_tcr/` guard, the host guard, the relay classifier — decides on `path`,
    //     and this is the check that `path` still means upstream what it means here.
    //     `path` is the RAW request target; the upstream URL is parsed from it by
    //     reqwest, which collapses dot segments (and treats `\` as a separator) per
    //     WHATWG. A path where the two disagree is one where every decision below is
    //     made about a URL that never goes on the wire — see [`path_is_ambiguous`]
    //     for the two measured bypasses. Reject rather than reconcile: keeping two
    //     path representations in agreement is correct only while both parsers agree
    //     forever, and no legitimate Anthropic client emits either shape.
    //
    //     Placed AFTER the api-key gate, like the guards below it: a request that
    //     cannot authenticate learns nothing here it would not learn anywhere else.
    if path_is_ambiguous(&path) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Request path must not contain a dot segment or a backslash.",
            None,
        );
    }

    // 1b. Everything under `/_tcr/` is OURS. The router matches the registered
    //     routes (today: [`STATUS_PATH`]) BEFORE this catch-all, so any request that
    //     reaches `handle` under the prefix is one we do not serve — and the honest
    //     answer to that is a LOCAL 404, not a forward. Forwarding it would rewrite
    //     a proxy-private path onto api.anthropic.com WITH A POOLED OAUTH BEARER
    //     attached: that is how a typo'd status probe once put Gil's bearer on
    //     `api.anthropic.com/_tcr/status` and burned an account. [`path_is_under`]
    //     covers the bare `/_tcr` too, so the prefix has no unguarded edge.
    if path_is_under(&path, LOCAL_PREFIX) {
        return error_response(
            StatusCode::NOT_FOUND,
            "not_found_error",
            "Unknown tcr endpoint.",
            None,
        );
    }

    // 1c. Host guard: tcr is a credential-injecting reverse proxy for
    //     api.anthropic.com, NOT an open forward proxy. Every request is rewritten
    //     to `manager.upstream()` with the pooled OAuth Bearer, DISCARDING its
    //     target host. A plain-HTTP forward-proxy request aimed at a DIFFERENT host
    //     — e.g. an AWS SDK's IMDS probe to 169.254.169.254 that leaked in via a
    //     tool's `HTTP_PROXY` — would otherwise be silently misrouted to Anthropic
    //     (404-flooding the log/TUI, with Gil's Bearer attached). Reject such a
    //     misroute LOCALLY with 421: we neither rewrite it to Anthropic nor
    //     blind-forward it to its real host (that would reopen the SSRF/open-proxy
    //     surface the MITM CONNECT path deliberately closes via the same
    //     allowlist). Placed before body buffering / the rotation loop so a
    //     rejected misroute emits no per-request "serving"/"upstream" log line —
    //     silencing the flood, not relocating it.
    //
    //     Target host = the request-target's authority (absolute-form / forward-
    //     proxy requests carry it) else the `Host` header minus any `:port`. When
    //     BOTH are absent (an origin-form base-URL request with no Host, e.g. the
    //     direct-axum test harness) the guard is skipped — fail OPEN for the
    //     ambiguous local case, closed only for a host we can positively identify
    //     as neither loopback nor Anthropic.
    let target_host: Option<&str> = parts.uri.host().or_else(|| {
        req_headers
            .get(HOST)
            .and_then(|v| v.to_str().ok())
            .map(|h| h.rsplit_once(':').map_or(h, |(host, _)| host))
    });
    if let Some(host) = target_host {
        // Loopback (base-URL mode) reuses the `IpAddr::is_loopback` primitive the
        // client-peer check uses above; `localhost` is not an IP literal so it is
        // matched by name. `rsplit_once` mirrors the crate's CONNECT-target split.
        let is_loopback = host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
            || host == "localhost";
        if !is_loopback && !crate::mitm::host_allowed(host) {
            tracing::debug!(target_host = %host, "rejected misrouted forward-proxy request");
            return error_response(
                StatusCode::MISDIRECTED_REQUEST,
                "invalid_request_error",
                "This proxy only serves api.anthropic.com; refusing to forward a request for a different host.",
                None,
            );
        }
    }

    // 1d. Rotation-bypassing relays. A request bound to the CLIENT's identity
    //     ([`CLIENT_CREDENTIAL_PREFIXES`]) or to a credential exchange that is not
    //     ours ([`CLIENT_TOKEN_REFRESH_PATH`]) must not be re-credentialled, and it
    //     must not spend anything the rotation owns: no `select`, no in-flight slot,
    //     no throttle token, no `record_served`, no pin read or write. Every one of
    //     these used to burn a rotation slot and pollute the LRU key for a request
    //     the pooled account could never have answered.
    //
    //     Deliberately placed AFTER the api-key gate (1), the path-shape guard (1a)
    //     and the host guard (1c): relaying for an unauthenticated caller, on a path
    //     that means something else upstream, or to a host we just refused to forward
    //     to, would each be a new hole. Gate first, then relay.
    if let Some(mode) = relay_mode(&method, &path) {
        return relay_upstream(&manager, mode, method, &path_and_query, &req_headers, body).await;
    }

    // 2. Buffer the body once so it can be re-sent verbatim on every rotation.
    let Ok(body_bytes) = to_bytes(body, MAX_BODY_BYTES).await else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Failed to read request body.",
            None,
        );
    };

    // Parse the request's target model ONCE — it is constant across the rotation
    // loop, and drives per-model (Fable-aware) account selection below.
    let request_model = crate::model::parse_request_model(&body_bytes);
    // Whether THIS request targets a Fable model — the same classification
    // selection uses (`select.rs`). Threaded into every fleet-exhausted 429 so the
    // `retry-after` hint reflects the Fable-scoped weekly gate for a Fable request
    // and ignores it otherwise.
    let request_is_fable = request_model
        .as_deref()
        .is_some_and(crate::model::is_fable_model);

    // The session key pins this connection to one account (opt-in). The extension
    // is present iff session affinity is enabled, so `session_key` is `None` (LRU
    // rotation) by default. When on, key on the most STABLE client identity —
    // x-api-key, then body `metadata.user_id` — so a client that drops and
    // reconnects (new connection key) still maps to the SAME account and keeps
    // its per-account prompt cache warm. With NO stable identity the request routes
    // UNPINNED (plain LRU): a per-connection key is not a session key — it mints a
    // fresh pin per connection that nothing ever removes, and those ghosts (93% of
    // the live pin map) both bloat it and inflate the pinned-session counts that
    // drive the migration decision in `select`. `session_kind` records WHICH branch
    // produced the key (stable identity vs unpinned fallback) — DISPLAY provenance
    // only, threaded into `record_served`.
    let (session_key, session_kind) = match parts.extensions.get::<SessionKey>() {
        Some(_) => match stable_session_key(&req_headers, &body_bytes, manager.proxy_api_key()) {
            Some(key) => (Some(key), SessionKind::Stable),
            None => (None, SessionKind::Fallback),
        },
        None => (None, SessionKind::Fallback),
    };

    // 3. Selection + rotation loop.
    let account_count = manager.account_count();
    let http = manager.http_client();
    let mut tried: HashSet<usize> = HashSet::new();
    // Accounts that already got their one force-refresh on a 401.
    let mut forced_401: HashSet<usize> = HashSet::new();
    // Per-account inline-429 retry counters.
    let mut retried_429: HashMap<usize, u32> = HashMap::new();
    // Per-account in-place 529 retry counters. Separate from `retried_429`: a 429
    // is "this account is over quota" and may rotate, a 529 is "the upstream is
    // busy" and must not — sharing one counter would let one status spend the
    // other's budget.
    let mut retried_529: HashMap<usize, u32> = HashMap::new();
    // How many times THIS request has failed over to a DIFFERENT account on a 529.
    // Per-REQUEST, deliberately not per-account like the counter above: what needs
    // bounding is how much of the fleet one client request may walk, so it must not
    // reset when the account changes.
    let mut failovers_529 = 0u32;
    // Distinguishes "no account available" (429) from "every attempt hit a
    // transport failure" (502) once the loop can no longer make progress.
    //
    // These were ONE BOOL, and that was the bug: any single `send()` failure
    // latched it for the rest of the request, and the check sits BEFORE both the
    // soft-wait and the revalidation-serve — so one transport blip on one account
    // disabled the entire recovery ladder and returned a 502 whose message ("every
    // account transport-failed") was simply false. Observed live 09:16:46: the 502
    // fired while two accounts had already answered with honest 429s and a third
    // served a 200 1.6s later. Counting BOTH outcomes keeps the 502 for the only
    // case it actually describes — nothing ever reached an upstream at all.
    let mut transport_failures = 0usize;
    let mut upstream_responses = 0usize;
    // Bound the total attempts so per-account 401/429 retries can never loop.
    let max_attempts = max_attempts_for(account_count);
    // The account the NEXT iteration must use, bypassing select(). Two producers:
    //
    //  - A genuine SAME-account retry (401 force-refresh, transient-429 inline wait,
    //    529 in-place backoff), which select() would otherwise rotate AWAY from the
    //    very account the retry means to keep.
    //  - The 529 FAILOVER below, which parks the DIFFERENT account its availability
    //    probe already chose. That probe IS this iteration's selection: re-running
    //    select() next iteration would double its side effects (LRU stamp, divert
    //    log) and could race to a `None` after we already benched the 529'd account
    //    in `tried` — turning a forwardable 529 into a synthesized 429.
    let mut next_idx: Option<usize> = None;
    // One-shot guard: at most one transient-fleet-park soft-wait per client request.
    let mut soft_waited_exhaustion = false;
    // The subset of `tried` that is held out by a TRANSIENT 429 park and nothing
    // else — the only entries the soft-wait below is allowed to re-admit.
    //
    // `tried` alone cannot answer that question: it also carries accounts benched
    // by a 401, a transport blip, a dead token, and a durable quota rejection.
    // Only the transient park is a TIMER, so only it un-does itself when the clock
    // runs out; re-admitting any of the others would re-send this request to an
    // account that already failed it, in a loop. An account leaves this set the
    // moment it is re-admitted, so membership always means "currently parked".
    let mut parked_transient: HashSet<usize> = HashSet::new();

    for _ in 0..max_attempts {
        let now = OffsetDateTime::now_utc();
        let idx = match next_idx.take() {
            Some(i) => i,
            None => match manager.select(&tried, now, request_model.as_deref(), session_key) {
                Some(idx) => idx,
                None => {
                    if every_attempt_transport_failed(transport_failures, upstream_responses) {
                        return bad_gateway(transport_failures);
                    }
                    // A cold shared-limiter burst parks the whole fleet for ~15-20s.
                    // Rather than telling the client "all exhausted" for a transient
                    // blip, wait out the soonest un-park ONCE and retry select — the
                    // JS original's inline-wait-retry (L3). Real exhaustion (quota
                    // windows / long holds) has soonest_free >> the ceiling →
                    // soft_wait_secs returns None → fall through to the honest 429.
                    match soft_wait_secs(
                        manager.retry_after_hint(now, request_is_fable),
                        soft_waited_exhaustion,
                    ) {
                        Some(secs) => {
                            soft_waited_exhaustion = true;
                            // Desync concurrent waiters: without this, every request
                            // sleeping the same integer-ceil `secs` wakes on the SAME
                            // second and races for the single earliest-freeing account,
                            // re-synchronizing the herd NO_GUIDANCE_JITTER (:555) spreads
                            // and re-bursting that account. Waking `jitter` seconds later
                            // still serves (the account already freed at `secs`); worst
                            // case adds NO_GUIDANCE_JITTER_MAX_SECS (5s) → total ≤ 25s.
                            let jitter = (now.nanosecond() as i64
                                % (NO_GUIDANCE_JITTER_MAX_SECS + 1))
                                as u64;
                            let wait = secs + jitter;
                            tracing::info!(
                                wait_secs = wait,
                                "fleet transiently parked — soft-waiting once before exhausted"
                            );
                            tokio::time::sleep(Duration::from_secs(wait)).await;
                            // Re-admit exactly what the sleep was timed for.
                            // `Transient429::Park` benches an account TWICE — a
                            // hold via `mark_rate_limited` AND an entry in
                            // `tried` — and both `pick_eligible` and
                            // `pick_least_loaded` test `tried` BEFORE they ever
                            // evaluate eligibility. So clearing only the hold
                            // leaves the account this sleep was timed for still
                            // skipped: select() returns `None` a second time, the
                            // one-shot guard blocks another wait, and the wait
                            // buys nothing. Dropping the now-expired parks from
                            // `tried` is what makes the recovery path actually
                            // reach the account it was built to preserve.
                            //
                            // Precision matters more than reach here: an account
                            // whose hold is still running stays parked, and one
                            // benched for any NON-hold reason was never in
                            // `parked_transient` to begin with.
                            let now_ms = crate::now_ms();
                            let freed: Vec<usize> = parked_transient
                                .iter()
                                .copied()
                                .filter(|&i| manager.hold_expired(i, now_ms))
                                .collect();
                            for i in &freed {
                                tried.remove(i);
                                parked_transient.remove(i);
                            }
                            tracing::info!(
                                readmitted = freed.len(),
                                still_parked = parked_transient.len(),
                                "soft-wait over — re-admitted the accounts whose hold expired"
                            );
                            continue; // re-run select(); the re-admitted accounts are eligible again
                        }
                        None => {
                            // Not a transient park — the whole fleet reads over the
                            // SOFT switch threshold. Before synthesizing a 429, try a
                            // last-resort revalidation serve on the least-utilized
                            // account Anthropic still allows (default ON; disable via
                            // top-level `revalidationServe: false`). An upstream 200
                            // refreshes stale quota; a 429 arms a real hold via the
                            // existing inline-429 handling. `None` (throttled, or
                            // nothing servable) keeps the honest exhausted 429.
                            if manager.revalidation_serve_enabled() {
                                if let Some(idx) = manager.select_revalidation(
                                    &tried,
                                    now,
                                    request_model.as_deref(),
                                    session_key,
                                ) {
                                    idx
                                } else {
                                    return exhausted_response(
                                        &manager,
                                        now,
                                        account_count,
                                        request_is_fable,
                                    );
                                }
                            } else {
                                return exhausted_response(
                                    &manager,
                                    now,
                                    account_count,
                                    request_is_fable,
                                );
                            }
                        }
                    }
                }
            },
        };

        // RAII in-flight accounting for per-account pacing: bump this account's
        // concurrency count and stamp its last-served time NOW, releasing the slot
        // on EVERY exit of this iteration — rotate (`continue`), terminal `return`,
        // or panic-unwind. On rotation the old account's guard drops here and a
        // fresh one is taken for the new idx next iteration, so the counter tracks
        // true concurrent load and can never leak to strand an account. Inert when
        // pacing is unconfigured (the count is simply never read by `eligible`).
        let _in_flight = manager.enter_in_flight(idx);

        // Refresh the token if it is hard-expired (coalesced across concurrent
        // requests). If the refresh proved the credential dead, skip now.
        manager.ensure_fresh(idx).await;
        if manager.account_status(idx) == Some(AccountStatus::Error) {
            tried.insert(idx);
            continue;
        }
        let Some(token) = manager.access_token(idx) else {
            tried.insert(idx);
            continue;
        };

        // Build the upstream request: clone headers minus hop-by-hop / auth /
        // encoding, then set OUR bearer (the client's authorization was dropped).
        let url = format!("{}{}", manager.upstream(), path_and_query);
        let mut builder = http
            .request(method.clone(), &url)
            .headers(build_upstream_headers(&req_headers, &token));
        if method != Method::GET && method != Method::HEAD {
            // Rewrite the body's metadata.user_id.account_uuid to the serving
            // account's own UUID (rotation changes the account each attempt, so
            // this is recomputed from the pristine `body_bytes` per attempt). The
            // patch is same-length, so content-length is unaffected — and
            // `build_upstream_headers` strips content-length regardless. Absent a
            // configured 36-char UUID, the body is sent unchanged.
            // Forward the buffered `Bytes` handle by O(1) refcount clone on every
            // no-change path; only a genuine differing-UUID rewrite allocates (it
            // already owns a fresh Vec). `body_bytes` is immutable and re-read
            // pristine each rotation attempt, so cloning the handle is correct, and
            // reqwest's `Body: From<Bytes>` is O(1) — no full-body copy per request.
            let out_body: bytes::Bytes = match manager.account_uuid(idx) {
                Some(uuid) if uuid.len() == 36 => {
                    match crate::account_uuid::patch_account_uuid(&body_bytes, &uuid) {
                        std::borrow::Cow::Owned(v) => v.into(),
                        std::borrow::Cow::Borrowed(_) => body_bytes.clone(),
                    }
                }
                _ => body_bytes.clone(),
            };
            builder = builder.body(out_body);
        }

        // Fetch the serving account's name once per iteration — reused by the
        // transport-failure warning, the upstream-response line, and, on the
        // terminal path, by push_log (was two read-locks + clones back-to-back).
        // Hoisted ABOVE the send so the transport-failure arm can name the account
        // it failed on — that arm used to log nothing at any level.
        let account_name = manager.account_name(idx);

        // Global outbound throttle: pace the AGGREGATE egress so a cold fan-out
        // cannot burst the shared upstream limiter. Inert unless configured. Placed
        // after account selection/token so only real sends consume a slot; both the
        // 401 force-refresh retry and the transient-429 retry loop back here, so
        // every retry is paced automatically.
        manager.throttle_send().await;

        let resp = match builder.send().await {
            Ok(resp) => resp,
            Err(err) => {
                // Transport failure is not proof of a bad credential — fail this
                // request over to another account, keep this one eligible. The
                // `reqwest::Error` used to be discarded by a `let Ok(..) else`,
                // so a 502 assembled out of these failures had no line anywhere to
                // attribute it to; `is_connect` / `is_timeout` separate "never
                // reached the host" from "the host went quiet mid-request".
                transport_failures += 1;
                tracing::warn!(
                    account_index = idx,
                    account = account_name.as_deref().unwrap_or("?"),
                    is_connect = err.is_connect(),
                    is_timeout = err.is_timeout(),
                    error = %err,
                    "upstream transport failure — rotating to another account"
                );
                tried.insert(idx);
                continue;
            }
        };
        // This attempt reached an upstream and got an HTTP status back, whatever it
        // was. That fact alone disproves "every account transport-failed".
        upstream_responses += 1;

        let status = resp.status();
        // One greppable line per upstream response, tagged with the true serving
        // account, the PATH and the outcome status. "serving request" logs BEFORE the
        // outcome, so without this the logs are status-blind — the gap that hid the
        // 401 storm. The path is here because without it a status is undiagnosable:
        // the only other record of which path produced a 404 is the in-memory TUI
        // ring (`push_log` below), which is lost on every restart.
        tracing::info!(
            account_index = idx,
            account = account_name.as_deref().unwrap_or("?"),
            path = %path_and_query,
            status = status.as_u16(),
            "upstream response"
        );
        let up_headers = resp.headers().clone();
        manager.update_quota(idx, &up_headers);
        // Any non-429 is live proof a rate-limit hold no longer binds.
        if status.as_u16() != 429 {
            manager.clear_rate_limited(idx);
        }

        // 401 → force-refresh once, then retry the SAME account with the fresh
        // token. Neither arm may set the terminal `Error` status — see below.
        if status == StatusCode::UNAUTHORIZED {
            if forced_401.insert(idx) {
                if manager.ensure_fresh_force(idx).await {
                    next_idx = Some(idx);
                    continue; // retry the same account with the fresh token
                }
                // The forced refresh produced no new token — it was coalesced /
                // cooldown-suppressed by the refresh-storm throttle, or the upstream
                // refresh failed. Retrying would just 401 again on the same dead
                // token. Rotate away; the account stays Active and the cooldown
                // self-clears for the next request.
                tried.insert(idx);
                continue;
            }
            // A SECOND 401: the retry with a freshly-minted token ALSO 401'd. That is
            // almost always ROTATION CHURN — a concurrent request's force-refresh
            // superseded the very token we retried with — and is NOT evidence the
            // credential is dead. So it must never set `Error`.
            //
            // `Error` means "refresh token rejected, re-login needed" and is
            // deliberately TERMINAL: the prober (`probeable_indices`), selection
            // (`Manager::select`) and warming all skip errored rows by design, and the
            // only thing that clears it is a successful refresh — which can never run
            // on a row nobody probes or selects. So condemning a HEALTHY account here
            // is permanent: left unchecked it walks every account to `error` one
            // transient 401 at a time until the proxy goes dark holding a full set of
            // perfectly good tokens (observed live 2026-07-17: 7/7 `error` while every
            // token still probed 200).
            //
            // A genuinely dead credential is still caught at the source: its refresh
            // 400s and the AuthRejected arm sets `Error` there. Here we just rotate —
            // this request fails over and the account stays eligible for the next one.
            tried.insert(idx);
            continue;
        }

        // 429 → durable quota rejection rotates; a transient limit waits/rotates.
        if status == StatusCode::TOO_MANY_REQUESTS {
            let header_str = |name: &str| {
                up_headers
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("<absent>")
            };
            tracing::info!(
                account = account_name.as_deref().unwrap_or("?"),
                account_index = idx,
                retry_after_raw = header_str("retry-after"),
                retry_after_parsed = format!("{:?}", parse_retry_after(&up_headers)),
                unified_status = header_str("anthropic-ratelimit-unified-status"),
                // The MODEL-SCOPED twin of `unified_status`. Logged to make one
                // specific class observable: a Fable-weekly-only rejection that
                // `is_quota_rejected` reads as account-wide, arming a 3600s hold that
                // then re-keys the session. If this ever reads `rejected` while
                // `unified_status` does not, that is the case — diagnostic only, no
                // routing reads this.
                unified_7d_oi_status = header_str("anthropic-ratelimit-unified-7d_oi-status"),
                unified_5h_reset = header_str("anthropic-ratelimit-unified-5h-reset"),
                unified_7d_reset = header_str("anthropic-ratelimit-unified-7d-reset"),
                quota_rejected = is_quota_rejected(&up_headers),
                "429 diagnostic"
            );
            let retry_after_raw = parse_retry_after(&up_headers);
            let retry_after = retry_after_raw.unwrap_or(60);
            if is_quota_rejected(&up_headers) {
                manager.mark_rate_limited(idx, jittered_quota_hold(retry_after, now.nanosecond()));
                tried.insert(idx);
                continue;
            }
            let count = retried_429.entry(idx).or_insert(0);
            // Desync the no-guidance un-park across accounts that tripped together;
            // derive the jitter from the loop clock so no `rand` dependency is needed.
            let jitter = (now.nanosecond() as i64) % (NO_GUIDANCE_JITTER_MAX_SECS + 1);
            match classify_transient_429(retry_after_raw, *count, jitter) {
                Transient429::InlineWait(wait) => {
                    *count += 1;
                    tokio::time::sleep(Duration::from_secs(wait as u64)).await;
                    next_idx = Some(idx);
                    continue; // retry the same account after the bounded wait
                }
                Transient429::Park(wait) => {
                    manager.mark_rate_limited(idx, wait);
                    tried.insert(idx);
                    // Record WHY this entry is in `tried`: a timer that expires on
                    // its own, so the soft-wait above may take it back. Every other
                    // `tried.insert` in this loop deliberately omits this.
                    parked_transient.insert(idx);
                    continue;
                }
            }
        }

        // 529 Overloaded → this SEND was refused, and the ladder below spends two
        // budgets before the client ever sees it. Measured live 2026-07-26: 256 of
        // the last 4,963 upstream responses were 529 (5.2%), every one of them
        // surfaced in Claude Code as a failed request the user re-sent by hand.
        //
        // FIRST the SAME account, after a short escalating backoff: its prompt cache
        // is warm and a brief overload usually clears within a second or two.
        //
        // THEN — and this is what the arm originally refused to do — a bounded
        // FAILOVER to a different account. The original premise was that a 529 means
        // the SERVER is saturated, so a sibling would be equally overloaded and the
        // rotation would buy a cold prompt cache for nothing. The live log falsifies
        // it: the overload is ACCOUNT-scoped and time-varying (one account answered
        // 136 529s over 8 minutes while its siblings served 200s; a session diverted
        // off it got a 200 from a sibling 2s later). See
        // [`MAX_529_FAILOVERS_PER_REQUEST`].
        //
        // The failover mechanism is exactly one line — `tried.insert(idx)` — because
        // `select` already has the branch for it: a PINNED session whose pin is in
        // `tried` takes the documented "pin-tried" path, which diverts THIS REQUEST
        // and KEEPS THE PIN (`src/manager/select.rs`). So the session comes home to
        // its warm account on its next request; nothing re-keys, and `tried` is
        // per-request so the 529'd account is fully eligible again immediately.
        // `mark_rate_limited` is still NEVER called here — an overloaded account is
        // not over quota, and arming a hold would bench it for OTHER requests.
        //
        // The failover is gated on `select` actually offering another account, and
        // that gate is load-bearing rather than defensive: benching the account
        // FIRST and discovering the fleet is empty AFTERWARDS would drop this
        // request into the exhausted path and answer a synthesized 429 — replacing
        // the upstream's own 529 with a status that means something else entirely.
        // With no account to move to, the arm degrades exactly into its pre-failover
        // self. (Under a hard account lock `select` returns `None` for a tried
        // account by construction, so the lock's "no failover to the pool" contract
        // holds here for free.)
        //
        // Latency: the in-flight guard taken at the top of this iteration is HELD
        // across every backoff, so a retrying request keeps its per-account
        // concurrency slot. Per account that is MAX_SAME_ACCOUNT_529_RETRIES waits
        // each clamped to RETRY_529_MAX_BACKOFF_SECS (3s on the no-`retry-after`
        // ladder the live captures actually carry, 8s if the upstream asks for
        // more); per REQUEST it is that times `1 + MAX_529_FAILOVERS_PER_REQUEST`,
        // i.e. 9s and 24s. Both are asserted, not assumed —
        // `overloaded_529_failover_worst_case_latency_is_bounded`.
        //
        // On BOTH budgets being spent this does NOT return. Control falls through to
        // the terminal-outcome arm below, so the client sees the 529 forwarded
        // verbatim — byte-identical to the behaviour before this arm existed.
        if status.as_u16() == STATUS_OVERLOADED {
            let retried = retried_529.entry(idx).or_insert(0);
            if *retried < MAX_SAME_ACCOUNT_529_RETRIES {
                let backoff = backoff_529_secs(*retried, parse_retry_after(&up_headers));
                *retried += 1;
                tracing::warn!(
                    account = account_name.as_deref().unwrap_or("?"),
                    account_index = idx,
                    retry = *retried,
                    max_retries = MAX_SAME_ACCOUNT_529_RETRIES,
                    backoff_secs = backoff,
                    "upstream 529 Overloaded — backing off and retrying the SAME account"
                );
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                next_idx = Some(idx);
                continue; // retry the same account after the bounded backoff
            }
            // This account's in-place budget is spent. Fail the request over while
            // the per-request failover budget lasts AND another account is free.
            let attempts = *retried + 1;
            if failovers_529 < MAX_529_FAILOVERS_PER_REQUEST {
                // Ask for the replacement BEFORE benching this account, and pass the
                // `tried` set the next iteration WOULD have passed. A fresh clock,
                // not the loop's `now`: the backoffs above have made that value up to
                // several seconds stale, and an account whose hold expired during
                // them is a legitimate target.
                let mut with_this_one = tried.clone();
                with_this_one.insert(idx);
                let failover_now = OffsetDateTime::now_utc();
                if let Some(other) = manager.select(
                    &with_this_one,
                    failover_now,
                    request_model.as_deref(),
                    session_key,
                ) {
                    debug_assert_ne!(
                        other, idx,
                        "select must honour `tried`, or the failover re-sends to the overloaded account"
                    );
                    tried.insert(idx);
                    failovers_529 += 1;
                    tracing::warn!(
                        account = account_name.as_deref().unwrap_or("?"),
                        account_index = idx,
                        attempts,
                        failover = failovers_529,
                        max_failovers = MAX_529_FAILOVERS_PER_REQUEST,
                        next_account = manager.account_name(other).as_deref().unwrap_or("?"),
                        next_account_index = other,
                        "upstream 529 Overloaded — in-place budget spent, failing this request over to another account"
                    );
                    // The probe above IS this failover's selection; consume it rather
                    // than re-selecting (see `next_idx`).
                    next_idx = Some(other);
                    continue;
                }
            }
            tracing::warn!(
                account = account_name.as_deref().unwrap_or("?"),
                account_index = idx,
                attempts,
                // Distinguishes the two give-up shapes in the log: 0 means nothing
                // else was eligible to fail over to, `MAX_529_FAILOVERS_PER_REQUEST`
                // means the fleet was walked and every account was overloaded too.
                failovers = failovers_529,
                "upstream 529 Overloaded — retry and failover budgets spent, forwarding 529 to the client"
            );
        }

        // Terminal outcome (2xx, or a 3xx/4xx/5xx forwarded verbatim). Count the
        // request once against the true serving account and log it.
        let served_at = OffsetDateTime::now_utc();
        manager.record_served(idx, served_at, session_key, session_kind);
        manager.push_log(RequestLogEntry {
            time: served_at,
            method: method.to_string(),
            path: path_and_query.clone(),
            status: status.as_u16(),
            account: account_name.unwrap_or_default(),
        });

        let is_stream = up_headers
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.contains("text/event-stream"));

        if is_stream {
            // Tee: the passthrough to the client is NEVER gated on the parser.
            // Each chunk is offered to a BOUNDED side channel (`SSE_TEE_CAPACITY`)
            // that a spawned task drains through eventsource-stream (which
            // reassembles events split across chunks). The bound stops a fast
            // upstream + slow parser from retaining the whole response in the
            // channel; `try_send` on the passthrough side keeps that bound from
            // ever applying backpressure to the client — see below.
            let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(SSE_TEE_CAPACITY);
            let manager_side = manager.clone();
            tokio::spawn(async move {
                let byte_stream = futures::stream::unfold(rx, |mut rx| async move {
                    rx.recv()
                        .await
                        .map(|bytes| (Ok::<Bytes, Infallible>(bytes), rx))
                });
                let (parsed, stream_error) = parse_sse_usage(byte_stream).await;
                if parsed.input_total > 0 || parsed.output > 0 {
                    manager_side.update_usage(
                        idx,
                        parsed.input_total,
                        parsed.output,
                        parsed.cache_read,
                        parsed.cache_creation,
                    );
                }
                // Sibling of the usage guard above, not nested in it: an error
                // event with NO message_start leaves input_total == output == 0,
                // and this must still be recorded — that is precisely the bug
                // (a truncated stream booked as a clean serve). Called ONCE per
                // stream, after the parse loop ends, never per event — a
                // per-event call on a long erroring stream is a log-flood vector.
                if let Some(kind) = stream_error {
                    manager_side.record_stream_error(idx, &kind);
                }
            });
            let passthrough = resp.bytes_stream().map(move |chunk| {
                // Anchor the in-flight guard to THIS streamed body. Returning
                // `Body::from_stream(..)` drops every handler-local, but axum polls
                // the body AFTER the handler returns — so a handler-local guard would
                // decrement `in_flight` at response-headers while the stream keeps
                // flowing for seconds. Moving the OWNED guard into this `move` closure
                // makes the closure (hence the stream body) own it, so its Drop — the
                // account's `in_flight` decrement — fires at stream completion / client
                // disconnect / body drop, which is when pacing must count the load. The
                // `&` reference forces edition-2021 precise capture to take the guard by
                // value; an unmentioned capture would be elided and dropped early.
                let _anchor = &_in_flight;
                if let Ok(bytes) = &chunk {
                    // Best-effort tee: `try_send` never blocks the passthrough. A
                    // full channel (slow/starved parser) or a dropped receiver
                    // drops the chunk for the PARSER only — usage counting becomes
                    // best-effort under backpressure; the client stream forwards
                    // untouched. Intentional discard, hence `let _`.
                    let _ = tx.try_send(bytes.clone());
                }
                chunk
            });
            return build_response(
                status,
                &up_headers,
                Body::from_stream(passthrough),
                ServedBy::PooledAccount,
            );
        }

        // Non-stream body: buffer it (capped like the request path — an unbounded
        // `resp.bytes().await` is the same DoS surface `MAX_BODY_BYTES` guards on
        // the way in), parse usage from the JSON, forward it.
        let bytes = match read_capped_body(resp.bytes_stream(), MAX_BODY_BYTES).await {
            Ok(bytes) => bytes,
            Err(BodyReadError::Transport) => {
                return error_response(
                    StatusCode::BAD_GATEWAY,
                    "proxy_error",
                    "Failed to read upstream response body.",
                    None,
                );
            }
            Err(BodyReadError::TooLarge) => {
                return error_response(
                    StatusCode::BAD_GATEWAY,
                    "proxy_error",
                    "Upstream response body exceeded the size cap.",
                    None,
                );
            }
        };
        if status.is_success() {
            let parsed = usage_from_json(&bytes);
            if parsed.input_total > 0 || parsed.output > 0 {
                manager.update_usage(
                    idx,
                    parsed.input_total,
                    parsed.output,
                    parsed.cache_read,
                    parsed.cache_creation,
                );
            }
        }
        return build_response(
            status,
            &up_headers,
            Body::from(bytes),
            ServedBy::PooledAccount,
        );
    }

    // Ran out of attempts while still rotating — a bad gateway only if transport
    // failure was the WHOLE story (same rule as the mid-loop check above);
    // otherwise an upstream did answer us and the honest verdict is exhausted quota.
    if every_attempt_transport_failed(transport_failures, upstream_responses) {
        bad_gateway(transport_failures)
    } else {
        exhausted_response(
            &manager,
            OffsetDateTime::now_utc(),
            account_count,
            request_is_fable,
        )
    }
}

/// Length-independent comparison of the presented key against the configured
/// one. It never returns early on a length mismatch, so the loop count depends
/// only on the *presented* key's length (attacker-controlled) and never reveals
/// the configured key's length via timing. The length delta is folded into the
/// accumulator so a matching-prefix but wrong-length key still compares unequal.
fn key_matches(provided: Option<&str>, expected: &str) -> bool {
    let Some(provided) = provided else {
        return false;
    };
    let (a, b) = (provided.as_bytes(), expected.as_bytes());
    // Seed the accumulator with the length mismatch instead of branching on it
    // (a bare `as u8` truncation would alias e.g. 256 vs 0, so reduce to 0/1).
    let mut diff: u8 = (a.len() != b.len()) as u8;
    for (i, &x) in a.iter().enumerate() {
        // Read past the configured key's end as 0 rather than short-circuiting;
        // the length delta above already forces a non-match in that case.
        let y = b.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    diff == 0
}

/// Why [`read_capped_body`] stopped short of a full buffer.
enum BodyReadError {
    /// The stream yielded a transport error before completing.
    Transport,
    /// The accumulated body would exceed the byte cap.
    TooLarge,
}

/// Buffer a byte stream into `Bytes`, aborting once the accumulated size would
/// exceed `cap`. Mirrors the request-path [`MAX_BODY_BYTES`] cap on the response
/// path: `resp.bytes().await` reads the whole upstream body with no bound, the
/// same DoS surface the inbound buffer guards against. A body whose total equals
/// the cap is accepted; only a strictly larger one is rejected.
async fn read_capped_body<S, E>(stream: S, cap: usize) -> Result<Bytes, BodyReadError>
where
    S: futures::Stream<Item = Result<Bytes, E>>,
{
    futures::pin_mut!(stream);
    let mut collected: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| BodyReadError::Transport)?;
        if collected.len().saturating_add(chunk.len()) > cap {
            return Err(BodyReadError::TooLarge);
        }
        collected.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(collected))
}

/// Clone request headers for the upstream call, dropping hop-by-hop headers,
/// the client's `x-api-key`, its inbound `authorization` (behaviour #2), and
/// `accept-encoding` (reqwest has no decompressor here, and a compressed body
/// would defeat SSE usage parsing). Sets our own `Bearer`.
fn build_upstream_headers(req_headers: &HeaderMap, token: &str) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in req_headers.iter() {
        let lower = name.as_str();
        if lower.starts_with(':')
            || is_request_hop_by_hop(lower)
            || lower == "x-api-key"
            || lower == "authorization"
            || lower == "accept-encoding"
        {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    if let Ok(value) = HeaderValue::from_str(&format!("Bearer {token}")) {
        out.insert(AUTHORIZATION, value);
    }
    out
}

/// Clone the client's request headers for a [`RelayMode::ClientCredential`] call:
/// drop pseudo-headers, hop-by-hop and `accept-encoding`, KEEP everything else —
/// crucially the client's own `authorization`, which is the entire point of this
/// path. Contrast [`build_upstream_headers`], which strips exactly that header to
/// make room for the pooled Bearer.
///
/// The one header removed beyond that set is an `x-api-key` whose value equals the
/// configured proxy key: that is OUR gate credential, not the client's upstream
/// credential, and forwarding it hands the operator's secret to a third party. A
/// client `x-api-key` that is NOT our key is the client's own and passes through.
fn build_passthrough_headers(req_headers: &HeaderMap, proxy_key: Option<&str>) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in req_headers.iter() {
        let lower = name.as_str();
        if lower.starts_with(':') || is_request_hop_by_hop(lower) || lower == "accept-encoding" {
            continue;
        }
        if lower == "x-api-key"
            && proxy_key.is_some_and(|expected| key_matches(value.to_str().ok(), expected))
        {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

/// The three headers a [`RelayMode::Raw`] relay carries — `content-type`, `accept`,
/// `user-agent` — and nothing else. No `authorization` and no `x-api-key`: a token
/// refresh authenticates with the credentials in its BODY, so a header credential
/// would be at best redundant and, for a pooled Bearer, the wrong identity's
/// entirely. Mirrors the JS proxy's `relayRaw`.
fn build_raw_relay_headers(req_headers: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, default) in [
        (CONTENT_TYPE, "application/json"),
        (ACCEPT, "application/json"),
        (USER_AGENT, RELAY_USER_AGENT),
    ] {
        let value = req_headers
            .get(&name)
            .cloned()
            .unwrap_or_else(|| HeaderValue::from_static(default));
        out.insert(name, value);
    }
    out
}

/// The HTTP client [`relay_upstream`] uses, built once and reused.
///
/// Deliberately NOT [`Manager::http_client`]: the pooled client follows up to 10
/// redirects, and a relay must hand a 3xx BACK to the client rather than chase it
/// (the JS proxy passes `redirect: 'manual'` for this reason). Following one here
/// would resolve a location the client never sees, while carrying a credential we
/// are only forwarding on its behalf. The rest mirrors the pooled client: `no_proxy`
/// (we ARE the proxy — an ambient `HTTP_PROXY` would loop us through ourselves) and
/// a CONNECT-phase-only timeout, never a total one, because a `/v1/code/` response
/// is a long-lived event stream that any total timeout would truncate mid-stream.
///
/// `None` only if the TLS backend fails to initialise, which the caller answers with
/// a 502 — a proxy never panics on a client request.
fn relay_client() -> Option<&'static reqwest::Client> {
    static RELAY_CLIENT: std::sync::OnceLock<Option<reqwest::Client>> = std::sync::OnceLock::new();
    RELAY_CLIENT
        .get_or_init(|| {
            match reqwest::Client::builder()
                .no_proxy()
                .connect_timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .build()
            {
                Ok(client) => Some(client),
                Err(err) => {
                    tracing::error!(error = %err, "could not build the relay HTTP client");
                    None
                }
            }
        })
        .as_ref()
}

/// Forward a request upstream WITHOUT touching account rotation — see the `1c`
/// block in [`handle`] for which requests land here and why.
///
/// `mode` picks the header policy ([`build_passthrough_headers`] vs
/// [`build_raw_relay_headers`]) and the body policy: a client-credential response is
/// STREAMED (`/v1/code/` is an event stream that must not be buffered before the
/// client sees it), a raw response is buffered under the same cap as every other
/// body this file reads. Response headers go through the shared [`build_response`],
/// which drops the framing headers a relayed body would mis-frame with.
async fn relay_upstream(
    manager: &Manager,
    mode: RelayMode,
    method: Method,
    path_and_query: &str,
    req_headers: &HeaderMap,
    body: Body,
) -> Response {
    let Some(client) = relay_client() else {
        return error_response(
            StatusCode::BAD_GATEWAY,
            "proxy_error",
            "Could not build the relay HTTP client.",
            None,
        );
    };
    let Ok(body_bytes) = to_bytes(body, MAX_BODY_BYTES).await else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Failed to read request body.",
            None,
        );
    };

    let headers = match mode {
        RelayMode::ClientCredential => {
            build_passthrough_headers(req_headers, manager.proxy_api_key())
        }
        RelayMode::Raw => build_raw_relay_headers(req_headers),
    };
    let url = format!("{}{}", manager.upstream(), path_and_query);
    let mut builder = client.request(method.clone(), &url).headers(headers);
    // A GET/HEAD carries no body, and an empty body is sent as no body at all —
    // both mirror the JS relays, and the latter keeps a bodyless POST from
    // acquiring a `content-length: 0` it did not arrive with.
    if method != Method::GET && method != Method::HEAD && !body_bytes.is_empty() {
        builder = builder.body(body_bytes);
    }

    let resp = match builder.send().await {
        Ok(resp) => resp,
        Err(err) => {
            tracing::warn!(
                path = %path_and_query,
                mode = ?mode,
                is_connect = err.is_connect(),
                is_timeout = err.is_timeout(),
                error = %err,
                "relay transport failure"
            );
            return error_response(
                StatusCode::BAD_GATEWAY,
                "proxy_error",
                "Upstream unreachable.",
                None,
            );
        }
    };

    let status = resp.status();
    let up_headers = resp.headers().clone();
    // The relay's twin of the "upstream response" line. These requests are invisible
    // to the per-account log by construction (they serve no account), so this line is
    // the ONLY record that one happened and what it answered.
    tracing::info!(
        path = %path_and_query,
        mode = ?mode,
        status = status.as_u16(),
        "relayed response"
    );

    match mode {
        // Both arms are served with the CALLER's own credential (or none at all),
        // so their rate-limit and org headers describe the caller — coherent, and
        // theirs to see. Only the rotated path lies; see [`ServedBy`].
        RelayMode::ClientCredential => build_response(
            status,
            &up_headers,
            Body::from_stream(resp.bytes_stream()),
            ServedBy::Caller,
        ),
        RelayMode::Raw => match read_capped_body(resp.bytes_stream(), MAX_BODY_BYTES).await {
            Ok(bytes) => build_response(status, &up_headers, Body::from(bytes), ServedBy::Caller),
            Err(BodyReadError::Transport) => error_response(
                StatusCode::BAD_GATEWAY,
                "proxy_error",
                "Failed to read upstream response body.",
                None,
            ),
            Err(BodyReadError::TooLarge) => error_response(
                StatusCode::BAD_GATEWAY,
                "proxy_error",
                "Upstream response body exceeded the size cap.",
                None,
            ),
        },
    }
}

/// Hop-by-hop request headers that must not be forwarded. Header names from a
/// [`HeaderMap`] are already lowercased.
fn is_request_hop_by_hop(name: &str) -> bool {
    matches!(
        name,
        "host"
            | "connection"
            | "keep-alive"
            | "transfer-encoding"
            | "te"
            | "trailer"
            | "trailers"
            | "upgrade"
            | "proxy-authorization"
            | "proxy-authenticate"
            | "content-length"
    )
}

/// Response headers that are connection-specific / would mis-frame the relayed
/// body and so are stripped from the response we return to the client.
fn is_response_skip(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "transfer-encoding"
            | "upgrade"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "trailers"
            | "content-length"
            | "content-encoding"
    )
}

/// Upstream headers that describe the ACCOUNT that served a request rather than
/// the request itself, and so are only meaningful to a client that owns that
/// account.
///
/// Matched by PREFIX, deliberately: the `anthropic-ratelimit-` family has more
/// members than the `unified-*` set this proxy reads (`requests-*`, `tokens-*`,
/// `input-tokens-*`, `output-tokens-*`, …), and an enumeration would leak
/// whichever one Anthropic adds next. Header names from a [`HeaderMap`] are
/// already lowercased.
///
/// `request-id` is deliberately absent: it identifies a REQUEST, not an account,
/// and it is the one id that makes a single failed call debuggable end-to-end.
fn is_account_scoped(name: &str) -> bool {
    name.starts_with("anthropic-ratelimit-") || name == "anthropic-organization-id"
}

/// Whose account produced the response being assembled — the one thing that
/// decides whether its per-account headers mean anything to the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServedBy {
    /// A pooled account picked by rotation. Its quota headers describe an account
    /// the caller has never heard of and that CHANGES per request, so
    /// [`is_account_scoped`] strips them on the way out — see [`build_response`].
    PooledAccount,
    /// The caller's own credential ([`RelayMode`] paths, which bypass rotation
    /// entirely). Those headers describe the caller's own account, so they are
    /// coherent and pass through untouched.
    Caller,
}

/// Assemble the client response: the upstream status + body, carrying every
/// upstream header except the connection-specific / framing ones — and, on the
/// rotated path, except the ones that belong to the serving ACCOUNT.
///
/// The account strip exists because Claude Code renders its usage UI straight
/// from `anthropic-ratelimit-unified-*`. Rotation means consecutive requests are
/// answered by different accounts, so forwarding those headers hands the client a
/// different quota picture every time: a request that lands on an account at 1.00
/// weekly renders a usage-limit banner, the next one lands elsewhere and
/// contradicts it. None of those numbers describe the pool the client is actually
/// talking to. `anthropic-organization-id` is stripped with them — it leaks a
/// distinct org identity per pooled account.
///
/// This is the CLIENT boundary, and the strip belongs here and nowhere earlier:
/// the proxy's own quota model is built from these same headers upstream of this
/// call — `manager.update_quota` and the `is_quota_rejected` gate both read the
/// untouched `up_headers`. Stripping at ingest would blind the rotation logic
/// while looking like it fixed something.
fn build_response(
    status: StatusCode,
    up_headers: &HeaderMap,
    body: Body,
    served_by: ServedBy,
) -> Response {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    let headers = response.headers_mut();
    for (name, value) in up_headers.iter() {
        if is_response_skip(name.as_str()) {
            continue;
        }
        if served_by == ServedBy::PooledAccount && is_account_scoped(name.as_str()) {
            continue;
        }
        headers.append(name.clone(), value.clone());
    }
    response
}

/// Parse a numeric `retry-after` header (seconds). RFC-date form is ignored (the
/// caller falls back to a default), matching the JS proxy.
fn parse_retry_after(headers: &HeaderMap) -> Option<i64> {
    headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<i64>().ok())
}

/// Is any unified rate-limit window reporting `rejected` (durable exhaustion)?
fn is_quota_rejected(headers: &HeaderMap) -> bool {
    let rejected = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|s| s.eq_ignore_ascii_case("rejected"))
    };
    rejected("anthropic-ratelimit-unified-status")
        || rejected("anthropic-ratelimit-unified-5h-status")
        || rejected("anthropic-ratelimit-unified-7d-status")
        || rejected("anthropic-ratelimit-unified-7d_oi-status")
}

/// Sum the input side of a `usage` object: base input plus cache-creation and
/// cache-read tokens (the JS proxy missed the cache tokens — behaviour fixed).
/// This is the QUOTA counter — `input_total` folds all three into one u64.
fn sum_input_tokens(usage: &Value) -> u64 {
    let field = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    field("input_tokens") + field("cache_creation_input_tokens") + field("cache_read_input_tokens")
}

/// The parsed token breakdown of a response. `input_total` keeps
/// [`sum_input_tokens`] semantics byte-for-byte (the quota counter — bug #4),
/// while `cache_read` / `cache_creation` are ALSO surfaced separately so an
/// operator can see prompt-cache warmth (`cache_read > 0` on post-first turns).
/// A single struct — rather than a 4-tuple — keeps the call sites legible.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ParsedUsage {
    input_total: u64,
    output: u64,
    cache_read: u64,
    cache_creation: u64,
}

/// Extract the cache-read + cache-creation components of a `usage` object.
/// R2: both parse paths (JSON and SSE) call THIS, so the two cache fields are
/// extracted identically and cache% can never depend on stream-mode.
fn cache_breakdown(usage: &Value) -> (u64, u64) {
    let field = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    (
        field("cache_read_input_tokens"),
        field("cache_creation_input_tokens"),
    )
}

/// Parse the token breakdown from a non-streamed JSON messages body.
fn usage_from_json(bytes: &[u8]) -> ParsedUsage {
    let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
        return ParsedUsage::default();
    };
    let Some(usage) = value.get("usage") else {
        return ParsedUsage::default();
    };
    let output = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let (cache_read, cache_creation) = cache_breakdown(usage);
    ParsedUsage {
        input_total: sum_input_tokens(usage),
        output,
        cache_read,
        cache_creation,
    }
}

/// Parse the total usage breakdown from an SSE messages stream, plus — out of
/// band, so [`ParsedUsage`] can stay `Copy` — the FIRST in-band `error` event's
/// `error.type`, if any arrived.
///
/// `input_total` is taken from `message_start` (base + cache tokens); `output`
/// is the latest cumulative count from `message_delta` (or `message_start` if
/// no delta arrives). Returning the totals — rather than incrementing per event
/// — is what makes the count applied exactly once, never doubled.
/// eventsource-stream reassembles events split across network chunks, so a
/// boundary-split `message_start` is still parsed whole (bug #1 designed out).
/// The cache components come from the same `message_start` usage via the shared
/// [`cache_breakdown`] (R2: identical extraction to the JSON path).
///
/// An Anthropic error envelope can arrive INSIDE a 200 `text/event-stream` body
/// (the same shape this proxy itself synthesizes — see [`error_response`] used
/// as a template, and the `event: error` fixture below) — that is a truncated,
/// failed turn, not usage to add to the quota. First `error` event wins; a
/// second is ignored, so multi-event streams still name their root cause.
async fn parse_sse_usage<S, B, E>(stream: S) -> (ParsedUsage, Option<String>)
where
    S: futures::Stream<Item = Result<B, E>>,
    B: AsRef<[u8]>,
{
    let events = stream.eventsource();
    futures::pin_mut!(events);

    let mut parsed = ParsedUsage::default();
    let mut stream_error: Option<String> = None;
    while let Some(item) = events.next().await {
        let Ok(event) = item else {
            break; // malformed/utf8/transport error — stop parsing, keep totals
        };
        if event.data.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&event.data) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                if let Some(usage) = value.get("message").and_then(|m| m.get("usage")) {
                    parsed.input_total = sum_input_tokens(usage);
                    (parsed.cache_read, parsed.cache_creation) = cache_breakdown(usage);
                    if let Some(out) = usage.get("output_tokens").and_then(Value::as_u64) {
                        parsed.output = out;
                    }
                }
            }
            Some("message_delta") => {
                if let Some(out) = value
                    .get("usage")
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(Value::as_u64)
                {
                    parsed.output = out;
                }
            }
            // First `error` event wins — a later one is ignored (guard skips the
            // arm rather than nesting an `if` inside it, per clippy).
            Some("error") if stream_error.is_none() => {
                let kind = value
                    .get("error")
                    .and_then(|e| e.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                stream_error = Some(kind);
            }
            _ => {}
        }
    }
    (parsed, stream_error)
}

/// A JSON error response in Anthropic's error envelope.
fn error_response(
    status: StatusCode,
    err_type: &str,
    message: &str,
    retry_after: Option<i64>,
) -> Response {
    let payload = serde_json::json!({
        "type": "error",
        "error": { "type": err_type, "message": message },
    });
    let mut response = Response::new(Body::from(payload.to_string()));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if let Some(seconds) = retry_after {
        if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
            response.headers_mut().insert("retry-after", value);
        }
    }
    response
}

/// 429 with a fleet-wide `retry-after` hint when no account is currently usable.
/// `is_fable` scopes the hint to the request's model class so a Fable request is
/// told the true Fable-weekly recovery instant (see [`Manager::retry_after_hint`]).
fn exhausted_response(
    manager: &Manager,
    now: OffsetDateTime,
    account_count: usize,
    is_fable: bool,
) -> Response {
    let retry_after = manager.retry_after_hint(now, is_fable);
    tracing::warn!(
        account_count,
        retry_after,
        "returning fleet-exhausted 429 to client"
    );
    error_response(
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limit_error",
        &format!("All {account_count} accounts exhausted. Retry in {retry_after}s."),
        Some(retry_after),
    )
}

/// Whether a 502 is the honest verdict: at least one attempt failed in transport
/// AND no attempt ever came back with an upstream HTTP status. Every attempt that
/// gets as far as the send either fails in transport or yields a response, so
/// `upstream_responses == 0` is exactly "transport failure accounts for all of
/// them" — the only state `bad_gateway`'s message describes truthfully.
///
/// A 429, a 5xx, even a 401 is an upstream ANSWER: it proves the network path
/// works and that the right verdict for this request is quota exhaustion (or a
/// forwarded status), never "upstream unreachable". Keeping the two counts apart
/// is what lets one blip rotate away instead of collapsing the recovery ladder.
fn every_attempt_transport_failed(transport_failures: usize, upstream_responses: usize) -> bool {
    transport_failures > 0 && upstream_responses == 0
}

/// 502 when every attempt hit a transport failure (upstream unreachable). Gated
/// by [`every_attempt_transport_failed`], and the count is in the message so the
/// line states what was actually observed rather than asserting a fleet-wide
/// claim it cannot support.
fn bad_gateway(transport_failures: usize) -> Response {
    tracing::warn!(
        transport_failures,
        "returning 502 to client — every attempt failed in transport, none reached an upstream"
    );
    error_response(
        StatusCode::BAD_GATEWAY,
        "proxy_error",
        &format!("Upstream unreachable: all {transport_failures} attempt(s) failed in transport."),
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Account, Config, ProxyConfig};

    fn dummy_config(api_key: Option<&str>, upstream: &str) -> Config {
        Config {
            proxy: ProxyConfig {
                port: 0,
                api_key: api_key.map(str::to_string),
                extra: serde_json::Map::new(),
            },
            upstream: upstream.to_string(),
            switch_threshold: 0.90,
            pacing: crate::config::PacingConfig::default(),
            throttle: crate::config::ThrottleConfig::default(),
            lock_account: None,
            accounts: vec![Account {
                name: "dummy".to_string(),
                account_type: "oauth".to_string(),
                account_uuid: None,
                org_uuid: None,
                org_name: None,
                access_token: "at-dummy".to_string(),
                refresh_token: Some("rt-dummy".to_string()),
                // Not expired, so booting the proxy triggers no token refresh.
                expires_at: Some(crate::now_ms() + 3_600_000),
                priority: Some(0),
                switch_threshold: None,
                disabled: None,
                extra: serde_json::Map::new(),
            }],
            extra: serde_json::Map::new(),
        }
    }

    fn headers_with_api_key(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("x-api-key", HeaderValue::from_str(value).unwrap());
        h
    }

    #[test]
    fn parse_retry_after_ignores_rfc_date() {
        // Numeric seconds parse to Some(n).
        let mut numeric = HeaderMap::new();
        numeric.insert("retry-after", HeaderValue::from_static("30"));
        assert_eq!(parse_retry_after(&numeric), Some(30));

        // RFC-date form is NOT parsed — current behavior returns None, so the
        // caller falls back to its default. (This documents the gap a follow-up
        // fix will close; do not change parse_retry_after to make this Some.)
        let mut rfc_date = HeaderMap::new();
        rfc_date.insert(
            "retry-after",
            HeaderValue::from_static("Wed, 21 Oct 2026 07:28:00 GMT"),
        );
        assert_eq!(parse_retry_after(&rfc_date), None);

        // Absent header → None.
        let empty = HeaderMap::new();
        assert_eq!(parse_retry_after(&empty), None);
    }

    #[test]
    fn classify_transient_429_absent_parks_short() {
        // THE BITING TEST: an absent retry-after must park the short
        // NO_GUIDANCE_HOLD_SECS base (15), not the old fabricated 60. The pre-fix
        // inline logic fabricated a 60s park for an absent retry-after
        // (unwrap_or(60), 60 > the 15s inline bound → Park(60)); this asserts the
        // new short base value instead — the oracle for the fleet-blackout fix.
        assert_eq!(
            classify_transient_429(None, 0, 0),
            Transient429::Park(NO_GUIDANCE_HOLD_SECS)
        );
        assert_eq!(classify_transient_429(None, 0, 0), Transient429::Park(15));
        // Jitter adds to the base hold so co-tripping accounts un-park staggered.
        assert_eq!(classify_transient_429(None, 0, 5), Transient429::Park(20));
    }

    #[test]
    fn classify_transient_429_present_retry_after_unchanged() {
        // A non-zero jitter must be IGNORED on the present-retry-after path — it
        // only affects the no-guidance (None) hold. Pass jitter=3 throughout and
        // assert the results are the historical byte-identical values.
        // Short hint under the inline bound, retry budget available → inline-wait.
        assert_eq!(
            classify_transient_429(Some(3), 0, 3),
            Transient429::InlineWait(3)
        );
        // Retry cap reached → park the same short wait, rotate.
        assert_eq!(
            classify_transient_429(Some(3), MAX_SAME_ACCOUNT_429, 3),
            Transient429::Park(3)
        );
        // Upper clamp: anything over 300 parks at 300.
        assert_eq!(
            classify_transient_429(Some(500), 0, 3),
            Transient429::Park(300)
        );
        // Lower clamp: 0 becomes 1, still within the inline bound → inline-wait.
        assert_eq!(
            classify_transient_429(Some(0), 0, 3),
            Transient429::InlineWait(1)
        );
    }

    #[test]
    fn backoff_529_ladder_escalates_then_clamps() {
        assert_eq!(
            backoff_529_secs(0, None),
            1,
            "first retry is near-immediate"
        );
        assert_eq!(backoff_529_secs(1, None), 2, "the ladder doubles");
        assert_eq!(
            backoff_529_secs(2, None),
            4,
            "third rung reaches the ceiling"
        );
        assert_eq!(
            backoff_529_secs(9, None),
            RETRY_529_MAX_BACKOFF_SECS as u64,
            "a rung past the ceiling clamps rather than overflowing the shift"
        );
    }

    #[test]
    fn backoff_529_honours_retry_after_as_a_floor_but_clamps_it() {
        assert_eq!(
            backoff_529_secs(0, Some(3)),
            3,
            "a server hint longer than the rung wins — the upstream knows its own load"
        );
        assert_eq!(
            backoff_529_secs(1, Some(1)),
            2,
            "a hint SHORTER than the rung must not defeat the escalation"
        );
        assert_eq!(
            backoff_529_secs(0, Some(300)),
            RETRY_529_MAX_BACKOFF_SECS as u64,
            "an arbitrary server-supplied wait is clamped — it holds an in-flight slot"
        );
        assert_eq!(
            backoff_529_secs(0, Some(0)),
            1,
            "a zero/absurd hint still waits the rung, never busy-loops"
        );
        assert_eq!(backoff_529_secs(0, Some(-5)), 1, "negative hints too");
    }

    /// The in-flight guard is held across every 529 backoff, so the ladder's TOTAL
    /// is a latency budget, not just a per-step one. This is the assertion that
    /// fails if someone raises the retry count or the per-step ceiling without
    /// re-reading the comment on the 529 arm.
    ///
    /// Scope: this is the PER-ACCOUNT ladder. A request may now spend it on up to
    /// `1 + MAX_529_FAILOVERS_PER_REQUEST` accounts, so the number a client actually
    /// waits is a multiple of this one — see
    /// `overloaded_529_failover_worst_case_latency_is_bounded`.
    #[test]
    fn backoff_529_worst_case_total_stays_single_digit() {
        let worst: u64 = (0..MAX_SAME_ACCOUNT_529_RETRIES)
            .map(|n| backoff_529_secs(n, Some(i64::MAX)))
            .sum();
        assert!(
            worst < 10,
            "529 retries hold an in-flight slot; worst-case added latency was {worst}s"
        );
    }

    /// The REQUEST-level 529 latency budget, and the loop headroom the failover needs.
    /// Pure arithmetic over the constants, so it is the assertion that fails when
    /// someone widens the failover budget without re-reading what it costs.
    ///
    /// Two ladders, because they are two different promises. On the no-`retry-after`
    /// ladder — the only shape the live captures actually carry — the whole request
    /// stays single-digit at 9s. With a hostile server hint every rung clamps to
    /// `RETRY_529_MAX_BACKOFF_SECS` and the ceiling is 24s: NOT single-digit, and
    /// stated here honestly rather than assumed away, because the in-flight guard is
    /// held across all of it. That ceiling is what bounds `MAX_529_FAILOVERS_PER_REQUEST`
    /// at 2.
    ///
    /// The headroom half is the one that would fail silently: the rotation loop stops
    /// after `max_attempts_for(account_count)` iterations, so a failover ladder longer
    /// than that budget would be truncated mid-walk — the request would stop early for
    /// a reason nothing logs. The binding case is the SMALLEST fleet that can host the
    /// full walk (`accounts_walked` accounts); bigger fleets only add headroom, and
    /// smaller ones cannot reach the last hop at all because the failover is gated on
    /// `select` offering another account.
    #[test]
    fn overloaded_529_failover_worst_case_latency_is_bounded() {
        let accounts_walked = 1 + MAX_529_FAILOVERS_PER_REQUEST as u64;
        let ladder = |hint: Option<i64>| -> u64 {
            (0..MAX_SAME_ACCOUNT_529_RETRIES)
                .map(|n| backoff_529_secs(n, hint))
                .sum()
        };

        let no_hint = ladder(None) * accounts_walked;
        assert!(
            no_hint < 10,
            "on the ladder the live upstream actually produces (no `retry-after`) the \
             whole request must stay single-digit; was {no_hint}s"
        );
        let hostile_hint = ladder(Some(i64::MAX)) * accounts_walked;
        assert!(
            hostile_hint <= 24,
            "even when every 529 asks for the maximum wait, a client connection and \
             the in-flight slots behind it are parked for at most 24s; was {hostile_hint}s"
        );

        let sends = accounts_walked as usize * (MAX_SAME_ACCOUNT_529_RETRIES as usize + 1);
        let budget = max_attempts_for(accounts_walked as usize);
        assert!(
            sends <= budget,
            "the full failover walk is {sends} sends but the rotation loop only allows \
             {budget} attempts on a {accounts_walked}-account fleet — the walk would be \
             silently truncated"
        );
    }

    #[test]
    fn soft_wait_secs_separates_transient_from_real_exhaustion() {
        // Transient burst-park (≤ ceiling), not yet soft-waited → wait that long.
        assert_eq!(soft_wait_secs(16, false), Some(16));
        // Exactly at the 20s ceiling → still a transient park, soft-wait.
        assert_eq!(
            soft_wait_secs(EXHAUSTION_SOFT_WAIT_MAX_SECS, false),
            Some(EXHAUSTION_SOFT_WAIT_MAX_SECS as u64)
        );
        // Just over the ceiling → real exhaustion, hard-fail (no wait).
        assert_eq!(
            soft_wait_secs(EXHAUSTION_SOFT_WAIT_MAX_SECS + 1, false),
            None
        );
        // A real quota window (hours) → far above ceiling → hard-fail.
        assert_eq!(soft_wait_secs(6000, false), None);
        // One-shot: already soft-waited this request → never wait again.
        assert_eq!(soft_wait_secs(16, true), None);
        // No promise of an imminent un-park (0 / negative hint) → hard-fail.
        assert_eq!(soft_wait_secs(0, false), None);
        assert_eq!(soft_wait_secs(-1, false), None);
    }

    #[test]
    fn stable_session_key_is_deterministic_for_api_key() {
        let h = headers_with_api_key("sk-team-alice");
        let a = stable_session_key(&h, b"{}", None);
        let b = stable_session_key(&h, b"{}", None);
        assert_eq!(a, b, "same x-api-key must survive a reconnect");
        assert!(a.is_some());
    }

    #[test]
    fn stable_session_key_is_deterministic_for_user_id() {
        let body = br#"{"metadata":{"user_id":"user-123"},"messages":[]}"#;
        let h = HeaderMap::new();
        let a = stable_session_key(&h, body, None);
        let b = stable_session_key(&h, body, None);
        assert_eq!(a, b, "same user_id must survive a reconnect");
        assert!(a.is_some());
    }

    #[test]
    fn stable_session_key_prefers_api_key_over_user_id() {
        let body = br#"{"metadata":{"user_id":"user-123"}}"#;
        let with_key = stable_session_key(&headers_with_api_key("the-key"), body, None);
        let key_only = stable_session_key(&headers_with_api_key("the-key"), b"{}", None);
        assert_eq!(with_key, key_only, "x-api-key must win over user_id");
    }

    #[test]
    fn stable_session_key_namespaces_key_vs_uid() {
        // An x-api-key "abc" and a user_id "abc" must NOT collide.
        let from_key = stable_session_key(&headers_with_api_key("abc"), b"{}", None);
        let from_uid = stable_session_key(
            &HeaderMap::new(),
            br#"{"metadata":{"user_id":"abc"}}"#,
            None,
        );
        assert_ne!(from_key, from_uid, "prefixes must isolate the two spaces");
    }

    #[test]
    fn stable_session_key_none_without_identity() {
        // No x-api-key and no top-level metadata.user_id → fall back to conn key.
        assert_eq!(
            stable_session_key(&HeaderMap::new(), br#"{"messages":[]}"#, None),
            None
        );
    }

    #[test]
    fn stable_session_key_ignores_nested_user_id() {
        // A user_id nested in message content is NOT top-level metadata.
        let body = br#"{"messages":[{"role":"user","content":{"metadata":{"user_id":"nested"}}}]}"#;
        assert_eq!(stable_session_key(&HeaderMap::new(), body, None), None);
    }

    #[test]
    fn stable_session_key_distinguishes_different_api_keys() {
        let a = stable_session_key(&headers_with_api_key("key-a"), b"{}", None);
        let b = stable_session_key(&headers_with_api_key("key-b"), b"{}", None);
        assert_ne!(a, b, "distinct team keys → distinct accounts");
    }

    #[test]
    fn stable_session_key_skips_shared_proxy_key() {
        // When the x-api-key IS the configured proxy secret, it is NOT a client
        // identity: fall through to user_id, else None. Otherwise every remote
        // client (all forced to send that one key) would collapse onto one
        // account, defeating rotation.
        let shared = "sk-proxy-secret";
        // No user_id → falls through to None (per-connection key at the caller).
        assert_eq!(
            stable_session_key(&headers_with_api_key(shared), b"{}", Some(shared)),
            None,
            "the shared proxy key must not be used as an affinity discriminator"
        );
        // With a body user_id, it falls through to that instead of the shared key.
        let body = br#"{"metadata":{"user_id":"user-123"}}"#;
        let via_shared = stable_session_key(&headers_with_api_key(shared), body, Some(shared));
        let via_uid = stable_session_key(&HeaderMap::new(), body, None);
        assert_eq!(
            via_shared, via_uid,
            "with the shared key skipped, the user_id is the discriminator"
        );
        assert!(via_shared.is_some());
        // A DIFFERENT (genuine team) key with the same proxy_key configured is
        // still used — only the exact shared secret is skipped.
        let team = stable_session_key(&headers_with_api_key("sk-team-alice"), b"{}", Some(shared));
        assert!(
            team.is_some(),
            "a distinct team key is a real identity and must still key"
        );
    }

    /// Bug #1 + cache counting: a `message_start` split across TWO chunks is
    /// still parsed whole, and `input` sums base + cache-creation + cache-read.
    #[tokio::test]
    async fn sse_usage_sums_cache_tokens_across_split_chunks() {
        let full = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{",
            "\"input_tokens\":10,\"cache_creation_input_tokens\":100,",
            "\"cache_read_input_tokens\":1000,\"output_tokens\":1}}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":42}}\n\n",
        );
        // Split mid-way through the first event's data line.
        let split = 60usize;
        let chunks = vec![
            Ok::<Bytes, Infallible>(Bytes::copy_from_slice(&full.as_bytes()[..split])),
            Ok::<Bytes, Infallible>(Bytes::copy_from_slice(&full.as_bytes()[split..])),
        ];
        let (parsed, stream_error) = parse_sse_usage(futures::stream::iter(chunks)).await;
        assert_eq!(stream_error, None, "a clean stream has no error event");
        // R1: the quota sum is byte-identical to before.
        assert_eq!(
            parsed.input_total, 1110,
            "10 + 100 (cache-creation) + 1000 (cache-read)"
        );
        assert_eq!(parsed.output, 42);
        // NEW: the cache components are now surfaced SEPARATELY (not summed away).
        assert_eq!(
            parsed.cache_read, 1000,
            "cache-read retained on the SSE path"
        );
        assert_eq!(parsed.cache_creation, 100, "cache-creation retained");
    }

    /// No double-count: multiple `message_delta`s yield the FINAL cumulative
    /// output, not the sum, so applying the total once is correct.
    #[tokio::test]
    async fn sse_usage_takes_final_output_not_sum() {
        let full = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":20}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":37}}\n\n",
        );
        let stream = futures::stream::iter(vec![Ok::<Bytes, Infallible>(Bytes::from(full))]);
        let (parsed, stream_error) = parse_sse_usage(stream).await;
        assert_eq!(stream_error, None, "a clean stream has no error event");
        assert_eq!(parsed.input_total, 5);
        assert_eq!(parsed.output, 37, "final cumulative output, not 20 + 37");
        // No cache tokens in this fixture → both stay zero.
        assert_eq!(parsed.cache_read, 0);
        assert_eq!(parsed.cache_creation, 0);
    }

    /// Non-stream JSON path also sums the cache tokens into `input`.
    #[test]
    fn json_usage_sums_cache_tokens() {
        let body = br#"{"usage":{"input_tokens":7,"cache_creation_input_tokens":3,"cache_read_input_tokens":90,"output_tokens":11}}"#;
        let parsed = usage_from_json(body);
        // R1: 7 + 3 + 90 = 100, the quota sum, unchanged.
        assert_eq!(parsed.input_total, 100);
        assert_eq!(parsed.output, 11);
        // R2: the JSON path surfaces the SAME cache fields the SSE path does.
        assert_eq!(
            parsed.cache_read, 90,
            "cache-read retained on the JSON path"
        );
        assert_eq!(parsed.cache_creation, 3, "cache-creation retained");
    }

    #[test]
    fn strips_client_authorization_and_sets_bearer() {
        let mut req = HeaderMap::new();
        req.insert(
            "authorization",
            HeaderValue::from_static("Bearer client-secret"),
        );
        req.insert("x-api-key", HeaderValue::from_static("proxy-key"));
        req.insert("accept-encoding", HeaderValue::from_static("gzip"));
        req.insert(
            "anthropic-beta",
            HeaderValue::from_static("oauth-2025-04-20"),
        );
        let out = build_upstream_headers(&req, "fresh-access-token");
        assert_eq!(
            out.get("authorization").unwrap().to_str().unwrap(),
            "Bearer fresh-access-token"
        );
        assert!(
            out.get("x-api-key").is_none(),
            "client key must not leak upstream"
        );
        assert!(
            out.get("accept-encoding").is_none(),
            "encoding stripped for SSE parse"
        );
        assert_eq!(
            out.get("anthropic-beta").unwrap().to_str().unwrap(),
            "oauth-2025-04-20",
            "unrelated headers pass through"
        );
    }

    #[test]
    fn quota_rejected_detects_any_window() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "anthropic-ratelimit-unified-7d-status",
            HeaderValue::from_static("rejected"),
        );
        assert!(is_quota_rejected(&headers));

        let mut allowed = HeaderMap::new();
        allowed.insert(
            "anthropic-ratelimit-unified-status",
            HeaderValue::from_static("allowed_warning"),
        );
        assert!(!is_quota_rejected(&allowed));
    }

    /// Boot smoke test on a FREE port (never 3456): driven through `axum::serve`
    /// directly, so no `ClientAddr` extension is injected and the client is
    /// treated as remote (fail-closed) — a keyless request is 401, a keyed one
    /// proceeds upstream. The loopback exemption (the production path via
    /// `mitm::serve`) is covered separately by `loopback_client_is_exempt`.
    /// The upstream points at a dead local port so the request never reaches the
    /// real Anthropic API — it fails transport and returns 502.
    #[tokio::test]
    async fn auth_rejects_without_key_and_attempts_upstream_with_key() {
        // 127.0.0.1:1 is reliably connection-refused, so the upstream forward
        // fails fast without any network egress to Anthropic.
        let manager = Manager::with_live_refresher(
            dummy_config(Some("test-key"), "http://127.0.0.1:1"),
            None,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app(manager)).await;
        });

        let client = reqwest::Client::new();

        let no_key = client.get(format!("http://{addr}/")).send().await.unwrap();
        assert_eq!(
            no_key.status().as_u16(),
            401,
            "keyless request must be rejected"
        );

        let with_key = client
            .get(format!("http://{addr}/"))
            .header("x-api-key", "test-key")
            .send()
            .await
            .unwrap();
        assert_eq!(
            with_key.status().as_u16(),
            502,
            "authenticated request proceeds upstream (dead port → 502), never 401"
        );
    }

    /// Loopback exemption: served through the production hybrid listener
    /// (`mitm::serve`, which injects `ClientAddr`), a KEYLESS request from a
    /// loopback client must NOT be 401 even though a proxy key is configured — it
    /// proceeds upstream (dead port → 502). This is what lets `claude` (which
    /// sends no proxy key, only its own OAuth) talk to the local proxy, exactly
    /// as it did with the JS proxy's loopback exemption.
    #[tokio::test]
    async fn loopback_client_is_exempt() {
        let manager = Manager::with_live_refresher(
            dummy_config(Some("secret-proxy-key"), "http://127.0.0.1:1"),
            None,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // The hybrid server injects ClientAddr; base-URL (non-CONNECT) needs no TLS.
        tokio::spawn(async move { crate::mitm::serve(listener, manager, None).await });

        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        // NO x-api-key — but the client is loopback, so it must be exempt.
        let resp = client.get(format!("http://{addr}/")).send().await.unwrap();
        assert_ne!(
            resp.status().as_u16(),
            401,
            "a loopback client must be exempt from the api-key gate"
        );
        assert_eq!(
            resp.status().as_u16(),
            502,
            "the exempt request proceeds upstream (dead port → 502)"
        );
    }

    // --- GET /_tcr/status ---------------------------------------------------

    /// Drive the router directly with a `ClientAddr` extension we control.
    ///
    /// This is the same extension `mitm::serve_http` injects from the real peer
    /// socket, and driving the router by hand is the only way to present a
    /// NON-loopback peer without binding a routable address on the test machine.
    /// `peer = None` models a request that never went through the listener at all.
    async fn status_request(
        manager: Arc<Manager>,
        method: Method,
        peer: Option<SocketAddr>,
        api_key: Option<&str>,
    ) -> (StatusCode, HeaderMap, Bytes) {
        use tower::ServiceExt as _;
        let mut builder = Request::builder().method(method).uri(STATUS_PATH);
        if let Some(key) = api_key {
            builder = builder.header("x-api-key", key);
        }
        let mut req = builder.body(Body::empty()).expect("build request");
        if let Some(addr) = peer {
            req.extensions_mut().insert(ClientAddr(addr));
        }
        let response = app(manager).oneshot(req).await.expect("router response");
        let status = response.status();
        let headers = response.headers().clone();
        let body = to_bytes(response.into_body(), MAX_BODY_BYTES)
            .await
            .expect("read body");
        (status, headers, body)
    }

    fn loopback_peer() -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 54_321))
    }

    /// A routable, definitely-not-loopback peer (TEST-NET-3, RFC 5737).
    fn remote_peer() -> SocketAddr {
        SocketAddr::from(([203, 0, 113, 7], 44_444))
    }

    /// THE BITING TEST for the whole change: the endpoint reports the counters of
    /// the process that actually served, so a cache hit ratio computed from them
    /// is a real measurement.
    ///
    /// Pre-fix there was no route at all — `tcr status` built a FRESH `Manager`
    /// whose `input_tokens` is structurally 0, so its ratio was the literal `0.0`
    /// fallback for every account, always. Here 750 of 1000 input tokens were
    /// cache reads and the payload must carry exactly that, giving 0.75.
    #[tokio::test]
    async fn status_endpoint_returns_live_counters() {
        let manager = Manager::with_live_refresher(dummy_config(None, "http://127.0.0.1:1"), None);
        // Serve a request through the manager so the counters are non-zero, the
        // same two calls `handle` makes at a terminal outcome.
        manager.update_usage(0, 1_000, 200, 750, 50);
        manager.record_served(0, OffsetDateTime::now_utc(), None, SessionKind::Fallback);

        let (status, headers, body) =
            status_request(manager, Method::GET, Some(loopback_peer()), None).await;
        assert_eq!(status, StatusCode::OK, "loopback GET is served");
        assert_eq!(
            headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
        assert_eq!(
            headers.get("cache-control").and_then(|v| v.to_str().ok()),
            Some("no-store"),
            "live state from a credential-holding process is never stored"
        );

        let payload: crate::status::StatusPayload =
            serde_json::from_slice(&body).expect("a tcr status payload");
        assert_eq!(payload.kind, crate::status::STATUS_KIND);
        let account = &payload.accounts[0];
        assert_eq!(account.name, "dummy");
        assert_eq!(account.requests, 1, "the serve was counted");
        assert_eq!(account.input_tokens, 1_000);
        assert_eq!(account.cache_read_tokens, 750);
        assert_eq!(account.cache_creation_tokens, 50);
        assert_eq!(account.output_tokens, 200);
        // The number `tcr status` derives from this row — a real 0.75, never the
        // structural 0.0 an offline snapshot could only ever produce.
        let ratio = account.cache_read_tokens as f64 / account.input_tokens as f64;
        assert!(
            (ratio - 0.75).abs() < f64::EPSILON,
            "hit ratio reflects the real numbers: {ratio}"
        );
    }

    /// SECURITY GUARD 1 — origin. Binding loopback is not authorization, so the
    /// endpoint proves the peer instead: a non-loopback `ClientAddr` is refused,
    /// and so is an ABSENT one (a request that did not arrive through the hybrid
    /// listener has no provable origin, so it fails CLOSED). Neither answer may
    /// carry any fleet state.
    #[tokio::test]
    async fn status_endpoint_rejects_a_non_loopback_client() {
        let config = || dummy_config(None, "http://127.0.0.1:1");

        for (label, peer) in [
            ("a routable peer", Some(remote_peer())),
            ("no ClientAddr at all", None),
        ] {
            let manager = Manager::with_live_refresher(config(), None);
            let (status, _headers, body) = status_request(manager, Method::GET, peer, None).await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "{label} must be refused, got {status}"
            );
            let text = String::from_utf8_lossy(&body);
            assert!(
                !text.contains("dummy") && !text.contains("accounts"),
                "a refused caller learns nothing about the fleet: {text}"
            );
        }
    }

    /// SECURITY GUARD 2 — the key. When a proxy api-key is configured this
    /// endpoint requires it with NO loopback exemption, deliberately stricter than
    /// the forwarding path (which exempts loopback because `claude` sends its own
    /// OAuth and never the proxy key). Every case below is a LOOPBACK peer, so
    /// what is being asserted is precisely that loopback alone is not enough.
    #[tokio::test]
    async fn status_endpoint_rejects_a_bad_api_key() {
        let config = || dummy_config(Some("sk-proxy-secret"), "http://127.0.0.1:1");

        for (label, key) in [
            ("no key", None),
            ("wrong key", Some("sk-wrong-secret!")),
            ("prefix of the key", Some("sk-proxy")),
            ("key with a suffix", Some("sk-proxy-secret-extra")),
        ] {
            let manager = Manager::with_live_refresher(config(), None);
            let (status, _headers, body) =
                status_request(manager, Method::GET, Some(loopback_peer()), key).await;
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "{label} must be 401 even from loopback, got {status}"
            );
            let text = String::from_utf8_lossy(&body);
            assert!(
                !text.contains("dummy"),
                "a rejected caller learns nothing about the fleet: {text}"
            );
        }

        // ...and the correct key from loopback is served.
        let manager = Manager::with_live_refresher(config(), None);
        let (status, _headers, _body) = status_request(
            manager,
            Method::GET,
            Some(loopback_peer()),
            Some("sk-proxy-secret"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "loopback + correct key is served");
    }

    /// SECURITY GUARD 3 — no secrets on the wire. Asserted on the RESPONSE BYTES,
    /// not on a struct: a struct assertion only proves the fields you thought to
    /// name are clean, while the bytes are what actually leaves the process. The
    /// proxy holds every account's OAuth access and refresh token plus the proxy
    /// key; none of the three may appear, in any field, under any name.
    #[tokio::test]
    async fn status_endpoint_leaks_no_secrets() {
        let manager = Manager::with_live_refresher(
            dummy_config(Some("sk-proxy-secret"), "http://127.0.0.1:1"),
            None,
        );
        manager.update_usage(0, 1_000, 200, 750, 50);
        let (status, headers, body) = status_request(
            manager,
            Method::GET,
            Some(loopback_peer()),
            Some("sk-proxy-secret"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let text = String::from_utf8_lossy(&body);
        for secret in [
            "at-dummy",        // the account's OAuth ACCESS token
            "rt-dummy",        // the account's OAuth REFRESH token
            "sk-proxy-secret", // the proxy api-key (must not be echoed back)
            "Bearer",          // no upstream Authorization header, in any form
        ] {
            assert!(
                !text.contains(secret),
                "the response body leaked {secret:?}: {text}"
            );
        }
        // Sanity: the assertion above is meaningful only if a real payload WAS
        // rendered — an empty body would pass it vacuously.
        assert!(
            text.contains("\"name\":\"dummy\"") && text.contains("\"cacheReadTokens\":750"),
            "the body really is a populated status payload: {text}"
        );
        // Headers are part of the response too — nothing credential-shaped there.
        for (name, value) in headers.iter() {
            let rendered = format!("{name}: {}", value.to_str().unwrap_or_default());
            assert!(
                !rendered.contains("at-dummy")
                    && !rendered.contains("rt-dummy")
                    && !rendered.contains("sk-proxy-secret"),
                "a response header leaked a secret: {rendered}"
            );
        }
    }

    /// SECURITY GUARD 4 — GET only, and never a fall-through. A non-GET on the
    /// status path is answered LOCALLY with 405; it must not mutate anything and
    /// must not reach `handle`.
    ///
    /// The 405 is what proves the no-fall-through: `handle` would rewrite the
    /// request to the configured upstream — a dead port here — and return 502. So
    /// 405 (and not 502) is the assertion that a POST to this path was never sent
    /// to Anthropic with a pooled OAuth Bearer attached.
    ///
    /// Sensitivity, measured rather than assumed: this test still passes with the
    /// method router's explicit `.fallback` removed, because axum 0.8 already
    /// answers 405 instead of inheriting the outer catch-all. So it is a
    /// REGRESSION guard on the routing contract — it would catch the path being
    /// re-registered under `any(...)`, moved behind the fallback, or an axum
    /// upgrade that starts propagating the outer fallback — not proof that the
    /// explicit `.fallback` is currently load-bearing.
    #[tokio::test]
    async fn status_endpoint_is_get_only() {
        for method in [Method::POST, Method::PUT, Method::DELETE, Method::PATCH] {
            let manager =
                Manager::with_live_refresher(dummy_config(None, "http://127.0.0.1:1"), None);
            let before = manager.snapshot(OffsetDateTime::now_utc());
            let (status, _headers, body) = status_request(
                Arc::clone(&manager),
                method.clone(),
                Some(loopback_peer()),
                None,
            )
            .await;
            assert_eq!(
                status,
                StatusCode::METHOD_NOT_ALLOWED,
                "{method} on the status path is refused locally, got {status}"
            );
            assert_ne!(
                status,
                StatusCode::BAD_GATEWAY,
                "{method} must never fall through to the upstream forwarder"
            );
            let text = String::from_utf8_lossy(&body);
            assert!(
                !text.contains("cacheReadTokens"),
                "a refused method returns no snapshot: {text}"
            );

            // Nothing moved: no serve counted, no token usage, no probe.
            let after = manager.snapshot(OffsetDateTime::now_utc());
            assert_eq!(after.accounts[0].requests, before.accounts[0].requests);
            assert_eq!(
                after.accounts[0].input_tokens,
                before.accounts[0].input_tokens
            );
            assert_eq!(
                after.accounts[0].probe_status, before.accounts[0].probe_status,
                "{method} triggered no probe"
            );
            assert_eq!(after.current, before.current, "{method} moved no cursor");
        }
    }

    // --- rotation-bypassing relays -----------------------------------------

    /// What a fake upstream saw — one of these per request it received.
    #[derive(serde::Serialize, serde::Deserialize)]
    struct Echo {
        method: String,
        path: String,
        headers: Vec<(String, String)>,
        body: String,
    }

    impl Echo {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.as_str())
        }
    }

    /// Spawn a fake upstream that echoes every request back as JSON and counts hits.
    ///
    /// A real upstream is what makes these tests measure the thing that matters:
    /// what the proxy PUT ON THE WIRE. Asserting on the header-building helpers
    /// alone would prove only that the helpers work, not that the relay path calls
    /// them — the exact gap the bug lived in. The hit counter is the second half:
    /// it proves a locally-answered path reached NO upstream at all.
    async fn spawn_echo_upstream() -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let upstream = Router::new().fallback(move |req: Request| {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                let (parts, body) = req.into_parts();
                let bytes = to_bytes(body, MAX_BODY_BYTES).await.unwrap_or_default();
                axum::Json(Echo {
                    method: parts.method.to_string(),
                    path: parts
                        .uri
                        .path_and_query()
                        .map(|pq| pq.as_str().to_string())
                        .unwrap_or_default(),
                    headers: parts
                        .headers
                        .iter()
                        .map(|(n, v)| {
                            (n.as_str().to_string(), v.to_str().unwrap_or("").to_string())
                        })
                        .collect(),
                    body: String::from_utf8_lossy(&bytes).to_string(),
                })
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind echo upstream");
        let addr = listener.local_addr().expect("echo upstream addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, upstream).await;
        });
        (format!("http://{addr}"), hits)
    }

    /// Drive the proxy router with a peer we control, as `mitm::serve_http` would,
    /// returning the WHOLE client-visible response.
    ///
    /// Response headers are the surface the account strip acts on, so a helper
    /// that folds them away cannot test it — [`drive`] is the status+body view
    /// layered on top of this one.
    async fn drive_full(
        manager: Arc<Manager>,
        method: Method,
        uri: &str,
        peer: Option<SocketAddr>,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Response {
        use tower::ServiceExt as _;
        let mut builder = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let mut req = builder
            .body(Body::from(body.to_string()))
            .expect("build request");
        if let Some(addr) = peer {
            req.extensions_mut().insert(ClientAddr(addr));
        }
        app(manager).oneshot(req).await.expect("router response")
    }

    /// [`drive_full`] reduced to the status and body most tests assert on.
    async fn drive(
        manager: Arc<Manager>,
        method: Method,
        uri: &str,
        peer: Option<SocketAddr>,
        headers: &[(&str, &str)],
        body: &str,
    ) -> (StatusCode, Bytes) {
        let response = drive_full(manager, method, uri, peer, headers, body).await;
        let status = response.status();
        let bytes = to_bytes(response.into_body(), MAX_BODY_BYTES)
            .await
            .expect("read body");
        (status, bytes)
    }

    fn parse_echo(bytes: &Bytes) -> Echo {
        serde_json::from_slice(bytes).unwrap_or_else(|err| {
            panic!(
                "an echo payload ({err}): {}",
                String::from_utf8_lossy(bytes)
            )
        })
    }

    /// The classifier, in isolation. The negatives matter as much as the positives:
    /// `/v1/code/` is a prefix WITH its slash so `/v1/codex` is not a Remote Control
    /// path, and the token refresh is POST-only so a GET on it is ordinary traffic.
    #[test]
    fn relay_mode_classifies_only_the_bypass_paths() {
        for path in [
            "/v1/code/",
            "/v1/code/session/abc",
            "/api/oauth/files/file_0123",
            "/api/oauth/file_upload",
        ] {
            assert_eq!(
                relay_mode(&Method::POST, path),
                Some(RelayMode::ClientCredential),
                "{path} is client-credential"
            );
            assert_eq!(
                relay_mode(&Method::GET, path),
                Some(RelayMode::ClientCredential),
                "{path} is client-credential on GET too"
            );
        }
        assert_eq!(
            relay_mode(&Method::POST, CLIENT_TOKEN_REFRESH_PATH),
            Some(RelayMode::Raw)
        );
        for (method, path) in [
            (Method::GET, "/v1/oauth/token"), // refresh is POST-only
            (Method::POST, "/v1/messages"),
            (Method::GET, "/v1/models"),
            (Method::GET, "/v1/codex"), // NOT under /v1/code/
            (Method::GET, "/"),
        ] {
            assert_eq!(
                relay_mode(&method, path),
                None,
                "{method} {path} must take the normal rotation path"
            );
        }
    }

    /// The segment-boundary rule, at both edges. Every entry that reads as a prefix
    /// in this module is really "this path, or something under it" — a longer
    /// IDENTIFIER sharing the same leading characters is a different route.
    #[test]
    fn relay_mode_matches_whole_segments_at_both_edges() {
        // Bare, with no trailing slash: a real route, previously missed by `/v1/code/`.
        for path in ["/v1/code", "/api/oauth/files", "/api/oauth/file_upload"] {
            assert_eq!(
                relay_mode(&Method::POST, path),
                Some(RelayMode::ClientCredential),
                "the bare {path} is the same route as {path}/"
            );
        }
        // Longer identifiers: NOT the route, previously swallowed by the entry that
        // carried no terminator.
        for path in [
            "/v1/codex",
            "/api/oauth/file_upload_v2",
            "/api/oauth/file_uploadX",
            "/api/oauth/filesystem",
        ] {
            assert_eq!(
                relay_mode(&Method::POST, path),
                None,
                "{path} only shares a prefix — it is not the route"
            );
        }
        // The token refresh takes the same rule. `/v1/oauth/token/` used to miss the
        // exact compare and fall through to the POOLED path, which is the one outcome
        // a client's own credential exchange must never have.
        for path in ["/v1/oauth/token", "/v1/oauth/token/"] {
            assert_eq!(
                relay_mode(&Method::POST, path),
                Some(RelayMode::Raw),
                "{path} is the client's own token exchange"
            );
        }
        assert_eq!(
            relay_mode(&Method::POST, "/v1/oauth/tokens"),
            None,
            "a longer identifier is not the token endpoint"
        );
    }

    /// `path_is_under` in isolation — the single rule every prefix decision uses.
    #[test]
    fn path_is_under_matches_only_whole_segments() {
        for (path, base, want) in [
            ("/_tcr", "/_tcr", true),
            ("/_tcr/", "/_tcr", true),
            ("/_tcr/status", "/_tcr", true),
            ("/_tcrx", "/_tcr", false),
            ("/_tc", "/_tcr", false),
            ("/v1/code/session/abc", "/v1/code", true),
            ("", "/_tcr", false),
        ] {
            assert_eq!(
                path_is_under(path, base),
                want,
                "path_is_under({path:?}, {base:?})"
            );
        }
    }

    /// Dot-segment recognition, in every spelling the upstream URL parser folds —
    /// and, as importantly, NOT in the spellings it leaves alone. Over-rejecting a
    /// legitimate path is a real failure mode, not a safe default.
    #[test]
    fn dot_segments_are_recognised_in_every_spelling() {
        // Single-dot (`.`, `%2e`) and double-dot (`..` and its three mixed
        // spellings) segments — the parser folds `%2e`/`%2E` before classifying.
        for segment in [".", "%2e", "%2E", "..", "%2e%2e", "%2E%2E", ".%2e", "%2e."] {
            assert!(is_dot_segment(segment), "{segment} is a dot segment");
        }
        for segment in [
            "",
            "...",       // three dots is an ordinary name
            "%2e%2e%2e", // …in any spelling
            "a.json",    // a literal dot INSIDE a name
            "..a",
            "a..",
            "..%2f..", // `%2f` is not decoded, so this is one opaque segment
            "%2f",
            "%25 2e",
            "file_upload",
        ] {
            assert!(!is_dot_segment(segment), "{segment} is NOT a dot segment");
        }
    }

    /// The whole-path guard: which request targets disagree with what reqwest will
    /// put on the wire. The negatives are the half that keeps the guard honest.
    #[test]
    fn path_is_ambiguous_flags_traversal_and_leaves_ordinary_paths_alone() {
        for path in [
            "/v1/code/../../v1/messages",
            "/v1/code/%2e%2e/%2e%2e/v1/messages",
            "/v1/code/../../_tcr/status",
            "/x/../_tcr/status",
            "/v1/./messages",
            "/v1/code/.%2e/../v1/messages",
            "/..",
            "/v1/code/..\\../v1/messages", // `\` is a WHATWG path separator
            "/v1/code\\foo",               // …so this lands on /v1/code/foo upstream
        ] {
            assert!(path_is_ambiguous(path), "{path} must be refused");
        }
        for path in [
            "/",
            "/v1/messages",
            "/v1/models",
            "/api/oauth/files/a.json",       // a literal dot in a filename
            "/api/oauth/files/...",          // three dots
            "/api/oauth/files/..%2f..",      // `%2f` is not decoded by the parser
            "/api/oauth/files/%5c..",        // nor is `%5c`
            "/v1/organizations/x.y.z/usage", // dots inside segments
            "/_tcr/status",
        ] {
            assert!(!path_is_ambiguous(path), "{path} is legitimate");
        }
    }

    /// Every client-credential prefix reaches upstream carrying the CLIENT's own
    /// `authorization` — and never the pooled account Bearer, which is what 403'd
    /// the Remote Control stream and silently dropped claude.ai attachments.
    #[tokio::test]
    async fn client_credential_paths_forward_the_clients_own_credential() {
        let (upstream, hits) = spawn_echo_upstream().await;
        for path in [
            "/v1/code/session/abc",
            "/api/oauth/files/file_0123",
            "/api/oauth/file_upload",
        ] {
            let manager = Manager::with_live_refresher(dummy_config(None, &upstream), None);
            let (status, body) = drive(
                manager,
                Method::POST,
                path,
                Some(loopback_peer()),
                &[
                    ("authorization", "Bearer client-own-token"),
                    ("content-type", "application/json"),
                ],
                "{}",
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{path} was relayed");
            let echo = parse_echo(&body);
            assert_eq!(echo.path, path, "the path is forwarded verbatim");
            assert_eq!(
                echo.header("authorization"),
                Some("Bearer client-own-token"),
                "{path} must carry the client's OWN credential"
            );
            assert_ne!(
                echo.header("authorization"),
                Some("Bearer at-dummy"),
                "{path} must never carry the pooled account token"
            );
        }
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    /// The proxy's own gate credential never leaves this process. An `x-api-key`
    /// equal to the configured proxy key is OUR secret and is stripped; a different
    /// one is the client's own and passes through untouched.
    #[tokio::test]
    async fn client_credential_strips_only_our_own_proxy_key() {
        let (upstream, _hits) = spawn_echo_upstream().await;
        let config = || dummy_config(Some("sk-proxy-secret"), &upstream);

        let manager = Manager::with_live_refresher(config(), None);
        let (status, body) = drive(
            manager,
            Method::GET,
            "/api/oauth/files/file_0123",
            Some(loopback_peer()),
            &[("x-api-key", "sk-proxy-secret")],
            "",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let echo = parse_echo(&body);
        assert_eq!(
            echo.header("x-api-key"),
            None,
            "our own proxy key must never reach Anthropic"
        );

        let manager = Manager::with_live_refresher(config(), None);
        let (status, body) = drive(
            manager,
            Method::GET,
            "/api/oauth/files/file_0123",
            Some(loopback_peer()),
            &[("x-api-key", "sk-the-clients-own")],
            "",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let echo = parse_echo(&body);
        assert_eq!(
            echo.header("x-api-key"),
            Some("sk-the-clients-own"),
            "a key that is not ours is the client's credential and passes through"
        );
    }

    /// `POST /v1/oauth/token` is a credential exchange that is not ours: it goes up
    /// with NO auth header in any form (its credentials are in the body, which is
    /// forwarded byte-for-byte) and no header we did not explicitly choose.
    #[tokio::test]
    async fn token_refresh_relays_raw_with_no_auth_header() {
        let (upstream, _hits) = spawn_echo_upstream().await;
        let manager =
            Manager::with_live_refresher(dummy_config(Some("sk-proxy-secret"), &upstream), None);
        let payload = r#"{"grant_type":"refresh_token","refresh_token":"rt-client"}"#;
        let (status, body) = drive(
            manager,
            Method::POST,
            "/v1/oauth/token",
            Some(loopback_peer()),
            &[
                ("authorization", "Bearer client-own-token"),
                ("x-api-key", "sk-proxy-secret"),
                ("content-type", "application/json"),
                ("x-stainless-lang", "js"),
            ],
            payload,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let echo = parse_echo(&body);
        assert_eq!(echo.method, "POST");
        assert_eq!(echo.body, payload, "the refresh body is forwarded verbatim");
        assert_eq!(
            echo.header("authorization"),
            None,
            "a raw relay sends no auth header at all"
        );
        assert_eq!(echo.header("x-api-key"), None, "nor our gate credential");
        assert_eq!(echo.header("x-stainless-lang"), None, "nor anything else");
        assert_eq!(echo.header("content-type"), Some("application/json"));
    }

    /// THE BITING TEST: a relayed request spends none of the rotation's state, and
    /// the `/v1/messages` control in the same test proves the assertion can fail —
    /// the identical counters DO move when a request goes the pooled-credential way,
    /// which is also the regression guard that this change left that path alone.
    #[tokio::test]
    async fn rotation_state_moves_for_messages_and_never_for_a_relay() {
        let (upstream, _hits) = spawn_echo_upstream().await;

        let manager = Manager::with_live_refresher(dummy_config(None, &upstream), None);
        let before = manager.snapshot(OffsetDateTime::now_utc());
        for (method, path) in [
            (Method::POST, "/v1/code/session/abc"),
            (Method::GET, "/api/oauth/files/file_0123"),
            (Method::POST, "/api/oauth/file_upload"),
            (Method::POST, "/v1/oauth/token"),
        ] {
            let (status, _body) = drive(
                Arc::clone(&manager),
                method.clone(),
                path,
                Some(loopback_peer()),
                &[("authorization", "Bearer client-own-token")],
                "{}",
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{method} {path} was relayed");
        }
        let after = manager.snapshot(OffsetDateTime::now_utc());
        assert_eq!(
            after.accounts[0].requests, before.accounts[0].requests,
            "a relayed request must not be counted against any account"
        );
        assert_eq!(
            after.accounts[0].last_used, before.accounts[0].last_used,
            "nor touch its LRU key"
        );
        assert_eq!(after.current, before.current, "nor move the cursor");
        assert_eq!(
            after.sessions.len(),
            before.sessions.len(),
            "nor write a pin"
        );

        // The control: the same counters on the same manager, for a request that
        // DOES go through rotation. It also re-asserts the pooled-credential
        // contract on `/v1/messages` — the client's authorization is replaced, not
        // forwarded.
        let manager = Manager::with_live_refresher(dummy_config(None, &upstream), None);
        let (status, body) = drive(
            Arc::clone(&manager),
            Method::POST,
            "/v1/messages",
            Some(loopback_peer()),
            &[
                ("authorization", "Bearer client-own-token"),
                ("content-type", "application/json"),
            ],
            r#"{"model":"claude-sonnet-5"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let echo = parse_echo(&body);
        assert_eq!(
            echo.header("authorization"),
            Some("Bearer at-dummy"),
            "/v1/messages still carries the POOLED token"
        );
        assert_eq!(
            manager.snapshot(OffsetDateTime::now_utc()).accounts[0].requests,
            1,
            "…and is counted against the serving account"
        );
    }

    /// Anything under the proxy's own prefix that is not a registered route is
    /// answered LOCALLY. The hit counter is the assertion that matters: a fall-through
    /// would have sent a proxy-private path to Anthropic with a pooled Bearer on it.
    #[tokio::test]
    async fn unknown_local_paths_are_answered_locally() {
        let (upstream, hits) = spawn_echo_upstream().await;
        for (method, uri) in [
            (Method::GET, "/_tcr/status-typo"),
            (Method::GET, "/_tcr/status/extra"),
            (Method::POST, "/_tcr/anything-else"),
            (Method::GET, "/_tcr/"),
            (Method::GET, "/_tcr"),
            (Method::GET, "/_tcr/status?x=1&y=2/../nope"),
        ] {
            let manager = Manager::with_live_refresher(dummy_config(None, &upstream), None);
            let (status, body) = drive(
                Arc::clone(&manager),
                method.clone(),
                uri,
                Some(loopback_peer()),
                &[],
                "",
            )
            .await;
            let text = String::from_utf8_lossy(&body);
            // `/_tcr/status?…` IS a registered route (the query does not change the
            // path), so it is served; every other shape is a local 404. Neither is
            // ever forwarded, which is the single claim this test makes.
            if uri.starts_with("/_tcr/status?") {
                assert_eq!(status, StatusCode::OK, "{uri} is the real status route");
            } else {
                assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri} → local 404");
                assert!(
                    text.contains("not_found_error"),
                    "in the standard error envelope: {text}"
                );
            }
            assert_eq!(
                manager.snapshot(OffsetDateTime::now_utc()).accounts[0].requests,
                0,
                "{uri} spent no account"
            );
        }
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no `/_tcr/…` request may ever reach an upstream"
        );
    }

    /// The traversal table, end to end, against the fake upstream that reports what
    /// it actually RECEIVED. Every row here was measured reaching an upstream before
    /// the guard existed, and the received path is the proof that the classification
    /// and the wire disagreed:
    ///
    /// | raw request target                  | classified as | upstream received |
    /// |-------------------------------------|---------------|-------------------|
    /// | `/v1/code/../../v1/messages`        | RELAY         | `/v1/messages`    |
    /// | `/v1/code/%2e%2e/%2e%2e/v1/messages`| RELAY         | `/v1/messages`    |
    /// | `/v1/code/../../_tcr/status`        | RELAY         | `/_tcr/status`    |
    /// | `/x/../_tcr/status`                 | pooled        | `/_tcr/status`    |
    /// | `/v1/code\..\../v1/messages`        | RELAY         | `/v1/messages`    |
    /// | `/v1/code\foo`                      | pooled        | `/v1/code/foo`    |
    ///
    /// Rows 1-2 and 5 are a real `/v1/messages` routed as a relay: no `select`, no
    /// in-flight slot, no throttle, no `record_served`, no pin, no retry ladder, and
    /// the CLIENT's credential instead of a pooled one. Rows 3-4 defeat the `/_tcr/`
    /// guard, row 4 putting a POOLED BEARER on `/_tcr/status` — the shape that burned
    /// an account. Row 6 is the inverse: a Remote Control path routed as pooled.
    ///
    /// The hit counter is the assertion that matters — 400, and nothing on the wire.
    #[tokio::test]
    async fn traversal_paths_are_refused_before_any_routing_decision() {
        let (upstream, hits) = spawn_echo_upstream().await;
        for uri in [
            "/v1/code/../../v1/messages",
            "/v1/code/%2e%2e/%2e%2e/v1/messages",
            "/v1/code/%2E%2E/%2E%2E/v1/messages",
            "/v1/code/../../_tcr/status",
            "/x/../_tcr/status",
            "/v1/code/.%2e/../v1/messages",
            "/v1/messages/./",
            "/v1/code/..\\../v1/messages",
            "/v1/code\\foo",
        ] {
            let manager = Manager::with_live_refresher(dummy_config(None, &upstream), None);
            let (status, body) = drive(
                Arc::clone(&manager),
                Method::POST,
                uri,
                Some(loopback_peer()),
                &[
                    ("authorization", "Bearer client-own-token"),
                    ("content-type", "application/json"),
                ],
                r#"{"model":"claude-sonnet-5"}"#,
            )
            .await;
            let text = String::from_utf8_lossy(&body);
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri} → 400, got {text}");
            assert!(
                text.contains("invalid_request_error"),
                "in the standard error envelope: {text}"
            );
            assert_eq!(
                manager.snapshot(OffsetDateTime::now_utc()).accounts[0].requests,
                0,
                "{uri} spent no account"
            );
        }
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "not one ambiguous path may reach an upstream, by either route"
        );
    }

    /// The other half of the guard: a dot that is not a dot SEGMENT is ordinary path
    /// text and must still be forwarded. A guard that rejected `a.json` would break
    /// attachment fetches to fix a traversal — trading one silent failure for another.
    #[tokio::test]
    async fn legitimate_paths_containing_dots_are_still_forwarded() {
        let (upstream, hits) = spawn_echo_upstream().await;
        for uri in [
            "/api/oauth/files/a.json",
            "/api/oauth/files/....",
            "/api/oauth/files/..%2f..",
            "/api/oauth/files/%5c..",
            "/v1/messages",
        ] {
            let manager = Manager::with_live_refresher(dummy_config(None, &upstream), None);
            let (status, body) = drive(
                manager,
                Method::POST,
                uri,
                Some(loopback_peer()),
                &[
                    ("authorization", "Bearer client-own-token"),
                    ("content-type", "application/json"),
                ],
                r#"{"model":"claude-sonnet-5"}"#,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{uri} must not be rejected");
            assert_eq!(
                parse_echo(&body).path,
                uri,
                "{uri} reaches the upstream verbatim"
            );
        }
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 5);
    }

    /// The segment-boundary fix on the wire, in both directions. The bare `/v1/code`
    /// is the Remote Control route and must carry the CLIENT's token; the longer
    /// identifier `/api/oauth/file_upload_v2` is NOT that route and takes rotation.
    #[tokio::test]
    async fn segment_boundaries_decide_which_credential_goes_on_the_wire() {
        let (upstream, _hits) = spawn_echo_upstream().await;

        let manager = Manager::with_live_refresher(dummy_config(None, &upstream), None);
        let (status, body) = drive(
            Arc::clone(&manager),
            Method::POST,
            "/v1/code",
            Some(loopback_peer()),
            &[
                ("authorization", "Bearer client-own-token"),
                ("content-type", "application/json"),
            ],
            "{}",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            parse_echo(&body).header("authorization"),
            Some("Bearer client-own-token"),
            "the bare /v1/code is Remote Control and keeps the client's credential"
        );
        assert_eq!(
            manager.snapshot(OffsetDateTime::now_utc()).accounts[0].requests,
            0,
            "…and spends no account"
        );

        let manager = Manager::with_live_refresher(dummy_config(None, &upstream), None);
        let (status, body) = drive(
            Arc::clone(&manager),
            Method::POST,
            "/api/oauth/file_upload_v2",
            Some(loopback_peer()),
            &[
                ("authorization", "Bearer client-own-token"),
                ("content-type", "application/json"),
            ],
            r#"{"model":"claude-sonnet-5"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            parse_echo(&body).header("authorization"),
            Some("Bearer at-dummy"),
            "a longer identifier is not the upload route — it takes the pooled token"
        );
        assert_eq!(
            manager.snapshot(OffsetDateTime::now_utc()).accounts[0].requests,
            1,
            "…and is counted against the serving account"
        );
    }

    /// `POST /v1/oauth/token/` — the trailing-slash spelling of the client's own
    /// credential exchange. It used to miss the exact compare and take the POOLED
    /// path, attaching our Bearer to an exchange that is not ours. A raw relay sends
    /// NO authorization at all, which is the whole point of [`RelayMode::Raw`].
    #[tokio::test]
    async fn token_refresh_never_takes_a_pooled_bearer_on_any_spelling() {
        let (upstream, _hits) = spawn_echo_upstream().await;
        for uri in ["/v1/oauth/token", "/v1/oauth/token/"] {
            let manager = Manager::with_live_refresher(dummy_config(None, &upstream), None);
            let (status, body) = drive(
                Arc::clone(&manager),
                Method::POST,
                uri,
                Some(loopback_peer()),
                &[
                    ("authorization", "Bearer client-own-token"),
                    ("content-type", "application/json"),
                ],
                r#"{"grant_type":"refresh_token"}"#,
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            let echo = parse_echo(&body);
            assert_ne!(
                echo.header("authorization"),
                Some("Bearer at-dummy"),
                "{uri} must NEVER carry the pooled account token"
            );
            assert_eq!(
                echo.header("authorization"),
                None,
                "{uri} is a raw relay — it carries no authorization in any form"
            );
            assert_eq!(
                manager.snapshot(OffsetDateTime::now_utc()).accounts[0].requests,
                0,
                "{uri} spends no account"
            );
        }
    }

    /// The relays bypass ROTATION, not the api-key gate. A non-loopback caller with
    /// no key is refused before anything is forwarded — relaying for an
    /// unauthenticated caller would be a new hole, not a fix.
    #[tokio::test]
    async fn relay_paths_stay_behind_the_api_key_gate() {
        let (upstream, hits) = spawn_echo_upstream().await;
        for (method, path) in [
            (Method::POST, "/v1/code/session/abc"),
            (Method::GET, "/api/oauth/files/file_0123"),
            (Method::POST, "/v1/oauth/token"),
        ] {
            let manager = Manager::with_live_refresher(
                dummy_config(Some("sk-proxy-secret"), &upstream),
                None,
            );
            let (status, _body) = drive(
                manager,
                method.clone(),
                path,
                Some(remote_peer()),
                &[("authorization", "Bearer client-own-token")],
                "{}",
            )
            .await;
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "keyless remote {method} {path} must be refused"
            );
        }
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a refused caller's request never reached upstream"
        );
    }

    /// key_matches behaviour is preserved after the length side-channel fix:
    /// equal ⇔ true, and any mismatch (byte or length) ⇔ false.
    #[test]
    fn key_matches_equal_keys_true() {
        assert!(key_matches(Some("sk-proxy-secret"), "sk-proxy-secret"));
    }

    #[test]
    fn key_matches_unequal_same_length_false() {
        // Same length, one byte differs.
        assert!(!key_matches(Some("sk-proxy-secreu"), "sk-proxy-secret"));
    }

    #[test]
    fn key_matches_different_length_false() {
        // Shorter and longer than the configured key, incl. a matching prefix.
        assert!(!key_matches(Some("sk-proxy"), "sk-proxy-secret"));
        assert!(!key_matches(
            Some("sk-proxy-secret-extra"),
            "sk-proxy-secret"
        ));
        assert!(!key_matches(Some("sk-proxy-secret"), "sk-proxy"));
    }

    #[test]
    fn key_matches_edge_cases() {
        assert!(
            !key_matches(None, "sk-proxy-secret"),
            "absent key never matches"
        );
        assert!(
            key_matches(Some(""), ""),
            "empty ⇔ empty (degenerate config)"
        );
        assert!(
            !key_matches(Some("x"), ""),
            "non-empty vs empty configured key"
        );
        assert!(
            !key_matches(Some(""), "x"),
            "empty vs non-empty configured key"
        );
    }

    /// The response-body cap rejects an over-cap stream (BAD_GATEWAY path) while
    /// buffering a within-cap body verbatim and surfacing transport errors.
    #[tokio::test]
    async fn read_capped_body_rejects_oversized() {
        let chunks = vec![
            Ok::<Bytes, Infallible>(Bytes::from_static(b"hello ")),
            Ok::<Bytes, Infallible>(Bytes::from_static(b"world!")),
        ];
        // 12 bytes total against an 8-byte cap → TooLarge (drives BAD_GATEWAY).
        let result = read_capped_body(futures::stream::iter(chunks), 8).await;
        assert!(matches!(result, Err(BodyReadError::TooLarge)));
    }

    #[tokio::test]
    async fn read_capped_body_accepts_within_and_at_cap() {
        let within = vec![
            Ok::<Bytes, Infallible>(Bytes::from_static(b"hello ")),
            Ok::<Bytes, Infallible>(Bytes::from_static(b"world")),
        ];
        let bytes = read_capped_body(futures::stream::iter(within), 1024)
            .await
            .ok()
            .expect("within-cap body buffers");
        assert_eq!(bytes, Bytes::from_static(b"hello world"));

        // A body whose total exactly equals the cap is accepted (only > rejects).
        let at_cap = vec![Ok::<Bytes, Infallible>(Bytes::from_static(b"12345678"))];
        assert!(read_capped_body(futures::stream::iter(at_cap), 8)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn read_capped_body_surfaces_transport_error() {
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from_static(b"partial")),
            Err(std::io::Error::other("boom")),
        ];
        let result = read_capped_body(futures::stream::iter(chunks), 1024).await;
        assert!(matches!(result, Err(BodyReadError::Transport)));
    }

    /// U2e (storm fix): an upstream `401` whose forced refresh produces NO new token
    /// (here: an always-transient refresher, the same `false` return a
    /// cooldown-suppressed force gives) must leave the account **Active** and rotate
    /// away — never `mark_error`. Before the fix a suppressed force retried the same
    /// dead token, hit a 2nd 401, and sidelined a healthy account ~75s (cascade).
    #[tokio::test]
    async fn suppressed_force_401_keeps_account_active_and_rotates() {
        // A refresher that always fails transiently → apply_refresh never runs, so
        // ensure_fresh_force returns false (identical to a cooldown-suppressed force).
        struct AlwaysTransient;
        impl crate::oauth::TokenRefresher for AlwaysTransient {
            fn refresh(&self, _refresh_token: String) -> crate::oauth::RefreshFuture {
                Box::pin(async {
                    Err(crate::oauth::OAuthError::Transient("simulated blip".into()))
                })
            }
        }

        // A canned upstream that answers every connection with a bare 401.
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = upstream.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = upstream.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 1024];
                    let _ = sock.read(&mut buf).await;
                    let _ = sock
                        .write_all(
                            b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                        )
                        .await;
                });
            }
        });

        // No proxy api-key configured → the keyless test request is accepted.
        let manager = Manager::new(
            dummy_config(None, &format!("http://{up_addr}")),
            Arc::new(AlwaysTransient),
            Arc::new(crate::probe::LiveUsageProber::new()),
            Arc::new(crate::warmer::LiveWarmer::new()),
            None,
        );

        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy.local_addr().unwrap();
        let served = manager.clone();
        tokio::spawn(async move {
            let _ = axum::serve(proxy, app(served)).await;
        });

        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let _ = client
            .post(format!("http://{proxy_addr}/v1/messages"))
            .body("{}")
            .send()
            .await
            .unwrap();

        assert_eq!(
            manager.account_status(0),
            Some(AccountStatus::Active),
            "a forced refresh that produced no new token must rotate, not sideline the account"
        );
    }

    /// A SECOND 401 — the retry with a freshly-minted token also 401s — must NOT
    /// condemn the account to the terminal `Error` status. That 2nd 401 is almost
    /// always rotation churn (a concurrent force-refresh superseded the token we
    /// retried with), not a dead credential. `Error` is unrecoverable BY DESIGN —
    /// `probeable_indices`, `Manager::select` and warming all skip errored rows, and
    /// only a successful refresh clears it, which can never run on a row nobody
    /// probes or selects. So condemning a healthy account here sidelines it forever.
    /// Observed live 2026-07-17: all 7 accounts walked to `error` one transient 401
    /// at a time and the proxy went dark while every token still probed 200.
    #[tokio::test]
    async fn second_401_does_not_condemn_account_to_terminal_error() {
        // A refresher that SUCCEEDS: ensure_fresh_force returns true, so the request
        // retries the SAME account, the canned upstream 401s again, and we land on
        // the 2nd-401 path under test.
        struct AlwaysRefreshes;
        impl crate::oauth::TokenRefresher for AlwaysRefreshes {
            fn refresh(&self, _refresh_token: String) -> crate::oauth::RefreshFuture {
                Box::pin(async {
                    Ok(crate::oauth::Tokens {
                        access_token: "fresh-access".into(),
                        refresh_token: "fresh-refresh".into(),
                        expires_at_ms: crate::now_ms() + 3_600_000,
                    })
                })
            }
        }

        // A canned upstream that answers every connection with a bare 401.
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = upstream.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = upstream.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 1024];
                    let _ = sock.read(&mut buf).await;
                    let _ = sock
                        .write_all(
                            b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                        )
                        .await;
                });
            }
        });

        let manager = Manager::new(
            dummy_config(None, &format!("http://{up_addr}")),
            Arc::new(AlwaysRefreshes),
            Arc::new(crate::probe::LiveUsageProber::new()),
            Arc::new(crate::warmer::LiveWarmer::new()),
            None,
        );

        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy.local_addr().unwrap();
        let served = manager.clone();
        tokio::spawn(async move {
            let _ = axum::serve(proxy, app(served)).await;
        });

        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let _ = client
            .post(format!("http://{proxy_addr}/v1/messages"))
            .body("{}")
            .send()
            .await
            .unwrap();

        assert_eq!(
            manager.account_status(0),
            Some(AccountStatus::Active),
            "a 2nd 401 is rotation churn, not a dead credential — it must leave the \
             account Active; `Error` is terminal and would sideline a healthy account \
             forever (nothing re-probes or re-selects an errored row)"
        );
    }

    /// Bounded-poll the manager's decayed stream-error count for account 0 until
    /// it matches `want`, or give up after ~2s. The tee task that records it runs
    /// in a DETACHED `tokio::spawn`, so the client can observe body EOF before
    /// that task has drained its channel and finished the parse loop — reading to
    /// EOF then asserting immediately is racy. Returns the last-observed count and
    /// how many times it polled, so a caller's failure message states what it saw
    /// rather than reading as a hang or a bare timing artifact.
    async fn poll_stream_error_count(manager: &Manager, want: usize) -> (usize, u32) {
        let mut polls = 0u32;
        let mut seen = manager.snapshot(OffsetDateTime::now_utc()).accounts[0].stream_error_count;
        while seen != want && polls < 200 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            seen = manager.snapshot(OffsetDateTime::now_utc()).accounts[0].stream_error_count;
            polls += 1;
        }
        (seen, polls)
    }

    /// THE BUG: an Anthropic error envelope delivered inside a 200
    /// `text/event-stream` body — the same shape this proxy itself synthesizes,
    /// and the shape Anthropic's real upstream uses for a mid-stream failure —
    /// must be OBSERVED, not silently booked as a clean serve. Before the fix,
    /// `parse_sse_usage`'s match had no `error` arm, so the event fell into
    /// `_ => {}` and nothing was recorded.
    #[tokio::test]
    async fn sse_error_event_is_observed_not_counted_as_clean_serve() {
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = upstream.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = upstream.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 1024];
                    let _ = sock.read(&mut buf).await;
                    let body = concat!(
                        "event: error\n",
                        "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
                    );
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(response.as_bytes()).await;
                });
            }
        });

        let manager =
            Manager::with_live_refresher(dummy_config(None, &format!("http://{up_addr}")), None);

        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy.local_addr().unwrap();
        let served = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = axum::serve(proxy, app(served)).await;
        });

        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let resp = client
            .post(format!("http://{proxy_addr}/v1/messages"))
            .body("{}")
            .send()
            .await
            .unwrap();
        // Read the body to completion FIRST — the client can see EOF before the
        // detached tee task has finished parsing (see `poll_stream_error_count`).
        let _ = resp.bytes().await.unwrap();

        let (seen, polls) = poll_stream_error_count(&manager, 1).await;
        assert_eq!(
            seen, 1,
            "polled {polls} times, stream-error count stayed {seen} — an in-band \
             SSE error event must be observed, not silently dropped as a clean serve"
        );

        // The terminal-outcome counter (`record_served`) is UNCHANGED by this —
        // it fires on the upstream's 200 status same as any clean serve, and this
        // fix is deliberately observability-only: it adds a SECOND signal, it
        // does not replace or gate the first.
        assert_eq!(
            manager.snapshot(OffsetDateTime::now_utc()).accounts[0].requests,
            1,
            "the request-served counter is untouched by this fix"
        );
    }

    /// NEGATIVE CONTROL: a clean 200 SSE stream (normal `message_start` /
    /// `message_delta`, no `error` event) must record NO stream error. Uses the
    /// same bounded-poll discipline as the positive case — asserting absence
    /// after a trivially-passing zero-wait would be vacuous.
    #[tokio::test]
    async fn sse_clean_stream_records_no_stream_error() {
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = upstream.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = upstream.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 1024];
                    let _ = sock.read(&mut buf).await;
                    let body = concat!(
                        "event: message_start\n",
                        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n",
                        "event: message_delta\n",
                        "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":9}}\n\n",
                    );
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(response.as_bytes()).await;
                });
            }
        });

        let manager =
            Manager::with_live_refresher(dummy_config(None, &format!("http://{up_addr}")), None);

        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy.local_addr().unwrap();
        let served = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = axum::serve(proxy, app(served)).await;
        });

        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let resp = client
            .post(format!("http://{proxy_addr}/v1/messages"))
            .body("{}")
            .send()
            .await
            .unwrap();
        let _ = resp.bytes().await.unwrap();

        // Poll for the USAGE to land (a positive, waitable signal that the tee
        // task has finished), then assert the error count is absent — never a
        // zero-wait check, which would pass vacuously before the tee even runs.
        let mut polls = 0u32;
        let mut input_tokens = manager.snapshot(OffsetDateTime::now_utc()).accounts[0].input_tokens;
        while input_tokens == 0 && polls < 200 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            input_tokens = manager.snapshot(OffsetDateTime::now_utc()).accounts[0].input_tokens;
            polls += 1;
        }
        assert_eq!(
            input_tokens, 5,
            "polled {polls} times waiting for usage to land"
        );

        assert_eq!(
            manager.snapshot(OffsetDateTime::now_utc()).accounts[0].stream_error_count,
            0,
            "a clean SSE stream must record no stream error"
        );
    }

    /// G1 (best-practices review, Apollo ch5.3 integration): `patch_account_uuid`
    /// is thoroughly UNIT-tested, but the proxy wiring at the call site is not — every
    /// other integration test posts a bare `{}`, so the rewrite branch never fires on
    /// a real request. Drive one whose `metadata.user_id` blob carries a DIFFERENT
    /// account_uuid and assert, from what the UPSTREAM actually received, that the
    /// serving account's uuid was substituted, the client's is gone, and the body
    /// length is unchanged (the whole point of the same-length patch — it keeps the
    /// buffered request's Content-Length valid in flight).
    #[tokio::test]
    async fn account_uuid_is_rewritten_to_the_serving_account_on_a_real_request() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        const CLIENT_UUID: &str = "11111111-1111-1111-1111-111111111111";
        const ACCT_UUID: &str = "22222222-2222-2222-2222-222222222222"; // both 36 chars

        struct NoRefresh;
        impl crate::oauth::TokenRefresher for NoRefresh {
            fn refresh(&self, _t: String) -> crate::oauth::RefreshFuture {
                Box::pin(async { Err(crate::oauth::OAuthError::Transient("unused".into())) })
            }
        }

        // Echo upstream: capture the full forwarded request (headers + Content-Length
        // body), then answer 200.
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = upstream.local_addr().unwrap();
        let captured = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
        let cap = captured.clone();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = upstream.accept().await {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                loop {
                    match sock.read(&mut tmp).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    }
                    if let Some(h) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&buf[..h]).to_lowercase();
                        let cl = headers
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if buf.len() >= h + 4 + cl {
                            break;
                        }
                    }
                }
                *cap.lock().await = buf;
                let _ = sock
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                    )
                    .await;
            }
        });

        let mut config = dummy_config(None, &format!("http://{up_addr}"));
        config.accounts[0].account_uuid = Some(ACCT_UUID.to_string());
        let manager = Manager::new(
            config,
            Arc::new(NoRefresh),
            Arc::new(crate::probe::LiveUsageProber::new()),
            Arc::new(crate::warmer::LiveWarmer::new()),
            None,
        );
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy.local_addr().unwrap();
        let served = manager.clone();
        tokio::spawn(async move {
            let _ = axum::serve(proxy, app(served)).await;
        });

        let inner = format!(r#"{{"account_uuid":"{CLIENT_UUID}","subscriptionType":"pro"}}"#);
        let body = serde_json::to_vec(&serde_json::json!({
            "model": "claude-x",
            "metadata": { "user_id": inner },
            "messages": [],
        }))
        .unwrap();
        let sent_len = body.len();

        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let _ = client
            .post(format!("http://{proxy_addr}/v1/messages"))
            .body(body)
            .send()
            .await
            .unwrap();

        let received = captured.lock().await.clone();
        assert!(
            !received.is_empty(),
            "upstream received the forwarded request"
        );
        let body_at = received
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|i| i + 4)
            .expect("request has a header/body separator");
        let up_body = &received[body_at..];
        let up_body_str = String::from_utf8_lossy(up_body);
        assert!(
            up_body_str.contains(ACCT_UUID),
            "the serving account's uuid must be injected into the forwarded body"
        );
        assert!(
            !up_body_str.contains(CLIENT_UUID),
            "the client's account_uuid must be overwritten, not forwarded"
        );
        assert_eq!(
            up_body.len(),
            sent_len,
            "the same-length patch must preserve the body length so Content-Length stays valid"
        );
    }

    /// End-to-end affinity + cache surfacing over a REAL 2-account fleet: two
    /// requests carrying the SAME `metadata.user_id` must pin to ONE account
    /// (requests 2 / 0 — the seam no unit test spans), and that account's snapshot
    /// must expose the upstream's `cache_read_input_tokens` separately (not summed
    /// away into the quota total). The `SessionKey` extension is injected by a
    /// layer here exactly as the hybrid CONNECT server (`mitm.rs`) does per
    /// connection — `app()` alone injects none, so without it affinity is inert.
    #[tokio::test]
    async fn same_user_id_pins_one_account_and_surfaces_cache_read() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        struct NoRefresh;
        impl crate::oauth::TokenRefresher for NoRefresh {
            fn refresh(&self, _t: String) -> crate::oauth::RefreshFuture {
                Box::pin(async { Err(crate::oauth::OAuthError::Transient("unused".into())) })
            }
        }

        // Fake upstream: every connection gets a 200 whose JSON `usage` carries a
        // non-zero `cache_read_input_tokens` (a warm-cache turn). Loops so BOTH
        // forwarded requests are answered.
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = upstream.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = upstream.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    let body = br#"{"usage":{"input_tokens":10,"cache_creation_input_tokens":0,"cache_read_input_tokens":500,"output_tokens":20}}"#;
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(body).await;
                });
            }
        });

        // A 2-account fleet pointed at the one fake upstream.
        let mut config = dummy_config(None, &format!("http://{up_addr}"));
        let mut second = config.accounts[0].clone();
        second.name = "dummy2".to_string();
        config.accounts.push(second);
        let manager = Manager::new(
            config,
            Arc::new(NoRefresh),
            Arc::new(crate::probe::LiveUsageProber::new()),
            Arc::new(crate::warmer::LiveWarmer::new()),
            None,
        );

        // Inject a DISTINCT per-request SessionKey the way mitm.rs does (its
        // `next_session_key()` = `session_seq.fetch_add(1)` hands every connection
        // a unique key). Distinct fallback keys are load-bearing to the proof: if
        // `metadata.user_id` extraction were dead, key resolution would fall back
        // to these distinct per-connection keys and the two requests would SPREAD
        // to 2 accounts (test fails). Only the SHARED `user_id` "cache-affinity-user"
        // resolving to one stable key can pin them 2/0 — so a green test proves the
        // body's user_id is what pins, not merely affinity-on.
        use std::sync::atomic::{AtomicU64, Ordering};
        let served = manager.clone();
        let seq = Arc::new(AtomicU64::new(1));
        let affinity_app = app(served).layer(axum::middleware::from_fn(
            move |mut req: axum::extract::Request, next: axum::middleware::Next| {
                let seq = seq.clone();
                async move {
                    req.extensions_mut()
                        .insert(SessionKey(seq.fetch_add(1, Ordering::Relaxed)));
                    next.run(req).await
                }
            },
        ));
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(proxy, affinity_app).await;
        });

        let body = serde_json::to_vec(&serde_json::json!({
            "model": "claude-x",
            "metadata": { "user_id": "cache-affinity-user" },
            "messages": [],
        }))
        .unwrap();

        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        for _ in 0..2 {
            let _ = client
                .post(format!("http://{proxy_addr}/v1/messages"))
                .body(body.clone())
                .send()
                .await
                .unwrap();
        }

        let snap = manager.snapshot(OffsetDateTime::now_utc());
        let reqs: Vec<u64> = snap.accounts.iter().map(|a| a.requests).collect();
        // Affinity end-to-end: one account served BOTH, the other served NONE.
        assert!(
            reqs.contains(&2),
            "one account must serve both same-user_id requests, got {reqs:?}"
        );
        assert!(
            reqs.contains(&0),
            "the other account must serve none (affinity pinned the session), got {reqs:?}"
        );
        // Cache surfacing: the serving account exposes the retained cache-read
        // tokens (500 per request x2), NOT summed away into the quota total.
        let serving = snap
            .accounts
            .iter()
            .find(|a| a.requests == 2)
            .expect("a serving account exists");
        assert!(
            serving.cache_read_tokens > 0,
            "cache-read must be surfaced on the serving account"
        );
        assert_eq!(
            serving.cache_read_tokens, 1000,
            "500 cache-read per request, twice"
        );
    }

    /// A fake upstream that answers by CONNECTION ORDINAL, so one test can script the
    /// exact per-attempt sequence the rotation loop sees. `None` = read the request
    /// and hang up without replying, which the client surfaces as a TRANSPORT failure
    /// (the shape the 502 decision turns on); `Some(raw)` = write `raw` verbatim.
    /// Connections past the end of the script reuse the last entry. Every scripted
    /// reply must send `connection: close` so the client pool cannot reuse a socket
    /// and attempt N is always connection N.
    async fn spawn_scripted_upstream(script: Vec<Option<String>>) -> SocketAddr {
        spawn_counted_upstream(script).await.0
    }

    /// [`spawn_scripted_upstream`] plus a live count of accepted connections —
    /// i.e. of upstream ATTEMPTS. Retries are invisible in the account stats (only
    /// the terminal outcome is recorded), so a test that must pin the exact number
    /// of sends reads it here instead of inferring it from the client's status.
    async fn spawn_counted_upstream(
        script: Vec<Option<String>>,
    ) -> (SocketAddr, Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = attempts.clone();
        tokio::spawn(async move {
            let mut n = 0usize;
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                // Ordinal is assigned in ACCEPT order, before the per-connection task
                // is spawned, so it cannot race with a concurrent handler.
                let reply = script.get(n).or_else(|| script.last()).cloned().flatten();
                n += 1;
                counter.store(n, std::sync::atomic::Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    match reply {
                        Some(raw) => {
                            let _ = sock.write_all(raw.as_bytes()).await;
                        }
                        // Drop with no reply: "connection closed before message
                        // completed" on the client side — a transport error.
                        None => drop(sock),
                    }
                });
            }
        });
        (addr, attempts)
    }

    /// A `200 OK` with a minimal JSON usage body.
    fn raw_200() -> String {
        let body = br#"{"usage":{"input_tokens":1,"output_tokens":1}}"#;
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            String::from_utf8_lossy(body)
        )
    }

    /// A `429` reporting DURABLE quota rejection. `is_quota_rejected` matches on
    /// `unified-status: rejected`, which is the ONE 429 arm that parks the account
    /// and rotates with no inline sleep — so a test using it stays fast and has no
    /// timing dependency.
    fn raw_429_rejected(retry_after: u32) -> String {
        format!(
            "HTTP/1.1 429 Too Many Requests\r\ncontent-length: 0\r\nconnection: close\r\n\
             retry-after: {retry_after}\r\nanthropic-ratelimit-unified-status: rejected\r\n\r\n"
        )
    }

    /// The per-account header block a real Anthropic response carries, as raw
    /// header lines. Includes `requests-remaining` and `tokens-limit` alongside the
    /// `unified-*` family precisely because the proxy itself only reads `unified-*`:
    /// a strip that enumerated the names it knows would forward these two.
    const ACCOUNT_HEADER_LINES: &str = "anthropic-ratelimit-unified-status: allowed\r\n\
         anthropic-ratelimit-unified-5h-utilization: 0.42\r\n\
         anthropic-ratelimit-unified-7d-utilization: 1.0\r\n\
         anthropic-ratelimit-unified-7d-reset: 1800000000\r\n\
         anthropic-ratelimit-requests-remaining: 7\r\n\
         anthropic-ratelimit-tokens-limit: 40000\r\n\
         anthropic-organization-id: org-01234567\r\n";

    /// A `200 OK` carrying the full per-account header set, plus the two headers
    /// that must SURVIVE: `request-id` (a request id, not an account id) and an
    /// ordinary `anthropic-version` standing in for every unrelated header.
    fn raw_200_with_account_headers() -> String {
        let body = br#"{"usage":{"input_tokens":1,"output_tokens":1}}"#;
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
             connection: close\r\n{ACCOUNT_HEADER_LINES}\
             request-id: req_011CabcdEFGH\r\nanthropic-version: 2023-06-01\r\n\r\n{}",
            body.len(),
            String::from_utf8_lossy(body)
        )
    }

    /// [`raw_429_rejected`] carrying the per-account headers a live rejection does.
    /// The quota model is built from exactly these, so this is the reply that shows
    /// whether the client-boundary strip blinded it. `unified-status` is `rejected`
    /// here, so it cannot share [`ACCOUNT_HEADER_LINES`]'s `allowed`.
    fn raw_429_rejected_with_account_headers(retry_after: u32) -> String {
        format!(
            "HTTP/1.1 429 Too Many Requests\r\ncontent-length: 0\r\nconnection: close\r\n\
             retry-after: {retry_after}\r\n\
             anthropic-ratelimit-unified-status: rejected\r\n\
             anthropic-ratelimit-unified-5h-utilization: 0.42\r\n\
             anthropic-ratelimit-unified-7d-utilization: 1.0\r\n\
             anthropic-ratelimit-unified-7d-reset: 1800000000\r\n\
             anthropic-ratelimit-requests-remaining: 7\r\n\
             anthropic-ratelimit-tokens-limit: 40000\r\n\
             anthropic-organization-id: org-01234567\r\n\r\n"
        )
    }

    /// A `429` with NO `unified-status`, i.e. a TRANSIENT rate limit: the arm that
    /// inline-waits `retry_after` seconds on the same account and, once
    /// [`MAX_SAME_ACCOUNT_429`] inline retries are spent, PARKS it for that long and
    /// rotates — the `Transient429::Park` that puts an account into BOTH
    /// `mark_rate_limited` and `tried`. `retry_after: 1` is what keeps a test that
    /// must reach the park down to two one-second inline waits.
    fn raw_429_transient(retry_after: u32) -> String {
        format!(
            "HTTP/1.1 429 Too Many Requests\r\ncontent-length: 0\r\nconnection: close\r\n\
             retry-after: {retry_after}\r\n\r\n"
        )
    }

    /// A bare `401`. With the `fleet` refresher (every refresh a transient error)
    /// the force-refresh yields no new token, so the account is benched in `tried`
    /// and the request rotates — an entry no timer ever clears.
    fn raw_401() -> String {
        "HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_string()
    }

    /// A `529 Overloaded` shaped like Anthropic's — the status the retry ladder
    /// exists for. Carries NO `retry-after` (the live captures do not), so the
    /// backoff comes from the ladder alone and the test's timing is deterministic.
    fn raw_529() -> String {
        let body =
            br#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
        format!(
            "HTTP/1.1 529 \r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            String::from_utf8_lossy(body)
        )
    }

    /// Boot an N-account fleet (all cloned from `dummy`) pointed at `upstream`.
    fn fleet(upstream: SocketAddr, names: &[&str]) -> Arc<Manager> {
        struct NoRefresh;
        impl crate::oauth::TokenRefresher for NoRefresh {
            fn refresh(&self, _t: String) -> crate::oauth::RefreshFuture {
                Box::pin(async { Err(crate::oauth::OAuthError::Transient("unused".into())) })
            }
        }
        let mut config = dummy_config(None, &format!("http://{upstream}"));
        for name in names {
            let mut extra = config.accounts[0].clone();
            extra.name = (*name).to_string();
            config.accounts.push(extra);
        }
        config.accounts.remove(0); // keep exactly `names`, in order
        Manager::new(
            config,
            Arc::new(NoRefresh),
            Arc::new(crate::probe::LiveUsageProber::new()),
            Arc::new(crate::warmer::LiveWarmer::new()),
            None,
        )
    }

    /// [`post_one`] plus the client-visible BODY — for asserting that a status the
    /// proxy gives up on is forwarded verbatim, not replaced by a synthesized one.
    async fn post_one_with_body(manager: Arc<Manager>) -> (u16, String) {
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(proxy, app(manager)).await;
        });
        let body = serde_json::to_vec(&serde_json::json!({ "model": "claude-x", "messages": [] }))
            .unwrap();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let resp = client
            .post(format!("http://{proxy_addr}/v1/messages"))
            .body(body)
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        (status, resp.text().await.unwrap())
    }

    /// Serve `manager` on a fresh loopback listener and POST one `/v1/messages`,
    /// returning the client-visible status.
    async fn post_one(manager: Arc<Manager>) -> u16 {
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(proxy, app(manager)).await;
        });
        let body = serde_json::to_vec(&serde_json::json!({ "model": "claude-x", "messages": [] }))
            .unwrap();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        client
            .post(format!("http://{proxy_addr}/v1/messages"))
            .body(body)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    /// [`post_one`] with session affinity LIVE, so a test can assert what happened to
    /// the session's PIN and not just to the client's status.
    ///
    /// Two things have to be true for a request to be pinned, and `app()` alone
    /// provides neither: the [`SessionKey`] extension must be present (injected here
    /// by a layer, exactly as the hybrid CONNECT server does per connection) and the
    /// request must carry a stable identity for [`stable_session_key`] to hash —
    /// hence `metadata.user_id` in the body. The pin lives in the MANAGER, so two
    /// calls with the same `user_id` are two requests of ONE session even though each
    /// call serves the app on a fresh listener.
    async fn post_one_pinned(manager: Arc<Manager>, user_id: &str) -> u16 {
        let affinity_app = app(manager).layer(axum::middleware::from_fn(
            move |mut req: axum::extract::Request, next: axum::middleware::Next| async move {
                // The value is a per-connection ordinal upstream and is deliberately
                // NOT the routing key (`stable_session_key` never falls back to it),
                // so a constant is as faithful here as a counter.
                req.extensions_mut().insert(SessionKey(1));
                next.run(req).await
            },
        ));
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(proxy, affinity_app).await;
        });
        let body = serde_json::to_vec(&serde_json::json!({
            "model": "claude-x",
            "metadata": { "user_id": user_id },
            "messages": [],
        }))
        .unwrap();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        client
            .post(format!("http://{proxy_addr}/v1/messages"))
            .body(body)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    /// The every-account-scoped header block the upstream in these tests sends, by
    /// name. Spelled out here — rather than derived from [`is_account_scoped`] — so
    /// the assertions describe the WIRE, not the predicate under test.
    const LEAKED_HEADER_NAMES: &[&str] = &[
        "anthropic-ratelimit-unified-status",
        "anthropic-ratelimit-unified-5h-utilization",
        "anthropic-ratelimit-unified-7d-utilization",
        "anthropic-ratelimit-unified-7d-reset",
        "anthropic-ratelimit-requests-remaining",
        "anthropic-ratelimit-tokens-limit",
        "anthropic-organization-id",
    ];

    /// The classifier alone. The `request-id` and `anthropic-version` negatives are
    /// the point: a prefix loose enough to swallow either would strip the one id
    /// that makes a failed call traceable.
    #[test]
    fn is_account_scoped_matches_the_family_and_nothing_else() {
        for name in LEAKED_HEADER_NAMES {
            assert!(is_account_scoped(name), "{name} belongs to the account");
        }
        // Not yet invented, but the prefix must already cover it.
        assert!(is_account_scoped("anthropic-ratelimit-something-new"));
        for name in [
            "request-id",
            "anthropic-version",
            "content-type",
            "retry-after",
            "anthropic-organization",  // not the id
            "x-anthropic-ratelimit-a", // the family is not a substring match
        ] {
            assert!(!is_account_scoped(name), "{name} must survive");
        }
    }

    /// THE REPORTED BUG: a rotated response must reach the client carrying NONE of
    /// the serving account's quota or org headers.
    ///
    /// Claude Code renders its usage banner from `anthropic-ratelimit-unified-*`,
    /// and with 13 accounts in rotation those headers describe a different account
    /// on every request — the upstream here reports `7d-utilization: 1.0`, i.e. the
    /// exact shape that made a freshly-opened session show a usage-limit banner.
    #[tokio::test]
    async fn rotated_response_strips_every_account_header() {
        let up_addr = spawn_scripted_upstream(vec![Some(raw_200_with_account_headers())]).await;
        let manager =
            Manager::with_live_refresher(dummy_config(None, &format!("http://{up_addr}")), None);
        let response = drive_messages(manager).await;

        assert_eq!(response.status(), StatusCode::OK);
        for name in LEAKED_HEADER_NAMES {
            assert!(
                response.headers().get(*name).is_none(),
                "{name} describes the serving account, not the caller"
            );
        }
        let leaked: Vec<&str> = response
            .headers()
            .keys()
            .map(|k| k.as_str())
            .filter(|k| k.starts_with("anthropic-ratelimit"))
            .collect();
        assert!(
            leaked.is_empty(),
            "no rate-limit header may survive, by prefix: {leaked:?}"
        );
    }

    /// The over-stripping guard. `request-id` identifies a REQUEST — it is what
    /// makes one failed call traceable to Anthropic — and every unrelated upstream
    /// header is still the client's to see. Their presence is also what proves the
    /// test above can fail: these arrived over the same wire.
    #[tokio::test]
    async fn rotated_response_keeps_request_id_and_unrelated_headers() {
        let up_addr = spawn_scripted_upstream(vec![Some(raw_200_with_account_headers())]).await;
        let manager =
            Manager::with_live_refresher(dummy_config(None, &format!("http://{up_addr}")), None);
        let response = drive_messages(manager).await;

        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        assert_eq!(
            header("request-id").as_deref(),
            Some("req_011CabcdEFGH"),
            "request-id identifies a request, not an account"
        );
        assert_eq!(
            header("anthropic-version").as_deref(),
            Some("2023-06-01"),
            "an unrelated upstream header is untouched"
        );
        assert_eq!(header("content-type").as_deref(), Some("application/json"));
    }

    /// THE REGRESSION GUARD THAT MATTERS: the strip is at the CLIENT boundary, so
    /// everything upstream of it still sees the untouched headers.
    ///
    /// A durable `429` must still (a) set the account's quota status to `rejected`,
    /// which is the red REJECTED state in the TUI and the hard gate that re-keys a
    /// session pin, (b) arm the rate-limit hold, and (c) fold the reported weekly
    /// utilization into the quota model. Strip at ingest instead and all three go
    /// silently dark while the client-facing bug still looks fixed.
    #[tokio::test]
    async fn durable_429_still_rejects_the_account_after_the_strip() {
        let up_addr =
            spawn_scripted_upstream(vec![Some(raw_429_rejected_with_account_headers(120))]).await;
        let manager = fleet(up_addr, &["a"]);
        let response = drive_messages(Arc::clone(&manager)).await;

        // The only account is held out, so the client gets the synthesized
        // fleet-exhausted 429 — which carries no upstream headers by construction.
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        for name in LEAKED_HEADER_NAMES {
            assert!(
                response.headers().get(*name).is_none(),
                "{name} is stripped"
            );
        }

        let account = &manager.snapshot(OffsetDateTime::now_utc()).accounts[0];
        assert_eq!(
            account.gate,
            crate::stats::GateReason::Rejected,
            "`update_quota` must still have seen `unified-status: rejected`"
        );
        assert!(
            account.rate_limited_until.is_some(),
            "the durable-rejection hold must still be armed"
        );
        assert_eq!(
            account.seven_day,
            Some(1.0),
            "the reported weekly utilization must still reach the quota model"
        );
    }

    /// The relay paths are served with the CALLER's own credential, so their
    /// rate-limit and org headers describe the caller and are coherent. Stripping
    /// them would hide the caller's real quota from them — the inverse of the bug.
    #[tokio::test]
    async fn relay_response_keeps_its_account_headers() {
        for path in [
            "/v1/code/session/abc",
            "/api/oauth/files/file_0123",
            "/api/oauth/file_upload",
        ] {
            let up_addr = spawn_scripted_upstream(vec![Some(raw_200_with_account_headers())]).await;
            let manager = Manager::with_live_refresher(
                dummy_config(None, &format!("http://{up_addr}")),
                None,
            );
            let response = drive_full(
                manager,
                Method::POST,
                path,
                Some(loopback_peer()),
                &[
                    ("authorization", "Bearer client-own-token"),
                    ("content-type", "application/json"),
                ],
                "{}",
            )
            .await;

            assert_eq!(response.status(), StatusCode::OK, "{path} was relayed");
            for name in LEAKED_HEADER_NAMES {
                assert!(
                    response.headers().get(*name).is_some(),
                    "{name} is the CALLER's own on {path} and must pass through"
                );
            }
        }
    }

    /// One rotated `/v1/messages` through the router, returning the whole response.
    async fn drive_messages(manager: Arc<Manager>) -> Response {
        drive_full(
            manager,
            Method::POST,
            "/v1/messages",
            Some(loopback_peer()),
            &[("content-type", "application/json")],
            r#"{"model":"claude-sonnet-5","messages":[]}"#,
        )
        .await
    }

    /// R2 — ONE transport blip must not disable the whole recovery ladder.
    ///
    /// The counter this asserts on used to be a BOOL (`saw_network_error`) set by any
    /// `send()` failure and never cleared, checked BEFORE both the soft-wait and the
    /// revalidation-serve. So a single blip on a single account short-circuited every
    /// recovery path for the rest of the request and returned a 502 claiming "every
    /// account transport-failed" — false, and observed live at 09:16:46 while two
    /// accounts had already answered with honest 429s and a third served a 200 1.6s
    /// later.
    ///
    /// The scripted sequence is exactly that shape: `a` blips, `b` answers with a
    /// durable 429 (an upstream ANSWER — proof the network path works), and `c` is
    /// over the SOFT threshold so normal `select` benches it while the
    /// revalidation-serve may still use it. Reaching `c` at all requires surviving
    /// the check the bool used to fail. Old code: 502. New code: `c` serves a 200.
    #[tokio::test]
    async fn transport_blip_does_not_disable_recovery() {
        let up_addr = spawn_scripted_upstream(vec![
            None,                        // attempt 1 — `a` fails in transport
            Some(raw_429_rejected(120)), // attempt 2 — `b` answers, durably 429
            Some(raw_200()),             // attempt 3 — `c` serves via revalidation
        ])
        .await;
        let manager = fleet(up_addr, &["a", "b", "c"]);

        // Drive `c` OVER the soft switch threshold (0.90 in `dummy_config`) on the
        // shared weekly window with a reset 48h out, via the same public path a real
        // upstream response takes. `select` now benches it — so attempts 1 and 2 must
        // land on `a`/`b` — while `select_revalidation` still allows it, since an
        // over-threshold `allowed_warning` account is a SOFT block, not a hard one.
        let mut over = HeaderMap::new();
        over.insert(
            "anthropic-ratelimit-unified-7d-utilization",
            HeaderValue::from_static("0.99"),
        );
        over.insert(
            "anthropic-ratelimit-unified-7d-reset",
            HeaderValue::from_str(&(crate::now_ms() / 1000 + 172_800).to_string()).unwrap(),
        );
        over.insert(
            "anthropic-ratelimit-unified-status",
            HeaderValue::from_static("allowed_warning"),
        );
        manager.update_quota(2, &over);

        let status = post_one(manager.clone()).await;
        assert_eq!(
            status, 200,
            "one transport blip alongside a real upstream 429 must not synthesize a \
             502 — the recovery ladder still had a servable account"
        );

        let snap = manager.snapshot(OffsetDateTime::now_utc());
        let served: Vec<&str> = snap
            .accounts
            .iter()
            .filter(|a| a.requests > 0)
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(
            served,
            ["c"],
            "the revalidation-serve account must be the one that served, exactly once"
        );
    }

    /// R2 over-correction guard — when transport failure really IS the whole story,
    /// the 502 stays. Every account hangs up without replying, so no attempt ever
    /// reaches an upstream HTTP status and `every_attempt_transport_failed` holds.
    /// This is what stops the fix above from turning a genuinely unreachable upstream
    /// into a misleading 429.
    #[tokio::test]
    async fn all_transport_failures_still_502() {
        let up_addr = spawn_scripted_upstream(vec![None]).await; // every connection blips
        let manager = fleet(up_addr, &["a", "b"]);

        let status = post_one(manager.clone()).await;
        assert_eq!(
            status, 502,
            "no attempt reached an upstream at all — 502 is the honest verdict"
        );

        let snap = manager.snapshot(OffsetDateTime::now_utc());
        let total: u64 = snap.accounts.iter().map(|a| a.requests).sum();
        assert_eq!(
            total, 0,
            "a transport failure serves nothing, so nothing counts"
        );
    }

    /// A 529 is transient upstream overload, so it is retried IN PLACE — the client
    /// sees the eventual 200 instead of an error it has to re-send by hand.
    ///
    /// The fleet has two accounts and selection is LRU, so a fresh fleet picks `a`
    /// first and would pick `b` on any rotation. Scripting `529` then `200` makes
    /// the two behaviours distinguishable in the stats: retry-in-place credits `a`
    /// with the 200, a rotation credits `b`. Before this arm existed the client got
    /// the 529 straight through and neither account served anything.
    #[tokio::test]
    async fn overloaded_529_retries_the_same_account() {
        let up_addr = spawn_scripted_upstream(vec![
            Some(raw_529()), // attempt 1 — upstream reports itself overloaded
            Some(raw_200()), // attempt 2 — the SAME account, after the backoff
        ])
        .await;
        let manager = fleet(up_addr, &["a", "b"]);

        let status = post_one(manager.clone()).await;
        assert_eq!(
            status, 200,
            "a 529 is transient — the retry must serve the client the 200 rather \
             than surfacing an error it has to re-send by hand"
        );

        let snap = manager.snapshot(OffsetDateTime::now_utc());
        let served: Vec<&str> = snap
            .accounts
            .iter()
            .filter(|a| a.requests > 0)
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(
            served,
            ["a"],
            "the retry must reuse the account that 529'd — landing on `b` means the \
             529 rotated and threw away a warm prompt cache for nothing"
        );
        let a = snap.accounts.iter().find(|a| a.name == "a").unwrap();
        assert_eq!(
            a.requests, 1,
            "only the terminal outcome counts: a retried 529 is not a served request"
        );
    }

    /// The ladder is bounded, and on exhaustion the 529 is forwarded VERBATIM —
    /// the pre-fix behaviour, just later. Both halves run concurrently because each
    /// pays the real 1s + 2s backoff.
    ///
    /// Two scripts pin the attempt count from both sides: with the budget exactly
    /// spent the third send lands on a scripted 200 (proving both retries ran), and
    /// with one 529 too many the fourth send never happens, so the 200 behind it is
    /// unreachable and the client gets the 529.
    ///
    /// The SINGLE-account fleet is load-bearing, not incidental: it is what keeps this
    /// the IN-PLACE budget's test after the failover stage was added. With a second
    /// account the spent budget would fail over instead of giving up, and this would
    /// silently become a (worse) copy of `overloaded_529_fails_over_to_another_account`.
    /// The failover's own bound is `overloaded_529_failover_is_bounded`.
    #[tokio::test]
    async fn overloaded_529_gives_up_after_the_budget() {
        let exact = async {
            let up_addr = spawn_scripted_upstream(vec![
                Some(raw_529()),
                Some(raw_529()),
                Some(raw_200()), // reachable only if BOTH retries fire
            ])
            .await;
            post_one(fleet(up_addr, &["a"])).await
        };
        let over = async {
            let (up_addr, attempts) = spawn_counted_upstream(vec![
                Some(raw_529()),
                Some(raw_529()),
                Some(raw_529()),
                Some(raw_200()), // must stay unreachable — the budget is spent
            ])
            .await;
            let (status, body) = post_one_with_body(fleet(up_addr, &["a"])).await;
            (
                status,
                body,
                attempts.load(std::sync::atomic::Ordering::SeqCst),
            )
        };
        let (exact_status, (over_status, over_body, over_attempts)) = tokio::join!(exact, over);

        assert_eq!(
            exact_status, 200,
            "the budget is {MAX_SAME_ACCOUNT_529_RETRIES} retries — the third send \
             must still happen and serve the client"
        );
        assert_eq!(
            over_status, 529,
            "one 529 past the budget must terminate, not retry forever — the \
             scripted 200 behind it proves the send never happened"
        );
        assert_eq!(
            over_attempts,
            MAX_SAME_ACCOUNT_529_RETRIES as usize + 1,
            "the ladder is exactly one send plus {MAX_SAME_ACCOUNT_529_RETRIES} retries"
        );
        assert!(
            over_body.contains("overloaded_error"),
            "give-up forwards the upstream 529 verbatim, exactly as before this arm \
             existed; a synthesized body would break clients that parse it. Got: {over_body}"
        );
    }

    /// The constraint that survives the failover stage, and the one no client ever
    /// sees: a 529 must not cost the account its ELIGIBILITY. It means "this send was
    /// refused", never "this account is over quota", so `mark_rate_limited` is the one
    /// call this arm may never make — a hold would bench a healthy account for every
    /// OTHER request in flight, and only a later request can observe that.
    ///
    /// A single-account fleet, so the failover has nowhere to go and the give-up path
    /// runs: request 1 spends the in-place budget and gets the 529 forwarded. The bite
    /// is request 2 on the SAME manager — it must be served a 200 by the SAME account,
    /// which is only possible if the 529 left behind neither a hold nor a durable
    /// bench (`tried` is per-request and dies with the request).
    ///
    /// This was `overloaded_529_does_not_rotate_or_hold` and asserted a no-rotation
    /// half too. That half is now false BY DESIGN; what bounds the rotation instead is
    /// `overloaded_529_failover_is_bounded`.
    #[tokio::test]
    async fn overloaded_529_never_arms_a_hold_or_benches_the_account() {
        let (up_addr, attempts) = spawn_counted_upstream(vec![
            Some(raw_529()),
            Some(raw_529()),
            Some(raw_529()),
            Some(raw_200()), // request 2 — reachable only if `a` is still eligible
        ])
        .await;
        let manager = fleet(up_addr, &["a"]);

        assert_eq!(
            post_one(manager.clone()).await,
            529,
            "one account means nothing to fail over to, so the in-place ladder is the \
             whole budget and the upstream's 529 is forwarded"
        );
        // Without this the test passes vacuously on a proxy that never retries at
        // all: one 529 forwarded straight through leaves the account unheld too.
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            MAX_SAME_ACCOUNT_529_RETRIES as usize + 1,
            "exactly one initial send plus {MAX_SAME_ACCOUNT_529_RETRIES} retries"
        );

        let snap = manager.snapshot(OffsetDateTime::now_utc());
        let a = snap.accounts.iter().find(|x| x.name == "a").unwrap();
        assert!(
            a.rate_limited_until.is_none(),
            "a 529 is upstream overload, not a quota rejection: it must never arm a \
             rate-limit hold that would bench the account for later requests"
        );
        assert_eq!(
            a.gate,
            crate::stats::GateReason::Ok,
            "no hard gate of any kind may be left behind — the account stays in rotation"
        );
        assert_eq!(
            a.requests, 1,
            "the forwarded 529 counts once — the retries must not each count as served"
        );

        // The durable half of the claim, which only a second request can make.
        assert_eq!(
            post_one(manager.clone()).await,
            200,
            "the 529'd account must be fully eligible for the NEXT request"
        );
        let snap = manager.snapshot(OffsetDateTime::now_utc());
        let a = snap.accounts.iter().find(|x| x.name == "a").unwrap();
        assert_eq!(a.requests, 2, "both requests were served by `a`");
    }

    /// The failover, end to end, together with the pin invariant that makes it safe.
    ///
    /// `a` is overloaded for its whole in-place budget while `b` is healthy, so the
    /// client must see `b`'s 200 — before this stage existed it saw `a`'s 529 and
    /// re-sent by hand. The SESSION, meanwhile, must still be pinned to `a`: a 529 is
    /// a fact about one request, not about the account. `tried.insert` buys both at
    /// once, because it sends `select` down its "pin-tried" branch, which diverts THIS
    /// REQUEST and keeps the pin.
    ///
    /// `sessions[0].account` is read from the affinity map (the sole authority on the
    /// pin) and `last_served_account` from the serve, so the two DIFFERING is the
    /// assertion: one request moved, the session did not. The fifth scripted reply
    /// proves it behaviourally — the session's next request comes home to `a`.
    #[tokio::test]
    async fn overloaded_529_fails_over_to_another_account() {
        let (up_addr, attempts) = spawn_counted_upstream(vec![
            Some(raw_529()), // `a` — send 1
            Some(raw_529()), // `a` — in-place retry 1
            Some(raw_529()), // `a` — in-place retry 2, budget now spent
            Some(raw_200()), // the failover target serves the client
            Some(raw_200()), // the session's NEXT request, back home on `a`
        ])
        .await;
        let manager = fleet(up_addr, &["a", "b"]);
        const SESSION: &str = "529-failover-session";

        assert_eq!(
            post_one_pinned(manager.clone(), SESSION).await,
            200,
            "the overload is account-scoped, so a sibling can serve: the client must \
             get the 200 rather than a 529 it has to re-send by hand"
        );
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            MAX_SAME_ACCOUNT_529_RETRIES as usize + 2,
            "the in-place ladder still runs in FULL on `a` first — the failover is the \
             send AFTER it, not a replacement for it"
        );

        let snap = manager.snapshot(OffsetDateTime::now_utc());
        let a = snap.accounts.iter().find(|x| x.name == "a").unwrap();
        let b = snap.accounts.iter().find(|x| x.name == "b").unwrap();
        assert_eq!(
            (a.requests, b.requests),
            (0, 1),
            "only the terminal outcome counts, and it happened on `b`"
        );
        assert!(
            a.rate_limited_until.is_none(),
            "the failover benches `a` for THIS REQUEST only — no hold may be armed"
        );
        let session = snap
            .sessions
            .first()
            .expect("the pinned session is visible in the snapshot");
        assert_eq!(
            session.account, "a",
            "the pin must NOT move: a 529 diverts one request, and re-keying the \
             session would cold-start its whole prompt-cache prefix on `b`"
        );
        assert_eq!(
            session.last_served_account, "b",
            "…while the account that actually served it is the failover target"
        );

        assert_eq!(
            post_one_pinned(manager.clone(), SESSION).await,
            200,
            "the session's next request must be servable"
        );
        let snap = manager.snapshot(OffsetDateTime::now_utc());
        let a = snap.accounts.iter().find(|x| x.name == "a").unwrap();
        assert_eq!(
            a.requests, 1,
            "and it comes HOME to the pinned account — the divert really was for one \
             request only"
        );
    }

    /// The bound. Every account in a five-account fleet is overloaded, so the request
    /// walks accounts until its failover budget runs out — and then STOPS, instead of
    /// cascading across the fleet and paying every account's in-place ladder on the
    /// way (which, with every concurrent request doing the same, is how a partial
    /// overload becomes a total one).
    ///
    /// The fleet is deliberately LARGER than the budget allows, so the attempt count
    /// separates a bounded walk (`1 + MAX_529_FAILOVERS_PER_REQUEST` accounts, each
    /// getting exactly its in-place ladder) from a cascade over all five. The client
    /// still receives the upstream's own 529 verbatim: the give-up SHAPE is unchanged,
    /// it just arrives later.
    #[tokio::test]
    async fn overloaded_529_failover_is_bounded() {
        // One script entry, reused for every connection: nothing but 529s, anywhere.
        let (up_addr, attempts) = spawn_counted_upstream(vec![Some(raw_529())]).await;
        let manager = fleet(up_addr, &["a", "b", "c", "d", "e"]);

        let (status, body) = post_one_with_body(manager.clone()).await;
        assert_eq!(
            status, 529,
            "with the whole fleet overloaded the failover cannot help, so the client \
             gets the 529"
        );
        assert!(
            body.contains("overloaded_error"),
            "give-up still forwards the upstream 529 VERBATIM, exactly as it did \
             before failover existed; a synthesized body would break clients that \
             parse it. Got: {body}"
        );

        let accounts_walked = 1 + MAX_529_FAILOVERS_PER_REQUEST as usize;
        let sends_per_account = MAX_SAME_ACCOUNT_529_RETRIES as usize + 1;
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            accounts_walked * sends_per_account,
            "{accounts_walked} accounts at {sends_per_account} sends each — two idle \
             accounts must be left untouched, or one overloaded request walks the fleet"
        );

        let snap = manager.snapshot(OffsetDateTime::now_utc());
        let held: Vec<&str> = snap
            .accounts
            .iter()
            .filter(|a| a.rate_limited_until.is_some())
            .map(|a| a.name.as_str())
            .collect();
        assert!(
            held.is_empty(),
            "however many accounts a 529 walks, it arms a hold on none of them: {held:?}"
        );
        assert_eq!(
            snap.accounts.iter().map(|a| a.requests).sum::<u64>(),
            1,
            "one client request is one terminal outcome, no matter how many accounts \
             it was sent to"
        );
    }

    /// Invariant 1 on the FAILOVER path specifically: benching an account in the
    /// per-request `tried` set must not shade into benching it for everyone else.
    /// After a completed failover, no account carries a rate-limit hold and none is
    /// held out of rotation for any other reason either (an upstream `rejected` mark
    /// included) — [`crate::stats::GateReason::Ok`] is exactly that claim, read from
    /// the same `account_gate` the selector's hard gates come from.
    ///
    /// The sibling of `overloaded_529_never_arms_a_hold_or_benches_the_account`, which
    /// asserts the same thing on the give-up path where no failover happens at all.
    #[tokio::test]
    async fn overloaded_529_failover_arms_no_hold() {
        use crate::stats::GateReason;
        let up_addr = spawn_scripted_upstream(vec![
            Some(raw_529()),
            Some(raw_529()),
            Some(raw_529()),
            Some(raw_200()), // the failover target serves
        ])
        .await;
        let manager = fleet(up_addr, &["a", "b", "c"]);

        assert_eq!(
            post_one(manager.clone()).await,
            200,
            "precondition: the failover happened and served the client"
        );

        let snap = manager.snapshot(OffsetDateTime::now_utc());
        let held: Vec<&str> = snap
            .accounts
            .iter()
            .filter(|a| a.rate_limited_until.is_some())
            .map(|a| a.name.as_str())
            .collect();
        assert!(
            held.is_empty(),
            "a 529 is not a quota rejection — `mark_rate_limited` must never be \
             reached from this arm. Held: {held:?}"
        );
        let gated: Vec<(&str, GateReason)> = snap
            .accounts
            .iter()
            .filter(|a| a.gate != GateReason::Ok)
            .map(|a| (a.name.as_str(), a.gate))
            .collect();
        assert!(
            gated.is_empty(),
            "every account, the overloaded one included, must still be in rotation \
             for the next request. Gated: {gated:?}"
        );
    }

    /// THE BITING TEST for the soft-wait: the account the sleep was TIMED FOR must
    /// actually be selectable when the sleep ends.
    ///
    /// `Transient429::Park` benches an account twice — a hold via
    /// `mark_rate_limited` AND an entry in `tried` — and `pick_eligible` /
    /// `pick_least_loaded` both test `tried` BEFORE they evaluate eligibility. So
    /// waiting out the hold alone changed nothing: `select()` returned `None` a
    /// second time, the one-soft-wait guard blocked another wait, and the request
    /// fell through to a revalidation-serve that skips `tried` too — an honest-looking
    /// 429 handed to the client after a sleep that bought nothing.
    ///
    /// One account, so "some account served" and "the account we waited for served"
    /// are the same claim. Three transient 429s spend the two inline retries and then
    /// park it for 1s; the fourth connection is the 200 that only exists if the
    /// re-admission works. Pre-fix this test reads 429.
    #[tokio::test]
    async fn soft_wait_readmits_the_unparked_account() {
        let (up_addr, attempts) = spawn_counted_upstream(vec![
            Some(raw_429_transient(1)), // attempt 1 — inline-wait 1s, same account
            Some(raw_429_transient(1)), // attempt 2 — inline budget spent
            Some(raw_429_transient(1)), // attempt 3 — park 1s + `tried`, fleet empty
            Some(raw_200()),            // attempt 4 — only reachable after re-admission
        ])
        .await;
        let manager = fleet(up_addr, &["a"]);

        let status = post_one(manager.clone()).await;
        assert_eq!(
            status, 200,
            "the soft-wait slept for THIS account's un-park — it must be back in \
             rotation when the sleep ends, not still excluded by `tried`"
        );
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            4,
            "three 429s then one serve — no extra attempt, so the re-admission did \
             not turn the rotation loop into a retry spin"
        );

        let snap = manager.snapshot(OffsetDateTime::now_utc());
        let a = snap.accounts.iter().find(|x| x.name == "a").unwrap();
        assert_eq!(
            a.requests, 1,
            "the re-admitted account is the one that served"
        );
        assert!(
            a.rate_limited_until.is_none(),
            "the 200 proves the hold no longer binds, so it is cleared"
        );
    }

    /// The precision guard on the re-admission: ONLY a transient park is a timer.
    ///
    /// `tried` carries accounts benched for reasons a clock never undoes — a 401
    /// whose force-refresh produced no token, a transport blip, a dead token, a
    /// durable quota rejection. Re-admitting those after the sleep would re-send this
    /// request to an account that already failed it, which is how a recovery path
    /// becomes a retry loop. So the sweep is keyed on WHY the entry is in `tried`,
    /// not merely on "has no live hold" — `a` and `b` have no hold at all and must
    /// still stay out.
    ///
    /// LRU makes the failure loud rather than silent: `a` was stamped first, so a
    /// sweep that cleared every hold-free entry would re-admit `a` and serve the 200
    /// from it. The assertion is on WHICH account served.
    #[tokio::test]
    async fn soft_wait_does_not_readmit_a_401_or_transport_failure() {
        let (up_addr, attempts) = spawn_counted_upstream(vec![
            Some(raw_401()),            // attempt 1 — `a`: refresh fails, benched in `tried`
            None,                       // attempt 2 — `b`: transport blip, benched in `tried`
            Some(raw_429_transient(1)), // attempt 3 — `c`: inline-wait
            Some(raw_429_transient(1)), // attempt 4 — `c`: inline budget spent
            Some(raw_429_transient(1)), // attempt 5 — `c`: park 1s + `tried`, fleet empty
            Some(raw_200()),            // attempt 6 — must be `c`, never `a` or `b`
        ])
        .await;
        let manager = fleet(up_addr, &["a", "b", "c"]);

        let status = post_one(manager.clone()).await;
        assert_eq!(status, 200, "the transiently parked `c` still recovers");
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            6,
            "one attempt each on `a` and `b`, three on `c`, then `c`'s serve — a \
             re-admitted `a` or `b` would add attempts here"
        );

        let snap = manager.snapshot(OffsetDateTime::now_utc());
        let served: Vec<&str> = snap
            .accounts
            .iter()
            .filter(|x| x.requests > 0)
            .map(|x| x.name.as_str())
            .collect();
        assert_eq!(
            served,
            ["c"],
            "only the account parked by a HOLD may come back; the 401 and the \
             transport failure are not timers and stay benched for this request"
        );
    }

    /// Termination: re-admitting accounts must not turn the rotation loop into an
    /// unbounded retry. Two things bound it and this asserts both.
    ///
    /// The one-soft-wait-per-request guard means the sweep can run AT MOST ONCE, and
    /// every iteration — the soft-waiting one included — still spends one unit of the
    /// `max_attempts` budget. So when the re-admitted account immediately re-parks,
    /// the second `select()` miss cannot wait again and control reaches the existing
    /// exhausted-429 path.
    ///
    /// `a` is durably quota-rejected (a 3600s hold, and never in the transient set);
    /// `b` parks transiently, is re-admitted, and 429s straight back. Five upstream
    /// attempts, then a 429 — the connection count is what proves it did not spin.
    #[tokio::test]
    async fn soft_wait_still_terminates_when_nothing_recovers() {
        let (up_addr, attempts) = spawn_counted_upstream(vec![
            Some(raw_429_rejected(3600)), // attempt 1 — `a`: durable, no re-admission
            Some(raw_429_transient(1)),   // attempt 2 — `b`: inline-wait
            Some(raw_429_transient(1)),   // attempt 3 — `b`: inline budget spent
            Some(raw_429_transient(1)),   // attempt 4 — `b`: park 1s + `tried`
            Some(raw_429_transient(1)),   // attempt 5 — `b` re-admitted, 429s again
        ])
        .await;
        let manager = fleet(up_addr, &["a", "b"]);

        let status = post_one(manager.clone()).await;
        assert_eq!(
            status, 429,
            "nothing recovered, so the honest exhausted-429 is still the verdict"
        );
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            5,
            "the second select() miss cannot soft-wait again, so the re-admission \
             buys exactly ONE extra attempt and the loop ends"
        );

        let snap = manager.snapshot(OffsetDateTime::now_utc());
        let total: u64 = snap.accounts.iter().map(|x| x.requests).sum();
        assert_eq!(total, 0, "no account ever served this request");
    }

    /// The 502 predicate itself, over the four states the two counters can be in.
    /// The bug was that the second row returned `true`.
    /// The measured failure this exists to prevent: 42 of 52 quota rejections on
    /// 2026-08-04 armed an IDENTICAL 3600s hold, eight accounts inside four
    /// seconds, so the whole fleet would have un-parked as one wave.
    #[test]
    fn a_ceiling_clamped_hold_is_spread_but_a_genuine_one_is_verbatim() {
        // AT THE CEILING: upstream asked for >= 3600 and we were already going to
        // return early, so spreading inside [3600-N, 3600] costs nothing and
        // desyncs the herd. Distinct nanos MUST yield distinct holds.
        let spread: std::collections::HashSet<i64> = (0..8)
            .map(|k| jittered_quota_hold(7200, k * 111_111_111))
            .collect();
        assert!(
            spread.len() > 1,
            "eight accounts rejected together must not share one hold: {spread:?}"
        );
        for h in &spread {
            assert!(
                (MAX_QUOTA_HOLD_SECS - QUOTA_HOLD_JITTER_MAX_SECS..=MAX_QUOTA_HOLD_SECS)
                    .contains(h),
                "stays inside the envelope we already chose: {h}"
            );
        }

        // BELOW THE CEILING: armed verbatim. Returning an account before upstream
        // said it may come back is the one thing this path must never do — the
        // 829s hold observed live must stay 829s.
        assert_eq!(jittered_quota_hold(829, 999_999_999), 829);
        assert_eq!(jittered_quota_hold(1, 999_999_999), 1);
        // And a nonsense/negative header can never produce a zero-length hold.
        assert_eq!(jittered_quota_hold(0, 12_345), 1);
        assert_eq!(jittered_quota_hold(-5, 12_345), 1);
    }

    #[test]
    fn bad_gateway_only_when_nothing_reached_an_upstream() {
        assert!(
            every_attempt_transport_failed(2, 0),
            "blips only, nothing answered → 502"
        );
        assert!(
            !every_attempt_transport_failed(1, 1),
            "an upstream answered → the blip is not the whole story (the live bug)"
        );
        assert!(
            !every_attempt_transport_failed(0, 3),
            "no transport failure at all → never a 502"
        );
        assert!(
            !every_attempt_transport_failed(0, 0),
            "no attempt made → exhausted, not unreachable"
        );
    }

    /// Boot the proxy over a fresh loopback listener with a DEAD upstream
    /// (127.0.0.1:1, reliably connection-refused) and no proxy key, returning its
    /// address. A request the host guard lets THROUGH reaches the rotation loop
    /// and fails transport → 502; a request the guard REJECTS returns its local
    /// status (421) with no egress. The 421-vs-502 split is what proves whether
    /// the guard fired.
    async fn spawn_dead_upstream_proxy() -> SocketAddr {
        let manager = Manager::with_live_refresher(dummy_config(None, "http://127.0.0.1:1"), None);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app(manager)).await;
        });
        addr
    }

    /// Send a raw HTTP/1.1 request over a fresh TCP connection and return the
    /// response status code. Lets a test control the exact request-target form
    /// (absolute vs origin) and `Host` header, which reqwest derives from the URI
    /// and will not let a test spoof — required to exercise both guard branches.
    async fn raw_request_status(addr: SocketAddr, raw: &str) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
        sock.write_all(raw.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 512];
        loop {
            let n = sock.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            // The status line ends at the first CRLF — enough to read the code.
            if buf.windows(2).any(|w| w == b"\r\n") {
                break;
            }
        }
        // "HTTP/1.1 421 Misdirected Request" → 421
        String::from_utf8_lossy(&buf)
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or(0)
    }

    /// Test 1 — a forward-proxy IMDS probe in ABSOLUTE-form (the shape an AWS SDK
    /// emits when `HTTP_PROXY` points at tcr) is rejected locally with 421, read
    /// from the request-target's authority. 421 (not the 502 a real forward to the
    /// dead upstream would give) proves the guard fired BEFORE any egress.
    #[tokio::test]
    async fn misroute_absolute_form_imds_rejected_locally_421() {
        let addr = spawn_dead_upstream_proxy().await;
        let status = raw_request_status(
            addr,
            "PUT http://169.254.169.254/latest/api/token HTTP/1.1\r\n\
             Host: 169.254.169.254\r\n\
             Content-Length: 0\r\n\
             Connection: close\r\n\r\n",
        )
        .await;
        assert_eq!(
            status, 421,
            "an absolute-form misroute must be rejected locally (421), never forwarded (would be 502)"
        );
    }

    /// Test 2 — the same misroute in ORIGIN-form, the target host carried only in
    /// the `Host` header, is likewise rejected with 421. Exercises the guard's
    /// Host-header fallback branch.
    #[tokio::test]
    async fn misroute_host_header_imds_rejected_421() {
        let addr = spawn_dead_upstream_proxy().await;
        let status = raw_request_status(
            addr,
            "PUT /latest/api/token HTTP/1.1\r\n\
             Host: 169.254.169.254\r\n\
             Content-Length: 0\r\n\
             Connection: close\r\n\r\n",
        )
        .await;
        assert_eq!(
            status, 421,
            "a Host-header misroute must also be rejected locally (421)"
        );
    }

    /// Test 3 — FALSE-REJECT REGRESSION GATE (non-negotiable). A legitimate
    /// base-URL request (origin-form `/v1/messages`, loopback `Host`) MUST pass
    /// the guard and reach the rotation loop → dead upstream → 502. A 421 here
    /// would mean the guard wrongly rejected real Anthropic traffic.
    #[tokio::test]
    async fn base_url_loopback_proceeds_not_rejected_502() {
        let addr = spawn_dead_upstream_proxy().await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let resp = client
            .post(format!("http://{addr}/v1/messages"))
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status().as_u16(),
            502,
            "the guard must let a loopback base-URL request through (dead upstream → 502), never 421"
        );
    }

    /// Test 4 — the MITM-terminated shape (origin-form `/v1/messages`, `Host:
    /// api.anthropic.com`) passes the guard via the allowlist branch
    /// (`crate::mitm::host_allowed`) → rotation loop → dead upstream → 502.
    #[tokio::test]
    async fn anthropic_host_proceeds_not_rejected_502() {
        let addr = spawn_dead_upstream_proxy().await;
        let status = raw_request_status(
            addr,
            "POST /v1/messages HTTP/1.1\r\n\
             Host: api.anthropic.com\r\n\
             Content-Length: 2\r\n\
             Connection: close\r\n\r\n{}",
        )
        .await;
        assert_eq!(
            status, 502,
            "an api.anthropic.com request must pass the guard (dead upstream → 502), never 421"
        );
    }

    /// Test 5 — the path-shape guard over a REAL socket, with the request target
    /// written byte for byte. The `drive` harness builds its URI through `http::Uri`
    /// in-process; this proves the same shapes survive hyper's own request-line
    /// parser and are refused there too, so the guard is not an artifact of the test
    /// harness. 400 — not the 502 a forwarded request to the dead upstream gives —
    /// is what proves it fired BEFORE any egress.
    #[tokio::test]
    async fn traversal_targets_refused_over_a_raw_socket_400() {
        let addr = spawn_dead_upstream_proxy().await;
        for target in [
            "/v1/code/../../v1/messages",
            "/v1/code/%2e%2e/%2e%2e/v1/messages",
            "/x/../_tcr/status",
            "/v1/code/..\\../v1/messages",
            "/v1/code\\foo",
        ] {
            let status = raw_request_status(
                addr,
                &format!(
                    "POST {target} HTTP/1.1\r\n\
                     Host: api.anthropic.com\r\n\
                     Content-Length: 2\r\n\
                     Connection: close\r\n\r\n{{}}"
                ),
            )
            .await;
            assert_eq!(
                status, 400,
                "{target} must be refused locally, never forwarded (502)"
            );
        }
        // The control: the same socket, the same dead upstream, an ordinary path.
        // 502 proves the guard rejects the ambiguous shape and nothing else.
        let status = raw_request_status(
            addr,
            "POST /v1/messages HTTP/1.1\r\n\
             Host: api.anthropic.com\r\n\
             Content-Length: 2\r\n\
             Connection: close\r\n\r\n{}",
        )
        .await;
        assert_eq!(
            status, 502,
            "an ordinary path still reaches the rotation loop (dead upstream → 502)"
        );
    }
}
