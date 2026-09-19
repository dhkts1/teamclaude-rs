//! Reaching the internet through a peer.
//!
//! # Deny by default, which is the INVERSE of the local default
//!
//! Locally, a host that is not on the policy list gets blind-tunnelled, and
//! that is correct there: the surface is loopback and `tunnel()`'s own doc says
//! so. **The same default on a peer socket is an open proxy on the LAN.** So
//! this path gets its own constant, [`PEER_EGRESS_HOSTS`], deny by default, and
//! a test that proves the refusal.
//!
//! The list is the one the local proxy already enforces (`ALLOWED_HOSTS`,
//! `src/mitm.rs:66`): `api.anthropic.com` and `platform.claude.com`. The second
//! is not an extra, it is the host Claude Code's own OAuth refresh targets, so
//! carrying it is what lets a machine with no internet refresh its own tokens
//! through a peer and stop dying at the 29-day wall. Reused, not reinvented.
//!
//! # The splice socket, specified, because an unspecified one is a free proxy
//! for every process on the box
//!
//! The outbound client is pointed at a loopback splice with
//! `reqwest::ClientBuilder::resolve`, which overrides DNS ONLY, so TLS still
//! validates against the real hostname. That socket:
//!
//! - binds `127.0.0.1` on **port 0**, a kernel-assigned ephemeral port. Never a
//!   fixed number, never `0.0.0.0`, and never written to a config, a log line
//!   or `tcr status --json`;
//! - is **accept-once**: one listener per in-flight outbound request, closed
//!   the instant its single connection is accepted.
//!
//! Every local process can reach loopback, `local_endpoint_gate`'s own doc
//! says bind scope is not authorization, so a long-lived shared splice
//! listener would be a free peer-egress proxy for anything running on this box,
//! spending a peer's granted byte budget with no request of ours attached. A
//! second connection to the port cannot happen because the listener is already
//! gone; a race that arrives first spends the budget for exactly one request
//! and the real request fails closed with a transport error rather than
//! silently sharing the tunnel.
//!
//! # No ambient proxy variable, ever
//!
//! Every outbound client in this tree calls `.no_proxy()`, for the reason
//! `src/proxy.rs:3529-3530` gives: we ARE the proxy, and an ambient
//! `HTTP_PROXY` would loop us through ourselves. The peer egress path is an
//! explicit, per-origin, allow-listed loopback splice and honours no
//! environment variable.
//!
//! # Where it attaches: ONE hunk that covers both entry points
//!
//! `is_offline_error` (`src/proxy.rs:366`) walks the error chain for DNS
//! markers and returns true for NOTHING ELSE, and an existing test pins that
//! false case deliberately, because a refused connection to a reachable host IS
//! real evidence about a route and must keep taking the rotate arm. So a
//! captive portal, the most likely shape of "somewhere the internet barely
//! works", never reaches the resolver arm at all. The design therefore names
//! two entry points: the resolver arm, offered BEFORE the fixed six-second
//! offline wait, and the exhausted-transport terminal gated on
//! `unknown_outcome_transport_failure == false`.
//!
//! Both are the SAME predicate read at two places, "nothing this request sent
//! ever left this box", so [`retry_through_peer`] is called from ONE hunk, the
//! top of `handle`'s transport-failure arm, under
//! `err.is_connect() || is_offline_error(&err)`. `is_connect()` is exactly the
//! condition that leaves `unknown_outcome_transport_failure` false (it is set
//! at `src/proxy.rs:2684` under `!err.is_connect()`), and the resolver arm is
//! reached before its own sleep because the hunk sits above it. One call site
//! that cannot drift from the other rather than two that can, and the
//! once-per-request bound is the `peer_egress_tried` flag beside
//! `unknown_outcome_transport_failure` itself.
//!
//! Neither existing classification is widened and no existing test needs a new
//! expectation: the broader "this machine has no path" predicate is
//! [`EgressState`], fed by both entry points' outcomes.
//!
//! # A second hunk, which is about an account and not about a failure
//!
//! The paragraph above is the whole story for [`retry_through_peer`], and it is
//! no longer the whole story for this module. The exit lock of decisions row 15
//! ("we need to be able to lock where accounts go out from which server
//! sometimes it might be important for saving the same ip") adds
//! [`pinned_egress`], called from one more hunk in `proxy.rs`, ABOVE the direct
//! attempt rather than inside its failure arm. It reads one account's pin, it
//! never touches [`EgressState`], and it never asks whether a transport failed,
//! because on that path nothing has been attempted yet. The two hunks cannot
//! both run for one request: a pinned account is answered or refused before the
//! direct attempt exists to fail.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{anyhow, Context as _, Result};
use axum::http::{HeaderMap, Method};
use axum::response::IntoResponse as _;
use tcr_peer_wire::{PeerId, StreamHeader, StreamKind, TunnelTarget};
use tokio::net::TcpListener;

use crate::peer::config::{PeerFile, PeerRow};
use crate::peer::id::NodeKey;
use crate::peer::noise::{self, Handshake, KEY_BYTES};
use crate::peer::tunnel::{self, NoiseStream};

/// The ONLY origins a gateway will carry. Deny by default: a host absent from
/// this list is refused, not tunnelled.
pub const PEER_EGRESS_HOSTS: &[&str] = &["api.anthropic.com", "platform.claude.com"];

/// The only port those origins are carried on.
///
/// Part of the allow-list decision rather than a free field, for the reason
/// [`TunnelTarget::Origin`] gives: host-and-port is the whole of what a blind
/// hop can enforce, so leaving the port open would hand a granted peer every
/// service on the same name.
pub const PEER_EGRESS_PORT: u16 = 443;

/// How long a peer carry may take (dial, handshake AND the origin's reply)
/// before it is abandoned.
///
/// One bound over the whole act, not one per hop: from the waiting client's
/// side a carry is a single answer, and a per-hop bound of five seconds is a
/// ten-second wait on a request that has already failed its direct attempt.
/// [`retry_through_peer`] turns it into a deadline the moment a candidate is
/// picked.
///
/// Small on purpose. This path runs when the direct one has just failed, so the
/// request has already paid a connect timeout; a gateway that does not answer
/// promptly is worth less than the 503 the caller already has in hand.
///
/// It does NOT bound the response BODY. `send` resolves on the head, so an SSE
/// answer streams for as long as it streams. See [`axum_response_from`].
pub const CARRY_SETUP_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the direct path is left alone after it failed.
pub const COLD_MS: i64 = 30_000;

/// Whether `host` and `port` are carried at all.
pub fn host_allowed(host: &str, port: u16) -> bool {
    port == PEER_EGRESS_PORT
        && PEER_EGRESS_HOSTS
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(host))
}

// ---------------------------------------------------------------------------
// What this node believes about its own path out
// ---------------------------------------------------------------------------

/// What this node currently believes about its own path to the internet.
///
/// A cached belief, not a measurement per request, and deliberately NOT a
/// widening of `is_offline_error`: that function keeps meaning "the resolver is
/// dead", and this is the broader question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EgressState {
    /// Direct works. Rung one of the ladder, and the only rung on a healthy
    /// machine.
    Healthy,
    /// Direct failed; do not retry it until this instant, in Unix
    /// milliseconds.
    Cold {
        /// When to try direct again.
        until_ms: i64,
    },
}

impl EgressState {
    /// Whether the direct path is worth trying at `now_ms`.
    pub fn direct_is_worth_trying(self, now_ms: i64) -> bool {
        match self {
            Self::Healthy => true,
            Self::Cold { until_ms } => now_ms >= until_ms,
        }
    }
}

/// This process's belief about its own egress, shared by every request.
///
/// A process-wide cell rather than a field on `Manager`: `Manager` is the
/// account fleet and this is a fact about the machine's network, and the one
/// thing this feature must never do is make a fact about the LAN look like a
/// fact about an account. Nothing here is persisted, a belief with a
/// thirty-second horizon that outlived a restart would be a stale belief.
static STATE: Mutex<EgressState> = Mutex::new(EgressState::Healthy);

/// This node's belief right now.
pub fn state() -> EgressState {
    STATE.lock().map_or(EgressState::Healthy, |state| *state)
}

/// Record that the direct path just failed with nothing having left the box.
pub fn note_direct_failure(now_ms: i64) {
    if let Ok(mut state) = STATE.lock() {
        *state = EgressState::Cold {
            until_ms: now_ms + COLD_MS,
        };
    }
}

