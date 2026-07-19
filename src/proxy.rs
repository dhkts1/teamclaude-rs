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
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, HOST};
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

/// How a transient (non-quota-rejected) 429 should be handled.
#[derive(Debug, PartialEq, Eq)]
enum Transient429 {
    /// Wait `secs` inline on the same account, then retry it.
    InlineWait(i64),
    /// Park the account for `secs` and rotate to another.
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
/// Its presence pins every request on this connection to one account via
/// [`Manager::select`]'s `affinity` arg; its absence (the default) leaves
/// selection at the per-request LRU rotation. One connection = one `claude`
/// session, so a connection-scoped key is a session-scoped key.
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
///   3. `None` — the caller falls back to the per-connection `SessionKey`.
///
/// Returns `None` on absence/parse failure so the caller keeps current behaviour.
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

/// Build the proxy router. Every method and path funnels through the single
/// catch-all [`handle`]; the [`Manager`] is shared state.
pub fn app(manager: Arc<Manager>) -> Router {
    Router::new().fallback(handle).with_state(manager)
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

    // 1b. Host guard: tcr is a credential-injecting reverse proxy for
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
    // its per-account prompt cache warm; fall back to the per-connection key only
    // when no stable identity is available. `session_kind` records WHICH branch
    // produced the key (stable identity vs per-connection fallback) — DISPLAY
    // provenance only, threaded into `record_served`; routing keys on
    // `session_key` byte-for-byte as before.
    let (session_key, session_kind) = match parts.extensions.get::<SessionKey>() {
        Some(conn) => {
            match stable_session_key(&req_headers, &body_bytes, manager.proxy_api_key()) {
                Some(key) => (Some(key), SessionKind::Stable),
                None => (Some(conn.0), SessionKind::Fallback),
            }
        }
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
    // Distinguishes "no account available" (429) from "every attempt hit a
    // transport failure" (502) once the loop can no longer make progress.
    let mut saw_network_error = false;
    // Bound the total attempts so per-account 401/429 retries can never loop.
    let max_attempts = account_count.saturating_mul(2).saturating_add(4).max(1);
    // A genuine same-account retry (401 force-refresh, transient-429 wait) parks
    // its idx here so the next iteration reuses it and bypasses select(), which
    // would otherwise rotate AWAY from the account the retry meant to keep.
    let mut retry_same: Option<usize> = None;

    for _ in 0..max_attempts {
        let now = OffsetDateTime::now_utc();
        let idx = match retry_same.take() {
            Some(i) => i,
            None => match manager.select(&tried, now, request_model.as_deref(), session_key) {
                Some(idx) => idx,
                None => {
                    return if saw_network_error {
                        bad_gateway()
                    } else {
                        exhausted_response(&manager, now, account_count, request_is_fable)
                    };
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

        // Global outbound throttle: pace the AGGREGATE egress so a cold fan-out
        // cannot burst the shared upstream limiter. Inert unless configured. Placed
        // after account selection/token so only real sends consume a slot; both the
        // 401 force-refresh retry and the transient-429 retry loop back here, so
        // every retry is paced automatically.
        manager.throttle_send().await;

        let Ok(resp) = builder.send().await else {
            // Transport failure is not proof of a bad credential — fail this
            // request over to another account, keep this one eligible.
            saw_network_error = true;
            tried.insert(idx);
            continue;
        };

        let status = resp.status();
        // Fetch the serving account's name once per iteration — reused by the log
        // line below and, on the terminal path, by push_log (was two read-locks +
        // clones back-to-back).
        let account_name = manager.account_name(idx);
        // One greppable line per upstream response, tagged with the true serving
        // account and outcome status. "serving request" logs BEFORE the outcome, so
        // without this the logs are status-blind — the gap that hid the 401 storm.
        tracing::info!(
            account_index = idx,
            account = account_name.as_deref().unwrap_or("?"),
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
                    retry_same = Some(idx);
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
                unified_5h_reset = header_str("anthropic-ratelimit-unified-5h-reset"),
                unified_7d_reset = header_str("anthropic-ratelimit-unified-7d-reset"),
                quota_rejected = is_quota_rejected(&up_headers),
                "429 diagnostic"
            );
            let retry_after_raw = parse_retry_after(&up_headers);
            let retry_after = retry_after_raw.unwrap_or(60);
            if is_quota_rejected(&up_headers) {
                manager.mark_rate_limited(idx, retry_after.clamp(1, 3600));
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
                    retry_same = Some(idx);
                    continue; // retry the same account after the bounded wait
                }
                Transient429::Park(wait) => {
                    manager.mark_rate_limited(idx, wait);
                    tried.insert(idx);
                    continue;
                }
            }
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
                let (input, output) = parse_sse_usage(byte_stream).await;
                if input > 0 || output > 0 {
                    manager_side.update_usage(idx, input, output);
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
            return build_response(status, &up_headers, Body::from_stream(passthrough));
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
            let (input, output) = usage_from_json(&bytes);
            if input > 0 || output > 0 {
                manager.update_usage(idx, input, output);
            }
        }
        return build_response(status, &up_headers, Body::from(bytes));
    }

    // Ran out of attempts while still rotating — treat repeated transport
    // failures as a bad gateway, otherwise as exhausted quota.
    if saw_network_error {
        bad_gateway()
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

/// Assemble the client response: the upstream status + body, carrying every
/// upstream header except the connection-specific / framing ones.
fn build_response(status: StatusCode, up_headers: &HeaderMap, body: Body) -> Response {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    let headers = response.headers_mut();
    for (name, value) in up_headers.iter() {
        if is_response_skip(name.as_str()) {
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
fn sum_input_tokens(usage: &Value) -> u64 {
    let field = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    field("input_tokens") + field("cache_creation_input_tokens") + field("cache_read_input_tokens")
}

/// Parse `(input, output)` token counts from a non-streamed JSON messages body.
fn usage_from_json(bytes: &[u8]) -> (u64, u64) {
    let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
        return (0, 0);
    };
    let Some(usage) = value.get("usage") else {
        return (0, 0);
    };
    let output = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    (sum_input_tokens(usage), output)
}

/// Parse total `(input, output)` usage from an SSE messages stream.
///
/// `input` is taken from `message_start` (base + cache tokens); `output` is the
/// latest cumulative count from `message_delta` (or `message_start` if no delta
/// arrives). Returning the totals — rather than incrementing per event — is what
/// makes the count applied exactly once, never doubled. eventsource-stream
/// reassembles events split across network chunks, so a boundary-split
/// `message_start` is still parsed whole (bug #1 designed out).
async fn parse_sse_usage<S, B, E>(stream: S) -> (u64, u64)
where
    S: futures::Stream<Item = Result<B, E>>,
    B: AsRef<[u8]>,
{
    let events = stream.eventsource();
    futures::pin_mut!(events);

    let mut input: u64 = 0;
    let mut output: u64 = 0;
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
                    input = sum_input_tokens(usage);
                    if let Some(out) = usage.get("output_tokens").and_then(Value::as_u64) {
                        output = out;
                    }
                }
            }
            Some("message_delta") => {
                if let Some(out) = value
                    .get("usage")
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(Value::as_u64)
                {
                    output = out;
                }
            }
            _ => {}
        }
    }
    (input, output)
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

/// 502 when every attempt hit a transport failure (upstream unreachable).
fn bad_gateway() -> Response {
    tracing::warn!("returning 502 to client — every account transport-failed");
    error_response(
        StatusCode::BAD_GATEWAY,
        "proxy_error",
        "Upstream unreachable after trying every account.",
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
        let (input, output) = parse_sse_usage(futures::stream::iter(chunks)).await;
        assert_eq!(input, 1110, "10 + 100 (cache-creation) + 1000 (cache-read)");
        assert_eq!(output, 42);
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
        let (input, output) = parse_sse_usage(stream).await;
        assert_eq!(input, 5);
        assert_eq!(output, 37, "final cumulative output, not 20 + 37");
    }

    /// Non-stream JSON path also sums the cache tokens into `input`.
    #[test]
    fn json_usage_sums_cache_tokens() {
        let body = br#"{"usage":{"input_tokens":7,"cache_creation_input_tokens":3,"cache_read_input_tokens":90,"output_tokens":11}}"#;
        let (input, output) = usage_from_json(body);
        assert_eq!(input, 100);
        assert_eq!(output, 11);
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
}
