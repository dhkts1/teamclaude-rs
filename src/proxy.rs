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
use std::sync::atomic::{AtomicBool, Ordering};
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

use crate::config;
use crate::manager::{
    AccountStatus, AddAccountOutcome, AddPersist, ControlPersist, DisablePersist, InFlightGuard,
    Manager, SetControlOutcome, SetDisabledOutcome,
};
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
/// The fixed `stream_error` kind [`parse_sse_usage`] synthesizes when a stream
/// ends without Anthropic's `message_stop` terminator and no more specific
/// in-band `error` event already explained why. Named so the proxy handler's
/// classifier can tell "we witnessed an explicit error" (always trustworthy)
/// apart from "we never saw the terminator" (trustworthy only when we also
/// know we saw everything there was to see — see the handler's use of this).
const TRUNCATED_STREAM_ERROR_KIND: &str = "truncated";
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

/// How many times ONE client request may wait in place for name resolution to
/// come back before it gives up and answers `503`.
///
/// Per-REQUEST, not per-account, and deliberately small. A DNS failure is a
/// statement about the MACHINE, so there is no fleet to walk and nothing to be
/// gained by spending more of the attempt budget on it — the only question is
/// whether the outage is short enough to ride out without the client noticing.
/// With [`OFFLINE_WAIT_SECS`] this bounds the added latency at 6s per request.
const MAX_OFFLINE_WAITS_PER_REQUEST: u32 = 3;
/// Pause between the in-place retries counted by [`MAX_OFFLINE_WAITS_PER_REQUEST`].
/// Short: a lid-wake resolver comes back within a couple of seconds or not at
/// all, and the per-account in-flight slot is held across the wait.
const OFFLINE_WAIT_SECS: u64 = 2;
/// `retry-after` handed to the client with the offline `503`. The condition is
/// recoverable and usually short — measured 2026-08-10, DNS was back within
/// seconds of each full wake — so this is a nudge to come back, not a park.
const OFFLINE_RETRY_AFTER_SECS: i64 = 5;

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

/// Frames in a transport error's `source()` chain that name a NAME-RESOLUTION
/// failure. Matched case-insensitively as substrings of each frame's `Display`.
///
/// The first is hyper-util's own label for the connector's DNS phase
/// (`ConnectError::new("dns error", ..)`); the rest are the operating system's
/// `getaddrinfo` texts that `std` wraps into an `io::Error` whose `ErrorKind` is
/// `Uncategorized` — a kind that cannot be matched on stable Rust, which is why
/// this is a string match and not a `downcast_ref` + kind comparison.
const DNS_FAILURE_MARKERS: [&str; 4] = [
    "dns error",
    "failed to lookup address information",
    "nodename nor servname provided",
    "temporary failure in name resolution",
];

/// Is this transport failure the machine being OFFLINE rather than a bad route
/// to one upstream?
///
/// Why it is not `err.is_connect()`: `is_connect()` is true for a DNS failure
/// AND for a refused/blackholed connection to a host that resolves fine. Only
/// the first is machine-level. Every account in the fleet resolves the SAME
/// hostname, so a resolver failure is not evidence about any account — answering
/// it by rotating walks all thirteen accounts in under a second, unpins the
/// session (a re-pin costs a full cold prefix, the most expensive event here) and
/// hands the client a 502. Measured 2026-08-10: 57 client-visible 502s, all
/// inside one lid-close/dark-wake window, 39-159 resolver failures per minute.
/// A refused connection to a reachable host IS real evidence about a route, so
/// it must keep taking the rotate arm.
///
/// The chain must be WALKED. `reqwest::Error`'s `Display` prints only its own top
/// frame and never recurses (the same property that forces `?err` over `%err` at
/// the log site below), so the resolver text lives strictly in a `source()`.
fn is_offline_error(err: &reqwest::Error) -> bool {
    let mut frame: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(cause) = frame {
        let rendered = cause.to_string().to_ascii_lowercase();
        if DNS_FAILURE_MARKERS
            .iter()
            .any(|marker| rendered.contains(marker))
        {
            return true;
        }
        frame = cause.source();
    }
    false
}

/// The most upstream sends ONE account can absorb inside a single client
/// request, derived from the per-account ladders rather than guessed.
///
/// Every term is a counter in [`handle`] that is monotone per account, so this
/// is a real ceiling and not an estimate:
///
/// * `1` — the send itself.
/// * `1` — the same-account transport retry (`transport_retried`).
/// * `1` — the 401 force-refresh retry (`forced_401`).
/// * [`MAX_SAME_ACCOUNT_429`] — transient-429 inline waits (`retried_429`).
/// * [`MAX_SAME_ACCOUNT_529_RETRIES`] — 529 in-place backoffs (`retried_529`).
const MAX_SENDS_PER_ACCOUNT: usize =
    3 + MAX_SAME_ACCOUNT_429 as usize + MAX_SAME_ACCOUNT_529_RETRIES as usize;

/// The rotation loop's TOTAL attempt budget for a fleet of `account_count`: the
/// per-account ladder ceiling times the fleet, plus a small constant for the
/// iterations that consume a turn without sending (the one-shot exhaustion
/// soft-wait). Bounds [`handle`]'s loop without ever truncating a walk the
/// ladders themselves permit.
///
/// It used to be `2n + 4` — "two sends per account plus a small constant" — which
/// stopped being true when the same-account transport retry added a third
/// potential send. The failure was silent and specifically MIXED: a blip on one
/// account plus the 529 ladder on the next spends 4 sends per account, so a
/// 3-account walk needs 12 against a budget of 10. The loop fell out mid-walk,
/// `every_attempt_transport_failed` was false, and the client got a SYNTHESIZED
/// 429 ("all accounts exhausted", with a fabricated retry-after) in place of the
/// real 529 — or of a 200 from an account that was never tried.
///
/// Every ladder in the sum is independently bounded and each rung is capped
/// (`RETRY_529_MAX_BACKOFF_SECS`, `INLINE_WAIT_MAX_SECS`), so widening this does
/// not widen any latency ceiling; it removes a truncation, not a guard.
///
/// Extracted from the loop so the headroom assertions in
/// `overloaded_529_failover_worst_case_latency_is_bounded` and
/// `the_mixed_transport_and_529_ladder_fits_the_attempt_budget` bind THIS
/// formula instead of a copy of it that could silently drift from it.
///
/// [`MAX_OFFLINE_WAITS_PER_REQUEST`] is added on top because the offline arm's
/// in-place retries are bounded per REQUEST, not per account, so they are not
/// expressible in [`MAX_SENDS_PER_ACCOUNT`]. Without the term a request that
/// rides out a resolver blip could be truncated mid-walk — the exact silent
/// failure the `2n + 4` formula caused.
fn max_attempts_for(account_count: usize) -> usize {
    account_count
        .saturating_mul(MAX_SENDS_PER_ACCOUNT)
        .saturating_add(4 + MAX_OFFLINE_WAITS_PER_REQUEST as usize)
        .max(1)
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
/// key. What this needs to hold is DETERMINISM ACROSS PROCESSES, not just
/// within one — pins now persist to disk and are restored at boot (see
/// [`crate::affinity`]), so a hash that only agreed with itself for the
/// lifetime of one process would silently stop matching every restart. That
/// property is documented, measured and cited at [`crate::affinity::StoredPin::key`]
/// — read it there rather than re-deriving it here.
fn stable_hash(prefix: &str, value: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    prefix.hash(&mut hasher);
    value.hash(&mut hasher);
    hasher.finish()
}

/// Combined single-pass peek for [`stable_session_key`]: the body's top-level
/// `metadata.user_id` (tier 2) and its `system`/`tools` fields (tier 3's
/// cacheable prefix), read with ONE `serde_json::from_slice` instead of one
/// per tier — the two used to be independent structs (`MetadataPeek` /
/// `PrefixPeek`), each parsing the same bytes again.
///
/// `system`/`tools` are `&RawValue`, so the returned slices borrow the
/// VERBATIM source bytes out of `body` rather than a re-serialized copy (same
/// technique as `account_uuid.rs`'s `BodyPeek`). That verbatim-ness is
/// load-bearing for tier 3: see [`prefix_session_key`].
#[derive(serde::Deserialize)]
struct SessionKeyPeek<'a> {
    metadata: Option<UserIdMeta>,
    #[serde(borrow)]
    system: Option<&'a serde_json::value::RawValue>,
    #[serde(borrow)]
    tools: Option<&'a serde_json::value::RawValue>,
}

#[derive(serde::Deserialize)]
struct UserIdMeta {
    user_id: Option<String>,
}

/// Tier 3 of [`stable_session_key`]: derive a fallback affinity key from the
/// request's cacheable prefix — its already-parsed `system`/`tools` fields
/// (see [`SessionKeyPeek`]) — when the client carries no stable identity at
/// all. Returns `None` when BOTH are absent — there is then no cacheable
/// prefix, so a pin buys no cache win and only concentrates unrelated
/// anonymous traffic onto one account. This is the guard that stops every
/// trivial anonymous request (no system prompt, no tools) from piling onto a
/// single account; do not drop it.
///
/// Hashes the RAW bytes of each field via `RawValue::get()` — never
/// canonicalized — namespaced under `"pfx:"` so this input space cannot collide
/// with the `"key:"` / `"uid:"` tiers. `system` and `tools` are hashed as
/// `Option<&str>` (not concatenated into one string) so presence/absence of
/// each field is part of the hash input and a `(None, Some("ab"))` pair can
/// never collide with a `(Some("ab"), None)` pair; `str`'s own `Hash` impl
/// appends a sentinel byte after each value, so this is also safe against a
/// `system`/`tools` boundary shift (e.g. `("a", "bc")` vs `("ab", "c")`).
///
/// Deliberately NO minimum-size floor. A tiny `system`/`tools` field that can
/// never reach Anthropic's minimum cacheable-prefix length pins for zero cache
/// benefit — a floor would fix that. But the floor would have to be a BYTE
/// count on the raw JSON text, while Anthropic's minimum is a TOKEN count on
/// tokenized content, and this proxy has no tokenizer: it never decodes model
/// text, only relays bytes. Any byte→token constant here would be an
/// undefended guess — wrong in one direction wastes the tier's whole purpose,
/// wrong in the other pins traffic that was never going to hit cache. Omitted;
/// revisit only with either a real tokenizer in this process or a measured
/// bytes-per-token ratio from live traffic to ground the constant.
fn prefix_session_key(
    system: Option<&serde_json::value::RawValue>,
    tools: Option<&serde_json::value::RawValue>,
) -> Option<u64> {
    if system.is_none() && tools.is_none() {
        return None;
    }
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    "pfx:".hash(&mut hasher);
    system.map(|v| v.get()).hash(&mut hasher);
    tools.map(|v| v.get()).hash(&mut hasher);
    Some(hasher.finish())
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
///   3. a hash of the request's cacheable prefix (`system` + `tools`, see
///      [`prefix_session_key`]) — weaker than 1/2 because it says nothing about
///      the CLIENT, only about what this one request would cache identically
///      with another, so it is scoped MUCH more narrowly than tiers 1/2. Still
///      routes reconnects of an SDK/`curl` caller with no identity onto the
///      same account instead of cold-starting every request, and self-balances
///      because distinct prefixes hash to distinct accounts. Else
///   4. `None` — no stable identity and no in-scope cacheable prefix, so the
///      request routes UNPINNED (plain LRU). It deliberately does NOT fall
///      back to the per-connection [`SessionKey`]: that mints a pin no
///      reconnect can ever reuse or reclaim.
///
/// Tier 3's scope, and why both halves are required:
///
/// - **`POST /v1/messages` only**, exact match on `path`, not a prefix match —
///   `/v1/messages/count_tokens` must NOT qualify. Anthropic documents that
///   token counting never uses prompt caching, so pinning it would concentrate
///   load for zero cache benefit. `path` is the caller's ALREADY
///   query-stripped path — see `handle`, which matches the same guards this
///   way.
/// - **Loopback callers only**, via `client_is_loopback` (the same
///   [`ClientAddr`] extension `handle`'s api-key gate uses). Tier 1 already
///   refuses to key on `x-api-key` when it equals the configured proxy secret,
///   specifically so remote clients sharing that secret don't collapse onto
///   one account (see the comment below). Tier 3 has no such secret to check —
///   it keys off the request BODY — so without this gate N remote workers
///   sharing one harness (one system prompt) would collapse onto one account
///   through the exact back door tier 1 closes. A loopback proxy serves one
///   real user; a non-loopback deployment may not. (`IpAddr::is_loopback`
///   does not recognize an IPv4-mapped IPv6 peer like `::ffff:127.0.0.1` as
///   loopback, so such a peer reads as remote and fails CLOSED here — the
///   safe direction, and the same primitive [`ClientAddr`]'s own doc-comment
///   and the api-key exemption already accept this same way.)
///
/// Returns `None` on absence/parse failure at every tier, which routes the
/// request unpinned. The paired [`SessionKind`] records WHICH tier produced the
/// key — display provenance only, never a routing input.
fn stable_session_key(
    headers: &HeaderMap,
    body: &[u8],
    proxy_key: Option<&str>,
    method: &Method,
    path: &str,
    client_is_loopback: bool,
) -> Option<(u64, SessionKind)> {
    if let Some(key) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        // The shared proxy secret is not a client identity — skip it so remote
        // clients don't all collapse onto one account.
        if proxy_key != Some(key) {
            return Some((stable_hash("key:", key), SessionKind::Stable));
        }
    }

    // ONE parse serves both tier 2 (`metadata.user_id`) and tier 3
    // (`system`/`tools`) — see [`SessionKeyPeek`].
    let peek = serde_json::from_slice::<SessionKeyPeek>(body).ok();

    if let Some(user_id) = peek
        .as_ref()
        .and_then(|p| p.metadata.as_ref())
        .and_then(|m| m.user_id.as_deref())
    {
        return Some((stable_hash("uid:", user_id), SessionKind::Stable));
    }

    // Tier 3's scope guard — see the doc-comment above for why both halves are
    // load-bearing. `path` must match EXACTLY: `/v1/messages` and nothing
    // longer (`/v1/messages/count_tokens`), nothing shorter.
    if !client_is_loopback || *method != Method::POST || path != "/v1/messages" {
        return None;
    }
    let peek = peek?;
    prefix_session_key(peek.system, peek.tools).map(|key| (key, SessionKind::Prefix))
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

/// Path of the live account-control endpoint [`set_disabled_handler`] serves —
/// `POST` only, the one route on this process that MUTATES rotation.
///
/// It exists because `disabled` was, until now, a boot-time read: `tcr disable`
/// wrote the config file and the running proxy never re-read it, so an account the
/// operator had parked kept being handed live traffic while every surface reported
/// it benched. The TUI's `d` key was the only correct path, and a `--headless`
/// proxy has no TUI.
///
/// Under [`LOCAL_PREFIX`] for the reason given there — but what protects a
/// MUTATING verb is not the prefix guard, whose reach is only the exact spelling.
/// It is that this path is REGISTERED as a real route, so axum matches it exactly
/// and matches it BEFORE the catch-all: a `POST` here can never be rewritten onto
/// api.anthropic.com carrying a pooled OAuth Bearer, and a near-miss spelling can
/// never land ON the handler. What authorizes the mutation itself is
/// [`local_endpoint_gate`] plus the `application/json` requirement in
/// [`set_disabled_handler`], neither of which the path shape has any part in.
pub const DISABLED_PATH: &str = "/_tcr/accounts/disabled";

/// Marks every response produced by the account-control route itself, so a caller
/// can tell "this proxy has no such route" from "the route answered and said no".
///
/// Both are a 404: a tcr too old to have the route hits the [`LOCAL_PREFIX`]
/// catch-all guard (or, older still, gets Anthropic's own 404 back), and a query
/// naming no account is a 404 from the handler. The CLI's two reactions are
/// opposite — write the file and warn loudly that the live proxy is stale, versus
/// report a bad query — so the distinction cannot rest on matching an error
/// string. This header is the structural discriminator. It is a response header,
/// never a request one, so it authorizes nothing.
pub const ENDPOINT_HEADER: &str = "x-tcr-endpoint";

/// The [`ENDPOINT_HEADER`] value the account-control route stamps.
pub const DISABLED_ENDPOINT: &str = "accounts-disabled";

/// Path of the live account-ADD endpoint [`add_account_handler`] serves — `POST`
/// only, the other route on this process that MUTATES rotation.
///
/// It exists because adding an account has, until now, required stopping the
/// proxy, running `tcr login`, and starting it again — which wipes the in-memory
/// session→account pin map, so every live session cold-starts its prompt cache
/// on the next turn. That is the most expensive event in this system; see
/// [`Manager::add_account`] for the append primitive this route drives and why
/// it must never insert anywhere but the end.
///
/// Same shape as [`DISABLED_PATH`] and for the same reasons: registered as a
/// real route so axum matches it exactly and BEFORE the catch-all — a `POST`
/// here can never be rewritten onto api.anthropic.com carrying a pooled OAuth
/// Bearer. What authorizes the mutation is [`local_endpoint_gate`] plus the
/// `application/json` requirement, neither of which the path shape has any part
/// in.
pub const ADD_ACCOUNT_PATH: &str = "/_tcr/accounts";

/// The [`ENDPOINT_HEADER`] value [`add_account_handler`] stamps. Distinct from
/// [`DISABLED_ENDPOINT`] so a caller can tell which of the two mutating routes
/// answered — the same structural discriminator [`ENDPOINT_HEADER`] documents.
pub const ADD_ACCOUNT_ENDPOINT: &str = "accounts-add";

/// Path of the control-account endpoint [`set_control_handler`] serves — `POST`
/// only, the third route on this process that MUTATES rotation state.
///
/// Same shape as [`DISABLED_PATH`] and for the same reasons: registered as a
/// real route so axum matches it exactly and BEFORE the catch-all, so a `POST`
/// here can never be rewritten onto api.anthropic.com carrying a pooled OAuth
/// Bearer. What authorizes the mutation is [`local_endpoint_gate`] plus the
/// `application/json` requirement, neither of which the path shape has any
/// part in.
pub const CONTROL_PATH: &str = "/_tcr/accounts/control";

/// The [`ENDPOINT_HEADER`] value [`set_control_handler`] stamps. Distinct from
/// [`DISABLED_ENDPOINT`]/[`ADD_ACCOUNT_ENDPOINT`] so a caller can tell which of
/// the three mutating routes answered.
pub const CONTROL_ENDPOINT: &str = "account-control";

/// Body cap for the local control route: a JSON object with three short fields.
/// Deliberately not [`MAX_BODY_BYTES`] (256 MiB, sized for model requests) — this
/// route buffers into memory in a credential-holding process, and nothing
/// legitimate here is larger than a line.
const CONTROL_BODY_LIMIT: usize = 8 * 1024;

/// The path segment [`STATUS_PATH`] lives under. Every path beneath it belongs to
/// the PROXY, never to Anthropic, so anything under it that is not a registered
/// route is answered with a LOCAL 404 (see the guard in [`handle`]) instead of
/// being forwarded. That is not hygiene: before the guard existed a typo'd status
/// probe fell through the catch-all and was sent to `api.anthropic.com/_tcr/status`
/// carrying a pooled OAuth Bearer, which burned an account on a request no
/// upstream route could ever answer.
///
/// Stored WITHOUT a trailing slash and matched by [`path_is_under`], so the bare
/// `/_tcr` is covered along with everything beneath it — but the guard is a
/// BYTE-EXACT, case-sensitive compare on the raw request target, and that is the
/// whole of what it covers. Three spellings a URL parser would fold onto this
/// prefix are NOT under it and are forwarded upstream (measured): `//_tcr/…`
/// (empty first segment), `/_TCR/…` (case) and `/%5ftcr/…` (percent-encoded `_`,
/// which `uri.path()` hands over undecoded).
///
/// That is a wart, not a hole, and it is deliberately left alone here: those
/// spellings reach Anthropic with a pooled Bearer and come back 404 — the
/// account-burn shape this guard exists to stop, at the cost of one 404 rather
/// than a mutation. They cannot mutate anything, because the local routes are
/// matched by their EXACT paths ([`STATUS_PATH`], [`DISABLED_PATH`]) before this
/// catch-all, so a spelling that misses the prefix misses the routes too. Do not
/// read this comment as "nothing unguarded gets past" — read it as "what gets past
/// is a forwarded 404".
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
/// - `/v1/mcp_servers` is the claude.ai connector list. The ids it returns are scoped
///   to the identity that asked, and the MCP traffic that follows them goes to
///   `mcp-proxy.anthropic.com` — a host the CONNECT path blind-tunnels, so it carries
///   the CLIENT's own token. A pooled list therefore hands the client ids its own
///   identity cannot resolve.
///
/// Measured on the live log while every one of these still went through rotation:
/// 13 of 57 sessionless requests came back 404 (22.8%), against 0 of 1556 pinned ones.
/// The connector list was measured the same way, one `claude mcp list` per pooled
/// account: served by the wrong one, all 9 connectors reported
/// `not_found_error: "Server not found"`; served by the client's own identity, every
/// one connected. That is why this reads as intermittent — it tracks rotation.
///
/// Written WITHOUT trailing slashes and matched by [`path_is_under`] — an entry
/// matches the exact path or that path followed by `/`, never a longer identifier.
/// Both edges of a raw `starts_with` were live defects: `"/api/oauth/file_upload"`
/// (no terminator) also relayed `/api/oauth/file_upload_v2`, and `"/v1/code/"`
/// (with one) missed the bare `/v1/code`.
const CLIENT_CREDENTIAL_PREFIXES: [&str; 4] = [
    "/v1/code",
    "/api/oauth/files",
    "/api/oauth/file_upload",
    "/v1/mcp_servers",
];

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

/// The host this request was ADDRESSED to, or `None` when nothing says.
///
/// Absolute-form / forward-proxy request lines carry an authority in the target
/// itself, which wins; otherwise it is the `Host` header minus any `:port`. `None`
/// is a real answer, not a failure: an origin-form base-URL request with no `Host`
/// (every direct-axum caller, and HTTP/1.0) names no host at all.
///
/// ONE derivation, shared by the forwarding path's misroute guard and by
/// [`local_endpoint_gate`], so "which host did the client mean" cannot come out
/// two different ways on the two paths. Pure — unit-tested.
fn target_host<'a>(uri: &'a axum::http::Uri, headers: &'a HeaderMap) -> Option<&'a str> {
    uri.host().or_else(|| {
        headers
            .get(HOST)
            .and_then(|v| v.to_str().ok())
            .map(strip_port)
    })
}

/// `authority` without its `:port`, if it has one.
///
/// A bracketed IPv6 literal is why this is not one `rsplit_once(':')`: the last
/// colon in `[::1]` is INSIDE the address, so splitting on it yields `[:` — a host
/// that matches nothing, which turned an IPv6 loopback client into a misroute. The
/// port, when present, always follows the `]`.
fn strip_port(authority: &str) -> &str {
    if authority.starts_with('[') {
        return authority.split_inclusive(']').next().unwrap_or(authority);
    }
    authority
        .rsplit_once(':')
        .map_or(authority, |(host, _)| host)
}

/// Is `host` a name for THIS machine's loopback interface?
///
/// The `IpAddr::is_loopback` primitive the client-peer checks use, plus the one
/// name that is loopback without being an IP literal. `[::1]` arrives bracketed in
/// a `Host` header, and the brackets are not part of the address, so they are
/// stripped before parsing — without that, the v6 loopback fails a check the v4
/// one passes. Pure — unit-tested.
fn host_is_loopback(host: &str) -> bool {
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    bare.parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
        || host.eq_ignore_ascii_case("localhost")
}

/// Is this `content-type` `application/json`?
///
/// The mutating local route's whole browser defence, and the reason it is a
/// content-type check rather than something cleverer: `text/plain`,
/// `application/x-www-form-urlencoded` and `multipart/form-data` are the three
/// CORS **simple** media types, so a cross-origin POST carrying one is sent with
/// NO preflight. `application/json` is not simple, and this process answers no
/// `OPTIONS` and emits no `Access-Control-*` header — so requiring it means a
/// browser's preflight fails and the POST is never sent at all.
///
/// Measured on the route as merged (#71), from a loopback peer with no proxy
/// api-key configured, which is a fresh install: `text/plain` and
/// `application/x-www-form-urlencoded` both returned **200 and parked a live
/// account**. That the page cannot read the reply is irrelevant — the entire
/// payoff of a mutating route IS the side effect.
///
/// ABSENT is a refusal too: a request with no `content-type` is simple as well.
/// Only the media type is compared, case-insensitively — RFC 9110 makes the type
/// case-insensitive and a `; charset=utf-8` parameter legitimate, and a check that
/// rejected either would break real callers while closing nothing.
fn is_json_content_type(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|value| {
            value.split(';').next().is_some_and(|media_type| {
                media_type.trim().eq_ignore_ascii_case("application/json")
            })
        })
}