/// Record that the direct path worked.
pub fn note_direct_success() {
    if let Ok(mut state) = STATE.lock() {
        *state = EgressState::Healthy;
    }
}

// ---------------------------------------------------------------------------
// Which way out
// ---------------------------------------------------------------------------

/// The operator's answer to "how does this Mac reach the internet", plus the
/// one number they may tune about it.
///
/// `tcr peer via auto|off|<peer> [--setup-timeout-ms <n>]`, stored as the single
/// `via` key in `tcr-peers.json`.
///
/// # Why one field and not two
///
/// The peers file gains exactly ONE key for this whole feature, so the route
/// and the operator's timeout live in the same value, and the serde
/// representation is the string when there is nothing to tune and an object
/// when there is:
///
/// ```json
/// "via": "auto"
/// "via": { "via": "off" }
/// "via": { "via": "auto", "setupTimeoutMs": 1500 }
/// ```
///
/// A file written by a build with no timeout set round-trips as the bare word,
/// so the common file stays readable by eye.
///
/// # Lower, never higher
///
/// [`Self::with_setup_timeout_ms`] refuses anything above
/// [`CARRY_SETUP_TIMEOUT`]. A carry only ever runs after the direct path has
/// already failed, the request has already paid a connect timeout, so a
/// longer wait here is strictly worse than the 503 the caller already has in
/// hand. The knob exists to make a carry give up SOONER, which is the choice an
/// operator on a slow LAN actually needs.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "ViaJson", into = "ViaJson")]
pub struct ViaSetting {
    /// Which way out.
    pub route: ViaRoute,
    /// The operator's own setup bound in milliseconds, when they set one.
    /// `None` is [`CARRY_SETUP_TIMEOUT`].
    setup_timeout_ms: Option<u64>,
}

/// The serde shape of [`ViaSetting`]: a bare word, or a word with the one
/// tunable beside it.
///
/// `untagged` with the string arm FIRST, so `"auto"` is read as a word and
/// never as a malformed object.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
enum ViaJson {
    /// `"auto"`, `"off"`, or a peer id.
    Word(String),
    /// The same word, plus the operator's setup bound.
    Tuned {
        /// `auto`, `off`, or a peer id.
        via: String,
        /// The setup bound in milliseconds.
        #[serde(rename = "setupTimeoutMs", skip_serializing_if = "Option::is_none")]
        setup_timeout_ms: Option<u64>,
    },
}

impl TryFrom<ViaJson> for ViaSetting {
    type Error = anyhow::Error;

    fn try_from(json: ViaJson) -> Result<Self> {
        match json {
            ViaJson::Word(word) => Self::parse(&word),
            ViaJson::Tuned {
                via,
                setup_timeout_ms: None,
            } => Self::parse(&via),
            ViaJson::Tuned {
                via,
                setup_timeout_ms: Some(ms),
            } => Self::parse(&via)?.with_setup_timeout_ms(ms),
        }
    }
}

impl From<ViaSetting> for ViaJson {
    fn from(setting: ViaSetting) -> Self {
        match setting.setup_timeout_ms {
            None => Self::Word(setting.to_spec()),
            Some(ms) => Self::Tuned {
                via: setting.to_spec(),
                setup_timeout_ms: Some(ms),
            },
        }
    }
}

/// Which way out, with no tuning attached.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ViaRoute {
    /// Use a trusted Mac whenever the direct path is dead. The default, and it
    /// is not a disclosure decision: a carry is blind, and the Mac that carries
    /// is the one that has to have granted it (`allow.gateway` in ITS file, and
    /// its listener refuses without it). So the consent that matters is the
    /// carrier's, and an operator who wants this Mac never to ask says so with
    /// `off`.
    #[default]
    Auto,
    /// Never route out through a peer. Byte-identical to the behaviour before
    /// this feature existed.
    Off,
    /// Always ask this one Mac.
    Pinned(PeerId),
}

impl ViaSetting {
    /// The default: ask a trusted Mac once the direct path is dead.
    pub fn auto() -> Self {
        Self::from_route(ViaRoute::Auto)
    }

    /// Never route out through a peer.
    pub fn off() -> Self {
        Self::from_route(ViaRoute::Off)
    }

    /// Always ask this one Mac, and never another.
    pub fn pinned(peer: PeerId) -> Self {
        Self::from_route(ViaRoute::Pinned(peer))
    }

    /// A route with no operator tuning attached.
    fn from_route(route: ViaRoute) -> Self {
        Self {
            route,
            setup_timeout_ms: None,
        }
    }

    /// Parse the CLI's third word: `auto`, `off`, or a peer id.
    pub fn parse(spec: &str) -> Result<Self> {
        let route = match spec.trim() {
            "auto" => ViaRoute::Auto,
            "off" => ViaRoute::Off,
            other => PeerId::parse(other)
                .map(ViaRoute::Pinned)
                .with_context(|| {
                    format!(
                        "peer via: `{other}` is neither `auto`, `off`, nor a peer id \
                         (`tcr peer ls` prints the ids this Mac has pinned)"
                    )
                })?,
        };
        Ok(Self::from_route(route))
    }

    /// The same setting with the operator's own setup bound, which may only be
    /// LOWER than [`CARRY_SETUP_TIMEOUT`]. See the type's docs for why a
    /// higher one is refused rather than accepted and clamped.
    pub fn with_setup_timeout_ms(mut self, ms: u64) -> Result<Self> {
        let ceiling = u64::try_from(CARRY_SETUP_TIMEOUT.as_millis()).unwrap_or(u64::MAX);
        if ms == 0 {
            return Err(anyhow!(
                "peer via: a setup timeout of 0ms would refuse every carry before it was \
                 dialled; use `off` to mean that"
            ));
        }
        if ms > ceiling {
            return Err(anyhow!(
                "peer via: {ms}ms is above the {ceiling}ms a carry already waits, and this \
                 knob only lowers it, a carry runs after the direct path has failed, so \
                 waiting longer than the default is worse for the caller than the answer it \
                 already has"
            ));
        }
        self.setup_timeout_ms = Some(ms);
        Ok(self)
    }

    /// How long this setting gives one gateway for a whole carry: the dial, the
    /// handshake and the origin's reply, under one deadline.
    pub fn setup_timeout(&self) -> Duration {
        self.setup_timeout_ms
            .map_or(CARRY_SETUP_TIMEOUT, Duration::from_millis)
    }

    /// Whether this Mac refuses to route out through a peer at all.
    pub fn is_off(&self) -> bool {
        self.route == ViaRoute::Off
    }

    /// The word the CLI prints and the config stores.
    pub fn to_spec(&self) -> String {
        match &self.route {
            ViaRoute::Auto => "auto".to_string(),
            ViaRoute::Off => "off".to_string(),
            ViaRoute::Pinned(peer) => peer.to_wire(),
        }
    }
}

/// One Mac this node could ask to carry, and what is known about it locally.
#[derive(Debug, Clone)]
pub struct GatewayCandidate {
    /// The row, which carries the pinned key and the addresses.
    pub row: PeerRow,
    /// When this node last heard from it, in Unix milliseconds, if ever.
    ///
    /// `None` is "pinned but never seen since this node booted", which is not
    /// a refusal: a Mac that has been asleep answers a dial perfectly well.
    /// It only sorts last.
    pub last_seen_ms: Option<i64>,
}

/// Which Macs to ask, in the order to ask them.
///
/// Freshness used to break ties before latency because latency was a guess.
/// It is measured now: `crate::peer::probe` records a round trip and a loss
/// figure per path on every live session, and
/// [`crate::peer::probe::carrier_key_installed`] is the order those figures
/// put the candidates in. Loss above `paths.maxLossPct` sorts last, measured
/// round trip sorts next, and **a carrier nobody has probed keeps exactly the
/// order it had before**: every unmeasured candidate ties at `u32::MAX` and
/// falls through to most recently seen first, never-seen last, which is the
/// rule this function was written to. The first one that answers is the one
/// that carries.
///
/// `Off` is an empty list and `Pinned` is at most one, so neither can silently
/// fall back to a Mac the operator did not choose. A silent fallback here would
/// be a request leaving by a route the operator refused.
pub fn resolve_via(setting: &ViaSetting, candidates: &[GatewayCandidate]) -> Vec<GatewayCandidate> {
    let mut chosen: Vec<GatewayCandidate> = match &setting.route {
        ViaRoute::Off => return Vec::new(),
        ViaRoute::Pinned(peer) => candidates
            .iter()
            .filter(|candidate| candidate.row.node == *peer)
            .cloned()
            .collect(),
        ViaRoute::Auto => candidates.to_vec(),
    };
    chosen.retain(|candidate| candidate.row.has_endpoint());
    chosen.sort_by_key(|candidate| {
        crate::peer::probe::carrier_key_installed(&candidate.row.node, candidate.last_seen_ms)
    });
    chosen
}