/// Does this request carry positive evidence that a BROWSER initiated it on
/// behalf of some other site?
///
/// Two independent signals, either of which is disqualifying:
/// - an `Origin` header. A cross-origin POST always carries one, and this process
///   serves no HTML, so there is no page whose same-origin `Origin` we would need
///   to allow — any value at all means a browser context we do not own.
/// - `Sec-Fetch-Site` anything other than `same-origin` or `none` — the browser
///   stating the relationship itself. `none` is a user-initiated request (typed
///   URL, bookmark), not a site's.
///
/// A non-browser caller sends neither, so this costs `tcr disable` nothing. It is
/// deliberately additive to [`is_json_content_type`] rather than a replacement:
/// the content-type check is what closes the no-preflight class, and this is what
/// keeps a future local route that is NOT JSON from reopening it. Neither closes
/// DNS rebinding, where the page is genuinely same-origin — that is
/// [`host_is_loopback`]'s job.
fn is_cross_site_request(headers: &HeaderMap) -> bool {
    if headers.contains_key(axum::http::header::ORIGIN) {
        return true;
    }
    headers
        .get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|site| !matches!(site.trim(), "same-origin" | "none"))
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
        // The two MUTATING local routes. Same reasoning as above, and the method
        // fallback matters more here: an unregistered method must answer a local
        // 405, never fall through to `handle` and be forwarded upstream.
        .route(
            DISABLED_PATH,
            axum::routing::post(set_disabled_handler).fallback(disabled_method_not_allowed),
        )
        .route(
            ADD_ACCOUNT_PATH,
            axum::routing::post(add_account_handler).fallback(add_account_method_not_allowed),
        )
        .route(
            CONTROL_PATH,
            axum::routing::post(set_control_handler).fallback(control_method_not_allowed),
        )
        .fallback(handle)
        .with_state(manager)
}

/// The two gates every locally-answered `/_tcr/…` route passes, or the refusal to
/// return. `Some(response)` means REFUSED; `None` means proceed.
///
/// ONE implementation, deliberately: these routes live on a process holding every
/// account's OAuth access and refresh token, and the gate is the whole of their
/// authorization. Two copies of it drift, and the copy that drifts is the one on
/// whichever route was added later — which is also the more dangerous route,
/// since the second one added mutates rotation.
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
///    the proxy key. Nothing on this host needs to read or steer the fleet without
///    the operator's secret, so doing either costs the same secret that using the
///    proxy does. The compare is [`key_matches`] (constant-time, length-safe).
/// 3. **Addressed to us.** The host the request names ([`target_host`]) must be
///    loopback, or absent. This is the DNS-rebinding check, and it is the only one
///    of the four that closes it: a page served from a name resolving to 127.0.0.1
///    is genuinely SAME-ORIGIN with this process, so it sends no `Origin`, needs no
///    preflight, and may use any content type — the one thing that still gives it
///    away is the name it addressed us by. It also refuses an absolute-form
///    (forward-proxy) request line, which is never how a caller reaches a route
///    that only exists here.
/// 4. **Not cross-site.** [`is_cross_site_request`] — belt-and-braces for a
///    browser that did preflight, or a future local route that is not JSON.
///
/// Checks 3 and 4 were added after the FIRST mutating route shipped under this
/// gate: a read that a page cannot read is harmless, but the account-control route
/// is the first one whose entire payoff is the side effect, and a browser on this
/// host is a loopback caller. They live here, on the shared gate, rather than on
/// the mutating handler — see the paragraph above about which copy drifts. What
/// stays route-local is the `application/json` requirement
/// ([`is_json_content_type`]), because a GET has no body to type.
///
/// `endpoint` names the route in the 403 body only. It is not a capability: no
/// value of it weakens any check.
fn local_endpoint_gate(
    parts: &axum::http::request::Parts,
    manager: &Manager,
    endpoint: &str,
) -> Option<Response> {
    let client_is_loopback = parts
        .extensions
        .get::<ClientAddr>()
        .is_some_and(|a| a.0.ip().is_loopback());
    if !client_is_loopback {
        return Some(error_response(
            StatusCode::FORBIDDEN,
            "permission_error",
            &format!("The tcr {endpoint} endpoint is loopback-only."),
            None,
        ));
    }

    if let Some(expected) = manager.proxy_api_key() {
        let provided = parts.headers.get("x-api-key").and_then(|v| v.to_str().ok());
        if !key_matches(provided, expected) {
            return Some(error_response(
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                "Missing or invalid x-api-key.",
                None,
            ));
        }
    }

    if let Some(host) = target_host(&parts.uri, &parts.headers) {
        if !host_is_loopback(host) {
            tracing::debug!(
                target_host = %host,
                endpoint,
                "refused a tcr endpoint request addressed to a non-loopback host"
            );
            return Some(error_response(
                StatusCode::MISDIRECTED_REQUEST,
                "invalid_request_error",
                &format!(
                    "The tcr {endpoint} endpoint answers only to a loopback host, \
                     not to '{host}'."
                ),
                None,
            ));
        }
    }

    if is_cross_site_request(&parts.headers) {
        return Some(error_response(
            StatusCode::FORBIDDEN,
            "permission_error",
            &format!("The tcr {endpoint} endpoint does not serve cross-site requests."),
            None,
        ));
    }

    None
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
/// This is an **attack surface on a process holding every account's OAuth access
/// and refresh token**, so it is gated twice — origin and key, see
/// [`local_endpoint_gate`] for both and for why bind scope is not authorization —
/// and reads nothing but state that is already on screen in the TUI.
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
    if let Some(refusal) = local_endpoint_gate(&parts, &manager, "status") {
        return refusal;
    }

    let now = OffsetDateTime::now_utc();
    let payload = crate::status::StatusPayload::from_snapshot(
        &manager.snapshot(now),
        &manager.thresholds(),
        manager.http1_only(),
        manager.control_name(),
    );
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

/// A non-`POST` on [`DISABLED_PATH`]. Answered locally with 405 — the route is a
/// command, and a command has exactly one verb. Stamped with [`ENDPOINT_HEADER`]
/// so a caller can tell this 405 (the route exists, wrong verb) from the local 404
/// a tcr without the route returns.
async fn disabled_method_not_allowed() -> Response {
    stamp_endpoint(error_response(
        StatusCode::METHOD_NOT_ALLOWED,
        "invalid_request_error",
        "The tcr account-control endpoint takes POST.",
        None,
    ))
}

/// Add [`ENDPOINT_HEADER`] to a response, naming which local mutating route
/// produced it. ONE implementation shared by both stampers below, so the two
/// routes cannot drift on how the header is set — only on which value they set
/// it to.
fn stamp_endpoint_as(mut response: Response, endpoint: &'static str) -> Response {
    response
        .headers_mut()
        .insert(ENDPOINT_HEADER, HeaderValue::from_static(endpoint));
    response
}

/// Add [`ENDPOINT_HEADER`] to a response the account-control (disable) route
/// produced.
fn stamp_endpoint(response: Response) -> Response {
    stamp_endpoint_as(response, DISABLED_ENDPOINT)
}

/// Add [`ENDPOINT_HEADER`] to a response the account-add route produced.
fn stamp_add_endpoint(response: Response) -> Response {
    stamp_endpoint_as(response, ADD_ACCOUNT_ENDPOINT)
}

/// The request body of [`DISABLED_PATH`].
///
/// `query` and `disabled` are `Option` rather than required fields so a body that
/// omits either is a 400 naming the field, instead of axum's own rejection text:
/// this route is what a person reaches for when the fleet is already misbehaving,
/// and an unhelpful 400 there is its own outage.
#[derive(serde::Deserialize)]
struct SetDisabledRequest {
    query: Option<String>,
    #[serde(default)]
    org: Option<String>,
    disabled: Option<bool>,
}

/// The 200 body of [`DISABLED_PATH`]. Deserializable too — `tcr enable`/`disable`
/// read it, and both halves of the wire contract belong to one type.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SetDisabledResponse {
    /// The RESOLVED account name. The query may have been the account's bare
    /// EMAIL where its stored name carries an org suffix (`me@example.com (Acme)`),
    /// so the answer names what was actually parked.
    ///
    /// It is NOT a partial or fuzzy match: [`crate::identity::match_accounts`] is
    /// exact name, then exact email, byte-for-byte, case-sensitive and untrimmed.
    /// Measured against a fleet holding `alice@example.com`: `alice`, `alice@`,
    /// `example`, `ALICE@EXAMPLE.COM` and a trailing space are each a 404.
    pub name: String,
    /// The state now in force in the live rotation.
    pub disabled: bool,
    /// Whether the config file also carries it. `false` with a `warning` is a
    /// change that is live but will not survive a restart.
    pub persisted: bool,
    /// [`crate::manager::DisablePersist::warning`], verbatim, or `null`.
    pub warning: Option<String>,
}

/// `POST /_tcr/accounts/disabled` — park or unpark an account IN THE LIVE
/// ROTATION, for `tcr disable` / `tcr enable`.
///
/// The endpoint exists because the flag was previously only ever read at boot:
/// see [`DISABLED_PATH`] for the defect. This is the only route on the proxy that
/// changes what the next request is routed to, so it is gated exactly as
/// [`status_handler`] is — [`local_endpoint_gate`], no weaker for being a write —
/// and it does the work through [`Manager::set_disabled_by_query`], which is the
/// same call the TUI's `d` key makes. It does not invent a resolution rule: the
/// CLI's own [`crate::identity::match_one`] runs against the server's live
/// rotation slots, so the two cannot disagree about which account a query names.
///
/// The durable half is NOT swallowed. `persisted: false` plus a `warning` is a
/// change that is in force right now and will vanish on restart, which is
/// precisely the state the old code left the operator in silently.
async fn set_disabled_handler(State(manager): State<Arc<Manager>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    if let Some(refusal) = local_endpoint_gate(&parts, &manager, "account-control") {
        // NOT stamped: a refusal must not confirm the route exists to a caller
        // that failed the gate, and the CLI never needs the discriminator on a
        // 401/403 — both are terminal for it.
        return refusal;
    }

    // The browser gate. `application/json` is not a CORS simple media type, so
    // requiring it forces a preflight this process never answers — see
    // [`is_json_content_type`] for the four shapes that reached this handler and
    // parked a live account before the check existed. Stamped, unlike the gate's
    // refusals above: this is a request-shape error like the 400s below it, and the
    // CLI reads the stamp to tell a route that answered from a tcr too old to have
    // one.
    if !is_json_content_type(&parts.headers) {
        return stamp_endpoint(error_response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "invalid_request_error",
            "The tcr account-control endpoint requires Content-Type: application/json.",
            None,
        ));
    }

    let Ok(bytes) = to_bytes(body, CONTROL_BODY_LIMIT).await else {
        return stamp_endpoint(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Could not read the request body.",
            None,
        ));
    };
    let Ok(parsed) = serde_json::from_slice::<SetDisabledRequest>(&bytes) else {
        return stamp_endpoint(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Expected a JSON object: {\"query\": \"<account>\", \"org\": null, \"disabled\": true}.",
            None,
        ));
    };
    let (Some(query), Some(disabled)) = (parsed.query, parsed.disabled) else {
        return stamp_endpoint(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Both \"query\" (string) and \"disabled\" (bool) are required.",
            None,
        ));
    };
    if query.trim().is_empty() {
        return stamp_endpoint(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "\"query\" must not be empty.",
            None,
        ));
    }

    match manager.set_disabled_by_query(&query, parsed.org.as_deref(), disabled) {
        SetDisabledOutcome::NoMatch => stamp_endpoint(error_response(
            StatusCode::NOT_FOUND,
            "not_found_error",
            &format!("No account in the live rotation matches '{query}'."),
            None,
        )),
        SetDisabledOutcome::Ambiguous(names) => stamp_endpoint(error_response(
            StatusCode::CONFLICT,
            "invalid_request_error",
            &crate::cli::ambiguous_query_message(&query, &names),
            None,
        )),
        SetDisabledOutcome::Applied { name, persist } => {
            let payload = SetDisabledResponse {
                name,
                disabled,
                persisted: matches!(persist, DisablePersist::Persisted),
                warning: persist.warning(disabled).map(str::to_string),
            };
            let Ok(body) = serde_json::to_string(&payload) else {
                return stamp_endpoint(error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "api_error",
                    "Could not serialize the account-control result.",
                    None,
                ));
            };
            let mut response = Response::new(Body::from(body));
            let headers = response.headers_mut();
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
            headers.insert("cache-control", HeaderValue::from_static("no-store"));
            stamp_endpoint(response)
        }
    }
}

/// A non-`POST` on [`CONTROL_PATH`]. Answered locally with 405, same reasoning
/// as [`disabled_method_not_allowed`] — the route is a command with exactly
/// one verb.
async fn control_method_not_allowed() -> Response {
    stamp_control_endpoint(error_response(
        StatusCode::METHOD_NOT_ALLOWED,
        "invalid_request_error",
        "The tcr control-account endpoint takes POST.",
        None,
    ))
}

/// Add [`ENDPOINT_HEADER`] to a response the control-account route produced.
fn stamp_control_endpoint(response: Response) -> Response {
    stamp_endpoint_as(response, CONTROL_ENDPOINT)
}

/// The request body of [`CONTROL_PATH`].
///
/// `query: null` (or the field omitted) CLEARS the control account —
/// deliberately not an error, unlike [`SetDisabledRequest`] where a missing
/// `query` is a 400: there is no destructive-vs-informative ambiguity here,
/// clearing is a complete and meaningful request on its own.
#[derive(serde::Deserialize, Default)]
struct SetControlRequest {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    org: Option<String>,
}

/// The 200 body of [`CONTROL_PATH`]. Deserializable too — `tcr control` reads
/// it, and both halves of the wire contract belong to one type.
///
/// Every field `#[serde(default)]`, with the safer reading as the default —
/// same cross-build posture [`SetDisabledResponse`] would want if a future
/// tcr shipped a field this one predates: an OLDER client parsing a NEWER
/// server's body (or vice versa) must land on "nothing happened / not saved"
/// rather than silently assume success on a field it never received.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SetControlResponse {
    /// The RESOLVED account name now set as control, or `None` when this call
    /// cleared it. Resolved rather than echoed for the same reason
    /// [`SetDisabledResponse::name`] is.
    #[serde(default)]
    pub name: Option<String>,
    /// Whether this call cleared the control account (`query` was `null`).
    #[serde(default)]
    pub cleared: bool,
    /// Whether the config file also carries it. `false` with a `warning` is a
    /// change that is live but will not survive a restart.
    #[serde(default)]
    pub persisted: bool,
    /// [`crate::manager::ControlPersist::warning`], verbatim, or `null`.
    #[serde(default)]
    pub warning: Option<String>,
}

/// `POST /_tcr/accounts/control` — set or clear the identity-bound CONTROL
/// account IN THE LIVE ROTATION, for `tcr control`.
///
/// Cloned from [`set_disabled_handler`], same gate order: [`local_endpoint_gate`]
/// first (refusal NOT stamped — a caller that failed the gate gets no
/// confirmation the route exists), then [`is_json_content_type`] (stamped —
/// a request-shape error like the 400s below it). Does the work through
/// [`Manager::set_control_by_query`], the same call `tcr control` makes.
///
/// Unlike [`set_disabled_handler`], a missing/`null` `query` is not a 400 — it
/// is the CLEAR request, a complete operation on its own.
async fn set_control_handler(State(manager): State<Arc<Manager>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    if let Some(refusal) = local_endpoint_gate(&parts, &manager, "control-account") {
        // NOT stamped — see `set_disabled_handler`'s identical comment.
        return refusal;
    }

    if !is_json_content_type(&parts.headers) {
        return stamp_control_endpoint(error_response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "invalid_request_error",
            "The tcr control-account endpoint requires Content-Type: application/json.",
            None,
        ));
    }

    let Ok(bytes) = to_bytes(body, CONTROL_BODY_LIMIT).await else {
        return stamp_control_endpoint(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Could not read the request body.",
            None,
        ));
    };
    let Ok(parsed) = serde_json::from_slice::<SetControlRequest>(&bytes) else {
        return stamp_control_endpoint(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Expected a JSON object: {\"query\": \"<account>\" | null, \"org\": null}.",
            None,
        ));
    };
    if matches!(&parsed.query, Some(q) if q.trim().is_empty()) {
        return stamp_control_endpoint(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "\"query\" must not be empty; omit it or send null to clear.",
            None,
        ));
    }
    let cleared = parsed.query.is_none();

    match manager.set_control_by_query(parsed.query.as_deref(), parsed.org.as_deref()) {
        SetControlOutcome::NoMatch => stamp_control_endpoint(error_response(
            StatusCode::NOT_FOUND,
            "not_found_error",
            &format!(
                "No account in the live rotation matches '{}'.",
                parsed.query.unwrap_or_default()
            ),
            None,
        )),
        SetControlOutcome::Ambiguous(names) => stamp_control_endpoint(error_response(
            StatusCode::CONFLICT,
            "invalid_request_error",
            &crate::cli::ambiguous_query_message(&parsed.query.unwrap_or_default(), &names),
            None,
        )),
        SetControlOutcome::Applied { name, persist } => {
            let payload = SetControlResponse {
                name,
                cleared,
                persisted: matches!(persist, ControlPersist::Persisted),
                warning: persist.warning().map(str::to_string),
            };
            let Ok(body) = serde_json::to_string(&payload) else {
                return stamp_control_endpoint(error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "api_error",
                    "Could not serialize the control-account result.",
                    None,
                ));
            };
            let mut response = Response::new(Body::from(body));
            let headers = response.headers_mut();
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
            headers.insert("cache-control", HeaderValue::from_static("no-store"));
            stamp_control_endpoint(response)
        }
    }
}

/// A non-`POST` on [`ADD_ACCOUNT_PATH`]. Answered locally with 405, same
/// reasoning as [`disabled_method_not_allowed`] — the route is a command with
/// exactly one verb, and it is stamped so a caller can tell this 405 (route
/// exists, wrong verb) from the local 404 a tcr without the route returns.
async fn add_account_method_not_allowed() -> Response {
    stamp_add_endpoint(error_response(
        StatusCode::METHOD_NOT_ALLOWED,
        "invalid_request_error",
        "The tcr account-add endpoint takes POST.",
        None,
    ))
}

/// The request body of [`ADD_ACCOUNT_PATH`].
///
/// Mirrors [`config::Account`]'s wire shape (camelCase, `type` for
/// `account_type`) so a caller can build the body directly from the credentials
/// a login already produced. `name` and `access_token` are `Option` — like
/// [`SetDisabledRequest`]'s fields — so a body missing either gets a specific
/// 400 naming the field instead of axum's own rejection text.
///
/// `priority` and `switch_threshold` are accepted for the NEW-account case only;
/// see [`add_account_handler`]. `disabled` is deliberately not accepted at all —
/// an account reaching this route is one the operator wants serving traffic now.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddAccountRequest {
    name: Option<String>,
    #[serde(rename = "type", default)]
    account_type: Option<String>,
    #[serde(default)]
    account_uuid: Option<String>,
    #[serde(default)]
    org_uuid: Option<String>,
    #[serde(default)]
    org_name: Option<String>,
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_at: Option<i64>,
    #[serde(default)]
    priority: Option<i64>,
    #[serde(default)]
    switch_threshold: Option<f64>,
}

/// The 200 body of [`ADD_ACCOUNT_PATH`]. Deserializable too — unit 3's CLI
/// deserializes it, and both halves of a wire contract belong to one type.
///
/// This is a cross-BUILD wire contract — the same class as
/// [`crate::singleton::ProxyHost::Unknown`] — because the `tcr` that
/// deserializes a reply can be older than the `tcr` that served it. Every
/// field below carries `#[serde(default)]` so a field a NEWER server has
/// renamed or dropped does not fail the whole deserialize: `cli::post_add_account`
/// maps a deserialize failure on a 2xx to `LiveControlError::Unusable`, and
/// `oauth::probe_add_capability` reads that as `AddCapability::Unusable` — a
/// *successful* live add would then look indistinguishable from a wedged or
/// route-less proxy, and `login_route` REFUSES outright (bridge item D)
/// instead of silently falling back to the file beside a server that just
/// handled the request correctly. Field ADDITIONS are already safe on their
/// own (an older CLI just ignores a key it doesn't know); this attribute is
/// what makes removals and renames safe too. Each default is chosen as the
/// SAFER of the two readings, not merely the type's zero value: `persisted`
/// defaults to `false` (never claim durability we can't confirm) and `added`
/// defaults to `false` (never claim a fresh account was created when it
/// might have been an in-place credential replacement) — same
/// degrade-to-the-safer-state shape as
/// `ProxyHost::Unknown`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct AddAccountResponse {
    /// The account's name AS STORED. For an update this is the EXISTING row's
    /// name, which may differ from what was submitted (e.g. a bare email
    /// meeting a stored `"email (org)"` display name) — never a name that only
    /// ever existed in the request.
    #[serde(default)]
    pub name: String,
    /// `true` when this identity had no match in the live rotation and was
    /// appended; `false` when an existing account's credentials were replaced
    /// in place. "Created" and "credentials replaced" are different facts and
    /// the operator needs to know which one they got.
    #[serde(default)]
    pub added: bool,
    /// The account's index in the live rotation. Stable for the process
    /// lifetime — append-only, never reused, and an update never moves it.
    #[serde(default)]
    pub index: usize,
    /// Whether the config file also carries it. `false` with a `warning` is a
    /// change that is live but will not survive a restart.
    #[serde(default)]
    pub persisted: bool,
    /// [`AddPersist::warning`] and/or the no-refresh-token warning, joined when
    /// both apply, or `null`.
    #[serde(default)]
    pub warning: Option<String>,
}

/// Shown when the submitted account carries no refresh token: it will serve
/// until its access token expires and then go dead, silently, unless the
/// operator hears about it now.
const NO_REFRESH_TOKEN_WARNING: &str =
    "this account has no refresh token — it will serve until its access token expires, then go dead";