/// Pick a path out: direct when healthy, else a peer that advertises egress,
/// else nothing, and then today's 503/502 ladder fires exactly as it does now.
///
/// **This signature cannot pick a peer, and that is reported rather than worked
/// around**, the same shape `src/peer/serve.rs:599`'s `handle_serve` is in.
/// Picking needs a candidate set, and a function with only a cached belief and
/// a clock has none, so what it can answer is the half it has the inputs for:
/// whether the direct path is still worth trying, which is the question that
/// decides whether a peer is consulted at all. The whole answer is
/// [`resolve_via`] over [`GatewayCandidate`]s, and [`retry_through_peer`] is
/// the caller that has both.
pub fn pick_egress(state: EgressState, now_ms: i64) -> Option<PeerId> {
    if state.direct_is_worth_trying(now_ms) {
        return None;
    }
    None
}

/// Every pinned Mac this node may ask to carry, with what the runtime state
/// knows about when it was last heard from.
///
/// The peers file holds no `last_seen`, it is operator intent, and a timestamp
/// that moved on every beacon would rewrite the operator's file all day, so
/// freshness comes from the runtime state file beside it, which is where
/// `src/peer/state.rs` already keeps it.
///
/// # Only a Mac inside the carry grant is a candidate
///
/// A row without [`crate::peer::config::Allow::carry`] is not asked, and that
/// filter is here rather than in [`resolve_via`] so every caller gets it, a
/// candidate list is the input to the pick, and a Mac that is not a carry
/// relation has no business being in it even for the ordering.
///
/// # The fixed direction
///
/// `allow.gateway` is the OTHER carry grant a row has, and it means "this peer
/// may ask US to carry", the opposite of what this call needs. Reading it here
/// was the second review's MEDIUM at `egress.rs:461`: `via` defaults to `auto`,
/// so granting a peer the right to route OUT through this Mac, one operator
/// act, about that peer's rights over us, was also what made this Mac route
/// its own traffic out through that peer. One act, two directions, and the
/// second one was never asked for.
///
/// [`crate::peer::config::Allow::carry`] ("we may ask this Mac to carry us")
/// is the separate grant that fixes it: this filter reads that field, and the
/// CLI (`tcr peer allow <peer> carry`) is the act that sets it.
pub fn candidates_from(file: &PeerFile, last_seen: &[(PeerId, i64)]) -> Vec<GatewayCandidate> {
    file.peers
        .iter()
        .filter(|row| row.allow.carry)
        .map(|row| GatewayCandidate {
            row: row.clone(),
            last_seen_ms: last_seen
                .iter()
                .find(|(peer, _)| *peer == row.node)
                .map(|(_, at)| *at),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The loopback splice
// ---------------------------------------------------------------------------

/// One in-flight carry: the loopback port to dial, and the task doing the work.
pub struct PeerSplice {
    /// The kernel-chosen loopback port to hand to `ClientBuilder::resolve`.
    /// Returned to exactly one caller and never logged or persisted.
    pub port: u16,
    /// Which Mac is carrying, for the caller's own log line.
    pub gateway: PeerId,
    /// The splice. Dropping this handle does NOT stop it, tokio detaches on
    /// drop, which is what lets a streamed response outlive the function that
    /// started it.
    task: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for PeerSplice {
    /// The port is deliberately not printed. The module docs call "never
    /// logged" a security property of this socket, and a `{:?}` in a log line
    /// is how a thing ends up in a file, including through a test's
    /// `expect_err`, which is what made this impl necessary.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerSplice")
            .field("gateway", &self.gateway.display())
            .finish_non_exhaustive()
    }
}

impl PeerSplice {
    /// Stop the carry. Used when the request that asked for it failed before it
    /// ever dialled the loopback port, so the listener and the peer session are
    /// not left behind.
    pub fn abort(self) {
        self.task.abort();
    }
}

/// Stand up one accept-once loopback splice for one outbound request and return
/// the port to hand to `ClientBuilder::resolve`.
///
/// The port is returned to exactly one caller and is never logged or
/// persisted. See the module docs for why both of those are security
/// properties and not hygiene.
///
/// **A signature of `(peer: &PeerId, host, port) -> Result<u16>` will not do**:
/// a peer id cannot be dialled (the addresses and the pinned key are on the
/// row) and this node cannot handshake without
/// its own secret, so those two are parameters now. The `u16` is still what a
/// caller uses; it comes back inside [`PeerSplice`] so the task's failure has
/// somewhere to be reported instead of being detached and forgotten.
pub async fn accept_once_splice(
    gateway: &PeerRow,
    node_secret: &[u8; KEY_BYTES],
    host: &str,
    port: u16,
) -> Result<PeerSplice> {
    if !host_allowed(host, port) {
        return Err(anyhow!(
            "peer egress: {host}:{port} is not an origin this mesh carries, so nothing was \
             dialled (the list is api.anthropic.com and platform.claude.com on 443)"
        ));
    }

    let mut stream = crate::peer::serve::dial_peer(gateway)
        .await
        .ok_or_else(|| {
            anyhow!(
                "peer egress: none of {}'s addresses answered",
                gateway.label
            )
        })?;
    let mut session = noise::dial_handshake(
        &mut stream,
        node_secret,
        Handshake::Return,
        Some(&gateway.node.0),
        None,
    )
    .await
    .context("peer egress: the handshake with the gateway failed")?;

    let header = StreamHeader {
        kind: StreamKind::Tunnel,
        target: Some(TunnelTarget::Origin {
            host: host.to_string(),
            port,
        }),
        via: Vec::new(),
        hops_remaining: 1,
        request_id: crate::peer::lease::random_id()?,
    };
    let bytes = serde_json::to_vec(&header).context("peer egress: the header did not serialize")?;
    noise::send_encrypted(&mut stream, &mut session.transport, &bytes)
        .await
        .context("peer egress: the gateway did not take the stream header")?;

    // Port 0, loopback, and the listener is dropped the moment its one
    // connection is accepted. Both halves of that sentence are security
    // properties; see the module docs.
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .context("peer egress: could not open a loopback splice")?;
    let local = listener
        .local_addr()
        .context("peer egress: the loopback splice has no address")?;
    let gateway_id = gateway.node;
    let label = gateway.label.clone();
    let host_for_log = host.to_string();

    let task = tokio::spawn(async move {
        let accepted = tokio::time::timeout(CARRY_SETUP_TIMEOUT, listener.accept()).await;
        // Accept-once: whatever happened, this port stops existing here.
        drop(listener);
        let local_stream = match accepted {
            Ok(Ok((local_stream, _))) => local_stream,
            Ok(Err(err)) => {
                tracing::warn!(
                    gateway = %label,
                    error = %err,
                    "peer egress: the loopback splice could not accept its one connection"
                );
                return;
            }
            Err(_) => {
                tracing::warn!(
                    gateway = %label,
                    "peer egress: nothing dialled the loopback splice inside the setup \
                     timeout; the carry was abandoned"
                );
                return;
            }
        };
        let peer_side = NoiseStream::start(stream, session);
        let started = std::time::Instant::now();
        match tunnel::splice(local_stream, peer_side).await {
            Ok((up, down)) => tracing::info!(
                gateway = %label,
                host = %host_for_log,
                bytes_up = up,
                bytes_down = down,
                ms = started.elapsed().as_millis(),
                "peer egress: carried"
            ),
            Err(err) => tracing::warn!(
                gateway = %label,
                host = %host_for_log,
                error = %err,
                "peer egress: the carry failed mid-stream"
            ),
        }
    });

    Ok(PeerSplice {
        port: local.port(),
        gateway: gateway_id,
        task,
    })
}

// ---------------------------------------------------------------------------
// The seam
// ---------------------------------------------------------------------------

/// Everything the seam hands over: the request as the direct path had already
/// built it, and where to find the mesh.
///
/// The headers are the ones `build_upstream_headers` produced, which means the
/// client's own `authorization` is already gone and this node's own Bearer is
/// already on. That credential travels inside this node's own TLS to the
/// origin; the gateway holds no key for it. **No credential is on the peer wire
/// at any point on this path**, which is the whole reason a carry needs no
/// lease, no ledger and no terms question.
pub struct CarriedRequest<'a> {
    /// The full upstream URL the direct attempt used.
    pub url: &'a str,
    pub method: &'a Method,
    /// The upstream headers, already built and already scrubbed.
    pub headers: HeaderMap,
    /// The body, for the methods that have one.
    pub body: Option<bytes::Bytes>,
    /// Where `tcr-peers.json` is.
    pub peers_path: &'a Path,
}

/// One completed carry: what the client gets, and what the ROTATION MODEL gets.
///
/// Two values rather than one, because the two readers need different bytes out
/// of the same response and the difference is deliberate:
///
/// - [`Self::response`] is the client's, already filtered by
///   [`axum_response_from`], the pooled account's `anthropic-ratelimit-*` and
///   `anthropic-organization-id` are gone, exactly as `build_response` strips
///   them on the direct path;
/// - [`Self::upstream_headers`] is the origin's answer UNTOUCHED, and it exists
///   for `Manager::update_quota`. `src/proxy.rs`'s `build_response` doc says it
///   in one line, "stripping at ingest would blind the rotation logic while
///   looking like it fixed something", so the unstripped map is carried out of
///   here rather than re-read off the filtered response, where the very headers
///   the quota model needs are the ones that are no longer there.
pub struct CarriedResponse {
    /// The response to hand the client, filtered.
    pub response: axum::response::Response,
    /// The origin's own headers, before any filtering.
    pub upstream_headers: HeaderMap,
}

/// Retry one request through a trusted gateway, ONCE, and only when nothing
/// this request sent ever left this box.
///
/// `None` is "this did not happen", and every `None` here is a route not taken
/// rather than an error hidden: no peers file, `via off`, no pinned Mac with an
/// address, no gateway that answered, or a host this mesh does not carry. Each
/// one logs at debug with its reason, and the caller falls through to exactly
/// the answer it would have given before this function existed.
///
/// A gateway that answered and then failed is different: that is a `None` with
/// a warning, because a Mac that accepted a carry and dropped it is a fact the
/// operator wants on the record.
///
/// # It is no longer the only way a request leaves through a peer
///
/// It was, and the sentence above was the whole contract. Since the exit lock
/// of decisions row 15 there is a second entry point, [`pinned_egress`], and
/// the two never overlap: this one runs AFTER the direct path failed, on the
/// machine's `via` setting; that one runs BEFORE the direct path is tried, on
/// one account's pin, and an account that is pinned has already been answered
/// by the time this function's hunk could be reached. Both take a carry the
/// same way, through [`carry_once`]. What is written above is still true of
/// this function and is no longer true of the module.
pub async fn retry_through_peer(request: CarriedRequest<'_>) -> Option<CarriedResponse> {
    let url = reqwest::Url::parse(request.url).ok()?;
    let host = url.host_str()?.to_string();
    let port = url.port_or_known_default().unwrap_or(PEER_EGRESS_PORT);
    if !host_allowed(&host, port) {
        tracing::debug!(
            %host,
            port,
            "peer egress: this upstream is not an origin the mesh carries; no peer was asked"
        );
        return None;
    }
    if !request.peers_path.exists() {
        return None;
    }

    let file = match crate::peer::config::read_or_default(request.peers_path) {
        Ok(file) => file,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "peer egress: the peers file would not parse, so no peer was asked"
            );
            return None;
        }
    };
    let setting = via_setting(&file);
    // **`via off` costs nothing beyond the one file read that discovered it.**
    // No runtime-state read, no keypair load, no name resolution and above all
    // no dial, so an operator who turned this off does not pay a setup
    // timeout, or any measurable latency at all, on a request that was already
    // failing. The check is here, first, rather than inside `resolve_via`,
    // because `resolve_via` answering "no candidates" happens three reads
    // later.
    if setting.is_off() {
        tracing::debug!(
            "peer egress: `via off`, this Mac never routes out through a peer, so nothing \
             was read or dialled"
        );
        return None;
    }
    let last_seen = last_seen_from_state(request.peers_path);
    let candidates = resolve_via(&setting, &candidates_from(&file, &last_seen));
    if candidates.is_empty() {
        tracing::debug!(
            via = %setting.to_spec(),
            "peer egress: no trusted Mac to ask, so this request keeps today's answer"
        );
        return None;
    }

    let key = match NodeKey::load_or_mint(&node_key_dir(request.peers_path)) {
        Ok(key) => key,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "peer egress: this node has no keypair to open a carry with"
            );
            return None;
        }
    };

    // The operator's own bound when they set one, else the constant. Read once
    // here rather than per candidate, so every Mac in one request's ladder gets
    // the same patience.
    let setup_timeout = setting.setup_timeout();
    for candidate in &candidates {
        match carry_once(
            &candidate.row,
            key.secret_bytes(),
            &request,
            &host,
            port,
            setup_timeout,
        )
        .await
        {
            Ok(carried) => return Some(carried),
            Err(CarryMiss::NotTaken) => continue,
            Err(CarryMiss::Failed) => return None,
        }
    }
    None
}

/// Why one offered carry produced no response.
///
/// Two misses and not one, because the difference between them is the
/// difference between asking the next Mac and stopping: a Mac that never took
/// the carry has told this node nothing about the request, while a Mac that
/// took it and failed may have put the request on the wire. Collapsing the two
/// is how a carry ladder turns into a resend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CarryMiss {
    /// This Mac did not take the carry. A later candidate may.
    NotTaken,
    /// This Mac took the carry and the request failed anyway. Ask nobody else.
    Failed,
}

/// Offer one carry to one Mac, under one deadline.
///
/// Extracted from [`retry_through_peer`]'s loop when [`pinned_egress`] became
/// the second entry point, so the two share one implementation of the deadline,
/// the splice and the abort rather than growing a second copy that drifts. The
/// two callers differ in which Macs they ask and in what a refusal costs, never
/// in how a carry is taken.
///
/// # One deadline for the WHOLE carry, not just for taking it
///
/// The dial, the handshake and the reply are one act from the waiting client's
/// point of view, and only the first two were bounded: a Mac that answered the
/// handshake and then never spoke again held the client's request open with no
/// bound at all, because `send_through` waits on a splice whose far end nobody
/// is driving. So the bound is a deadline computed once and read by both
/// halves, rather than a fresh `timeout` each: two fresh five-second timeouts
/// are a ten-second wait on a request that has ALREADY failed its direct
/// attempt, which is the thing this whole path exists not to do.
///
/// What it bounds on the second half is the response HEAD. `send` resolves when
/// the status line and headers arrive, so a slow or long SSE body is not cut by
/// this: see [`axum_response_from`], which hands the body out as a stream on
/// purpose.
async fn carry_once(
    gateway: &PeerRow,
    node_secret: &[u8; KEY_BYTES],
    request: &CarriedRequest<'_>,
    host: &str,
    port: u16,
    setup_timeout: Duration,
) -> std::result::Result<CarriedResponse, CarryMiss> {
    let deadline = tokio::time::Instant::now() + setup_timeout;
    let splice = match tokio::time::timeout_at(
        deadline,
        accept_once_splice(gateway, node_secret, host, port),
    )
    .await
    {
        Ok(Ok(splice)) => splice,
        Ok(Err(err)) => {
            tracing::debug!(
                gateway = %gateway.label,
                error = %err,
                "peer egress: that Mac did not take the carry; trying the next one"
            );
            return Err(CarryMiss::NotTaken);
        }
        Err(_) => {
            tracing::debug!(
                gateway = %gateway.label,
                timeout_ms = setup_timeout.as_millis(),
                "peer egress: that Mac did not answer inside the setup timeout; trying \
                 the next one"
            );
            return Err(CarryMiss::NotTaken);
        }
    };

    match tokio::time::timeout_at(deadline, send_through(request, host, splice.port)).await {
        Ok(Ok(response)) => {
            tracing::info!(
                gateway = %gateway.label,
                status = response.status().as_u16(),
                "peer egress: this request was carried by a trusted Mac"
            );
            let upstream_headers = response.headers().clone();
            Ok(CarriedResponse {
                response: axum_response_from(response),
                upstream_headers,
            })
        }
        Ok(Err(err)) => {
            splice.abort();
            tracing::warn!(
                gateway = %gateway.label,
                error = %err,
                "peer egress: that Mac accepted the carry and the request still failed"
            );
            Err(CarryMiss::Failed)
        }
        Err(_) => {
            // The splice is aborted rather than dropped: `PeerSplice`'s own
            // doc says dropping the handle does NOT stop the task, which is
            // what lets a streaming answer outlive this function. A carry
            // that ran out of time has no answer to stream, so leaving it
            // detached would leave a peer session and a granted byte budget
            // open for a client that is already being answered otherwise.
            splice.abort();
            tracing::warn!(
                gateway = %gateway.label,
                timeout_ms = setup_timeout.as_millis(),
                "peer egress: that Mac took the carry and never answered inside the \
                 carry deadline"
            );
            Err(CarryMiss::Failed)
        }
    }
}