/// Join whichever of the durable-persist warning and the no-refresh-token
/// warning actually apply into the single `warning` field the response carries.
fn combine_warnings(persist: Option<&str>, no_refresh_token: Option<&str>) -> Option<String> {
    match (persist, no_refresh_token) {
        (None, None) => None,
        (Some(a), None) => Some(a.to_string()),
        (None, Some(b)) => Some(b.to_string()),
        (Some(a), Some(b)) => Some(format!("{a} {b}")),
    }
}

/// Build the 200 body of [`ADD_ACCOUNT_PATH`], stamped like every response this
/// route produces.
fn add_account_response(
    name: String,
    added: bool,
    index: usize,
    persist: AddPersist,
    warning: Option<String>,
) -> Response {
    let payload = AddAccountResponse {
        name,
        added,
        index,
        persisted: matches!(persist, AddPersist::Persisted),
        warning,
    };
    let Ok(body) = serde_json::to_string(&payload) else {
        return stamp_add_endpoint(error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
            "Could not serialize the account-add result.",
            None,
        ));
    };
    let mut response = Response::new(Body::from(body));
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert("cache-control", HeaderValue::from_static("no-store"));
    stamp_add_endpoint(response)
}

/// `POST /_tcr/accounts` — add an account to the LIVE rotation with no proxy
/// restart, for a `tcr login` that should not have to stop and restart the
/// process to take effect.
///
/// Gated exactly as [`set_disabled_handler`] is — [`local_endpoint_gate`], the
/// `application/json` requirement, the same [`CONTROL_BODY_LIMIT`] — because this
/// route carries an OAuth access token and refresh token in its body and is a
/// write, so it is authorized no more weakly than the read routes.
///
/// The behaviour is [`Manager::add_or_update_account`]'s: the submitted identity
/// resolves against the live rotation via [`crate::identity::match_one`], and
/// either appends (nothing matched — a brand new account) or replaces an
/// existing account's credentials in place (exactly one matched — a re-login).
/// See that function's doc-comment for why the two cases use different
/// resolution anchors for their durable write. An ambiguous match is a 409
/// naming the candidates, via [`crate::cli::ambiguous_query_message`] — never
/// guessed.
async fn add_account_handler(State(manager): State<Arc<Manager>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    if let Some(refusal) = local_endpoint_gate(&parts, &manager, "account-add") {
        // NOT stamped — see [`set_disabled_handler`]'s identical comment: a
        // refusal must not confirm the route exists to a caller that failed
        // the gate.
        return refusal;
    }

    if !is_json_content_type(&parts.headers) {
        return stamp_add_endpoint(error_response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "invalid_request_error",
            "The tcr account-add endpoint requires Content-Type: application/json.",
            None,
        ));
    }

    let Ok(bytes) = to_bytes(body, CONTROL_BODY_LIMIT).await else {
        return stamp_add_endpoint(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Could not read the request body.",
            None,
        ));
    };
    let Ok(parsed) = serde_json::from_slice::<AddAccountRequest>(&bytes) else {
        return stamp_add_endpoint(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Expected a JSON object: {\"name\": \"<account>\", \"accessToken\": \"<token>\", …}.",
            None,
        ));
    };
    // LOAD-BEARING CAPABILITY SIGNAL: `oauth::probe_add_capability` deliberately
    // POSTs a blank name to trigger exactly this branch, and reads a STAMPED
    // 400 back as proof this route exists. Changing the status (e.g. to 422),
    // or answering without `stamp_add_endpoint`, silently turns that read into
    // `AddCapability::Unusable` — `login_route` then REFUSES outright (bridge
    // item D) rather than falling back to the file, so this degrades to a
    // needless refusal beside a server that actually has the route, not the
    // whole-file clobber this feature exists to remove. See the seam test
    // `probe_add_capabilitys_blank_body_is_a_stamped_400_and_mutates_nothing`.
    let Some(name) = parsed.name.filter(|n| !n.trim().is_empty()) else {
        return stamp_add_endpoint(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "\"name\" (string) is required and must not be empty.",
            None,
        ));
    };
    // Same load-bearing signal as the blank-name branch above — the probe's
    // body is ALSO blank on `accessToken`, so whichever of the two checks
    // fires first is the one the probe actually exercises. Keep this one a
    // stamped 400 too.
    let Some(access_token) = parsed.access_token.filter(|t| !t.trim().is_empty()) else {
        return stamp_add_endpoint(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "\"accessToken\" (string) is required and must not be empty.",
            None,
        ));
    };
    let refresh_token = parsed.refresh_token.filter(|t| !t.trim().is_empty());
    let no_refresh_token_warning = refresh_token.is_none().then_some(NO_REFRESH_TOKEN_WARNING);

    let account = config::Account {
        name,
        account_type: parsed.account_type.unwrap_or_else(|| "oauth".to_string()),
        account_uuid: parsed.account_uuid,
        org_uuid: parsed.org_uuid,
        org_name: parsed.org_name,
        access_token,
        refresh_token,
        expires_at: parsed.expires_at,
        priority: parsed.priority,
        switch_threshold: parsed.switch_threshold,
        disabled: None,
        extra: serde_json::Map::new(),
    };
    // Captured before the move below: on an ambiguous match this is the ONLY
    // copy of what the caller actually searched for.
    let query_name = account.name.clone();

    match manager.add_or_update_account(account) {
        AddAccountOutcome::Ambiguous(names) => stamp_add_endpoint(error_response(
            StatusCode::CONFLICT,
            "invalid_request_error",
            &crate::cli::ambiguous_query_message(&query_name, &names),
            None,
        )),
        AddAccountOutcome::Added { idx, name, persist } => {
            let warning = combine_warnings(persist.warning(), no_refresh_token_warning);
            add_account_response(name, true, idx, persist, warning)
        }
        AddAccountOutcome::Updated { idx, name, persist } => {
            let warning = combine_warnings(persist.warning(), no_refresh_token_warning);
            add_account_response(name, false, idx, persist, warning)
        }
    }
}