// ---------------------------------------------------------------------------
// The exit lock: one account, one way out, every request
// ---------------------------------------------------------------------------

/// How long a client is told to wait after a pinned account's Mac was not
/// there.
///
/// The peer is a Mac on a desk, so the thing that fixes this is someone waking
/// it or a network coming back, not a retry loop. Five seconds is the same
/// order as the carry's own setup bound, and it is advice, not a promise.
pub const PINNED_RETRY_AFTER_SECS: i64 = 5;

/// Why a pinned account could not leave the way it was pinned to.
///
/// A typed reason rather than a sentence, because the caller's answer depends
/// on which of these it is: the first three are the operator's own file
/// disagreeing with itself and are fixed by editing it, the last two are a Mac
/// that is asleep or off the network and are fixed by waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinnedPeerGap {
    /// This node's peers file holds no row for that Mac at all.
    NotPinned,
    /// The row is there and was never granted the carry
    /// ([`crate::peer::config::Allow::carry`], `tcr peer allow <peer> carry`).
    NoCarryGrant,
    /// The row is there with no address to dial.
    NoAddress,
    /// It was dialled and did not take the carry.
    DidNotAnswer,
    /// It took the carry and the request failed anyway.
    CarryFailed,
}

impl PinnedPeerGap {
    /// Whether the request may already have crossed to the gateway when this
    /// gap was found.
    ///
    /// **Only [`Self::CarryFailed`].** The first three are decided off this
    /// Mac's own peers file before a socket exists, and `DidNotAnswer` is a
    /// Mac that never took the carry. `CarryFailed` is the one that means the
    /// gateway took the request: it covers a splice that died mid-request and
    /// a response head that never arrived inside the deadline, and in neither
    /// case can this Mac tell whether the origin ran the request.
    ///
    /// A `true` here is what stops the direct path being taken afterwards: a
    /// non-strict pin falling back to the direct path would send the same POST
    /// a second time and pay for it twice.
    pub fn request_may_have_left(self) -> bool {
        matches!(self, Self::CarryFailed)
    }

    /// The half-sentence that says what the operator would have to change.
    pub fn reason(self) -> &'static str {
        match self {
            Self::NotPinned => "this Mac has no pinned row for it",
            Self::NoCarryGrant => {
                "its row was never granted the carry (`tcr peer allow <peer> carry`)"
            }
            Self::NoAddress => "its row carries no address to dial",
            Self::DidNotAnswer => "it did not answer",
            Self::CarryFailed => "it took the carry and the request failed anyway",
        }
    }
}

/// A request that could not leave by the route its account is locked to, on an
/// account whose operator said that is a refusal.
///
/// Its own type rather than a formatted string, so the request path refuses BY
/// NAME and a test asserts on the variant instead of on wording. Every arm
/// names the account, because an operator reading one line in a busy log needs
/// to know which row to open.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PinnedEgressError {
    /// The pinned Mac is not carrying this request, for one of five reasons.
    #[error(
        "account `{account}` is locked to leave from {peer}, and {reason}. The request was \
         NOT sent: `egressStrict` is on for this account, so leaving by another address is \
         refused rather than done quietly."
    )]
    PeerUnreachable {
        /// The account, as `tcr status` names it.
        account: String,
        /// The pinned Mac, in its short display form.
        peer: String,
        /// What is missing, from [`PinnedPeerGap::reason`].
        reason: &'static str,
        /// Whether the request may already have left the box when this gap was
        /// found, from [`PinnedPeerGap::request_may_have_left`]. Not in the
        /// message: it does not change what the operator would fix, it changes
        /// whether the caller may send the request again.
        delivered: bool,
    },
    /// The upstream this request is for is not one the mesh carries at all, so
    /// no pin can be honoured for it.
    #[error(
        "account `{account}` is locked to leave from {peer}, and `{host}` is not an origin \
         this mesh carries, so the pin cannot be honoured. The request was NOT sent: \
         `egressStrict` is on for this account."
    )]
    HostNotCarried {
        /// The account, as `tcr status` names it.
        account: String,
        /// The pinned Mac, in its short display form.
        peer: String,
        /// The upstream host that is not carried.
        host: String,
    },
    /// This node has no keypair, so it cannot open a carry with anyone.
    #[error(
        "account `{account}` is locked to leave from {peer}, and this Mac has no keypair to \
         open a carry with. The request was NOT sent: `egressStrict` is on for this account."
    )]
    NoNodeKey {
        /// The account, as `tcr status` names it.
        account: String,
        /// The pinned Mac, in its short display form.
        peer: String,
    },
}

impl PinnedEgressError {
    /// Whether the request may already have left this box when this error was
    /// produced.
    ///
    /// The whole of [`PinnedPeerGap::request_may_have_left`], carried on the
    /// error so the caller does not have to re-derive it from a sentence. The
    /// other two variants are decided before a socket exists, so they are
    /// `false` by construction rather than by omission.
    pub fn request_may_have_left(&self) -> bool {
        match self {
            Self::PeerUnreachable { delivered, .. } => *delivered,
            Self::HostNotCarried { .. } | Self::NoNodeKey { .. } => false,
        }
    }

    /// The refusal as the client reads it.
    ///
    /// A 503 with a `retry-after`, the shape this proxy already answers with
    /// when a request cannot be served for a reason that is nobody's fault
    /// upstream, plus `x-should-retry` so a client that knows the header does
    /// the waiting. The envelope is `proxy.rs`'s `error_response` spelled out
    /// rather than called: that function is private to `proxy.rs` and sits in
    /// the half of the file this change does not touch. Making it `pub(crate)`
    /// there would let this call it instead.
    pub fn into_response(self) -> axum::response::Response {
        // THE DELIVERED CASE IS A DIFFERENT ANSWER, and it has to be: this
        // type's own `Display` says "The request was NOT sent", which is the
        // one thing nobody can promise once the gateway took the carry. The
        // status goes with it. A 503 with `retry-after` and
        // `x-should-retry: true` tells a client to send the same POST again,
        // and it may already have been served.
        if self.request_may_have_left() {
            return delivered_unknown_response(&self.to_string());
        }
        let message = self.to_string();
        tracing::warn!(
            status = 503,
            error.r#type = "proxy_error",
            error.message = %message,
            "account egress: a locked account refused rather than leaving by another address"
        );
        let payload = serde_json::json!({
            "type": "error",
            "error": { "type": "proxy_error", "message": message },
        });
        let mut response =
            axum::response::Response::new(axum::body::Body::from(payload.to_string()));
        *response.status_mut() = axum::http::StatusCode::SERVICE_UNAVAILABLE;
        let headers = response.headers_mut();
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/json"),
        );
        headers.insert(
            "x-should-retry",
            axum::http::HeaderValue::from_static("true"),
        );
        if let Ok(value) = axum::http::HeaderValue::from_str(&PINNED_RETRY_AFTER_SECS.to_string()) {
            headers.insert("retry-after", value);
        }
        response
    }
}

/// The answer for a carry that was TAKEN and then failed: the request may have
/// run at the origin, and nothing on this Mac can tell.
///
/// **502 with `x-should-retry: false`**, which is `src/proxy.rs`'s own shape
/// for an attempt that got past connect before failing, for the same reason: a
/// client that retries could pay for the request twice. The gap's own sentence
/// is carried inside so an operator still reads which Mac and which account.
fn delivered_unknown_response(gap: &str) -> axum::response::Response {
    tracing::warn!(
        status = 502,
        error.r#type = "proxy_error",
        error.message = %gap,
        "account egress: the carry was taken and then failed, so the outcome of this \
         request is unknown and it was not sent again"
    );
    let payload = serde_json::json!({
        "type": "error",
        "error": {
            "type": "proxy_error",
            "message": format!(
                "{gap} The carry was taken before it failed, so this request may already \
                 have been served: it was NOT sent again, and retrying may pay for it twice."
            ),
        },
    });
    let mut response = axum::response::Response::new(axum::body::Body::from(payload.to_string()));
    *response.status_mut() = axum::http::StatusCode::BAD_GATEWAY;
    let headers = response.headers_mut();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    headers.insert(
        "x-should-retry",
        axum::http::HeaderValue::from_static("false"),
    );
    response
}

/// What the request path does next, once the exit lock has been consulted.
///
/// Two arms because there are exactly two things the caller can do, and the
/// four cases that reach them are all named inside [`pinned_egress`]: an
/// unpinned account and a non-strict pin that could not be honoured both take
/// the direct path, a carried response and a named refusal are both an answer.
pub enum PinnedEgress {
    /// Take the direct path, exactly as this handler did before the exit lock
    /// existed.
    TakeTheDirectPath,
    /// This request is answered: carried by the pinned Mac, or refused by name.
    Answered(axum::response::Response),
}

/// Everything the exit lock needs about the attempt the direct path was about
/// to make.
///
/// The headers are the ones `build_upstream_headers` produced, so the client's
/// own `authorization` is already gone and this account's own Bearer is already
/// on: a pinned request is the SAME request, sent on this node's credential,
/// through a Mac that cannot read it. The ledger fields below it are the ones
/// [`CarriedRecord`] needs, gathered here so the caller's hunk stays a call and
/// a match.
pub struct PinnedAttempt<'a> {
    /// The fleet this account belongs to.
    pub manager: &'a crate::manager::Manager,
    /// Which account is being served.
    pub account_idx: usize,
    /// The method the direct attempt would have used.
    pub method: &'a Method,
    /// The path and query, appended to the fleet's upstream to make the URL.
    pub path_and_query: &'a str,
    /// The upstream headers, already built and already scrubbed.
    pub headers: HeaderMap,
    /// The request body as it arrived, before the account-uuid patch.
    ///
    /// Pristine rather than patched, because the patch is per ACCOUNT and this
    /// function is the one that knows which account is serving: it applies the
    /// same rewrite the direct attempt applies, from the same pristine bytes,
    /// so a carried request is the same request the origin would have seen.
    pub body: bytes::Bytes,
    /// Where `tcr-peers.json` is.
    pub peers_path: &'a Path,
    /// The client's session key, for the served counter.
    pub session_key: Option<u64>,
    /// What kind of session it was.
    pub session_kind: crate::stats::SessionKind,
    /// The wire-session id, when the request carried one.
    pub wire_session_id: Option<&'a str>,
    /// The model the request asked for.
    pub model: Option<String>,
    /// The tool-use events parsed out of the request body.
    pub tool_uses: &'a [crate::session_wire::ToolUseEvent],
    /// The tool results parsed out of the request body.
    pub tool_results: &'a [crate::session_wire::ToolResultEvent],
}

/// Take the carry for an account that is locked to one exit, BEFORE the direct
/// path is tried at all.
///
/// The sibling of [`retry_through_peer`], and a sibling rather than a mode flag
/// on it, because the two agree on nothing except how a carry is taken
/// ([`carry_once`], which both call):
///
/// | | [`retry_through_peer`] | this |
/// |---|---|---|
/// | when | after the direct path failed with nothing having left the box | before the direct path is tried |
/// | who is asked | the MACHINE's `via` setting, any granted Mac | the ACCOUNT's pin, that Mac or nobody |
/// | what a miss costs | `None`, and the caller keeps today's answer | a named refusal, or the local path plus one log line |
///
/// Folding those into one function would make the first sentence of its doc
/// false for half its callers, which is the thing decisions row 15 asks for the
/// opposite of.
///
/// # It does not touch [`EgressState`] and does not read a transport failure
///
/// Both are deliberate and both are the reason this is a separate hunk in
/// `proxy.rs` rather than a widening of the existing one. [`EgressState`] is
/// this MACHINE's belief about its own path out, and a pinned account that
/// never tried the direct path has learned nothing about it: flipping it
/// `Cold` here would take the whole fleet's direct path out of service for
/// thirty seconds on the strength of one account's pin. `every_attempt_transport_failed`
/// is the other half of the same mistake, a fact about a request that has
/// already been attempted, which this one has not.
///
/// # A pin is honoured on every request, including the ones that would have
/// worked
///
/// The direct path being healthy is not a reason to use it here. The whole
/// point of the lock is that the origin sees ONE address for this account, so
/// an account that leaves through its Mac when the LAN is slow and directly
/// when it is not has no lock at all.
pub async fn pinned_egress(attempt: PinnedAttempt<'_>) -> PinnedEgress {
    // A stale index reads as unpinned: the same case `Manager::access_token`
    // returns `None` for, and the line below this call site handles it one way
    // for every reason. A pin that could not be READ is not a case here at all,
    // because serde refuses it at config load; the operator finds out when the
    // file is read, not one request at a time.
    let Some(pin) = attempt.manager.account_egress(attempt.account_idx) else {
        return PinnedEgress::TakeTheDirectPath;
    };
    let Some(peer) = pin.egress.peer() else {
        // The common case, and it costs one config read: no file, no key, no
        // dial, nothing else on this path runs for an unpinned account.
        return PinnedEgress::TakeTheDirectPath;
    };
    let account = attempt
        .manager
        .account_name(attempt.account_idx)
        .unwrap_or_default();
    let url = format!("{}{}", attempt.manager.upstream(), attempt.path_and_query);

    // **THE PACING GATE, HERE, BECAUSE A CARRIED REQUEST IS A SEND.** The
    // direct path waits on `Manager::throttle_send` before it builds its
    // request; a pinned account left through this function instead and skipped
    // it entirely, so the one account whose traffic the operator most wanted
    // shaped was the one account that never paced. The bucket is the serving
    // account's own, keyed the way the direct path keys it, so a fleet's total
    // rate is what its configuration says whichever way its requests leave.
    attempt
        .manager
        .throttle_send(attempt.path_and_query, attempt.account_idx)
        .await;

    match pinned_carry(&attempt, peer, &account, &url).await {
        Ok(carried) => {
            record_carried(
                attempt.manager,
                CarriedRecord {
                    account_idx: attempt.account_idx,
                    account: Some(account),
                    session_key: attempt.session_key,
                    session_kind: attempt.session_kind,
                    wire_session_id: attempt.wire_session_id,
                    model: attempt.model.clone(),
                    method: attempt.method.to_string(),
                    path: attempt.path_and_query.to_string(),
                    status: carried.response.status().as_u16(),
                    upstream_headers: &carried.upstream_headers,
                    tool_uses: attempt.tool_uses,
                    tool_results: attempt.tool_results,
                },
            );
            PinnedEgress::Answered(carried.response)
        }
        Err(err) if pin.strict => PinnedEgress::Answered(err.into_response()),
        // **A CARRY THAT FAILED AFTER THE GATEWAY TOOK THE REQUEST IS NOT A
        // FALLBACK.** The non-strict arm below exists for a pin that could not
        // be honoured, a Mac that is asleep, has no address or was never
        // granted the carry: nothing left the box, so sending the request
        // directly costs nothing. A carry that was TAKEN and then failed is
        // the other fact: the splice may have put the whole request on the
        // wire, and the five-second bound covers the response head, so a slow
        // origin reads exactly like a dead one. Falling through here sent the
        // same POST a second time and billed the account twice.
        Err(err) if err.request_may_have_left() => {
            tracing::warn!(
                account = %account,
                peer = %peer.display(),
                error = %err,
                "account egress: the carry was taken and then failed, so this request may \
                 already have been sent; it is NOT sent again from this Mac"
            );
            PinnedEgress::Answered(err.into_response())
        }
        Err(err) => {
            // **Exactly one line, at warn.** The operator asked for an address
            // and is not getting it, which is worth saying once per request and
            // never twice: the strict arm above is the one that refuses, and an
            // account that did not ask to be refused still deserves to know its
            // lock did not hold.
            tracing::warn!(
                account = %account,
                peer = %peer.display(),
                error = %err,
                "account egress: this account is locked to leave from a peer that did not \
                 carry it; sending it from this Mac instead"
            );
            PinnedEgress::TakeTheDirectPath
        }
    }
}