/// Whether a dropped SSE tee chunk represents genuine evidence loss for the
/// truncation classifier. `evidence_dropped` asserts exactly one thing: that
/// bytes arrived from UPSTREAM which the parser never got to see. `Full`
/// answers yes — the bounded channel could not keep up, those bytes are
/// gone, and "no `message_stop`" is no longer trustworthy. `Closed` answers
/// a different question: the CONSUMER (the parser task's own `rx`-backed
/// stream) went away. A consumer going away is never evidence that upstream
/// data was lost, so it must not latch the same flag — the distinction holds
/// no matter why `rx` closed, including a parser that later gains a read
/// timeout, an explicit cancellation, or has its task aborted at shutdown.
///
/// `eventsource-stream`'s `Utf8Stream` never surfaces a UTF-8 error
/// mid-stream — an unresolved tail is buffered, not rejected, until the
/// underlying byte stream itself reaches EOF — so `parse_sse_usage`'s
/// malformed-frame `break` cannot close `rx` while the upstream is still
/// sending; that specific race is not constructible through real TCP
/// behavior. What IS reachable: a parser-side panic drops `rx` while the
/// upstream keeps streaming, and without this split every subsequent
/// `try_send` on that request would latch `evidence_dropped`, making the
/// classifier silently abstain on a turn whose evidence it never actually
/// lost — an unrelated bug quietly disabling this feature.
fn is_genuine_evidence_loss<T>(err: &tokio::sync::mpsc::error::TrySendError<T>) -> bool {
    matches!(err, tokio::sync::mpsc::error::TrySendError::Full(_))
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
    //     covers the bare `/_tcr` too — but only as SPELLED: the compare is
    //     byte-exact and case-sensitive, so `//_tcr/…`, `/_TCR/…` and `/%5ftcr/…`
    //     miss it and are forwarded (measured). See [`LOCAL_PREFIX`] for why that is
    //     a forwarded 404 rather than a hole, and why it is left as it is.
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
    let target = target_host(&parts.uri, &req_headers);
    if let Some(host) = target {
        // [`host_is_loopback`] (base-URL mode) reuses the `IpAddr::is_loopback`
        // primitive the client-peer check uses above, and the same derivation the
        // local-endpoint gate runs — one answer to "which host did the client
        // mean", on both paths.
        if !host_is_loopback(host) && !crate::mitm::host_allowed(host) {
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
    // x-api-key, then body `metadata.user_id`, then (for a loopback
    // `POST /v1/messages` with neither) a hash of the body's cacheable prefix
    // (`system` + `tools`, see [`stable_session_key`] and [`prefix_session_key`])
    // — so a client that drops and reconnects (new connection key) still maps
    // to the SAME account and keeps its per-account prompt cache warm, and even
    // a client with no stable identity at all still pins on what it would cache
    // identically. Only with NEITHER an identity NOR an in-scope cacheable
    // prefix does the request route UNPINNED (plain LRU): a per-connection key
    // is not a session key — it mints a fresh pin per connection that nothing
    // ever removes, and those ghosts (93% of the live pin map, before this tier
    // existed) both bloat it and inflate the pinned-session counts that drive
    // the migration decision in `select`. `session_kind` records WHICH tier
    // produced the key (stable identity / prefix-derived / unpinned fallback)
    // — DISPLAY provenance only, threaded into `record_served`.
    // The raw per-connection key (present iff session affinity is enabled — see
    // `SessionKey`'s doc-comment), threaded separately from `session_key` above:
    // `conn_key` is the noise-routing grain (`Manager::conn_affinity`, keyed on
    // the CONNECTION), `session_key` is the identity-routing grain (keyed on the
    // client's STABLE identity). The two are deliberately different keys for the
    // same underlying extension — conflating them is exactly the ghost-pin bug
    // `SessionKey`'s doc-comment describes for `session_key`.
    let conn_key = parts.extensions.get::<SessionKey>().map(|k| k.0);
    let (session_key, session_kind) = match parts.extensions.get::<SessionKey>() {
        Some(_) => match stable_session_key(
            &req_headers,
            &body_bytes,
            manager.proxy_api_key(),
            &method,
            &path,
            client_is_loopback,
        ) {
            Some((key, kind)) => (Some(key), kind),
            None => (None, SessionKind::Fallback),
        },
        None => (None, SessionKind::Fallback),
    };

    // 3. Selection + rotation loop.
    let account_count = manager.account_count();
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
    // transport failure" once the loop can no longer make progress. The latter is
    // a 502 — EXCEPT when `offline_failures` below says the transport failures
    // were this machine's resolver, which is a 503.
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
    // Accounts already granted their ONE same-account transport retry in this
    // request. The failing resource in a transport error is the CONNECTION, not
    // the account: reqwest pools by `(scheme, authority)` and the account is only a
    // Bearer header, so it is not in the pool key at all. Benching the account for
    // the whole request therefore fails over on the wrong axis — and with a small
    // eligible pool a single blip walks it to empty and answers a 502 (measured:
    // 42 of 205 client-visible 502s fired at `transport_failures == 1`). A dead
    // connection is evicted from the pool by the error itself, so the retry gets a
    // fresh one. Bounded to one per account per request: the second failure IS
    // evidence about the account, and `tried.insert` then behaves exactly as before.
    //
    // Gated on the error KIND. The transport-failure branch below is THREE arms,
    // evaluated in this order — read them as narrowing scope, machine → connection
    // → account:
    //
    //  1. OFFLINE (`is_offline_error`): name resolution failed, so the fault is
    //     the MACHINE. Every account resolves the same hostname, so there is
    //     nothing to bench and nowhere to rotate to. Holds the pin, waits, retries
    //     the same account, and after `MAX_OFFLINE_WAITS_PER_REQUEST` answers 503.
    //  2. SAME-ACCOUNT RETRY (this set): a non-CONNECT failure, i.e. a pooled
    //     CONNECTION died. One retry per account per request, per the rationale
    //     above. Deliberately not taken for a CONNECT failure — there was no
    //     connection to evict, nothing is refreshed by trying again, and the retry
    //     only buys a second connect timeout on a route already not answering.
    //  3. ROTATE: everything else — a route to a resolvable host that is refused,
    //     blackholed or timing out. That IS evidence about this route, so bench the
    //     account and let the rotation do its job.
    let mut transport_retried: HashSet<usize> = HashSet::new();
    // In-place waits this request has spent on a name-resolution failure, and how
    // many such failures it saw at all. Per-REQUEST: an offline machine is one
    // condition shared by every account, so there is nothing to count per account.
    // `offline_failures` outlives the waits so the terminal answer can be the
    // honest 503 even if the request later rotates for an unrelated reason.
    let mut offline_waits = 0u32;
    let mut offline_failures = 0usize;

    for _ in 0..max_attempts {
        let now = OffsetDateTime::now_utc();
        let idx = match next_idx.take() {
            Some(i) => i,
            None => match manager.select(
                &tried,
                now,
                request_model.as_deref(),
                session_key,
                &path,
                conn_key,
            ) {
                Some(idx) => idx,
                None => {
                    if every_attempt_transport_failed(transport_failures, upstream_responses) {
                        // Nothing reached an upstream — but WHY decides the status.
                        // If any attempt died in the resolver, the honest answer is
                        // the recoverable 503, not a 502 asserting the upstream is
                        // unreachable when it is this machine that is off the
                        // network. Reachable here only if the offline arm's own
                        // bound was not what ended the walk (a mixed request that
                        // rotated for another reason first).
                        if offline_failures > 0 {
                            return offline_unavailable(offline_failures);
                        }
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
        // This account's OWN client — fetched INSIDE the loop, keyed on the idx
        // this iteration is actually about to serve. Hoisting this above the
        // loop (as it used to be) reused whichever account was selected FIRST
        // for every later rotation attempt too, which is the bug
        // `AccountRuntime::http` exists to fix: every account collapsing onto
        // one shared connection pool regardless of which one is serving.
        let Some(http) = manager.http_client(idx) else {
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
                // Transport failure is not proof of a bad credential, so nothing
                // here ever disables an account — the three arms differ only in
                // whether this request retries in place, rotates, or gives up.
                // Their order and rationale are at `transport_retried` above. The
                // `reqwest::Error` used to be discarded by a `let Ok(..) else`,
                // so a 502 assembled out of these failures had no line anywhere to
                // attribute it to; `is_connect` / `is_timeout` separate "never
                // reached the host" from "the host went quiet mid-request".
                transport_failures += 1;
                // `?err`, NOT `%err`. reqwest's `Display` prints only its own top
                // frame and never walks `source()`, so the h2 sub-kind
                // (`SendRequest` / `Canceled` / `ChannelClosed`) and its reason code
                // — the only things that say WHY the connection died — are
                // structurally unreachable from a `%` rendering. `Debug` carries the
                // whole chain. Do not "tidy" this back to `%err`.
                // A CONNECT-phase failure is not a blip on a pooled connection —
                // there was no connection. Nothing was evicted, nothing is
                // refreshed by trying again, and the retry is the identical
                // operation with no new information; all it buys is a second full
                // `connect_timeout` (10s, `manager/mod.rs`) on a route that is
                // already not answering. On a blackholed upstream (VPN drop,
                // captive portal, edge unreachable — no RST, no reply) that
                // doubles every request's time-to-502 — ~80s to ~160s on an
                // 8-account fleet — while each request holds its per-account
                // in-flight slot for the whole of it. Bench the account and let
                // the rotation do its job.
                // FIRST arm, ahead of both of the account-level ones: a resolver
                // failure is a fact about this MACHINE, not about the account that
                // happened to be selected. Every account resolves the same
                // hostname, so neither benching this one nor moving to the next can
                // possibly help — it only unpins the session and burns the fleet.
                // Hold the pin, wait briefly, retry the SAME account.
                if is_offline_error(&err) {
                    offline_failures += 1;
                    if offline_waits < MAX_OFFLINE_WAITS_PER_REQUEST {
                        offline_waits += 1;
                        tracing::warn!(
                            account_index = idx,
                            account = account_name.as_deref().unwrap_or("?"),
                            offline_wait = offline_waits,
                            max_offline_waits = MAX_OFFLINE_WAITS_PER_REQUEST,
                            error = ?err,
                            "name resolution failed — this machine is offline; \
                             holding the pinned account and waiting"
                        );
                        next_idx = Some(idx);
                        tokio::time::sleep(Duration::from_secs(OFFLINE_WAIT_SECS)).await;
                        continue;
                    }
                    return offline_unavailable(offline_failures);
                }
                if !err.is_connect() && transport_retried.insert(idx) {
                    // First blip on this account this request: retry it on a fresh
                    // connection rather than benching an account that is probably fine.
                    tracing::warn!(
                        account_index = idx,
                        account = account_name.as_deref().unwrap_or("?"),
                        is_connect = err.is_connect(),
                        is_timeout = err.is_timeout(),
                        error = ?err,
                        "upstream transport failure — retrying the SAME account on a fresh connection"
                    );
                    next_idx = Some(idx);
                    continue;
                }
                tracing::warn!(
                    account_index = idx,
                    account = account_name.as_deref().unwrap_or("?"),
                    is_connect = err.is_connect(),
                    is_timeout = err.is_timeout(),
                    error = ?err,
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
                    &path,
                    conn_key,
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
            // Cloned, not moved: the streaming arm below still needs
            // `account_name` to log a mid-stream transport failure by name.
            account: account_name.clone().unwrap_or_default(),
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
            // Distinguishes WHY the tee ends. A plain `.map()` over the upstream
            // stream (this passthrough's previous shape) only ever sees `Some`
            // items — it cannot observe `Ready(None)`/the stream being dropped,
            // so a client hitting Esc mid-turn (axum drops the response body,
            // dropping everything the passthrough owns) looked IDENTICAL, from
            // inside the parser, to the upstream finishing the stream itself.
            // This oneshot is consumed exactly once, the moment `TeeState` (below)
            // observes the WRAPPED stream reach a genuine terminal state — clean
            // EOF or a transport error, both real signals ABOUT THE UPSTREAM. If
            // `TeeState` is dropped before either happens (client walked away),
            // the sender drops unfired and `ended_rx` resolves to `Err` — the
            // signal for "we never learned how the stream would have ended,"
            // which must never be booked as truncation.
            let (ended_tx, ended_rx) = tokio::sync::oneshot::channel::<()>();
            // Set the instant a teed chunk is dropped by the bounded channel
            // below (full channel or a dead receiver). A dropped chunk might
            // have been the one carrying `message_stop` — from that point on,
            // "no message_stop was observed" no longer means "the turn was
            // truncated", it means "we don't know", so the classifier abstains
            // instead of guessing.
            let evidence_dropped = Arc::new(AtomicBool::new(false));
            let evidence_dropped_reader = Arc::clone(&evidence_dropped);
            let manager_side = manager.clone();
            let status_is_success = status.is_success();
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
                //
                // A forwarded 3xx/4xx/5xx SSE body never had a `message_stop`
                // contract to begin with — it is an error response, not a
                // truncated turn — so the SYNTHESIZED verdict below only fires
                // once the upstream actually answered 2xx. That status gate
                // must scope to the synthesized branch ALONE: a genuine in-band
                // `error` event is a positive observation read directly off
                // the wire, independent of the response's status line, and
                // wrapping it in the same `if` too was a coverage regression —
                // a real error on a forwarded non-2xx body went unrecorded.
                if let Some(kind) = stream_error {
                    if kind == TRUNCATED_STREAM_ERROR_KIND {
                        // The synthesized verdict is an ABSENCE (no
                        // `message_stop` seen) rather than a positive
                        // observation, and an absence is only evidence when we
                        // know we saw everything there was to see — hence the
                        // status gate plus the ended/evidence checks below,
                        // which the positive case in the `else` skips entirely.
                        if status_is_success {
                            let ended_naturally = ended_rx.await.is_ok();
                            let dropped = evidence_dropped_reader.load(Ordering::SeqCst);
                            if ended_naturally && !dropped {
                                manager_side.record_stream_error(idx, &kind);
                            } else {
                                tracing::debug!(
                                    account_index = idx,
                                    ended_naturally,
                                    evidence_dropped = dropped,
                                    "SSE stream ended without message_stop but the tee \
                                     cannot prove why — abstaining rather than recording \
                                     a fabricated truncation"
                                );
                            }
                        }
                    } else {
                        // An in-band `error` event is a POSITIVE observation —
                        // we directly read those bytes off the wire, so it is
                        // recorded regardless of the response status and
                        // regardless of how the tee itself ended.
                        manager_side.record_stream_error(idx, &kind);
                    }
                }
            });

            // Threads the tee's mutable state — the wrapped byte stream, the
            // side-channel sender, the dropped-evidence flag, the `ended`
            // oneshot, and the `_in_flight` RAII guard — through
            // `stream::unfold` so all of it lives exactly as long as the
            // passthrough stream does and drops in ONE place, together. That
            // single drop site is what lets `ended` (this arm's job: tell the
            // parser task client-abandonment from upstream-completion) and
            // `_in_flight` (unrelated: pacing's load counter) each fire exactly
            // once, at real completion — clean EOF, transport error, OR client
            // disconnect — rather than at response-headers. See the long-form
            // comment this replaced for why `_in_flight` cannot just be a
            // handler-local.
            // Generic over the concrete upstream stream type rather than boxed
            // as `dyn Stream` — there is exactly one call site (below), so a
            // trait object bought nothing but a v-table indirection and a heap
            // allocation on every streamed response.
            struct TeeState<S> {
                inner: S,
                tx: tokio::sync::mpsc::Sender<Bytes>,
                evidence_dropped: Arc<AtomicBool>,
                ended: Option<tokio::sync::oneshot::Sender<()>>,
                account_index: usize,
                account_name: Option<String>,
                _in_flight: InFlightGuard,
            }
            let state = TeeState {
                inner: resp.bytes_stream(),
                tx,
                evidence_dropped,
                ended: Some(ended_tx),
                account_index: idx,
                // Moved, not cloned: this is the last use of `account_name` in
                // this iteration — the retry loop above already re-fetches its
                // own copy from `manager.account_name(idx)` on the next pass.
                account_name,
                _in_flight,
            };
            let passthrough = futures::stream::unfold(state, |mut state| async move {
                match state.inner.next().await {
                    Some(Ok(bytes)) => {
                        // Best-effort tee: `try_send` never blocks the
                        // passthrough. A FULL channel (slow/starved parser)
                        // drops the chunk for the PARSER only — usage counting
                        // becomes best-effort under backpressure; the client
                        // stream forwards untouched. That drop is genuine
                        // evidence loss and flips `evidence_dropped` so the
                        // parser task can tell "no message_stop" apart from
                        // "no message_stop THAT WE COULD SEE". A CLOSED
                        // receiver is a different thing entirely — the parser
                        // task's own `rx`-side stream already ended and it does
                        // not want any more bytes — so it must not poison the
                        // verdict the same way; see [`is_genuine_evidence_loss`].
                        if let Err(err) = state.tx.try_send(bytes.clone()) {
                            if is_genuine_evidence_loss(&err) {
                                state.evidence_dropped.store(true, Ordering::SeqCst);
                            }
                        }
                        Some((Ok(bytes), state))
                    }
                    Some(Err(err)) => {
                        // The transport died AFTER headers were already
                        // forwarded — this `Err` is what silently reached the
                        // client as a severed connection while the request was
                        // already booked as a clean 200 upstream response.
                        // Nothing else observes a body-level transport
                        // failure, so this is the only place it is ever
                        // logged. Same field shape as the pre-header
                        // transport-failure ladder above, so both halves of a
                        // request's lifecycle log alike. It is ALSO a genuine
                        // observation about the upstream, not a client
                        // abandonment — fire `ended` before returning it.
                        tracing::warn!(
                            account_index = state.account_index,
                            account = state.account_name.as_deref().unwrap_or("?"),
                            error = ?err,
                            "upstream transport failure mid-stream — forwarding the \
                             severed connection to the client"
                        );
                        if let Some(ended) = state.ended.take() {
                            let _ = ended.send(());
                        }
                        Some((Err(err), state))
                    }
                    None => {
                        // Clean EOF — the upstream itself closed the stream.
                        // Also a genuine observation. Client abandonment never
                        // reaches this arm at all: axum drops the response
                        // body (hence this whole stream, mid-poll) instead of
                        // ever seeing it reach `None`, which drops `state` —
                        // and `state.ended` along with it — unfired.
                        if let Some(ended) = state.ended.take() {
                            let _ = ended.send(());
                        }
                        None
                    }
                }
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
///
/// `message_stop` is Anthropic's ONLY marker for "this turn completed" — it
/// does not appear anywhere else in this crate (grepped: zero hits before this
/// function existed). A stream that ends without one — the transport dying
/// mid-body, or the malformed/utf8 `break` above — is exactly as truncated as
/// one that carries an in-band `error` event, so it gets the same treatment:
/// `stream_error` is set, this time to the fixed kind [`TRUNCATED_STREAM_ERROR_KIND`],
/// UNLESS an `error` event already explained the failure (that one names a
/// root cause; this one only names the absence of proof the turn finished).
///
/// The remaining case is a stream that never saw `message_start` — this used
/// to suppress the verdict unconditionally, on the theory that such a stream
/// was "never confirmed to be a Messages turn" (a non-2xx SSE error body, a
/// HEAD response, or a future non-Messages endpoint sharing the content-type).
/// That conflated two shapes that are NOT the same: a stream cut off before
/// its first parseable event (zero events observed at all — `!saw_any_event`)
/// and a stream that genuinely produced OTHER events but none of them
/// `message_start` (`saw_any_event && !saw_message_start`). Only the second
/// one is evidence the stream was never a Messages turn; the first is
/// indistinguishable from a Messages turn severed before `message_start` ever
/// arrived — the single worst case this classifier exists to catch, since it
/// otherwise records nothing at all. So the verdict fires when `message_start`
/// was seen (truncated mid-turn) OR nothing was ever seen (severed before the
/// first event) — and stays silent only when events arrived that affirmatively
/// were not `message_start`.
async fn parse_sse_usage<S, B, E>(stream: S) -> (ParsedUsage, Option<String>)
where
    S: futures::Stream<Item = Result<B, E>>,
    B: AsRef<[u8]>,
{
    let events = stream.eventsource();
    futures::pin_mut!(events);

    let mut parsed = ParsedUsage::default();
    let mut stream_error: Option<String> = None;
    let mut saw_any_event = false;
    let mut saw_message_start = false;
    let mut saw_message_stop = false;
    while let Some(item) = events.next().await {
        let Ok(event) = item else {
            break; // malformed/utf8/transport error — stop parsing, keep totals
        };
        if event.data.is_empty() {
            continue;
        }
        saw_any_event = true;
        let Ok(value) = serde_json::from_str::<Value>(&event.data) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                saw_message_start = true;
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
            Some("message_stop") => {
                saw_message_stop = true;
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
    // The loop exited — either the upstream closed cleanly after `message_stop`,
    // or it stopped short. No terminator and no more specific error already
    // recorded means the turn's ending was never observed: that IS truncation,
    // for either of two shapes — `message_start` was seen (a confirmed Messages
    // turn that never reached its terminator) or NOTHING was ever seen at all
    // (`!saw_any_event`, severed before the stream produced a single event —
    // indistinguishable from a Messages turn cut off before `message_start`,
    // so it gets the same verdict rather than a silent pass). The one case
    // that stays silent is events having arrived that affirmatively were not
    // `message_start` — that is the actual proof this was never a Messages
    // turn (see the doc comment above).
    if !saw_message_stop && stream_error.is_none() && (saw_message_start || !saw_any_event) {
        stream_error = Some(TRUNCATED_STREAM_ERROR_KIND.to_string());
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

/// `503 + Retry-After` when name resolution kept failing: this machine is off the
/// network, which is recoverable and says nothing about any account.
///
/// Deliberately not [`bad_gateway`]. A 502 asserts "every attempt failed in
/// transport, none reached an upstream" — true, but it points the client at the
/// upstream when the fault is local, and it carries no `retry-after`, so a client
/// has no guidance beyond "give up". The account pool is NOT rotated on this path
/// and the session keeps its pin.
fn offline_unavailable(dns_failures: usize) -> Response {
    tracing::warn!(
        dns_failures,
        retry_after = OFFLINE_RETRY_AFTER_SECS,
        "returning 503 to client — name resolution is failing, this machine is offline; \
         the account pool was NOT rotated"
    );
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "proxy_error",
        &format!(
            "Name resolution failed on {dns_failures} attempt(s) — this machine appears to be \
             offline. No account was rotated. Retry in {OFFLINE_RETRY_AFTER_SECS}s."
        ),
        Some(OFFLINE_RETRY_AFTER_SECS),
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
            control_account: None,
            control_reserve: 0.05,
            http1_only: false,
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

    /// The scope tier 3 requires: a loopback `POST /v1/messages`. Tiers 1/2
    /// don't care about method/path/loopback, so tests exercising ONLY those
    /// tiers call `stable_session_key` through this helper rather than
    /// re-stating the tier-3 gate at every call site; tests of the gate itself
    /// call `stable_session_key` directly with a different method/path/loopback.
    fn messages_key(
        headers: &HeaderMap,
        body: &[u8],
        proxy_key: Option<&str>,
    ) -> Option<(u64, SessionKind)> {
        stable_session_key(
            headers,
            body,
            proxy_key,
            &Method::POST,
            "/v1/messages",
            true,
        )
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
        let a = messages_key(&h, b"{}", None);
        let b = messages_key(&h, b"{}", None);
        assert_eq!(a, b, "same x-api-key must survive a reconnect");
        assert!(a.is_some());
    }

    #[test]
    fn stable_session_key_is_deterministic_for_user_id() {
        let body = br#"{"metadata":{"user_id":"user-123"},"messages":[]}"#;
        let h = HeaderMap::new();
        let a = messages_key(&h, body, None);
        let b = messages_key(&h, body, None);
        assert_eq!(a, b, "same user_id must survive a reconnect");
        assert!(a.is_some());
    }

    #[test]
    fn stable_session_key_prefers_api_key_over_user_id() {
        let body = br#"{"metadata":{"user_id":"user-123"}}"#;
        let with_key = messages_key(&headers_with_api_key("the-key"), body, None);
        let key_only = messages_key(&headers_with_api_key("the-key"), b"{}", None);
        assert_eq!(with_key, key_only, "x-api-key must win over user_id");
    }

    #[test]
    fn stable_session_key_namespaces_key_vs_uid() {
        // An x-api-key "abc" and a user_id "abc" must NOT collide.
        let from_key = messages_key(&headers_with_api_key("abc"), b"{}", None);
        let from_uid = messages_key(
            &HeaderMap::new(),
            br#"{"metadata":{"user_id":"abc"}}"#,
            None,
        );
        assert_ne!(from_key, from_uid, "prefixes must isolate the two spaces");
    }

    #[test]
    fn stable_session_key_none_without_identity() {
        // No x-api-key, no top-level metadata.user_id, no system/tools → None
        // even ON the endpoint and origin tier 3 is scoped to.
        assert_eq!(
            messages_key(&HeaderMap::new(), br#"{"messages":[]}"#, None),
            None
        );
    }

    #[test]
    fn stable_session_key_ignores_nested_user_id() {
        // A user_id nested in message content is NOT top-level metadata.
        let body = br#"{"messages":[{"role":"user","content":{"metadata":{"user_id":"nested"}}}]}"#;
        assert_eq!(messages_key(&HeaderMap::new(), body, None), None);
    }

    #[test]
    fn stable_session_key_distinguishes_different_api_keys() {
        let a = messages_key(&headers_with_api_key("key-a"), b"{}", None);
        let b = messages_key(&headers_with_api_key("key-b"), b"{}", None);
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
            messages_key(&headers_with_api_key(shared), b"{}", Some(shared)),
            None,
            "the shared proxy key must not be used as an affinity discriminator"
        );
        // With a body user_id, it falls through to that instead of the shared key.
        let body = br#"{"metadata":{"user_id":"user-123"}}"#;
        let via_shared = messages_key(&headers_with_api_key(shared), body, Some(shared));
        let via_uid = messages_key(&HeaderMap::new(), body, None);
        assert_eq!(
            via_shared, via_uid,
            "with the shared key skipped, the user_id is the discriminator"
        );
        assert!(via_shared.is_some());
        // A DIFFERENT (genuine team) key with the same proxy_key configured is
        // still used — only the exact shared secret is skipped.
        let team = messages_key(&headers_with_api_key("sk-team-alice"), b"{}", Some(shared));
        assert!(
            team.is_some(),
            "a distinct team key is a real identity and must still key"
        );
    }

    #[test]
    fn stable_session_key_falls_back_to_prefix_hash_of_system_and_tools() {
        // No x-api-key, no metadata.user_id, but a system + tools prefix on an
        // in-scope (loopback POST /v1/messages) request — tier 3 pins on a hash
        // of that prefix instead of routing unpinned.
        let body = br#"{"system":"You are a helpful assistant.","tools":[{"name":"bash"}]}"#;
        let a = messages_key(&HeaderMap::new(), body, None);
        let b = messages_key(&HeaderMap::new(), body, None);
        assert_eq!(a, b, "same prefix must hash the same every time");
        assert_eq!(
            a.map(|(_, kind)| kind),
            Some(SessionKind::Prefix),
            "tier 3 must record SessionKind::Prefix, not Stable"
        );
    }

    #[test]
    fn stable_session_key_prefix_hash_accepts_system_or_tools_alone() {
        // Either field alone is a cacheable prefix — both need not be present.
        let system_only = messages_key(&HeaderMap::new(), br#"{"system":"hi"}"#, None);
        let tools_only = messages_key(&HeaderMap::new(), br#"{"tools":[{"name":"x"}]}"#, None);
        assert!(system_only.is_some(), "system alone must pin");
        assert!(tools_only.is_some(), "tools alone must pin");
        assert_ne!(
            system_only, tools_only,
            "a system-only and a tools-only prefix are different prefixes"
        );
    }

    #[test]
    fn stable_session_key_prefix_hash_distinguishes_different_prefixes() {
        let a = messages_key(&HeaderMap::new(), br#"{"system":"prompt A"}"#, None);
        let b = messages_key(&HeaderMap::new(), br#"{"system":"prompt B"}"#, None);
        assert_ne!(
            a, b,
            "distinct prefixes must spread across the fleet, not collide"
        );
    }

    #[test]
    fn stable_session_key_prefix_hash_is_byte_exact_not_canonicalized() {
        // Same fields, different KEY ORDER inside the `system` value. Anthropic's
        // own cache is byte-exact, so these are two DIFFERENT cache entries —
        // canonicalizing (sorting keys) before hashing would merge them onto one
        // account for zero cache benefit and only concentrate load.
        let a = messages_key(&HeaderMap::new(), br#"{"system":{"a":1,"b":2}}"#, None);
        let b = messages_key(&HeaderMap::new(), br#"{"system":{"b":2,"a":1}}"#, None);
        assert_ne!(
            a, b,
            "raw bytes must be hashed verbatim — key order must not be canonicalized"
        );
    }

    #[test]
    fn stable_session_key_prefix_hash_distinguishes_field_identity() {
        // The SAME raw text "ab", attached to a DIFFERENT field. A naive
        // implementation that concatenated system+tools into one string before
        // hashing (e.g. `format!("{system}{tools}")`) would treat
        // `{"system":"ab"}` (tools absent, so "" via unwrap_or) and
        // `{"tools":"ab"}` (system absent) as the identical string "ab" and hash
        // them the same. Hashing each field as its own `Option<&str>` must not.
        let system_ab = messages_key(&HeaderMap::new(), br#"{"system":"ab"}"#, None);
        let tools_ab = messages_key(&HeaderMap::new(), br#"{"tools":"ab"}"#, None);
        assert_ne!(
            system_ab, tools_ab,
            "system:\"ab\" and tools:\"ab\" must not collide"
        );
    }

    #[test]
    fn stable_session_key_prefix_hash_resists_boundary_shift() {
        // Bare JSON NUMBERS, not quoted strings: a quoted string's own `"`
        // delimiters would accidentally break a naive concatenation apart, which
        // defeats the point of this test. Numbers have no such delimiter, so a
        // concatenating implementation genuinely collides here: `system:12` +
        // `tools:3` and `system:1` + `tools:23` both concatenate their raw text
        // to "123". `str`'s own `Hash` impl appends a sentinel byte after each
        // value specifically to prevent this; hashing system and tools as two
        // separate `.hash()` calls relies on it.
        let a = messages_key(&HeaderMap::new(), br#"{"system":12,"tools":3}"#, None);
        let b = messages_key(&HeaderMap::new(), br#"{"system":1,"tools":23}"#, None);
        assert_ne!(a, b, "a field boundary shift must not collide");
    }

    #[test]
    fn stable_session_key_prefix_none_without_system_or_tools() {
        // Neither system nor tools → no cacheable prefix → stay unpinned even
        // in scope. This is the guard that stops every trivial anonymous
        // request from piling onto one account.
        assert_eq!(
            messages_key(&HeaderMap::new(), br#"{"messages":[]}"#, None),
            None
        );
    }

    #[test]
    fn stable_session_key_prefix_is_the_last_resort_after_identity_tiers() {
        // x-api-key and metadata.user_id both outrank the prefix hash even when a
        // cacheable prefix is also present.
        let body = br#"{"system":"hi","metadata":{"user_id":"user-123"}}"#;
        let with_key = messages_key(&headers_with_api_key("the-key"), body, None).map(|(_, k)| k);
        assert_eq!(with_key, Some(SessionKind::Stable), "x-api-key still wins");

        let with_uid = messages_key(&HeaderMap::new(), body, None).map(|(_, k)| k);
        assert_eq!(
            with_uid,
            Some(SessionKind::Stable),
            "user_id still wins over the prefix hash"
        );
    }

    /// The cacheable-prefix body used by every tier-3 scope-gating test below —
    /// no x-api-key, no user_id, so tier 3 is the only tier that could fire.
    const PREFIX_ONLY_BODY: &[u8] = br#"{"system":"You are a helpful assistant."}"#;

    #[test]
    fn stable_session_key_prefix_requires_exact_v1_messages_path() {
        // In scope: exact match.
        assert!(
            stable_session_key(
                &HeaderMap::new(),
                PREFIX_ONLY_BODY,
                None,
                &Method::POST,
                "/v1/messages",
                true,
            )
            .is_some(),
            "an exact /v1/messages match must pin"
        );
        // Out of scope: a LONGER path must not match as a prefix — this is the
        // exact trap the scope gate exists to close. /v1/messages/count_tokens
        // never uses prompt caching (Anthropic's own docs), so pinning it would
        // concentrate load for zero cache benefit.
        assert_eq!(
            stable_session_key(
                &HeaderMap::new(),
                PREFIX_ONLY_BODY,
                None,
                &Method::POST,
                "/v1/messages/count_tokens",
                true,
            ),
            None,
            "/v1/messages/count_tokens must NOT prefix-match /v1/messages"
        );
        // Defense in depth: an unstripped query string must not match either,
        // even though the real call site (`handle`) always passes an
        // already-stripped path — this guards the match itself, not just the
        // caller's discipline.
        assert_eq!(
            stable_session_key(
                &HeaderMap::new(),
                PREFIX_ONLY_BODY,
                None,
                &Method::POST,
                "/v1/messages?beta=true",
                true,
            ),
            None,
            "a path carrying its query string must not match /v1/messages"
        );
    }

    #[test]
    fn stable_session_key_prefix_requires_post() {
        assert_eq!(
            stable_session_key(
                &HeaderMap::new(),
                PREFIX_ONLY_BODY,
                None,
                &Method::GET,
                "/v1/messages",
                true,
            ),
            None,
            "a GET must not pin on the cacheable prefix"
        );
    }

    #[test]
    fn stable_session_key_prefix_requires_loopback() {
        // Tier 1 already refuses to key on x-api-key when it equals the shared
        // proxy secret, precisely so remote clients sharing that secret don't
        // collapse onto one account (`stable_session_key_skips_shared_proxy_key`
        // above). Tier 3 has no secret to check, so without a loopback gate N
        // remote callers sharing one system prompt would collapse through the
        // same back door.
        assert_eq!(
            stable_session_key(
                &HeaderMap::new(),
                PREFIX_ONLY_BODY,
                None,
                &Method::POST,
                "/v1/messages",
                false,
            ),
            None,
            "a non-loopback caller must not pin on the cacheable prefix"
        );
    }

    /// End-to-end through the REAL router (`app()`, not the unit-tested
    /// `stable_session_key` directly): a loopback `POST /v1/messages?beta=true`
    /// — the exact shape live traffic sends, query string included — with no
    /// `x-api-key` and no `metadata.user_id`, but a `system` field, must record
    /// as `SessionKind::Prefix` in the manager's own session snapshot.
    ///
    /// This is the one seam the unit tests above cannot cover: they call
    /// `stable_session_key` with a hand-fed `path`, which proves the MATCH
    /// logic but nothing about which variable the real call site passes it.
    /// `handle` has two candidates in scope — `path` (query-stripped) and
    /// `path_and_query` (raw) — and passing the wrong one would silently
    /// unpin 100% of real `/v1/messages?beta=true` traffic while every one of
    /// those unit tests kept passing, because they never touch `handle` at
    /// all. Driving a real request through `app()` with the query string
    /// attached is what actually pins the wiring, not just the function.
    #[tokio::test]
    async fn real_request_through_app_pins_on_prefix_for_loopback_v1_messages() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tower::ServiceExt as _;

        struct NoRefresh;
        impl crate::oauth::TokenRefresher for NoRefresh {
            fn refresh(&self, _t: String) -> crate::oauth::RefreshFuture {
                Box::pin(async { Err(crate::oauth::OAuthError::Transient("unused".into())) })
            }
        }

        // Fake upstream: one connection, one 200 with a minimal usage body.
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = upstream.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = upstream.accept().await {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let body = br#"{"usage":{"input_tokens":1,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":1}}"#;
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body).await;
            }
        });

        let config = dummy_config(None, &format!("http://{up_addr}"));
        let manager = Manager::new(
            config,
            Arc::new(NoRefresh),
            Arc::new(crate::probe::LiveUsageProber::new()),
            Arc::new(crate::warmer::LiveWarmer::new()),
            None,
        );

        // No x-api-key, no metadata.user_id — only a `system` field. Real
        // `/v1/messages` calls always carry the query string tacked on by
        // live clients (`?beta=true`); this is not incidental to the test, it
        // is the exact thing D1 exists to pin.
        let body = serde_json::to_vec(&serde_json::json!({
            "system": "You are a helpful assistant.",
            "messages": [],
        }))
        .unwrap();

        let mut req = Request::builder()
            .method(Method::POST)
            .uri("/v1/messages?beta=true")
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .expect("build request");
        // Session affinity's feature flag (see `SessionKey`'s doc-comment) and
        // the loopback proof `handle`'s auth gate and tier 3 both read — both
        // injected by the hybrid listener per real connection; `app()` alone
        // injects neither, so a request driven straight at it needs both by
        // hand.
        req.extensions_mut().insert(SessionKey(0xF00D));
        req.extensions_mut().insert(ClientAddr(loopback_peer()));

        let response = app(manager.clone()).oneshot(req).await.expect("response");
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "the request must actually reach and clear the fake upstream"
        );

        let snap = manager.snapshot(OffsetDateTime::now_utc());
        // Exactly one request was ever served by this manager, so its one
        // session is the whole list — no need to reach for the private
        // `short_session_id` helper `manager/mod.rs` uses to derive `s.id`.
        let session = snap
            .sessions
            .first()
            .expect("the request must have been tracked as a session");
        assert_eq!(
            session.kind,
            SessionKind::Prefix,
            "a loopback POST /v1/messages?beta=true with a system field and no \
             identity must pin via tier 3 through the REAL call site, not just \
             the unit-tested function"
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
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        // Split mid-way through the first event's data line.
        let split = 60usize;
        let chunks = vec![
            Ok::<Bytes, Infallible>(Bytes::copy_from_slice(&full.as_bytes()[..split])),
            Ok::<Bytes, Infallible>(Bytes::copy_from_slice(&full.as_bytes()[split..])),
        ];
        let (parsed, stream_error) = parse_sse_usage(futures::stream::iter(chunks)).await;
        assert_eq!(
            stream_error, None,
            "a stream that reaches message_stop has no error event"
        );
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
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        let stream = futures::stream::iter(vec![Ok::<Bytes, Infallible>(Bytes::from(full))]);
        let (parsed, stream_error) = parse_sse_usage(stream).await;
        assert_eq!(
            stream_error, None,
            "a stream that reaches message_stop has no error event"
        );
        assert_eq!(parsed.input_total, 5);
        assert_eq!(parsed.output, 37, "final cumulative output, not 20 + 37");
        // No cache tokens in this fixture → both stay zero.
        assert_eq!(parsed.cache_read, 0);
        assert_eq!(parsed.cache_creation, 0);
    }

    /// FALSE-POSITIVE case #3 (contract-derived, not a path allowlist): a
    /// stream that PRODUCED events but none of them `message_start` was
    /// never confirmed to BE a Messages turn in the first place — a future
    /// non-Messages endpoint that happens to share `text/event-stream` looks
    /// exactly like this from inside `parse_sse_usage`. This is the only
    /// shape that earns the exemption. A round-4 fix (see the sibling test
    /// below) narrowed this from "no message_start" to "no message_start
    /// AND at least one other event arrived" — a stream with ZERO events is
    /// a different, indistinguishable-from-severed-early shape and must NOT
    /// share this exemption; conflating the two was exactly the round-4 bug.
    #[tokio::test]
    async fn sse_stream_with_non_message_events_records_no_stream_error() {
        // A heartbeat-shaped event, never `message_start` — stands in for a
        // non-Messages endpoint that happens to emit `text/event-stream`.
        let full = concat!("event: heartbeat\n", "data: {\"type\":\"heartbeat\"}\n\n",);
        let stream = futures::stream::iter(vec![Ok::<Bytes, Infallible>(Bytes::from(full))]);
        let (parsed, stream_error) = parse_sse_usage(stream).await;
        assert_eq!(parsed, ParsedUsage::default());
        assert_eq!(
            stream_error, None,
            "a stream that produced events but none of them message_start \
             was genuinely never a Messages turn — it must not be classified \
             as truncated just because it also has no message_stop"
        );
    }

    /// THE SEVEREST case this classifier exists to catch (round-4 fix): a
    /// stream cut off BEFORE its first parseable event ever arrives. Before
    /// this fix, `saw_message_start` gated the verdict on its own, so this
    /// looked IDENTICAL to the sibling test above — zero events either way —
    /// and a 2xx Messages turn severed before `message_start` recorded
    /// NOTHING: zero content delivered to the client, `tcr status` showing a
    /// perfectly healthy account. That is precisely "a truncated turn booked
    /// as a clean serve," the bug this whole feature exists to catch. The
    /// fix keys on `saw_any_event` instead of `saw_message_start` alone: zero
    /// events is indistinguishable from a Messages turn severed early, so it
    /// gets the SAME verdict as the confirmed-truncated case, not the
    /// not-a-Messages-turn exemption above.
    #[tokio::test]
    async fn sse_stream_severed_before_any_event_records_truncation() {
        let stream = futures::stream::iter(Vec::<Result<Bytes, Infallible>>::new());
        let (parsed, stream_error) = parse_sse_usage(stream).await;
        assert_eq!(parsed, ParsedUsage::default());
        assert_eq!(
            stream_error.as_deref(),
            Some(TRUNCATED_STREAM_ERROR_KIND),
            "a stream that produced ZERO parseable events before ending must \
             be treated as truncated, not silently passed — it is \
             indistinguishable from a Messages turn severed before \
             message_start ever arrived"
        );
    }

    /// MUST-RECORD: "a malformed/utf8 frame that ends parsing mid-turn ⇒
    /// truncation, not abstention." `eventsource-stream` 0.2.3's `Utf8Stream`
    /// never surfaces a UTF-8 error MID-stream (it buffers an unresolved
    /// tail hoping the next chunk completes it) — the error only surfaces
    /// once the underlying byte stream ends with that tail still unresolved
    /// (verified by reading `utf8_stream.rs`: mid-stream `Ok` is returned
    /// unconditionally; `Err` is only constructed in the `Poll::Ready(None)`
    /// arm). So "malformed frame" and "stream end" are the same moment here
    /// — this fixture reproduces that: a confirmed `message_start`, then one
    /// byte (`0xFF`) that can never resolve into valid UTF-8 no matter what
    /// follows, and nothing follows. `parse_sse_usage`'s `break` on that `Err`
    /// must still leave `saw_message_start` set, so the post-loop check
    /// classifies it exactly like the ordinary "no message_stop" case.
    #[tokio::test]
    async fn sse_stream_with_malformed_utf8_after_message_start_records_truncation() {
        let head = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\
             \"input_tokens\":5,\"output_tokens\":1}}}\n\n",
        );
        let chunks = vec![
            Ok::<Bytes, Infallible>(Bytes::from(head)),
            Ok::<Bytes, Infallible>(Bytes::copy_from_slice(&[0xFFu8])),
        ];
        let (parsed, stream_error) = parse_sse_usage(futures::stream::iter(chunks)).await;
        assert_eq!(
            parsed.input_total, 5,
            "usage from message_start is retained even though the stream ends malformed"
        );
        assert_eq!(
            stream_error.as_deref(),
            Some(TRUNCATED_STREAM_ERROR_KIND),
            "a confirmed Messages turn that ends on an unresolvable malformed \
             byte must be treated as truncated, not silently abstained"
        );
    }

    /// The other half of the malformed-frame property: `evidence_dropped`
    /// must latch on `Full` (a real dropped chunk — genuine evidence loss)
    /// and NOT on `Closed` (the consumer went away — never evidence about
    /// upstream data). See [`is_genuine_evidence_loss`]'s doc comment for why
    /// that distinction holds independent of the malformed-frame path
    /// specifically. Exercised directly against the extracted predicate
    /// rather than a network-level reproduction of the race, which the
    /// malformed-frame path alone cannot trigger.
    #[test]
    fn evidence_loss_predicate_distinguishes_full_from_closed() {
        use tokio::sync::mpsc::error::TrySendError;
        assert!(
            is_genuine_evidence_loss(&TrySendError::Full(())),
            "a full channel is genuine evidence loss — the parser never saw those bytes"
        );
        assert!(
            !is_genuine_evidence_loss(&TrySendError::Closed(())),
            "a closed receiver is the parser's own exit, not evidence loss"
        );
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

    // --- POST /_tcr/accounts/disabled ---------------------------------------

    /// A two-account config with distinct orgs, written to `path` so the manager
    /// that boots from it has a real durable half to write back to.
    ///
    /// Obviously-fake identities only: this repository is public.
    fn two_account_config(api_key: Option<&str>) -> Config {
        let account = |name: &str, org: &str, uuid: &str, priority: i64| Account {
            name: name.to_string(),
            account_type: "oauth".to_string(),
            account_uuid: Some(uuid.to_string()),
            org_uuid: Some(format!("11111111-1111-1111-1111-{org}")),
            org_name: Some(format!("Org {org}")),
            access_token: format!("at-{org}"),
            refresh_token: Some(format!("rt-{org}")),
            // Not expired, so booting triggers no token refresh.
            expires_at: Some(crate::now_ms() + 3_600_000),
            priority: Some(priority),
            switch_threshold: None,
            disabled: None,
            extra: serde_json::Map::new(),
        };
        Config {
            accounts: vec![
                account("alice@example.com", "aaaaaaaaaaaa", "22222222-a", 0),
                account("bob@example.com", "bbbbbbbbbbbb", "33333333-b", 1),
            ],
            ..dummy_config(api_key, "http://127.0.0.1:1")
        }
    }

    fn control_config_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "tcr-control-test-{tag}-{}-{}.json",
            std::process::id(),
            crate::now_ms()
        ))
    }

    /// A manager booted from a config that is ALSO on disk, so both halves of a
    /// `disabled` change are observable: `manager.snapshot()` for the live rotation
    /// and the file for the durable flag.
    fn control_manager(tag: &str, api_key: Option<&str>) -> (Arc<Manager>, std::path::PathBuf) {
        let path = control_config_path(tag);
        let config = two_account_config(api_key);
        crate::config::save(&path, &config).expect("write the test config");
        let manager = Manager::with_live_refresher(config, Some(path.clone()));
        (manager, path)
    }

    /// Drive the router with a `ClientAddr` we control, like [`status_request`].
    /// `body` is sent verbatim so a malformed one is testable.
    async fn control_request(
        manager: Arc<Manager>,
        method: Method,
        peer: Option<SocketAddr>,
        api_key: Option<&str>,
        body: &str,
    ) -> (StatusCode, HeaderMap, Bytes) {
        use tower::ServiceExt as _;
        let mut builder = Request::builder().method(method).uri(DISABLED_PATH);
        if let Some(key) = api_key {
            builder = builder.header("x-api-key", key);
        }
        let mut req = builder
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .expect("build request");
        if let Some(addr) = peer {
            req.extensions_mut().insert(ClientAddr(addr));
        }
        let response = app(manager).oneshot(req).await.expect("router response");
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), MAX_BODY_BYTES)
            .await
            .expect("read body");
        (status, headers, bytes)
    }

    fn disabled_in_file(path: &std::path::Path, index: usize) -> Option<serde_json::Value> {
        let raw = std::fs::read_to_string(path).expect("read the test config");
        let doc: serde_json::Value = serde_json::from_str(&raw).expect("valid json");
        doc["accounts"][index].get("disabled").cloned()
    }

    /// THE BITING TEST for the whole change: after the endpoint answers, the
    /// account is parked **in the process that will serve the next request**, and
    /// the config file carries it too.
    ///
    /// Pre-change there was no route at all, and `tcr disable` wrote only the file
    /// — so the running rotation kept the account. Asserting HTTP 200 alone would
    /// re-create exactly that defect at a new altitude: the in-memory assertion is
    /// the one that fails against a handler that answers politely and changes
    /// nothing.
    #[tokio::test]
    async fn control_endpoint_parks_the_account_in_memory_and_on_disk() {
        let (manager, path) = control_manager("apply", None);
        let before = manager.snapshot(OffsetDateTime::now_utc());
        assert!(!before.accounts[0].disabled, "alice starts in rotation");

        let (status, headers, body) = control_request(
            Arc::clone(&manager),
            Method::POST,
            Some(loopback_peer()),
            None,
            r#"{"query":"alice@example.com","org":null,"disabled":true}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        assert_eq!(
            headers.get(ENDPOINT_HEADER).and_then(|v| v.to_str().ok()),
            Some(DISABLED_ENDPOINT),
            "the route stamps itself so a caller can tell it from a missing route"
        );

        let payload: SetDisabledResponse =
            serde_json::from_slice(&body).expect("an account-control payload");
        assert_eq!(payload.name, "alice@example.com", "the RESOLVED name");
        assert!(payload.disabled);
        assert!(payload.persisted, "the durable half succeeded");
        assert_eq!(payload.warning, None, "so there is nothing to warn about");

        // 1. THE LIVE ROTATION — the assertion that catches the original bug.
        let after = manager.snapshot(OffsetDateTime::now_utc());
        assert!(
            after.accounts[0].disabled,
            "alice is out of the LIVE rotation, not merely acknowledged"
        );
        assert!(
            !after.accounts[1].disabled,
            "and only the resolved account moved"
        );

        // 2. …AND the file, so the bench survives a restart.
        assert_eq!(disabled_in_file(&path, 0), Some(serde_json::json!(true)));
        assert_eq!(disabled_in_file(&path, 1), None, "bob's row is untouched");

        // Re-enabling drops the key rather than writing `false`, matching the CLI's
        // long-standing file contract.
        let (status, _headers, body) = control_request(
            Arc::clone(&manager),
            Method::POST,
            Some(loopback_peer()),
            None,
            r#"{"query":"alice@example.com","disabled":false}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        assert!(
            !manager.snapshot(OffsetDateTime::now_utc()).accounts[0].disabled,
            "re-enable reaches the live rotation"
        );
        assert_eq!(
            disabled_in_file(&path, 0),
            None,
            "re-enable DROPS the key, never writes false"
        );
        std::fs::remove_file(&path).ok();
    }

    /// SECURITY GUARD 1 — origin, on the MUTATING route. A non-loopback peer and an
    /// absent `ClientAddr` (a request that never came through the listener, so its
    /// origin is unprovable) are both refused, and neither may change rotation.
    #[tokio::test]
    async fn control_endpoint_rejects_a_non_loopback_client() {
        for (label, peer) in [
            ("a routable peer", Some(remote_peer())),
            ("no ClientAddr at all", None),
        ] {
            let (manager, path) = control_manager("origin", None);
            let (status, _headers, _body) = control_request(
                Arc::clone(&manager),
                Method::POST,
                peer,
                None,
                r#"{"query":"alice@example.com","disabled":true}"#,
            )
            .await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "{label} must be refused, got {status}"
            );
            assert!(
                !manager.snapshot(OffsetDateTime::now_utc()).accounts[0].disabled,
                "{label}: a refused caller changed the live rotation"
            );
            assert_eq!(
                disabled_in_file(&path, 0),
                None,
                "{label}: a refused caller wrote the config"
            );
            std::fs::remove_file(&path).ok();
        }
    }

    /// SECURITY GUARD 2 — the key, on the MUTATING route. Every case is a LOOPBACK
    /// peer, so what is asserted is precisely that loopback alone does not license
    /// steering the rotation of a process holding every account's tokens.
    #[tokio::test]
    async fn control_endpoint_rejects_a_bad_api_key() {
        for (label, provided) in [
            ("no key at all", None),
            ("a wrong key", Some("wrong-key")),
            ("a prefix of the key", Some("secret")),
            ("an empty key", Some("")),
        ] {
            let (manager, path) = control_manager("key", Some("secret-key"));
            let (status, _headers, _body) = control_request(
                Arc::clone(&manager),
                Method::POST,
                Some(loopback_peer()),
                provided,
                r#"{"query":"alice@example.com","disabled":true}"#,
            )
            .await;
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "{label} must be refused, got {status}"
            );
            assert!(
                !manager.snapshot(OffsetDateTime::now_utc()).accounts[0].disabled,
                "{label}: an unauthenticated caller changed the live rotation"
            );
            std::fs::remove_file(&path).ok();
        }

        // …and the right key, from loopback, is served.
        let (manager, path) = control_manager("key-ok", Some("secret-key"));
        let (status, _headers, body) = control_request(
            Arc::clone(&manager),
            Method::POST,
            Some(loopback_peer()),
            Some("secret-key"),
            r#"{"query":"alice@example.com","disabled":true}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        assert!(manager.snapshot(OffsetDateTime::now_utc()).accounts[0].disabled);
        std::fs::remove_file(&path).ok();
    }

    /// The bad-request table. Each row is a body a caller can realistically send,
    /// and none of them may half-apply anything.
    #[tokio::test]
    async fn control_endpoint_rejects_malformed_bodies() {
        for (label, body) in [
            ("an empty body", ""),
            ("not json", "disable alice"),
            ("a json array", r#"["alice@example.com", true]"#),
            ("no disabled field", r#"{"query":"alice@example.com"}"#),
            ("no query field", r#"{"disabled":true}"#),
            ("an empty query", r#"{"query":"   ","disabled":true}"#),
            (
                "disabled as a string",
                r#"{"query":"alice@example.com","disabled":"true"}"#,
            ),
        ] {
            let (manager, path) = control_manager("badbody", None);
            let (status, headers, response) = control_request(
                Arc::clone(&manager),
                Method::POST,
                Some(loopback_peer()),
                None,
                body,
            )
            .await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "{label} must be a 400, got {status}: {}",
                String::from_utf8_lossy(&response)
            );
            assert_eq!(
                headers.get(ENDPOINT_HEADER).and_then(|v| v.to_str().ok()),
                Some(DISABLED_ENDPOINT),
                "{label}: a 400 still identifies the route that produced it"
            );
            assert!(
                !manager.snapshot(OffsetDateTime::now_utc()).accounts[0].disabled,
                "{label}: a rejected body still changed the rotation"
            );
            assert_eq!(
                disabled_in_file(&path, 0),
                None,
                "{label}: and wrote the file"
            );
            std::fs::remove_file(&path).ok();
        }
    }

    /// A query naming no live account is a 404 **from the route** (stamped), which
    /// is what lets the CLI tell it from the local 404 an older tcr returns for a
    /// path it does not serve. Same status, opposite reactions.
    #[tokio::test]
    async fn control_endpoint_404s_an_unknown_account_and_names_itself() {
        let (manager, path) = control_manager("nomatch", None);
        let (status, headers, body) = control_request(
            Arc::clone(&manager),
            Method::POST,
            Some(loopback_peer()),
            None,
            r#"{"query":"nobody@example.com","disabled":true}"#,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            headers.get(ENDPOINT_HEADER).and_then(|v| v.to_str().ok()),
            Some(DISABLED_ENDPOINT),
            "without this stamp the CLI cannot tell this from a missing route"
        );
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("nobody@example.com"), "{text}");
        assert!(
            !manager.snapshot(OffsetDateTime::now_utc()).accounts[0].disabled
                && !manager.snapshot(OffsetDateTime::now_utc()).accounts[1].disabled,
            "a miss parks nobody"
        );
        std::fs::remove_file(&path).ok();
    }

    /// An ambiguous query is a 409 that NAMES the candidates — otherwise `--org` is
    /// advice the caller cannot act on. Refusing is not pedantry: guessing would
    /// bench an account the operator did not ask about.
    #[tokio::test]
    async fn control_endpoint_409s_an_ambiguous_query_naming_the_candidates() {
        let path = control_config_path("ambiguous");
        let mut config = two_account_config(None);
        // The same person in two orgs: one email, two rows, `--org` the only fix.
        config.accounts[1].name = "alice@example.com".to_string();
        crate::config::save(&path, &config).expect("write the test config");
        let manager = Manager::with_live_refresher(config, Some(path.clone()));

        let (status, headers, body) = control_request(
            Arc::clone(&manager),
            Method::POST,
            Some(loopback_peer()),
            None,
            r#"{"query":"alice@example.com","disabled":true}"#,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            headers.get(ENDPOINT_HEADER).and_then(|v| v.to_str().ok()),
            Some(DISABLED_ENDPOINT)
        );
        let text = String::from_utf8_lossy(&body);
        assert!(
            text.contains("ambiguous") && text.contains("--org"),
            "the 409 says what to do about it: {text}"
        );
        let live = manager.snapshot(OffsetDateTime::now_utc());
        assert!(
            !live.accounts[0].disabled && !live.accounts[1].disabled,
            "an unbreakable tie parks NEITHER row"
        );
        assert_eq!(disabled_in_file(&path, 0), None);
        assert_eq!(disabled_in_file(&path, 1), None);
        std::fs::remove_file(&path).ok();
    }

    /// `--org` narrows the same ambiguous fleet to exactly one row, and it is the
    /// row named — the resolution rule is the CLI's own
    /// [`crate::identity::match_one`], run against the LIVE rotation slots.
    #[tokio::test]
    async fn control_endpoint_org_narrows_to_one_account() {
        let path = control_config_path("org");
        let mut config = two_account_config(None);
        config.accounts[1].name = "alice@example.com".to_string();
        crate::config::save(&path, &config).expect("write the test config");
        let manager = Manager::with_live_refresher(config, Some(path.clone()));

        let (status, _headers, body) = control_request(
            Arc::clone(&manager),
            Method::POST,
            Some(loopback_peer()),
            None,
            r#"{"query":"alice@example.com","org":"Org bbbbbbbbbbbb","disabled":true}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let live = manager.snapshot(OffsetDateTime::now_utc());
        assert!(
            !live.accounts[0].disabled && live.accounts[1].disabled,
            "the SECOND row — the one whose org was named — is the one parked"
        );
        assert_eq!(disabled_in_file(&path, 0), None);
        assert_eq!(disabled_in_file(&path, 1), Some(serde_json::json!(true)));
        std::fs::remove_file(&path).ok();
    }

    /// A manager with no `config_path` (`tcr demo`, `tcr status --probe`, tests)
    /// applies the change LIVE and has nothing to persist to. That is reported
    /// honestly — `persisted: false` — and, per
    /// [`crate::manager::DisablePersist::warning`], it needs no warning: there is
    /// no file that was supposed to carry it.
    #[tokio::test]
    async fn control_endpoint_reports_an_unpersisted_change_honestly() {
        let manager = Manager::with_live_refresher(two_account_config(None), None);
        let (status, _headers, body) = control_request(
            Arc::clone(&manager),
            Method::POST,
            Some(loopback_peer()),
            None,
            r#"{"query":"alice@example.com","disabled":true}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let payload: SetDisabledResponse = serde_json::from_slice(&body).expect("payload");
        assert!(
            manager.snapshot(OffsetDateTime::now_utc()).accounts[0].disabled,
            "the live change still happened"
        );
        assert!(
            !payload.persisted,
            "…and the answer does not claim it is durable"
        );
        assert_eq!(
            payload.warning, None,
            "memory-only by design, not a failure"
        );
    }

    /// A `DisablePersist` arm that changed memory but NOT the file is surfaced, not
    /// swallowed: the change is in force now and dies on restart, which is the exact
    /// state the old file-only code left operators in silently. Here the config file
    /// does not exist at all, so the write fails.
    #[tokio::test]
    async fn control_endpoint_surfaces_a_failed_persist() {
        let path = control_config_path("nofile");
        std::fs::remove_file(&path).ok();
        let manager = Manager::with_live_refresher(two_account_config(None), Some(path.clone()));

        let (status, _headers, body) = control_request(
            Arc::clone(&manager),
            Method::POST,
            Some(loopback_peer()),
            None,
            r#"{"query":"alice@example.com","disabled":true}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "the LIVE half succeeded");
        let payload: SetDisabledResponse = serde_json::from_slice(&body).expect("payload");
        assert!(
            manager.snapshot(OffsetDateTime::now_utc()).accounts[0].disabled,
            "the account is parked in the live rotation"
        );
        assert!(!payload.persisted, "but the file does not carry it");
        let warning = payload.warning.expect("a not-saved warning, never silence");
        assert!(
            warning.contains("NOT SAVED") && warning.contains("returns to rotation on restart"),
            "the warning is DisablePersist::warning verbatim, direction included: {warning}"
        );
    }

    /// The OTHER half of the discriminator, and the half that is easy to get wrong:
    /// a path this proxy does not serve answers 404 **without** the stamp.
    ///
    /// This is what `tcr` reads as "the running proxy is too old to accept live
    /// account control", which makes it write the file and warn loudly. If the
    /// catch-all ever started stamping, that arm would silently become the
    /// route-refused arm and the CLI would report a bad query instead of a stale
    /// proxy — the original invisible-failure shape, restored.
    #[tokio::test]
    async fn an_unrouted_local_path_is_not_stamped_as_the_control_route() {
        use tower::ServiceExt as _;
        for uri in [
            // NOT `/_tcr/accounts` — that is now [`ADD_ACCOUNT_PATH`], a real
            // registered route (see the account-add tests below).
            "/_tcr/accounts/add",
            "/_tcr/accounts/disable",
            "/_tcr/accounts/disabled/extra",
        ] {
            let (manager, path) = control_manager("unstamped", None);
            let mut req = Request::builder()
                .method(Method::POST)
                .uri(uri)
                .body(Body::from(
                    r#"{"query":"alice@example.com","disabled":true}"#,
                ))
                .expect("build request");
            req.extensions_mut().insert(ClientAddr(loopback_peer()));
            let response = app(Arc::clone(&manager))
                .oneshot(req)
                .await
                .expect("router response");
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
            assert!(
                response.headers().get(ENDPOINT_HEADER).is_none(),
                "{uri} must not claim to be the account-control route"
            );
            assert!(
                !manager.snapshot(OffsetDateTime::now_utc()).accounts[0].disabled,
                "{uri} changed nothing"
            );
            std::fs::remove_file(&path).ok();
        }
    }

    /// The route is `POST`-only, and a wrong method is answered LOCALLY — never
    /// forwarded, and never able to mutate. The stamp distinguishes this 405 (route
    /// exists) from an older tcr's 404 (route absent).
    #[tokio::test]
    async fn control_endpoint_refuses_other_methods_locally() {
        for method in [Method::GET, Method::PUT, Method::DELETE, Method::PATCH] {
            let (manager, path) = control_manager("method", None);
            let (status, headers, _body) = control_request(
                Arc::clone(&manager),
                method.clone(),
                Some(loopback_peer()),
                None,
                r#"{"query":"alice@example.com","disabled":true}"#,
            )
            .await;
            assert_eq!(
                status,
                StatusCode::METHOD_NOT_ALLOWED,
                "{method} on the control path is refused locally"
            );
            assert_ne!(
                status,
                StatusCode::BAD_GATEWAY,
                "{method} must never fall through to the upstream forwarder"
            );
            assert_eq!(
                headers.get(ENDPOINT_HEADER).and_then(|v| v.to_str().ok()),
                Some(DISABLED_ENDPOINT),
                "{method}: the 405 identifies the route, so a caller does not read \
                 it as a proxy too old to have one"
            );
            assert!(
                !manager.snapshot(OffsetDateTime::now_utc()).accounts[0].disabled,
                "{method} changed the rotation"
            );
            std::fs::remove_file(&path).ok();
        }
    }

    // --- POST /_tcr/accounts/control ----------------------------------------

    fn control_account_in_file(path: &std::path::Path) -> Option<serde_json::Value> {
        let raw = std::fs::read_to_string(path).expect("read the test config");
        let doc: serde_json::Value = serde_json::from_str(&raw).expect("valid json");
        doc.get("controlAccount").cloned()
    }

    /// Drive the control-ACCOUNT route with a `ClientAddr` we control, the same
    /// shape as [`control_request`] but against [`CONTROL_PATH`].
    async fn control_account_request(
        manager: Arc<Manager>,
        method: Method,
        peer: Option<SocketAddr>,
        api_key: Option<&str>,
        body: &str,
    ) -> (StatusCode, HeaderMap, Bytes) {
        use tower::ServiceExt as _;
        let mut builder = Request::builder().method(method).uri(CONTROL_PATH);
        if let Some(key) = api_key {
            builder = builder.header("x-api-key", key);
        }
        let mut req = builder
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .expect("build request");
        if let Some(addr) = peer {
            req.extensions_mut().insert(ClientAddr(addr));
        }
        let response = app(manager).oneshot(req).await.expect("router response");
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), MAX_BODY_BYTES)
            .await
            .expect("read body");
        (status, headers, bytes)
    }

    /// THE BITING TEST for the control-account route: after the endpoint
    /// answers, the LIVE manager resolves it (`control_name()`), not merely the
    /// response body, and the file's top-level `controlAccount` carries it too.
    #[tokio::test]
    async fn control_account_endpoint_sets_it_in_memory_and_on_disk() {
        let (manager, path) = control_manager("control-apply", None);
        assert_eq!(manager.control(), None, "starts with no control account");

        let (status, headers, body) = control_account_request(
            Arc::clone(&manager),
            Method::POST,
            Some(loopback_peer()),
            None,
            r#"{"query":"alice@example.com","org":null}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        assert_eq!(
            headers.get(ENDPOINT_HEADER).and_then(|v| v.to_str().ok()),
            Some(CONTROL_ENDPOINT),
            "the route stamps itself so a caller can tell it from a missing route"
        );

        let payload: SetControlResponse =
            serde_json::from_slice(&body).expect("a control-account payload");
        assert_eq!(payload.name, Some("alice@example.com".to_string()));
        assert!(!payload.cleared);
        assert!(payload.persisted, "the durable half succeeded");
        assert_eq!(payload.warning, None);

        // 1. THE LIVE MANAGER resolves the control account — not merely an
        // acknowledgement in the response body.
        assert_eq!(
            manager.control_name(),
            Some("alice@example.com".to_string()),
            "the live manager must resolve the new control account"
        );

        // 2. …and the file's top-level key carries it too.
        assert_eq!(
            control_account_in_file(&path),
            Some(serde_json::json!("alice@example.com"))
        );

        // Clearing (`query: null`) removes it from both halves.
        let (status, _headers, body) = control_account_request(
            Arc::clone(&manager),
            Method::POST,
            Some(loopback_peer()),
            None,
            r#"{"query":null}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let payload: SetControlResponse = serde_json::from_slice(&body).expect("payload");
        assert_eq!(payload.name, None);
        assert!(payload.cleared);
        assert!(payload.persisted);
        assert_eq!(manager.control(), None, "cleared in memory");
        assert_eq!(control_account_in_file(&path), None, "cleared on disk");

        std::fs::remove_file(&path).ok();
    }

    /// SECURITY GUARD 1 — origin, on the control-account route: same posture as
    /// [`control_endpoint_rejects_a_non_loopback_client`].
    #[tokio::test]
    async fn control_account_endpoint_rejects_a_non_loopback_client() {
        for (label, peer) in [
            ("a routable peer", Some(remote_peer())),
            ("no ClientAddr at all", None),
        ] {
            let (manager, path) = control_manager("control-origin", None);
            let (status, _headers, _body) = control_account_request(
                Arc::clone(&manager),
                Method::POST,
                peer,
                None,
                r#"{"query":"alice@example.com"}"#,
            )
            .await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "{label} must be refused, got {status}"
            );
            assert_eq!(
                manager.control(),
                None,
                "{label}: a refused caller changed the control account"
            );
            std::fs::remove_file(&path).ok();
        }
    }

    /// SECURITY GUARD 2 — the key, on the control-account route: same posture as
    /// [`control_endpoint_rejects_a_bad_api_key`].
    #[tokio::test]
    async fn control_account_endpoint_rejects_a_bad_api_key() {
        for (label, provided) in [("no key at all", None), ("a wrong key", Some("wrong-key"))] {
            let (manager, path) = control_manager("control-key", Some("secret-key"));
            let (status, _headers, _body) = control_account_request(
                Arc::clone(&manager),
                Method::POST,
                Some(loopback_peer()),
                provided,
                r#"{"query":"alice@example.com"}"#,
            )
            .await;
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "{label} must be refused, got {status}"
            );
            assert_eq!(manager.control(), None);
            std::fs::remove_file(&path).ok();
        }

        let (manager, path) = control_manager("control-key-ok", Some("secret-key"));
        let (status, _headers, body) = control_account_request(
            Arc::clone(&manager),
            Method::POST,
            Some(loopback_peer()),
            Some("secret-key"),
            r#"{"query":"alice@example.com"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        assert_eq!(manager.control(), Some(0));
        std::fs::remove_file(&path).ok();
    }

    /// A missing/wrong `content-type` is refused (415), stamped, and changes
    /// nothing — same posture [`is_json_content_type`] enforces on the other two
    /// mutating routes.
    #[tokio::test]
    async fn control_account_endpoint_requires_json_content_type() {
        use tower::ServiceExt as _;
        let (manager, path) = control_manager("control-ctype", None);
        let req = Request::builder()
            .method(Method::POST)
            .uri(CONTROL_PATH)
            .body(Body::from(r#"{"query":"alice@example.com"}"#.to_string()))
            .expect("build request");
        let mut req = req;
        req.extensions_mut().insert(ClientAddr(loopback_peer()));
        let response = app(Arc::clone(&manager))
            .oneshot(req)
            .await
            .expect("router response");
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(
            response
                .headers()
                .get(ENDPOINT_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some(CONTROL_ENDPOINT),
            "a 415 still identifies the route that produced it"
        );
        assert_eq!(manager.control(), None);
        std::fs::remove_file(&path).ok();
    }

    /// The route is `POST`-only; a wrong method is answered LOCALLY (405),
    /// never forwarded, and never able to mutate — same posture as
    /// [`control_endpoint_refuses_other_methods_locally`].
    #[tokio::test]
    async fn control_account_endpoint_refuses_other_methods_locally() {
        for method in [Method::GET, Method::PUT, Method::DELETE, Method::PATCH] {
            let (manager, path) = control_manager("control-method", None);
            let (status, headers, _body) = control_account_request(
                Arc::clone(&manager),
                method.clone(),
                Some(loopback_peer()),
                None,
                r#"{"query":"alice@example.com"}"#,
            )
            .await;
            assert_eq!(
                status,
                StatusCode::METHOD_NOT_ALLOWED,
                "{method} on the control-account path is refused locally"
            );
            assert_ne!(status, StatusCode::BAD_GATEWAY);
            assert_eq!(
                headers.get(ENDPOINT_HEADER).and_then(|v| v.to_str().ok()),
                Some(CONTROL_ENDPOINT)
            );
            assert_eq!(
                manager.control(),
                None,
                "{method} changed the control account"
            );
            std::fs::remove_file(&path).ok();
        }
    }

    /// A query naming no live account is a 404 from the route (stamped), and an
    /// ambiguous query is a 409 naming the candidates — same posture as the
    /// disabled-flag route's equivalents.
    #[tokio::test]
    async fn control_account_endpoint_404s_and_409s() {
        let (manager, path) = control_manager("control-nomatch", None);
        let (status, headers, body) = control_account_request(
            Arc::clone(&manager),
            Method::POST,
            Some(loopback_peer()),
            None,
            r#"{"query":"nobody@example.com"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            headers.get(ENDPOINT_HEADER).and_then(|v| v.to_str().ok()),
            Some(CONTROL_ENDPOINT)
        );
        assert!(String::from_utf8_lossy(&body).contains("nobody@example.com"));
        assert_eq!(manager.control(), None);
        std::fs::remove_file(&path).ok();

        let path = control_config_path("control-ambiguous");
        let mut config = two_account_config(None);
        config.accounts[1].name = "alice@example.com".to_string();
        crate::config::save(&path, &config).expect("write the test config");
        let manager = Manager::with_live_refresher(config, Some(path.clone()));
        let (status, headers, body) = control_account_request(
            Arc::clone(&manager),
            Method::POST,
            Some(loopback_peer()),
            None,
            r#"{"query":"alice@example.com"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            headers.get(ENDPOINT_HEADER).and_then(|v| v.to_str().ok()),
            Some(CONTROL_ENDPOINT)
        );
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("ambiguous") && text.contains("--org"));
        assert_eq!(manager.control(), None, "an unbreakable tie sets nothing");
        std::fs::remove_file(&path).ok();
    }

    /// Drive the control route with FULL control over the request line and every
    /// header, so a BROWSER-shaped or forward-proxy-shaped request is testable as
    /// it would actually arrive. [`control_request`] always sends
    /// `content-type: application/json` on an origin-form target — which is
    /// precisely the one shape that was never the attack.
    ///
    /// The peer is always loopback and no proxy api-key is configured, because that
    /// is the population this class applies to: [`crate::config`] defaults
    /// `api_key: None` and nothing generates one, so on a fresh install loopback is
    /// the whole gate. With a key configured the route is closed by the key check
    /// (see [`control_endpoint_rejects_a_bad_api_key`]) — requiring a header also
    /// makes the request non-simple, so a browser never sends it at all.
    async fn control_request_shaped(
        manager: Arc<Manager>,
        uri: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> (StatusCode, HeaderMap, Bytes) {
        use tower::ServiceExt as _;
        let mut builder = Request::builder().method(Method::POST).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let mut req = builder
            .body(Body::from(body.to_string()))
            .expect("build request");
        req.extensions_mut().insert(ClientAddr(loopback_peer()));
        let response = app(manager).oneshot(req).await.expect("router response");
        let status = response.status();
        let response_headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), MAX_BODY_BYTES)
            .await
            .expect("read body");
        (status, response_headers, bytes)
    }

    /// One row of a request-SHAPE table: what it is, the request target, the
    /// headers, and the status it must come back with.
    type ShapeCase<'a> = (&'a str, &'a str, Vec<(&'a str, &'a str)>, StatusCode);
    /// A row of a table every case of which must be SERVED, so there is no status
    /// column to get wrong.
    type ServedCase<'a> = (&'a str, &'a str, Vec<(&'a str, &'a str)>);
    /// A row that varies only in its headers — the target is fixed.
    type HeaderCase<'a> = (&'a str, Vec<(&'a str, &'a str)>, StatusCode);

    /// THE CSRF TABLE — the biting test for this change. Every row is a request a
    /// WEB PAGE open in a browser on this host can actually cause, and each of the
    /// first four **returned 200 and parked a live account** before these checks
    /// existed (measured on the route as merged in #71).
    ///
    /// Why a browser can send them at all: `text/plain`,
    /// `application/x-www-form-urlencoded` and `multipart/form-data` are the three
    /// CORS **simple** content types, so a cross-origin `fetch`/form POST carrying
    /// one is sent with NO preflight. The page cannot read the reply — this proxy
    /// emits no `Access-Control-*` header — which is irrelevant: the entire payoff
    /// of this route IS the side effect, so write-only is the whole attack. The
    /// read route next door was immune by being a read; this class arrives with the
    /// FIRST mutating route, and the loopback gate's own doc-comment already argues
    /// that bind scope is not authorization. The browser is the caller it failed to
    /// enumerate.
    ///
    /// The rebound `Host` row is the DNS-rebinding shape and the reason a
    /// content-type check alone is not enough: a page served from a name that
    /// resolves to 127.0.0.1 is SAME-ORIGIN with the proxy, so it sends no `Origin`,
    /// triggers no preflight, and may use any content type it likes. The only thing
    /// that distinguishes it from a real local caller is the name it addressed us
    /// by.
    ///
    /// The assertion that bites is the in-memory one: a handler that answers 415
    /// politely and parks the account anyway passes a status-only check.
    #[tokio::test]
    async fn control_endpoint_refuses_every_browser_reachable_shape() {
        let json = r#"{"query":"alice@example.com","disabled":true}"#;
        let absolute_form = format!("http://api.anthropic.com{DISABLED_PATH}");
        let cases: Vec<ShapeCase> = vec![
            (
                "a text/plain body — a CORS simple request, so no preflight",
                DISABLED_PATH,
                vec![("content-type", "text/plain;charset=UTF-8")],
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ),
            (
                "an x-www-form-urlencoded body — simple too, and what a bare \
                 <form> posts",
                DISABLED_PATH,
                vec![("content-type", "application/x-www-form-urlencoded")],
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ),
            (
                "a multipart/form-data body — the third simple type",
                DISABLED_PATH,
                vec![("content-type", "multipart/form-data; boundary=x")],
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ),
            (
                "no content-type at all — absent is simple as well",
                DISABLED_PATH,
                vec![],
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ),
            (
                "a rebound Host — a name that resolves to 127.0.0.1, so the page \
                 is same-origin and sends nothing that marks it cross-site",
                DISABLED_PATH,
                vec![
                    ("content-type", "application/json"),
                    ("host", "rebound.example.com"),
                ],
                StatusCode::MISDIRECTED_REQUEST,
            ),
            (
                "an absolute-form request line naming anthropic — a forward-proxy \
                 request, never something addressed to this route",
                &absolute_form,
                vec![("content-type", "application/json")],
                StatusCode::MISDIRECTED_REQUEST,
            ),
            (
                "a cross-origin fetch that DID preflight (a future route may not \
                 be JSON, so the Origin is refused on its own merits)",
                DISABLED_PATH,
                vec![
                    ("content-type", "application/json"),
                    ("origin", "https://evil.example.com"),
                ],
                StatusCode::FORBIDDEN,
            ),
            (
                "Sec-Fetch-Site: cross-site — the browser saying so itself",
                DISABLED_PATH,
                vec![
                    ("content-type", "application/json"),
                    ("sec-fetch-site", "cross-site"),
                ],
                StatusCode::FORBIDDEN,
            ),
            (
                "Sec-Fetch-Site: same-site — a sibling subdomain is not us",
                DISABLED_PATH,
                vec![
                    ("content-type", "application/json"),
                    ("sec-fetch-site", "same-site"),
                ],
                StatusCode::FORBIDDEN,
            ),
        ];

        for (label, uri, headers, expected) in cases {
            let (manager, path) = control_manager("csrf", None);
            let (status, _headers, body) =
                control_request_shaped(Arc::clone(&manager), uri, &headers, json).await;
            assert_eq!(
                status,
                expected,
                "{label}: expected {expected}, got {status}: {}",
                String::from_utf8_lossy(&body)
            );
            // THE assertion. A refusal that still mutates is the same defect one
            // altitude up, and a status-only table would not see it.
            assert!(
                !manager.snapshot(OffsetDateTime::now_utc()).accounts[0].disabled,
                "{label}: a refused request changed the LIVE rotation"
            );
            assert_eq!(
                disabled_in_file(&path, 0),
                None,
                "{label}: a refused request wrote the config"
            );
            std::fs::remove_file(&path).ok();
        }
    }

    /// The other direction, so the checks above cannot be satisfied by refusing
    /// everything: every shape a LEGITIMATE local caller emits is still served and
    /// still mutates. `tcr disable` builds its request with reqwest's `.json()`
    /// against `http://127.0.0.1:<port>` (`cli.rs`), which is row 1.
    #[tokio::test]
    async fn control_endpoint_still_serves_every_legitimate_local_shape() {
        let json = r#"{"query":"alice@example.com","disabled":true}"#;
        let loopback_form = format!("http://127.0.0.1:3456{DISABLED_PATH}");
        let cases: Vec<ServedCase> = vec![
            (
                "what `tcr disable` sends: reqwest .json() to a loopback literal",
                DISABLED_PATH,
                vec![
                    ("content-type", "application/json"),
                    ("host", "127.0.0.1:3456"),
                ],
            ),
            (
                "a charset parameter on the media type — RFC 9110 legitimate",
                DISABLED_PATH,
                vec![("content-type", "application/json; charset=utf-8")],
            ),
            (
                "an upper-case media type — the type is case-insensitive",
                DISABLED_PATH,
                vec![("content-type", "Application/JSON")],
            ),
            (
                "Host: localhost — loopback by name, not by literal",
                DISABLED_PATH,
                vec![
                    ("content-type", "application/json"),
                    ("host", "localhost:3456"),
                ],
            ),
            (
                "Host: [::1] — the v6 loopback literal, port stripped like the \
                 forwarding path's own host guard does",
                DISABLED_PATH,
                vec![("content-type", "application/json"), ("host", "[::1]:3456")],
            ),
            (
                "no Host header at all — an origin-form base-URL request, which is \
                 also every direct-axum caller",
                DISABLED_PATH,
                vec![("content-type", "application/json")],
            ),
            (
                "an absolute-form loopback target — the authority is still ours",
                &loopback_form,
                vec![("content-type", "application/json")],
            ),
            (
                "Sec-Fetch-Site: same-origin",
                DISABLED_PATH,
                vec![
                    ("content-type", "application/json"),
                    ("sec-fetch-site", "same-origin"),
                ],
            ),
            (
                "Sec-Fetch-Site: none — a user-initiated request, not a site's",
                DISABLED_PATH,
                vec![
                    ("content-type", "application/json"),
                    ("sec-fetch-site", "none"),
                ],
            ),
        ];

        for (label, uri, headers) in cases {
            let (manager, path) = control_manager("legit", None);
            let (status, headers, body) =
                control_request_shaped(Arc::clone(&manager), uri, &headers, json).await;
            assert_eq!(
                status,
                StatusCode::OK,
                "{label}: {}",
                String::from_utf8_lossy(&body)
            );
            assert_eq!(
                headers.get(ENDPOINT_HEADER).and_then(|v| v.to_str().ok()),
                Some(DISABLED_ENDPOINT),
                "{label}: the route still stamps itself"
            );
            assert!(
                manager.snapshot(OffsetDateTime::now_utc()).accounts[0].disabled,
                "{label}: a served request must still reach the LIVE rotation"
            );
            assert_eq!(
                disabled_in_file(&path, 0),
                Some(serde_json::json!(true)),
                "{label}: and the durable half"
            );
            std::fs::remove_file(&path).ok();
        }
    }

    /// The READ route is deliberately NOT content-type gated (a GET has no body to
    /// type), and this pins that: the shapes the mutating route now refuses with a
    /// 415 are served here, unchanged. Changing it would buy nothing — the route is
    /// side-effect-free and a page cannot read its reply — and would break `tcr
    /// status`, which sends no content-type on a GET.
    ///
    /// The cross-site and host checks DO apply, because they live in the shared
    /// [`local_endpoint_gate`]: one implementation for both routes, per that
    /// function's own reasoning about which copy of a duplicated gate drifts.
    #[tokio::test]
    async fn status_endpoint_is_not_content_type_gated_but_is_cross_site_gated() {
        use tower::ServiceExt as _;
        let served = [
            ("no content-type — what `tcr status` actually sends", vec![]),
            (
                "a text/plain content-type on a GET",
                vec![("content-type", "text/plain")],
            ),
        ];
        let refused: [HeaderCase; 3] = [
            (
                "a cross-origin Origin",
                vec![("origin", "https://evil.example.com")],
                StatusCode::FORBIDDEN,
            ),
            (
                "Sec-Fetch-Site: cross-site",
                vec![("sec-fetch-site", "cross-site")],
                StatusCode::FORBIDDEN,
            ),
            (
                "a rebound Host",
                vec![("host", "rebound.example.com")],
                StatusCode::MISDIRECTED_REQUEST,
            ),
        ];

        let probe_status = |headers: Vec<(&'static str, &'static str)>| async move {
            let manager = Manager::with_live_refresher(two_account_config(None), None);
            let mut builder = Request::builder().method(Method::GET).uri(STATUS_PATH);
            for (name, value) in &headers {
                builder = builder.header(*name, *value);
            }
            let mut req = builder.body(Body::empty()).expect("build request");
            req.extensions_mut().insert(ClientAddr(loopback_peer()));
            app(manager)
                .oneshot(req)
                .await
                .expect("router response")
                .status()
        };

        for (label, headers) in served {
            assert_eq!(probe_status(headers).await, StatusCode::OK, "{label}");
        }
        for (label, headers, expected) in refused {
            assert_eq!(probe_status(headers).await, expected, "{label}");
        }
    }

    /// The three new pure helpers, in isolation — the negatives are the point.
    #[test]
    fn local_gate_helpers_classify_their_edges() {
        // A loopback name is any 127/8 address, the v6 loopback bracketed or bare,
        // and `localhost` in any case. Nothing else, including the addresses a
        // rebound page would be served from.
        for host in [
            "127.0.0.1",
            "127.0.0.53",
            "::1",
            "[::1]",
            "localhost",
            "LocalHost",
        ] {
            assert!(host_is_loopback(host), "{host} is loopback");
        }
        for host in [
            "rebound.example.com",
            "api.anthropic.com",
            "169.254.169.254",
            "0.0.0.0",
            "192.168.1.10",
            "localhost.evil.example.com",
            "",
        ] {
            assert!(!host_is_loopback(host), "{host} is NOT loopback");
        }

        let with = |name: &str, value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).expect("header name"),
                HeaderValue::from_str(value).expect("header value"),
            );
            headers
        };

        // Only `application/json`, parameters and case allowed; ABSENT is a no.
        for value in [
            "application/json",
            "Application/JSON",
            "application/json; charset=utf-8",
            "application/json;charset=UTF-8",
        ] {
            assert!(
                is_json_content_type(&with("content-type", value)),
                "{value} is json"
            );
        }
        for value in [
            "text/plain",
            "text/plain;charset=UTF-8",
            "application/x-www-form-urlencoded",
            "multipart/form-data; boundary=x",
            "application/json-patch+json",
            "application/jsonx",
            "",
        ] {
            assert!(
                !is_json_content_type(&with("content-type", value)),
                "{value} is NOT json"
            );
        }
        assert!(
            !is_json_content_type(&HeaderMap::new()),
            "no content-type at all is a CORS simple request too"
        );

        // Cross-site: any Origin, or a Sec-Fetch-Site that is not ours.
        assert!(is_cross_site_request(&with(
            "origin",
            "https://evil.example.com"
        )));
        assert!(is_cross_site_request(&with("origin", "null")));
        assert!(is_cross_site_request(&with("sec-fetch-site", "cross-site")));
        assert!(is_cross_site_request(&with("sec-fetch-site", "same-site")));
        assert!(!is_cross_site_request(&with(
            "sec-fetch-site",
            "same-origin"
        )));
        assert!(!is_cross_site_request(&with("sec-fetch-site", "none")));
        assert!(
            !is_cross_site_request(&HeaderMap::new()),
            "a non-browser caller sends neither header, and must not be refused"
        );

        // The authority in an absolute-form target WINS over the Host header, and
        // an origin-form request with no Host names no host at all.
        let absolute: axum::http::Uri =
            "http://api.anthropic.com/_tcr/status".parse().expect("uri");
        assert_eq!(
            target_host(&absolute, &with("host", "127.0.0.1:3456")),
            Some("api.anthropic.com"),
            "the request line is what the client asked for"
        );
        let origin_form: axum::http::Uri = "/_tcr/status".parse().expect("uri");
        assert_eq!(
            target_host(&origin_form, &with("host", "localhost:3456")),
            Some("localhost"),
            "the port is stripped"
        );
        assert_eq!(
            target_host(&origin_form, &with("host", "[::1]:3456")),
            Some("[::1]"),
            "…and a bracketed v6 literal survives it, brackets included"
        );
        assert_eq!(
            target_host(&origin_form, &with("host", "[::1]")),
            Some("[::1]"),
            "a v6 literal with NO port keeps every colon: splitting on the last \
             one yields '[:' and misroutes an IPv6 loopback client"
        );
        assert_eq!(target_host(&origin_form, &HeaderMap::new()), None);
    }

    // --- POST /_tcr/accounts (add) -------------------------------------------

    /// Drive the router at [`ADD_ACCOUNT_PATH`], mirroring [`control_request`].
    async fn add_request(
        manager: Arc<Manager>,
        peer: Option<SocketAddr>,
        api_key: Option<&str>,
        body: &str,
    ) -> (StatusCode, HeaderMap, Bytes) {
        use tower::ServiceExt as _;
        let mut builder = Request::builder()
            .method(Method::POST)
            .uri(ADD_ACCOUNT_PATH);
        if let Some(key) = api_key {
            builder = builder.header("x-api-key", key);
        }
        let mut req = builder
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .expect("build request");
        if let Some(addr) = peer {
            req.extensions_mut().insert(ClientAddr(addr));
        }
        let response = app(manager).oneshot(req).await.expect("router response");
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), MAX_BODY_BYTES)
            .await
            .expect("read body");
        (status, headers, bytes)
    }

    fn account_in_file(path: &std::path::Path, index: usize) -> Option<serde_json::Value> {
        let raw = std::fs::read_to_string(path).expect("read the test config");
        let doc: serde_json::Value = serde_json::from_str(&raw).expect("valid json");
        doc["accounts"].get(index).cloned()
    }

    fn account_count_in_file(path: &std::path::Path) -> usize {
        let raw = std::fs::read_to_string(path).expect("read the test config");
        let doc: serde_json::Value = serde_json::from_str(&raw).expect("valid json");
        doc["accounts"].as_array().map_or(0, |a| a.len())
    }

    /// THE BITING TEST for the append half: a brand-new identity lands at the END
    /// of the live rotation and is **immediately servable** — not merely
    /// acknowledged. Every OTHER account is put in `tried`, so `select` can only
    /// return the new one by actually finding it eligible.
    #[tokio::test]
    async fn add_endpoint_appends_a_new_account_and_it_is_immediately_servable() {
        let (manager, path) = control_manager("append", None);
        let before = manager.snapshot(OffsetDateTime::now_utc());
        assert_eq!(before.accounts.len(), 2, "starts with alice and bob");

        let (status, headers, body) = add_request(
            Arc::clone(&manager),
            Some(loopback_peer()),
            None,
            r#"{"name":"carol@example.com","accessToken":"at-carol","refreshToken":"rt-carol","expiresAt":9999999999999}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        assert_eq!(
            headers.get(ENDPOINT_HEADER).and_then(|v| v.to_str().ok()),
            Some(ADD_ACCOUNT_ENDPOINT),
            "the route stamps itself so a caller can tell it from a missing route"
        );

        let payload: AddAccountResponse =
            serde_json::from_slice(&body).expect("an account-add payload");
        assert_eq!(payload.name, "carol@example.com");
        assert!(
            payload.added,
            "a brand new identity is an APPEND, not an update"
        );
        assert_eq!(payload.index, 2, "appended at the END");
        assert!(payload.persisted, "the durable half succeeded");
        assert_eq!(payload.warning, None, "a refresh token was supplied");

        // 1. THE LIVE ROTATION — present AND selectable, not merely acknowledged.
        let after = manager.snapshot(OffsetDateTime::now_utc());
        assert_eq!(after.accounts.len(), 3);
        assert_eq!(after.accounts[2].name, "carol@example.com");
        let mut tried: HashSet<usize> = HashSet::new();
        tried.insert(0);
        tried.insert(1);
        assert_eq!(
            manager.select(
                &tried,
                OffsetDateTime::now_utc(),
                None,
                None,
                "/v1/messages",
                None
            ),
            Some(2),
            "the new account is eligible and selectable immediately, with no restart"
        );

        // 2. …and the file, so it survives one.
        assert_eq!(account_count_in_file(&path), 3);
        let on_disk = account_in_file(&path, 2).expect("the appended entry");
        assert_eq!(on_disk["name"], "carol@example.com");
        assert_eq!(on_disk["accessToken"], "at-carol");
        std::fs::remove_file(&path).ok();
    }

    /// THE BITING TEST for the update half: re-adding an EXISTING identity
    /// replaces its credentials in place — same index, same account count — and
    /// its routing state (priority) survives untouched.
    #[tokio::test]
    async fn add_endpoint_updates_an_existing_account_in_place_and_keeps_the_index() {
        let (manager, path) = control_manager("update", None);
        let before = manager.snapshot(OffsetDateTime::now_utc());
        assert_eq!(before.accounts.len(), 2);
        assert_eq!(before.accounts[0].name, "alice@example.com");
        assert_eq!(before.accounts[0].priority, 0);

        let (status, _headers, body) = add_request(
            Arc::clone(&manager),
            Some(loopback_peer()),
            None,
            r#"{"name":"alice@example.com","accessToken":"at-alice-fresh","refreshToken":"rt-alice-fresh","expiresAt":9999999999999}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));

        let payload: AddAccountResponse =
            serde_json::from_slice(&body).expect("an account-add payload");
        assert_eq!(payload.name, "alice@example.com");
        assert!(
            !payload.added,
            "an existing identity is an UPDATE, not an append"
        );
        assert_eq!(
            payload.index, 0,
            "the SAME index — never moved, never duplicated"
        );
        assert!(payload.persisted, "the durable half succeeded");

        // 1. THE LIVE ROTATION: same count, same index, NEW credentials, and
        // routing state (priority) untouched.
        let after = manager.snapshot(OffsetDateTime::now_utc());
        assert_eq!(after.accounts.len(), 2, "no duplicate appended");
        assert_eq!(after.accounts[0].name, "alice@example.com");
        assert_eq!(after.accounts[0].priority, 0, "routing state preserved");
        assert_eq!(
            manager.access_token(0).as_deref(),
            Some("at-alice-fresh"),
            "the credential actually changed"
        );

        // 2. …and the file: same entry, same position, new token.
        assert_eq!(account_count_in_file(&path), 2, "no duplicate row on disk");
        let on_disk = account_in_file(&path, 0).expect("alice's entry");
        assert_eq!(on_disk["accessToken"], "at-alice-fresh");
        std::fs::remove_file(&path).ok();
    }

    /// SECURITY GUARD 1 — origin, on the account-ADD route. [`local_endpoint_gate`]
    /// is the ONE implementation the disable and add routes both use, so this is
    /// largely a re-proof — but on a write that carries a fresh OAuth token in its
    /// body, a drift here would be worse.
    #[tokio::test]
    async fn add_endpoint_rejects_a_non_loopback_client() {
        for (label, peer) in [
            ("a routable peer", Some(remote_peer())),
            ("no ClientAddr at all", None),
        ] {
            let (manager, path) = control_manager("add-origin", None);
            let (status, _headers, _body) = add_request(
                Arc::clone(&manager),
                peer,
                None,
                r#"{"name":"carol@example.com","accessToken":"at-carol"}"#,
            )
            .await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "{label} must be refused, got {status}"
            );
            assert_eq!(
                manager.snapshot(OffsetDateTime::now_utc()).accounts.len(),
                2,
                "{label}: a refused caller appended an account"
            );
            std::fs::remove_file(&path).ok();
        }
    }

    /// SECURITY GUARD 2 — the key, on the account-ADD route.
    #[tokio::test]
    async fn add_endpoint_rejects_a_bad_api_key() {
        for (label, provided) in [("no key at all", None), ("a wrong key", Some("wrong-key"))] {
            let (manager, path) = control_manager("add-key", Some("secret-key"));
            let (status, _headers, _body) = add_request(
                Arc::clone(&manager),
                Some(loopback_peer()),
                provided,
                r#"{"name":"carol@example.com","accessToken":"at-carol"}"#,
            )
            .await;
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "{label} must be refused, got {status}"
            );
            assert_eq!(
                manager.snapshot(OffsetDateTime::now_utc()).accounts.len(),
                2,
                "{label}: an unauthenticated caller appended an account"
            );
            std::fs::remove_file(&path).ok();
        }

        // …and the right key, from loopback, is served.
        let (manager, path) = control_manager("add-key-ok", Some("secret-key"));
        let (status, _headers, body) = add_request(
            Arc::clone(&manager),
            Some(loopback_peer()),
            Some("secret-key"),
            r#"{"name":"carol@example.com","accessToken":"at-carol"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        assert_eq!(
            manager.snapshot(OffsetDateTime::now_utc()).accounts.len(),
            3
        );
        std::fs::remove_file(&path).ok();
    }

    /// A non-JSON content-type is refused — this route carries an OAuth token in
    /// its body and is a write, so the browser/CSRF defence must be no weaker
    /// than the disable route's.
    #[tokio::test]
    async fn add_endpoint_requires_json_content_type() {
        use tower::ServiceExt as _;
        let (manager, path) = control_manager("add-ctype", None);
        let mut req = Request::builder()
            .method(Method::POST)
            .uri(ADD_ACCOUNT_PATH)
            .header(CONTENT_TYPE, "text/plain")
            .body(Body::from(
                r#"{"name":"carol@example.com","accessToken":"at-carol"}"#,
            ))
            .expect("build request");
        req.extensions_mut().insert(ClientAddr(loopback_peer()));
        let response = app(Arc::clone(&manager))
            .oneshot(req)
            .await
            .expect("router response");
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(
            response
                .headers()
                .get(ENDPOINT_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some(ADD_ACCOUNT_ENDPOINT)
        );
        assert_eq!(
            manager.snapshot(OffsetDateTime::now_utc()).accounts.len(),
            2
        );
        std::fs::remove_file(&path).ok();
    }

    /// The bad-request table. None of these may half-apply anything.
    #[tokio::test]
    async fn add_endpoint_rejects_malformed_bodies() {
        for (label, body) in [
            ("an empty body", ""),
            ("not json", "add carol"),
            ("a json array", r#"["carol@example.com"]"#),
            ("no accessToken field", r#"{"name":"carol@example.com"}"#),
            ("no name field", r#"{"accessToken":"at-carol"}"#),
            (
                "an empty name",
                r#"{"name":"   ","accessToken":"at-carol"}"#,
            ),
            (
                "an empty accessToken",
                r#"{"name":"carol@example.com","accessToken":"  "}"#,
            ),
        ] {
            let (manager, path) = control_manager("add-badbody", None);
            let (status, headers, response) =
                add_request(Arc::clone(&manager), Some(loopback_peer()), None, body).await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "{label} must be a 400, got {status}: {}",
                String::from_utf8_lossy(&response)
            );
            assert_eq!(
                headers.get(ENDPOINT_HEADER).and_then(|v| v.to_str().ok()),
                Some(ADD_ACCOUNT_ENDPOINT),
                "{label}: a 400 still identifies the route that produced it"
            );
            assert_eq!(
                manager.snapshot(OffsetDateTime::now_utc()).accounts.len(),
                2,
                "{label}: a rejected body still appended an account"
            );
            std::fs::remove_file(&path).ok();
        }
    }

    /// An ambiguous identity — the same email in two orgs — is a 409 naming the
    /// candidates, never guessed. `--org` is the only fix.
    #[tokio::test]
    async fn add_endpoint_409s_an_ambiguous_identity_naming_the_candidates() {
        let path = control_config_path("add-ambiguous");
        let mut config = two_account_config(None);
        // The same person in two orgs: one email, two rows, `--org` the only fix.
        config.accounts[1].name = "alice@example.com".to_string();
        crate::config::save(&path, &config).expect("write the test config");
        let manager = Manager::with_live_refresher(config, Some(path.clone()));

        let (status, headers, body) = add_request(
            Arc::clone(&manager),
            Some(loopback_peer()),
            None,
            r#"{"name":"alice@example.com","accessToken":"at-new"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            headers.get(ENDPOINT_HEADER).and_then(|v| v.to_str().ok()),
            Some(ADD_ACCOUNT_ENDPOINT)
        );
        let text = String::from_utf8_lossy(&body);
        assert!(
            text.contains("ambiguous") && text.contains("--org"),
            "the 409 says what to do about it: {text}"
        );
        assert_eq!(
            manager.snapshot(OffsetDateTime::now_utc()).accounts.len(),
            2,
            "an unbreakable tie changes nothing"
        );
        std::fs::remove_file(&path).ok();
    }

    /// A manager with no `config_path` (`tcr demo`, `tcr status --probe`, tests)
    /// applies the change LIVE and has nothing to persist to — reported honestly
    /// (`persisted: false`, no warning), exactly as the disable route's
    /// equivalent case.
    #[tokio::test]
    async fn add_endpoint_reports_an_unpersisted_change_honestly() {
        let manager = Manager::with_live_refresher(two_account_config(None), None);
        let (status, _headers, body) = add_request(
            Arc::clone(&manager),
            Some(loopback_peer()),
            None,
            r#"{"name":"carol@example.com","accessToken":"at-carol","refreshToken":"rt-carol"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let payload: AddAccountResponse = serde_json::from_slice(&body).expect("payload");
        assert!(payload.added);
        assert_eq!(
            manager.snapshot(OffsetDateTime::now_utc()).accounts.len(),
            3,
            "the live append still happened"
        );
        assert!(
            !payload.persisted,
            "…and the answer does not claim it is durable"
        );
        assert_eq!(
            payload.warning, None,
            "memory-only by design, not a failure"
        );
    }

    /// A failed durable write still returns 200 — the live change stands — with
    /// `persisted: false` and a warning, never swallowed. Here the config file
    /// does not exist at all, so the write fails.
    #[tokio::test]
    async fn add_endpoint_surfaces_a_failed_persist() {
        let path = control_config_path("add-nofile");
        std::fs::remove_file(&path).ok();
        let manager = Manager::with_live_refresher(two_account_config(None), Some(path.clone()));

        let (status, _headers, body) = add_request(
            Arc::clone(&manager),
            Some(loopback_peer()),
            None,
            r#"{"name":"carol@example.com","accessToken":"at-carol","refreshToken":"rt-carol"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "the LIVE half succeeded");
        let payload: AddAccountResponse = serde_json::from_slice(&body).expect("payload");
        assert_eq!(
            manager.snapshot(OffsetDateTime::now_utc()).accounts.len(),
            3,
            "the account is appended in the live rotation"
        );
        assert!(!payload.persisted, "but the file does not carry it");
        let warning = payload.warning.expect("a NOT SAVED warning, never silence");
        assert!(
            warning.contains("NOT SAVED"),
            "the warning is AddPersist::warning verbatim: {warning}"
        );
    }

    /// The submitted account carries no refresh token — a fact the operator
    /// needs at add time, not a hard error: it will serve now and go dead once
    /// the access token expires.
    #[tokio::test]
    async fn add_endpoint_warns_when_no_refresh_token_is_supplied() {
        let (manager, path) = control_manager("add-norefresh", None);
        let (status, _headers, body) = add_request(
            Arc::clone(&manager),
            Some(loopback_peer()),
            None,
            r#"{"name":"carol@example.com","accessToken":"at-carol"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let payload: AddAccountResponse = serde_json::from_slice(&body).expect("payload");
        assert!(payload.persisted);
        let warning = payload.warning.expect("a no-refresh-token warning");
        assert!(warning.contains("no refresh token"), "{warning}");
        std::fs::remove_file(&path).ok();
    }

    /// F7 — the add route is `POST`-only, same as the disable route
    /// ([`control_endpoint_refuses_other_methods_locally`]), and had no test of
    /// its own: a wrong method must be answered LOCALLY (never forwarded, never
    /// able to mutate), and the 405 must still identify the route so a caller
    /// can tell it from an older tcr that has no route at all.
    #[tokio::test]
    async fn add_endpoint_refuses_other_methods_locally() {
        use tower::ServiceExt as _;
        for method in [Method::GET, Method::PUT, Method::DELETE, Method::PATCH] {
            let (manager, path) = control_manager("add-method", None);
            let mut req = Request::builder()
                .method(method.clone())
                .uri(ADD_ACCOUNT_PATH)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"name":"carol@example.com","accessToken":"at-carol"}"#,
                ))
                .expect("build request");
            req.extensions_mut().insert(ClientAddr(loopback_peer()));
            let response = app(Arc::clone(&manager))
                .oneshot(req)
                .await
                .expect("router response");
            assert_eq!(
                response.status(),
                StatusCode::METHOD_NOT_ALLOWED,
                "{method} on the add-account path is refused locally"
            );
            assert_ne!(
                response.status(),
                StatusCode::BAD_GATEWAY,
                "{method} must never fall through to the upstream forwarder"
            );
            assert_eq!(
                response
                    .headers()
                    .get(ENDPOINT_HEADER)
                    .and_then(|v| v.to_str().ok()),
                Some(ADD_ACCOUNT_ENDPOINT),
                "{method}: the 405 identifies the route, so a caller does not read \
                 it as a proxy too old to have one"
            );
            assert_eq!(
                manager.snapshot(OffsetDateTime::now_utc()).accounts.len(),
                2,
                "{method} appended or changed an account"
            );
            std::fs::remove_file(&path).ok();
        }
    }

    /// SEAM TEST. `oauth::probe_add_capability` (unit 3, `oauth.rs:901-932`)
    /// decides whether `tcr login` takes the LIVE route by POSTing this exact
    /// deliberately-blank body to `ADD_ACCOUNT_PATH` via
    /// `cli::post_add_account` and reading whether the reply is a STAMPED
    /// 400. An unstamped answer, or a different (non-400) status, reads as
    /// `AddCapability::Unusable` — `login_route` now REFUSES outright rather
    /// than falling back to the whole-file `config::save` path (bridge item
    /// D): a needless refusal beside a server that has the route, not the
    /// single-use-refresh-token clobber this whole feature exists to remove.
    /// Worst of all would be a 200 that actually appended a blank-named
    /// account to the live fleet: that reads as `AddCapability::Present`
    /// instead — a probe that is supposed to always fail silently succeeding
    /// and mutating the live server.
    ///
    /// Driven through the REAL production stack — a real `TcpListener` and
    /// `crate::mitm::serve`, exactly how `main()` invokes it — not a
    /// hand-built router, so a change to route registration or middleware
    /// ordering is covered here too, not only by the in-process `oneshot`
    /// tests above. Complements (does not replace) unit 3's own
    /// `probe_add_capability_reads_a_stamped_400_as_present` (oauth.rs) and
    /// `post_add_account_reads_a_stamped_400_as_rejected` (cli.rs), which
    /// this repo's cross-file boundary keeps this route from editing.
    #[tokio::test]
    async fn probe_add_capabilitys_blank_body_is_a_stamped_400_and_mutates_nothing() {
        let (manager, path) = control_manager("seam-probe", None);
        let before = manager.snapshot(OffsetDateTime::now_utc()).accounts.len();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind seam-probe listener");
        let addr = listener.local_addr().expect("listener addr");
        let serving = Arc::clone(&manager);
        tokio::spawn(async move { crate::mitm::serve(listener, serving, None).await });

        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("build client");
        // The EXACT `Account` `probe_add_capability` builds (oauth.rs:902-915):
        // a blank name, a blank access token, everything else absent.
        // DERIVED, not a restated literal — a future edit to the probe's body
        // (oauth.rs, out of this route's reach) changes this test's request
        // too, instead of leaving a stale literal green while it tests a body
        // nobody sends. Sent via `.json()` exactly like `post_add_account`
        // sends it (`cli.rs:478`, `.json(account)`).
        let probe_body = Account {
            name: String::new(),
            account_type: "oauth".to_string(),
            account_uuid: None,
            org_uuid: None,
            org_name: None,
            access_token: String::new(),
            refresh_token: None,
            expires_at: None,
            priority: None,
            switch_threshold: None,
            disabled: None,
            extra: serde_json::Map::new(),
        };
        let resp = client
            .post(format!("http://{addr}{ADD_ACCOUNT_PATH}"))
            .json(&probe_body)
            .send()
            .await
            .expect("send the probe's blank body");

        assert_eq!(
            resp.status().as_u16(),
            400,
            "the probe body must answer a local 400 — never forwarded, never a \
             soft-fail status the probe would misclassify"
        );
        assert_eq!(
            resp.headers()
                .get(ENDPOINT_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some(ADD_ACCOUNT_ENDPOINT),
            "unstamped and `probe_add_capability` reads this as Unusable — `login_route` \
             now refuses outright instead of falling back to the whole-file config::save \
             path with a live server running"
        );

        assert_eq!(
            manager.snapshot(OffsetDateTime::now_utc()).accounts.len(),
            before,
            "the probe's deliberately-blank body must never reach add_or_update_account"
        );
        assert_eq!(account_count_in_file(&path), before, "…nor the disk");

        std::fs::remove_file(&path).ok();
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
            "/v1/mcp_servers",
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
        for path in [
            "/v1/code",
            "/api/oauth/files",
            "/api/oauth/file_upload",
            "/v1/mcp_servers",
        ] {
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

    /// Pins the claude.ai connector list onto the client's own credential.
    #[test]
    fn the_connector_list_never_takes_a_pooled_token() {
        for method in [Method::GET, Method::POST] {
            assert_eq!(
                relay_mode(&method, "/v1/mcp_servers"),
                Some(RelayMode::ClientCredential),
                "{method} /v1/mcp_servers must not be re-credentialled"
            );
        }
        assert_eq!(
            relay_mode(&Method::GET, "/v1/mcp_servers/srv_0123"),
            Some(RelayMode::ClientCredential),
            "one connector is the same route as the collection"
        );
        for path in ["/v1/mcp_servers_v2", "/v1/mcp_serversX"] {
            assert_eq!(
                relay_mode(&Method::GET, path),
                None,
                "{path} only shares a prefix — it is not the route"
            );
        }
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
        let paths = [
            "/v1/code/session/abc",
            "/api/oauth/files/file_0123",
            "/api/oauth/file_upload",
            "/v1/mcp_servers",
        ];
        for path in paths {
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
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            paths.len(),
            "one upstream hit per relayed path"
        );
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
            // Near-misses of the two account-control routes. The mutating verb
            // makes these the rows that matter most: a `POST` that fell through
            // the catch-all would be rewritten onto api.anthropic.com carrying a
            // pooled OAuth Bearer, which is the shape that burned an account.
            (Method::POST, "/_tcr/accounts/"),
            (Method::POST, "/_tcr/accounts/disable"),
            (Method::POST, "/_tcr/accounts/disabled/extra"),
            (Method::POST, "/_tcr/accounts/priority"),
            // The bare path IS registered (ADD_ACCOUNT_PATH) — a `GET` on it
            // gets a local 405, not a 404 like every other row here — but it
            // belongs in this table anyway for the assertion this table makes
            // and `add_endpoint_refuses_other_methods_locally` does not: the
            // hit counter, proving a wrong-verb request STILL never reaches
            // upstream.
            (Method::GET, "/_tcr/accounts"),
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
            // path), so it is served; the bare add-account path is registered too,
            // wrong verb, so it is a local 405; every other shape is a local 404.
            // None of the three is ever forwarded, which is the single claim this
            // test makes.
            if uri.starts_with("/_tcr/status?") {
                assert_eq!(status, StatusCode::OK, "{uri} is the real status route");
            } else if uri == "/_tcr/accounts" {
                assert_eq!(
                    status,
                    StatusCode::METHOD_NOT_ALLOWED,
                    "{method} {uri} → the route exists, wrong verb"
                );
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

    /// THE OTHER BUG this fix closes: a stream that ends after `message_start` /
    /// `message_delta` with no `message_stop` is not a clean serve — it is
    /// indistinguishable, from inside `parse_sse_usage`, from a connection that
    /// was severed mid-turn. Anthropic's ONLY marker that a turn actually
    /// finished is `message_stop`; a complete turn carries one, and anything
    /// that reaches EOF without one — this fixture included — is truncated.
    /// Before this fix this test asserted `stream_error_count == 0` under the
    /// docstring "a clean 200 SSE stream" for exactly this fixture — the
    /// assertion WAS the bug, encoded as the contract. It is inverted here to
    /// assert the truncation IS detected.
    #[tokio::test]
    async fn sse_stream_without_message_stop_is_recorded_as_truncated() {
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

        let (seen, polls) = poll_stream_error_count(&manager, 1).await;
        assert_eq!(
            seen, 1,
            "polled {polls} times, stream-error count stayed {seen} — a stream that \
             ends without message_stop must be recorded as truncated, not booked as \
             a clean serve"
        );

        // The tee task updates usage BEFORE it records the stream error (see the
        // spawned task above `parse_sse_usage` is awaited in), so by the time the
        // poll above observes the error count, usage from `message_start` has
        // already landed too — truncation detection does not cost the quota
        // accounting anything.
        assert_eq!(
            manager.snapshot(OffsetDateTime::now_utc()).accounts[0].input_tokens,
            5,
            "usage already parsed before the stream was found truncated must still count"
        );
    }

    /// POSITIVE CONTROL for the truncation detector above: the identical
    /// fixture, this time WITH `message_stop`, must record NO stream error.
    /// Without this, the detector could not be told apart from one that fires
    /// unconditionally — this is what proves it discriminates on the
    /// terminator rather than on the mere absence of an in-band `error` event.
    #[tokio::test]
    async fn sse_stream_with_message_stop_records_no_stream_error() {
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
                        "event: message_stop\n",
                        "data: {\"type\":\"message_stop\"}\n\n",
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
            "a stream that reaches message_stop must record no stream error"
        );
    }

    /// FALSE-POSITIVE case #1 (the worst of the set): a CLIENT abandoning the
    /// connection mid-turn — exactly what happens when a user hits Esc in
    /// Claude Code — must record NOTHING, not `"truncated"`. Before the
    /// `ended`/`evidence_dropped` machinery, `tx` was owned by the `.map()`
    /// closure over `resp.bytes_stream()`; axum dropping the response body on
    /// client disconnect dropped that closure, hence `tx`, and `rx.recv()`
    /// returning `None` was INDISTINGUISHABLE from the upstream finishing the
    /// stream itself. The fake upstream here sends `message_start` and then
    /// hangs forever (never sends `message_stop`, never closes) — the ONLY
    /// way this stream ever ends is the test client dropping it, so any
    /// `stream_error` this test observes was fabricated from an interrupt,
    /// not from anything upstream actually did.
    ///
    /// Also covers the overwrite half of the same bug: a genuine PRIOR
    /// `last_stream_error` is seeded before the interrupted request, and must
    /// survive it untouched — `record_stream_error` always overwrites the
    /// label (see its doc comment), so a false "truncated" here would have
    /// erased real evidence of the account being `overloaded_error`.
    #[tokio::test]
    async fn client_abort_mid_stream_records_no_stream_error() {
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
                    let event = concat!(
                        "event: message_start\n",
                        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n",
                    );
                    // Chunked, not content-length: a real Messages turn's
                    // length isn't known up front. One chunk, then HANG —
                    // never a terminating "0\r\n\r\n" chunk, never a close.
                    // If the client does not abandon this connection, this
                    // handler simply blocks forever and the test times out —
                    // which is the point: nothing here ever produces
                    // `message_stop` or EOF on its own.
                    let headers = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n";
                    let chunk = format!("{:x}\r\n{event}\r\n", event.len());
                    let _ = sock.write_all(headers.as_bytes()).await;
                    let _ = sock.write_all(chunk.as_bytes()).await;
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                });
            }
        });

        let manager =
            Manager::with_live_refresher(dummy_config(None, &format!("http://{up_addr}")), None);
        // Seed a genuine prior error BEFORE the interrupted request — the
        // overwrite-on-abandonment bug would clobber this.
        manager.record_stream_error(0, "overloaded_error");

        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy.local_addr().unwrap();
        let served = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = axum::serve(proxy, app(served)).await;
        });

        tokio::time::timeout(Duration::from_secs(10), async {
            let client = reqwest::Client::builder().no_proxy().build().unwrap();
            let resp = client
                .post(format!("http://{proxy_addr}/v1/messages"))
                .body("{}")
                .send()
                .await
                .unwrap();
            let mut body = resp.bytes_stream();
            // Read the ONE chunk the fake upstream ever sends, proving the
            // tee actually started, THEN abandon — mirrors a client that
            // received partial output before the user hit Esc.
            let first = body.next().await;
            assert!(first.is_some(), "expected the message_start chunk");
            drop(body);
        })
        .await
        .expect("client-abort scenario must not hang the server");

        // Positive, waitable signal that the tee's parse loop drained the one
        // chunk it got: usage from `message_start` lands regardless of how
        // the stream ends (see `sse_stream_without_message_stop_...` above).
        let (input_tokens, polls) = {
            let mut polls = 0u32;
            let mut seen = manager.snapshot(OffsetDateTime::now_utc()).accounts[0].input_tokens;
            while seen == 0 && polls < 200 {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                seen = manager.snapshot(OffsetDateTime::now_utc()).accounts[0].input_tokens;
                polls += 1;
            }
            (seen, polls)
        };
        assert_eq!(
            input_tokens, 5,
            "polled {polls} times waiting for usage to land"
        );

        // No positive signal exists for "the classifier decided to abstain" —
        // silence IS the pass condition — so flat-wait comfortably past the
        // `ended`/`evidence_dropped` decision (a local oneshot + atomic load,
        // never network-bound) before asserting nothing changed.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let snapshot = manager.snapshot(OffsetDateTime::now_utc());
        assert_eq!(
            snapshot.accounts[0].stream_error_count, 1,
            "count must stay at the ONE seeded error — the client abandoning \
             the connection must add nothing"
        );
        assert_eq!(
            snapshot.accounts[0].last_stream_error.as_deref(),
            Some("overloaded_error"),
            "the genuine prior error label must survive a client abandoning \
             a later stream — record_stream_error unconditionally overwrites \
             this field, so a fabricated \"truncated\" here would erase real \
             evidence"
        );
    }

    /// FALSE-POSITIVE case #3: a forwarded non-2xx response never had a
    /// `message_stop` contract — it is an error response, not a truncated
    /// turn. The fixture is deliberately the SAME shape that would have been
    /// misclassified as truncated before this fix (a `message_start` with no
    /// `message_stop`), on a 400 instead of a 200, to prove the status gate
    /// — not the message_start gate above — is what suppresses it here.
    #[tokio::test]
    async fn non_2xx_sse_body_records_no_stream_error() {
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
                    );
                    let response = format!(
                        "HTTP/1.1 400 Bad Request\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
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
        assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
        let _ = resp.bytes().await.unwrap();

        // record_served fires on the terminal outcome regardless of status
        // (same as the in-band-error test above) — a waitable positive
        // signal that the request was actually processed before we check for
        // the absence of a stream error.
        let mut polls = 0u32;
        let mut requests = manager.snapshot(OffsetDateTime::now_utc()).accounts[0].requests;
        while requests == 0 && polls < 200 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            requests = manager.snapshot(OffsetDateTime::now_utc()).accounts[0].requests;
            polls += 1;
        }
        assert_eq!(
            requests, 1,
            "polled {polls} times waiting for the request to be booked"
        );

        // Flat-wait past where the tee task would have recorded a stream
        // error had the status gate not suppressed it — same rationale as
        // the client-abort test above (silence is the pass condition).
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            manager.snapshot(OffsetDateTime::now_utc()).accounts[0].stream_error_count,
            0,
            "a forwarded 400 body missing message_stop is an error response, \
             not a truncated Messages turn — it must not be classified"
        );
    }

    /// End-to-end version of `sse_stream_severed_before_any_event_records_truncation`
    /// above: a 2xx `text/event-stream` response that closes with an EMPTY
    /// body — no bytes at all, so `parse_sse_usage` sees zero events — must
    /// be recorded as truncated through the FULL pipeline, including the
    /// `status_is_success` / `ended_naturally` / `evidence_dropped` gates the
    /// unit-level test above never exercises. This is the severest live
    /// regression named in the round-4 review: before the fix, this exact
    /// shape recorded NOTHING — zero content delivered to the client, and
    /// the account looked perfectly healthy.
    #[tokio::test]
    async fn sse_stream_severed_before_message_start_is_recorded_as_truncated_e2e() {
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
                    // Headers claim an SSE body, then the connection closes
                    // having sent NOT ONE byte of it — severed before the
                    // stream could ever produce its first parseable event.
                    let response = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
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
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let _ = resp.bytes().await.unwrap();

        let (seen, polls) = poll_stream_error_count(&manager, 1).await;
        assert_eq!(
            seen, 1,
            "polled {polls} times, stream-error count stayed {seen} — a 2xx \
             SSE stream severed before its first event must be recorded as \
             truncated, not booked as a clean serve"
        );
        assert_eq!(
            manager.snapshot(OffsetDateTime::now_utc()).accounts[0]
                .last_stream_error
                .as_deref(),
            Some(TRUNCATED_STREAM_ERROR_KIND)
        );
    }

    /// MUST-RECORD: "an in-band `error` event, at ANY status, always." This is
    /// the live regression from `status_is_success` wrapping BOTH the
    /// synthesized-truncation branch and the positive-observation branch: a
    /// genuine `error` event forwarded inside a NON-2xx body used to be
    /// silently dropped, a coverage regression versus before the truncation
    /// feature existed at all. Same fixture shape as
    /// `sse_error_event_is_observed_not_counted_as_clean_serve` (a 200), this
    /// time on a 400 — proving the status gate no longer reaches the positive
    /// case.
    #[tokio::test]
    async fn in_band_error_on_non_2xx_is_recorded_regardless_of_status() {
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
                        "data: {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"bad\"}}\n\n",
                    );
                    let response = format!(
                        "HTTP/1.1 400 Bad Request\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
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
        assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
        let _ = resp.bytes().await.unwrap();

        let (seen, polls) = poll_stream_error_count(&manager, 1).await;
        assert_eq!(
            seen, 1,
            "polled {polls} times, stream-error count stayed {seen} — an \
             in-band SSE error event must be observed on a forwarded non-2xx \
             body too, not just on 200"
        );
        assert_eq!(
            manager.snapshot(OffsetDateTime::now_utc()).accounts[0]
                .last_stream_error
                .as_deref(),
            Some("invalid_request_error"),
            "the recorded kind must be the POSITIVE observation read off the \
             wire, never the synthesized \"truncated\" kind — this path must \
             not go anywhere near the status_is_success gate"
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
    ///
    /// The script carries TWO blips because a transport failure now buys the same
    /// account one retry on a fresh connection
    /// (`transport_failure_retries_the_same_account_first`), so it takes two to
    /// bench `a` and reach `b`. The scenario under test is unchanged — one account
    /// transport-benched, one real upstream 429, one revalidation serve; only the
    /// number of sends it takes to set it up moved. A single blip here would leave
    /// `a` retried into the 429 slot and never exercise the revalidation ladder.
    #[tokio::test]
    async fn transport_blip_does_not_disable_recovery() {
        let up_addr = spawn_scripted_upstream(vec![
            None,                        // attempt 1 — `a` fails in transport
            None,                        // attempt 2 — `a`'s one retry fails too
            Some(raw_429_rejected(120)), // attempt 3 — `b` answers, durably 429
            Some(raw_200()),             // attempt 4 — `c` serves via revalidation
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

    /// A transport failure kills a CONNECTION, not an account — reqwest pools by
    /// `(scheme, authority)` and the account is only a Bearer header, so it is not
    /// in the pool key. Retrying the same account on a fresh connection is
    /// therefore the failover on the RIGHT axis, and it keeps the account's warm
    /// prompt cache instead of paying a cold prefix for a network blip.
    ///
    /// Same shape as `overloaded_529_retries_the_same_account`: the fleet is LRU,
    /// so a fresh fleet picks `a` and any rotation picks `b`. Scripting blip-then-200
    /// makes the two behaviours distinguishable in the stats — retry-in-place
    /// credits `a`, the old bench-and-rotate credits `b`.
    #[tokio::test]
    async fn transport_failure_retries_the_same_account_first() {
        let up_addr = spawn_scripted_upstream(vec![
            None,            // attempt 1 — `a`'s connection dies
            Some(raw_200()), // attempt 2 — the SAME account, fresh connection
        ])
        .await;
        let manager = fleet(up_addr, &["a", "b"]);

        let status = post_one(manager.clone()).await;
        assert_eq!(status, 200, "the retry must serve the client");

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
            "a transport blip must retry the account it blipped on — landing on `b` \
             means one dead connection benched a healthy account and threw away its \
             warm prompt cache"
        );
    }

    /// The other half of the bound: the retry is ONE per account per request. A
    /// second failure is evidence about the account rather than the connection, so
    /// it benches it and rotates exactly as before this arm existed.
    ///
    /// The attempt count is read from the upstream rather than inferred: exactly
    /// three sends (`a`, `a` again, then `b`) — a fourth would mean the same-account
    /// retry can repeat, which is how a blipping upstream would burn the whole
    /// `max_attempts` budget on one account.
    #[tokio::test]
    async fn a_second_transport_failure_benches_the_account_and_rotates() {
        let (up_addr, attempts) = spawn_counted_upstream(vec![
            None,            // attempt 1 — `a` blips
            None,            // attempt 2 — `a`'s fresh connection blips too
            Some(raw_200()), // attempt 3 — rotated to `b`
        ])
        .await;
        let manager = fleet(up_addr, &["a", "b"]);

        let status = post_one(manager.clone()).await;
        assert_eq!(status, 200, "`b` still serves the request");

        let snap = manager.snapshot(OffsetDateTime::now_utc());
        let served: Vec<&str> = snap
            .accounts
            .iter()
            .filter(|a| a.requests > 0)
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(
            served,
            ["b"],
            "twice-failed `a` is benched for the rest of the request"
        );
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "one retry for `a`, then a rotation — not an unbounded same-account ladder"
        );
    }

    /// A CONNECT failure is not retried on the same account.
    ///
    /// The retry exists because a pooled connection can die between requests: the
    /// error itself evicts it, so the retry gets a fresh one and the account —
    /// which was never the failing resource — keeps its warm prompt cache. None
    /// of that holds when the connect itself failed: there was no pooled
    /// connection, nothing was evicted, and the second send is the identical
    /// operation. What it does buy is a second full `connect_timeout` (10s), and
    /// on a blackholed route (no RST, no reply) that doubles every request's
    /// worst-case time-to-502 — ~80s to ~160s on an eight-account fleet — with a
    /// per-account in-flight slot held for the whole of it.
    ///
    /// Read off the 502 the client actually receives, whose body carries the
    /// attempt count: two accounts that each fail to connect ONCE is `2`, and the
    /// retry firing here would make it `4`. An unreachable upstream is a dead
    /// port — the one shape that produces `is_connect()` for real rather than by
    /// simulation.
    #[tokio::test]
    async fn a_connect_failure_is_not_retried_on_the_same_account() {
        // Bind then drop: the address is guaranteed to have been free, and now
        // refuses. `spawn_counted_upstream`'s no-reply script cannot produce this
        // — dropping an ACCEPTED socket is a mid-request error, not a connect one.
        let dead = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            listener.local_addr().unwrap()
        };
        let manager = fleet(dead, &["a", "b"]);

        let (status, body) = post_one_with_body(manager).await;
        assert_eq!(
            status, 502,
            "nothing reached an upstream, so the honest answer is a gateway error: {body}"
        );
        assert!(
            body.contains("all 2 attempt(s)"),
            "one connect attempt per account. `all 4 attempt(s)` means each was \
             retried into a second connect timeout for no new information: {body}"
        );
    }

    /// The MIXED ladder — a transport blip AND the 529 walk on one request — must
    /// fit the rotation loop's budget.
    ///
    /// This replaces a guard that could not fail. The previous version asserted
    /// `accounts * 2 <= max_attempts_for(accounts)` against a formula of
    /// `2n + 4`, i.e. `2n <= 2n + 4`: true by construction for every input and
    /// for every fleet size, so it passed unchanged through the very change it
    /// was written to guard. A gate that cannot go red is not a gate.
    ///
    /// The ladder below is derived from the LADDER constants instead, so it is
    /// an independent claim about the loop and not a restatement of the budget:
    ///
    /// * Each of the (up to `1 + MAX_529_FAILOVERS_PER_REQUEST`) accounts on the
    ///   529 walk can spend `1` send, `1` transport retry, and
    ///   `MAX_SAME_ACCOUNT_529_RETRIES` in-place retries before it fails over.
    /// * Every remaining account in the fleet is still reachable by the plain
    ///   transport ladder, at `1` send plus `1` retry each.
    ///
    /// Under the old `2n + 4` this is false from three accounts up (12 sends
    /// against a budget of 10) — which is the truncation
    /// [`the_mixed_ladder_forwards_the_real_529_instead_of_synthesizing_a_429`]
    /// shows a client actually receiving.
    #[test]
    fn the_mixed_transport_and_529_ladder_fits_the_attempt_budget() {
        let walked = 1 + MAX_529_FAILOVERS_PER_REQUEST as usize;
        for accounts in 1..=16usize {
            let on_the_529_walk = accounts.min(walked);
            let sends = on_the_529_walk * (2 + MAX_SAME_ACCOUNT_529_RETRIES as usize)
                + (accounts - on_the_529_walk) * 2;
            assert!(
                sends <= max_attempts_for(accounts),
                "a {accounts}-account fleet can spend {sends} sends on a blip-plus-529 \
                 ladder but the loop allows only {} attempts — the walk is truncated \
                 mid-request and the client gets a synthesized 429",
                max_attempts_for(accounts)
            );
        }
    }

    /// THE CLIENT-VISIBLE CONSEQUENCE of the budget above, end to end through
    /// `handle`: a fleet where every account blips once and is then overloaded.
    ///
    /// Each account spends 4 sends (blip, retry, then the two in-place 529
    /// backoffs) before failing over, so the full three-account walk is 12 —
    /// against the old budget of `2*3 + 4 = 10`. Truncated, the loop falls out
    /// mid-walk, `every_attempt_transport_failed` is false because upstreams did
    /// respond, and the client is handed `exhausted_response`: HTTP 429 "All 3
    /// accounts exhausted" with a FABRICATED retry-after, for a request whose
    /// honest answer is the upstream's own 529. The status is the assertion; the
    /// attempt count is what proves the whole mixed ladder actually ran rather
    /// than the test having accidentally taken a shorter path to the same code.
    #[tokio::test]
    async fn the_mixed_ladder_forwards_the_real_529_instead_of_synthesizing_a_429() {
        let blip_then_overloaded = || vec![None, Some(raw_529()), Some(raw_529()), Some(raw_529())];
        let script: Vec<Option<String>> = (0..3).flat_map(|_| blip_then_overloaded()).collect();
        assert_eq!(
            script.len(),
            12,
            "three accounts of blip + a full 529 ladder"
        );

        let (up_addr, attempts) = spawn_counted_upstream(script).await;
        let manager = fleet(up_addr, &["a", "b", "c"]);

        let (status, body) = post_one_with_body(manager).await;
        assert_eq!(
            status, 529,
            "the upstream's own 529 must reach the client, not a 429 we invented \
             because the loop ran out of attempts: {body}"
        );
        assert!(
            !body.contains("exhausted"),
            "a synthesized exhaustion body means the walk was truncated: {body}"
        );
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            12,
            "every rung of the mixed ladder has to have been walked — a smaller \
             number means the request stopped early and the 529 above was luck"
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

    /// Issue a real request that cannot succeed and hand back the real
    /// `reqwest::Error`. A hand-built stub would prove nothing here: the whole
    /// question is what the LIVE error's `source()` chain looks like.
    async fn transport_error_for(url: &str) -> reqwest::Error {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("client builds")
            .get(url)
            .send()
            .await
            .expect_err("this request cannot succeed")
    }

    /// A port nothing is listening on: bind it, read it back, release it.
    fn closed_loopback_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        drop(listener);
        port
    }

    /// The two-sided gate on [`is_offline_error`]. One side alone is worthless —
    /// a classifier that answers `true` to everything passes the offline half and
    /// would then hold every dead route on its pinned account forever.
    ///
    /// Both errors below are `is_connect() == true`, which is precisely why the
    /// arm cannot be gated on `is_connect()`: that is the bug being fixed.
    #[tokio::test]
    async fn is_offline_error_separates_a_dead_resolver_from_a_dead_route() {
        // TRUE case: `.invalid` is reserved by RFC 2606 to never resolve.
        let dns = transport_error_for("http://offline-probe.invalid/v1/messages").await;
        println!(
            "offline case: is_offline={} is_connect={} err={dns:?}",
            is_offline_error(&dns),
            dns.is_connect()
        );
        assert!(
            dns.is_connect(),
            "precondition: a resolver failure is is_connect, so is_connect cannot discriminate"
        );
        assert!(
            is_offline_error(&dns),
            "a name-resolution failure must classify as offline: {dns:?}"
        );

        // FALSE case: the host resolves and answers with an RST. That IS evidence
        // about the route, so it must keep taking the rotate arm.
        let url = format!("http://127.0.0.1:{}/v1/messages", closed_loopback_port());
        let refused = transport_error_for(&url).await;
        println!(
            "refused case: is_offline={} is_connect={} err={refused:?}",
            is_offline_error(&refused),
            refused.is_connect()
        );
        assert!(
            refused.is_connect(),
            "precondition: a refused connection is is_connect too"
        );
        assert!(
            !is_offline_error(&refused),
            "connection refused to a resolvable host is a ROUTE failure, not an offline \
             machine — classifying it offline would pin a request to a dead account: {refused:?}"
        );
    }

    /// End to end: an upstream whose hostname cannot resolve must answer `503`
    /// with a `retry-after`, never the `502` this branch used to produce.
    #[tokio::test]
    async fn an_unresolvable_upstream_answers_503_with_retry_after_not_502() {
        let manager =
            Manager::with_live_refresher(dummy_config(None, "http://offline-probe.invalid"), None);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app(manager)).await;
        });

        let resp = reqwest::Client::new()
            .post(format!("http://{addr}/v1/messages"))
            .json(&serde_json::json!({"model": "claude-3-5-sonnet", "messages": []}))
            .send()
            .await
            .expect("the proxy itself is reachable");
        assert_eq!(
            resp.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "an offline machine is a recoverable local condition, not a bad gateway"
        );
        assert_eq!(
            resp.headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok()),
            Some(OFFLINE_RETRY_AFTER_SECS.to_string().as_str()),
            "the 503 must tell the client when to come back"
        );
    }

    /// The per-request wait bound must stay inside the loop's total attempt
    /// budget, for the smallest fleet there can be — otherwise a request that
    /// rides out a resolver blip is truncated mid-walk.
    #[test]
    fn the_offline_wait_bound_fits_the_attempt_budget() {
        assert!(
            (MAX_OFFLINE_WAITS_PER_REQUEST as usize) < max_attempts_for(1),
            "offline waits ({MAX_OFFLINE_WAITS_PER_REQUEST}) must not exhaust the attempt budget"
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