/// The carry itself, split out so [`pinned_egress`] reads as the decision it is
/// and every failure on the way has one named error rather than a `None` the
/// caller has to interpret.
async fn pinned_carry(
    attempt: &PinnedAttempt<'_>,
    peer: PeerId,
    account: &str,
    url: &str,
) -> std::result::Result<CarriedResponse, PinnedEgressError> {
    let unreachable = |gap: PinnedPeerGap| PinnedEgressError::PeerUnreachable {
        account: account.to_string(),
        peer: peer.display(),
        reason: gap.reason(),
        delivered: gap.request_may_have_left(),
    };

    let parsed = reqwest::Url::parse(url).map_err(|_| unreachable(PinnedPeerGap::NotPinned))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| unreachable(PinnedPeerGap::NotPinned))?
        .to_string();
    let port = parsed.port_or_known_default().unwrap_or(PEER_EGRESS_PORT);
    if !host_allowed(&host, port) {
        return Err(PinnedEgressError::HostNotCarried {
            account: account.to_string(),
            peer: peer.display(),
            host,
        });
    }

    // The row is looked up by hand rather than through `resolve_via`, which
    // filters a candidate LIST down by the same two gates and cannot say which
    // of them a single named Mac failed. A pin has exactly one candidate, so
    // ordering is not a question here and precision is: "you pinned a Mac you
    // never granted the carry" and "that Mac has no address" are different
    // edits to the operator's file.
    let file = crate::peer::config::read_or_default(attempt.peers_path)
        .map_err(|_| unreachable(PinnedPeerGap::NotPinned))?;
    let row = file
        .peers
        .iter()
        .find(|row| row.node == peer)
        .ok_or_else(|| unreachable(PinnedPeerGap::NotPinned))?;
    if !row.allow.carry {
        return Err(unreachable(PinnedPeerGap::NoCarryGrant));
    }
    if !row.has_endpoint() {
        return Err(unreachable(PinnedPeerGap::NoAddress));
    }

    let key = NodeKey::load_or_mint(&node_key_dir(attempt.peers_path)).map_err(|_| {
        PinnedEgressError::NoNodeKey {
            account: account.to_string(),
            peer: peer.display(),
        }
    })?;
    // The account-uuid patch, from the pristine bytes, exactly as the direct
    // attempt applies it: rotation changes which account serves, the body names
    // the account, and a carried request that skipped this would be a DIFFERENT
    // request from the one the direct path would have sent. The patch is
    // same-length, so no header changes with it.
    let body = (attempt.method != Method::GET && attempt.method != Method::HEAD).then(|| {
        match attempt.manager.account_uuid(attempt.account_idx) {
            Some(uuid) if uuid.len() == 36 => {
                match crate::account_uuid::patch_account_uuid(&attempt.body, &uuid) {
                    std::borrow::Cow::Owned(patched) => bytes::Bytes::from(patched),
                    std::borrow::Cow::Borrowed(_) => attempt.body.clone(),
                }
            }
            _ => attempt.body.clone(),
        }
    });
    let request = CarriedRequest {
        url,
        method: attempt.method,
        headers: attempt.headers.clone(),
        body,
        peers_path: attempt.peers_path,
    };
    // The MACHINE's `via` timeout, because it is the operator's answer to "how
    // long may a carry take on this LAN", and a pinned account's carry runs on
    // the same LAN. It is the one field of `ViaSetting` a pin reads: which Mac
    // to ask is the account's business, how long to wait is the machine's.
    let setup_timeout = via_setting(&file).setup_timeout();
    match carry_once(
        row,
        key.secret_bytes(),
        &request,
        &host,
        port,
        setup_timeout,
    )
    .await
    {
        Ok(carried) => Ok(carried),
        Err(CarryMiss::NotTaken) => Err(unreachable(PinnedPeerGap::DidNotAnswer)),
        Err(CarryMiss::Failed) => Err(unreachable(PinnedPeerGap::CarryFailed)),
    }
}

// ---------------------------------------------------------------------------
// A carried request is a served request
// ---------------------------------------------------------------------------

/// Everything the ledger needs about a request that went out through a peer.
///
/// The same values the direct path's terminal outcome records
/// (`src/proxy.rs:3169` and the two calls under it), gathered into one struct
/// so the seam's hunk is one call rather than three, and so the recording can
/// be driven by a test without a request having to cross the real internet,
/// which the origin allow-list means a carry otherwise does.
pub struct CarriedRecord<'a> {
    /// The fleet account whose credential the carried request used. Ours, not
    /// the gateway's: a carry borrows a route, never an account.
    pub account_idx: usize,
    /// That account's name, as the ledger prints it.
    pub account: Option<String>,
    /// The client's session key, for the served counter.
    pub session_key: Option<u64>,
    /// What kind of session it was.
    pub session_kind: crate::stats::SessionKind,
    /// The wire-session id, when the request carried one.
    pub wire_session_id: Option<&'a str>,
    /// The model the request asked for.
    pub model: Option<String>,
    /// The request's method and path-with-query, as the ring buffer shows
    /// them.
    pub method: String,
    /// The path and query.
    pub path: String,
    /// The status the carried response came back with.
    pub status: u16,
    /// The carried response's headers as the ORIGIN sent them, unfiltered.
    ///
    /// The same map `src/proxy.rs:2949` hands `Manager::update_quota` on the
    /// direct path, and it has to be the unfiltered one: the account-scoped
    /// headers this fleet's rotation model is built from are exactly the ones
    /// [`axum_response_from`] strips before the client sees them.
    pub upstream_headers: &'a HeaderMap,
    /// The tool-use and tool-result events parsed out of the request body.
    pub tool_uses: &'a [crate::session_wire::ToolUseEvent],
    /// The tool results parsed out of the request body.
    pub tool_results: &'a [crate::session_wire::ToolResultEvent],
}

/// Record a carried request exactly the way a directly-served one is recorded.
///
/// Four calls, same order as the direct path: the quota fold first (the direct
/// path does it at `src/proxy.rs:2949`, on the response headers, before
/// anything else looks at the response), then the served counter, the
/// wire-session table and the ring buffer `tcr status` and the TUI read. A
/// carry that did not count would make all four under-report this fleet's own
/// traffic, the account was ours and the credential was ours, and only the
/// route out belonged to a peer.
///
/// # The quota fold is what makes a carried spend visible to the mesh
///
/// A carried request spends the pooled account's window exactly as a direct one
/// does, and the origin says so in the same `anthropic-ratelimit-unified-*`
/// headers. Skipping the fold left that spend invisible to
/// [`crate::manager::Manager::lendable_fraction`], the figure this whole
/// feature lends on, so a Mac that had spent its 5-hour window through a peer
/// went on advertising it as lendable.
///
/// `update_quota` is deliberately NOT `#[must_use]` and its bool is not read
/// here: this call site wants the fold, and "did this response carry the 5h
/// window" is a question only keep-warm asks.
///
/// **Token usage is not recorded here, and that is structural rather than an
/// omission.** A carried response streams straight through to the client
/// ([`axum_response_from`] hands it out as `Body::from_stream`, which is what
/// keeps an SSE answer an SSE), so on this path there is no buffered body and
/// no SSE tee to parse a `usage` block out of. `record_served` still counts the
/// request; the token totals for a carried request are the gap.
/// Arm, for a CARRIED response, the hold the direct path's own status ladder
/// would have armed.
///
/// # Why a recording and not the ladder itself
///
/// `PinnedEgress::Answered` returns the carried response to the client from
/// ABOVE the rotation loop, so the 401/403/429/529 arms never see it: a pinned
/// account that was rejected went on being selected, and the next request took
/// the same rejection. The ladder cannot simply be reused, half of it is
/// `continue` and `tried.insert` in a loop this path is not inside, and
/// re-entering that loop would rotate a request whose account is the whole
/// point of the pin.
///
/// So the half that is a FACT ABOUT THE ACCOUNT is recorded here, and the half
/// that is about THIS REQUEST's routing is not. A 429's hold is the first;
/// rotating, waiting inline and re-picking are the second, and none of them
/// can apply to a response already on its way to the client.
///
/// A 401, a 403 and a 529 arm no hold on the direct path either, so nothing is
/// recorded for them beyond a line saying a carried response carried that
/// status: the direct path answers those by rotating, which is exactly the
/// half that does not exist here.
fn record_the_hold_the_ladder_would(
    manager: &crate::manager::Manager,
    idx: usize,
    status: u16,
    headers: &HeaderMap,
) {
    if status != 429 {
        if matches!(status, 401 | 403 | 529) {
            tracing::warn!(
                status,
                account_index = idx,
                "account egress: a carried response came back with a status the direct \
                 path answers by rotating; a pinned account has nowhere to rotate to, so \
                 the status is passed to the client as it is"
            );
        }
        return;
    }
    let rejections = crate::quota::quota_rejections(headers);
    let retry_after = crate::proxy::parse_retry_after(headers).unwrap_or(60);
    if rejections.is_empty() {
        // No unified rejection: a transient limit. The direct path may wait
        // inline and retry the SAME account here; this one cannot, the answer
        // is already going back, so what is left of that arm is the park.
        manager.mark_rate_limited(idx, retry_after.clamp(1, 300));
        tracing::info!(
            account_index = idx,
            retry_after,
            "account egress: a carried request was rate limited, so the account is held \
             for what the origin asked"
        );
        return;
    }
    if rejections.contains(&crate::quota::UnifiedRejectionKind::FableWeekly) {
        manager.mark_model_weekly_rejected(idx, headers);
    }
    if rejections
        .iter()
        .any(|kind| !matches!(kind, crate::quota::UnifiedRejectionKind::FableWeekly))
    {
        manager.mark_rate_limited(
            idx,
            crate::proxy::jittered_quota_hold(
                retry_after,
                time::OffsetDateTime::now_utc().nanosecond(),
            ),
        );
    }
    tracing::info!(
        account_index = idx,
        retry_after,
        rejections = ?rejections,
        "account egress: a carried request was rejected on quota, so the account is held \
         exactly as a directly-served rejection holds it"
    );
}

pub fn record_carried(manager: &crate::manager::Manager, record: CarriedRecord<'_>) {
    let served_at = time::OffsetDateTime::now_utc();
    manager.update_quota(record.account_idx, record.upstream_headers);
    record_the_hold_the_ladder_would(
        manager,
        record.account_idx,
        record.status,
        record.upstream_headers,
    );
    manager.record_served(
        record.account_idx,
        served_at,
        record.session_key,
        record.session_kind,
    );
    manager.record_wire_session(
        record.wire_session_id,
        record.account.clone(),
        record.model,
        served_at,
        record.tool_uses,
        record.tool_results,
    );
    manager.push_log(crate::stats::RequestLogEntry {
        time: served_at,
        method: record.method,
        path: record.path,
        status: record.status,
        account: record.account.unwrap_or_default(),
    });
}

/// Send the request through the loopback splice.
///
/// `resolve` overrides DNS for this one origin and nothing else, so rustls
/// still validates the real hostname against the real certificate: the gateway
/// is a router, not a party to the TLS session. `.no_proxy()` for the reason
/// every other client in this tree has it, we ARE the proxy.
async fn send_through(
    request: &CarriedRequest<'_>,
    host: &str,
    splice_port: u16,
) -> Result<reqwest::Response> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .resolve(host, SocketAddr::from(([127, 0, 0, 1], splice_port)))
        .build()
        .context("peer egress: could not build the carried client")?;
    let mut builder = client
        .request(request.method.clone(), request.url)
        .headers(request.headers.clone());
    if let Some(body) = &request.body {
        builder = builder.body(body.clone());
    }
    builder
        .send()
        .await
        .context("peer egress: the carried request failed")
}

/// Hand the carried response straight to the client, body still streaming.
///
/// `Body::from_stream` rather than `bytes().await`: this path serves
/// `/v1/messages`, which is server-sent events, and buffering an SSE response
/// would turn a streaming answer into one silent minute followed by everything
/// at once. The splice task outlives this function on purpose, that is what
/// the stream is reading through.
///
/// # The filter is the proxy's own, both halves of it
///
/// A carried response is served to the same local client, off the same pooled
/// account, as a directly-served one, so what that client may see is decided by
/// `src/proxy.rs`'s `build_response` and its two predicates, not by a list
/// kept here. This function used to hold a private eight-name hop-by-hop list,
/// which was two names short of [`crate::proxy::is_response_skip`]
/// (`content-length`, `content-encoding`) and had no equivalent of
/// [`crate::proxy::is_account_scoped`] at all: every carried response handed
/// the caller the pooled account's `anthropic-ratelimit-*` window and its
/// `anthropic-organization-id`, the exact headers the direct path strips
/// because rotation means they describe a different account on every request.
///
/// The account strip is unconditional here, which is `ServedBy::PooledAccount`
/// spelled out: a carry only ever runs after rotation has been walked, on a
/// pooled account. No caller's own credential travels this path.
///
/// `pub` so the gate can drive it with a real `reqwest::Response` off a
/// loopback origin: a carry that completes through [`retry_through_peer`]
/// cannot run on this box, because the allow-list means a real one resolves
/// `api.anthropic.com` for real.
pub fn axum_response_from(response: reqwest::Response) -> axum::response::Response {
    let mut out = axum::response::Response::builder().status(response.status());
    if let Some(headers) = out.headers_mut() {
        for (name, value) in response.headers() {
            // Connection-specific and framing headers describe the carried
            // connection, not the client's, the same rule, from the same
            // function, the direct path applies.
            if crate::proxy::is_response_skip(name.as_str()) {
                continue;
            }
            // And the serving account's own headers describe an account the
            // caller has never heard of and that changes per request.
            if crate::proxy::is_account_scoped(name.as_str()) {
                continue;
            }
            headers.insert(name.clone(), value.clone());
        }
    }
    match out.body(axum::body::Body::from_stream(response.bytes_stream())) {
        Ok(response) => response,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "peer egress: the carried response would not assemble"
            );
            axum::http::StatusCode::BAD_GATEWAY.into_response()
        }
    }
}

/// Where this process's peers file is, for the one caller that has a request
/// in its hands and no configuration.
///
/// `src/proxy.rs` knows about accounts, not about peers: the lease seam gets
/// its path at boot, inside the provider `src/server.rs` installs
/// (`fallback::install_peer_lease_provider`), and a provider cannot be asked
/// what path it was built from. Rather than widen that trait for a path, this
/// cell holds the same fact the same way, installed once at boot, read per
/// request.
///
/// Unset it falls back to [`crate::peer::config::default_path`], which is
/// correct in production and is why `tcr` carries requests today without the
/// boot path having been changed; `src/server.rs` installs an explicit
/// `--peers` here at boot.
static PEERS_PATH: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// Install the peers file this process serves. `false` when one was already
/// installed, in which case NOTHING changed.
pub fn install_peers_path(path: std::path::PathBuf) -> bool {
    PEERS_PATH.set(path).is_ok()
}

/// The peers file this process serves.
pub fn peers_path() -> std::path::PathBuf {
    PEERS_PATH
        .get()
        .cloned()
        .unwrap_or_else(crate::peer::config::default_path)
}

/// The operator's `via` choice, out of the file they wrote it into.
///
/// One function rather than a field read at four call sites, so the CLI's
/// "unchanged" comparison, the request path and the tests all read the setting
/// the same way. A file with no `via` key at all is [`ViaRoute::Auto`]. See
/// [`ViaSetting`] for why that default is not a disclosure decision.
pub fn via_setting(file: &PeerFile) -> ViaSetting {
    file.via.clone()
}

/// When each pinned Mac was last heard from, out of the runtime state file.
///
/// A missing or unreadable state file is an empty list, not a refusal: it only
/// costs the candidates their ordering.
fn last_seen_from_state(peers_path: &Path) -> Vec<(PeerId, i64)> {
    let state_path = crate::peer::serve::peer_state_path(peers_path);
    match crate::peer::state::load(&state_path, crate::peer::pair::now_ms()) {
        Ok(state) => state.last_seen,
        Err(err) => {
            tracing::debug!(
                error = %err,
                "peer egress: no runtime state to order the gateways by; asking them in file \
                 order"
            );
            Vec::new()
        }
    }
}

/// Where this node's keypair lives, given where its peers file is.
///
/// The same derivation `tcr peer id` uses, so a test pointing `--peers` at a
/// temp directory points this at the same one.
fn node_key_dir(peers_path: &Path) -> std::path::PathBuf {
    peers_path.parent().map_or_else(
        crate::peer::id::default_config_dir,
        std::path::Path::to_path_buf,
    )
}
