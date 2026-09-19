//! What each path to a peer costs: round trip and loss, measured on a live
//! session, and the order those measurements put the endpoints in.
//!
//! # This is a cost meter, not a failure detector
//!
//! [`tcr_peer_wire::Control::Ping`]'s doc comment used to say there is no
//! failure detector anywhere in this design, and that sentence is still true
//! after this module: nothing here declares a peer dead, evicts a row or
//! reschedules work away from a Mac. A peer that does not answer a probe is a
//! row with a last-seen (sleeping is the normal state for a laptop) and the
//! only thing a silent peer changes is the LOSS figure on the path that was
//! tried, which moves that path down the dial order behind a path that answers.
//!
//! # Whose clock
//!
//! [`tcr_peer_wire::Control::Probe`] carries the asker's `sent_ms` and the ack
//! echoes it back untouched, but the round trip is measured by the asker
//! against [`std::time::Instant`], a monotonic clock on one machine. Two Macs'
//! wall clocks differ by whatever NTP last left them, and a subtraction across
//! that difference would read as a negative RTT on one side and a doubled one
//! on the other.
//!
//! # An older peer is not a lossy peer
//!
//! A build that predates these two variants parses a `Probe` as
//! [`tcr_peer_wire::Control::Unknown`] and its listener closes the session. That
//! is not loss and recording it as loss would push a perfectly good direct path
//! behind a forwarded hop for as long as the state file survives. So
//! [`crate::peer::probe::ProbeOutcome::NotSupported`] is its own answer, it
//! records NOTHING, and [`crate::peer::probe::probe_session`] stops probing that
//! peer for the rest of that session.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tcr_peer_wire::{CollapseHint, Control, PeerId};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::peer::config::{Endpoint, EndpointSource, Locator, PeerRow};
use crate::peer::noise::{self, PeerSession};

/// One probe per trusted peer per minute, on a session that is already open.
///
/// A minute rather than a second because the figure this buys is a routing
/// hint with a twenty-sample memory, not a heartbeat: at this rate the EWMA
/// below spans twenty minutes of a session's life, which is the timescale a
/// path actually changes on (a Mac moves desks, a router re-maps, a link
/// congests).
pub const PROBE_INTERVAL: Duration = Duration::from_secs(60);

/// How long one probe waits for its ack before it is recorded as loss.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// How many samples the EWMA remembers.
///
/// Twenty, which is what makes `alpha` below: an exponentially
/// weighted average with `alpha = 2 / (N + 1)` has the same centre of mass as a
/// flat average over the last `N` samples, which is why this constant can be
/// read as "the last twenty probes" without keeping twenty numbers per path.
pub const PATH_SAMPLE_WINDOW: u32 = 20;

/// The default loss ceiling, in percent, above which a path is dialled after
/// the paths that are under it.
pub const DEFAULT_MAX_LOSS_PCT: u8 = 5;

/// The smoothing factor for [`PATH_SAMPLE_WINDOW`].
fn alpha() -> f64 {
    2.0 / (f64::from(PATH_SAMPLE_WINDOW) + 1.0)
}

/// One EWMA step, or the sample itself when there is nothing to average with.
fn ewma(previous: Option<f64>, sample: f64) -> f64 {
    match previous {
        Some(previous) => previous + alpha() * (sample - previous),
        None => sample,
    }
}

// ---------------------------------------------------------------------------
// The operator's policy: the `paths` section of the peers file
// ---------------------------------------------------------------------------

/// Which kind of path the operator wants tried first.
///
/// A typed enum and not a string, so the peers file, the dial order and a
/// future panel row cannot disagree about what `"direct"` meant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PathPolicy {
    /// A socket this Mac opens itself, first. The default, because a forwarded
    /// hop spends a third machine's bytes and needs its consent.
    #[default]
    Direct,
    /// A forwarded hop first. For the Mac whose direct path is the one that
    /// keeps failing (a hotel network, a carrier NAT), where trying it first
    /// buys nothing but a connect timeout per request.
    Via,
}

/// The `paths` section of the peers file ([`crate::peer::config::PeerFile`]).
///
/// Every field has a default that reproduces the behaviour of a build with no
/// `paths` section at all, which is what lets the field arrive without a
/// migration: `Direct` first, five percent, and an empty allow list that allows
/// every forwarder the row already carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PathsConfig {
    /// Which kind of path is tried first.
    pub prefer: PathPolicy,
    /// The loss ceiling in percent. A path measured above it is tried after
    /// every path under it. **After, and never dropped**: a lossy path that is
    /// the only path is still the way home.
    pub max_loss_pct: u8,
    /// The forwarders this Mac is willing to be carried by. **Empty means every
    /// forwarder already on the row**, not "none": the row's own `Via`
    /// endpoints are written by the pinning code, so an empty list here is the
    /// absence of a further restriction rather than a refusal.
    pub via_allow: Vec<PeerId>,
}

impl Default for PathsConfig {
    fn default() -> Self {
        Self {
            prefer: PathPolicy::Direct,
            max_loss_pct: DEFAULT_MAX_LOSS_PCT,
            via_allow: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// What was measured
// ---------------------------------------------------------------------------

/// One path's cost, as the EWMA left it.
///
/// Keyed by peer AND locator, because "how far away is that Mac" is not one
/// number: the same peer reached directly and through a forwarder are two
/// paths with two costs, and collapsing them is exactly how a lossy hop comes
/// to look like a lossy Mac.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PathStat {
    /// The peer at the far end.
    pub peer: PeerId,
    /// The path that was measured.
    #[serde(flatten)]
    pub locator: Locator,
    /// Round trip in milliseconds, smoothed. `None` until an ack arrives: a
    /// path that has only ever timed out has a loss figure and no RTT, and
    /// writing a zero there would make it look like the fastest path on the
    /// row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtt_ms: Option<f64>,
    /// Loss in percent, smoothed over the same window.
    pub loss_pct: f64,
    /// How many probes this row has seen. Kept because an EWMA over one sample
    /// and over twenty read the same and are not worth the same.
    pub samples: u32,
    /// When the last sample landed, Unix milliseconds.
    pub updated_at_ms: i64,
}

impl PathStat {
    /// [`Self::rtt_ms`] as the sort key uses it: a path nobody has measured
    /// sorts after every path that has been.
    fn rtt_key(&self) -> u32 {
        rtt_key(self.rtt_ms)
    }
}

/// A smoothed round trip as a whole number of milliseconds, saturating at both
/// ends.
///
/// One function rather than a cast at each reader: an `as` on a `f64` that is
/// NaN or out of range is a silent zero or a silent `u32::MAX`, and the two
/// readers here, the sort key and the collapse hint, would not have gone
/// wrong the same way.
fn rtt_key(rtt_ms: Option<f64>) -> u32 {
    rtt_ms.map_or(u32::MAX, |rtt| {
        let rounded = rtt.round();
        if !rounded.is_finite() || rounded <= 0.0 {
            0
        } else if rounded >= f64::from(u32::MAX) {
            u32::MAX
        } else {
            // Bounded by the two arms above, so the cast cannot saturate.
            rounded as u32
        }
    })
}

/// The policy and the measurements together: everything the dial order reads.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PathTable {
    /// The operator's `paths` section.
    pub config: PathsConfig,
    /// One row per (peer, locator) this process has probed, or restored from
    /// the state file at boot.
    pub stats: Vec<PathStat>,
}

impl PathTable {
    /// A table with the operator's policy and no measurements yet.
    pub fn new(config: PathsConfig, stats: Vec<PathStat>) -> Self {
        Self { config, stats }
    }

    /// The row for one path, if this process has measured it.
    pub fn stat(&self, peer: &PeerId, locator: &Locator) -> Option<&PathStat> {
        self.stats
            .iter()
            .find(|stat| &stat.peer == peer && &stat.locator == locator)
    }

    /// The best measured round trip to a peer over any path, or `None` when no
    /// path to it has ever been acked.
    pub fn best_rtt_ms(&self, peer: &PeerId) -> Option<f64> {
        self.stats
            .iter()
            .filter(|stat| &stat.peer == peer)
            .filter_map(|stat| stat.rtt_ms)
            .min_by(f64::total_cmp)
    }

    /// Fold one probe's answer into the EWMA for that path.
    ///
    /// Returns whether anything was recorded: [`ProbeOutcome::NotSupported`]
    /// records nothing, for the reason the module docs give.
    pub fn record(
        &mut self,
        peer: PeerId,
        locator: Locator,
        outcome: ProbeOutcome,
        now_ms: i64,
    ) -> bool {
        let (rtt_sample, loss_sample) = match outcome {
            ProbeOutcome::Acked { rtt_ms } => (Some(f64::from(rtt_ms)), 0.0),
            ProbeOutcome::NoAnswer => (None, 100.0),
            ProbeOutcome::NotSupported => return false,
        };
        let existing = self
            .stats
            .iter()
            .position(|stat| stat.peer == peer && stat.locator == locator);
        let index = match existing {
            Some(index) => index,
            None => {
                self.stats.push(PathStat {
                    peer,
                    locator,
                    rtt_ms: None,
                    loss_pct: 0.0,
                    samples: 0,
                    updated_at_ms: now_ms,
                });
                self.stats.len() - 1
            }
        };
        let Some(stat) = self.stats.get_mut(index) else {
            return false;
        };
        if let Some(sample) = rtt_sample {
            stat.rtt_ms = Some(ewma(stat.rtt_ms, sample));
        }
        let previous_loss = if stat.samples == 0 {
            None
        } else {
            Some(stat.loss_pct)
        };
        stat.loss_pct = ewma(previous_loss, loss_sample);
        stat.samples = stat.samples.saturating_add(1);
        stat.updated_at_ms = now_ms;
        true
    }
}

// ---------------------------------------------------------------------------
// The process-local table
// ---------------------------------------------------------------------------

/// The table this process dials against.
///
/// Process-local and never in [`crate::config::Config`], on the same rule the
/// rest of this feature follows: a measurement is not the operator's intent,
/// and the intent half, [`PathsConfig`], is in the peers file where an
/// operator can read it. Installed at boot from that file and the state file,
/// and updated by [`probe_session`] as answers arrive.
static INSTALLED: OnceLock<Mutex<PathTable>> = OnceLock::new();

fn installed() -> &'static Mutex<PathTable> {
    INSTALLED.get_or_init(|| Mutex::new(PathTable::default()))
}

/// Replace the process-local table: at boot, and when the peers file's
/// `paths` section changes.
pub fn install(table: PathTable) -> Result<()> {
    let mut held = installed()
        .lock()
        .map_err(|_| anyhow::anyhow!("peer probe: the path table's lock is poisoned"))?;
    *held = table;
    Ok(())
}

/// Read or update the process-local table.
pub fn with_table<R>(apply: impl FnOnce(&mut PathTable) -> R) -> Result<R> {
    let mut held = installed()
        .lock()
        .map_err(|_| anyhow::anyhow!("peer probe: the path table's lock is poisoned"))?;
    Ok(apply(&mut held))
}

// ---------------------------------------------------------------------------
// The dial order
// ---------------------------------------------------------------------------

/// The order this row's endpoints are tried in, against a table the caller
/// holds.
///
/// **The whole of this node's path policy, as a pure function**, so a test, the
/// dialler and the carry path all read the same order from the same rules.
/// Four keys, in this order and for these reasons:
///
/// 1. **Kind, as the operator asked.** [`PathPolicy::Direct`] puts sockets
///    before forwarded hops, because a hop spends a third machine's bytes;
///    [`PathPolicy::Via`] inverts it for the Mac whose direct path is the one
///    that fails. A hop through a forwarder the operator did not allow sorts
///    behind everything, and is still tried: a path that is the only path is
///    the way home.
/// 2. **Under the loss ceiling.** A path measured lossier than
///    [`PathsConfig::max_loss_pct`] goes behind every path under it. This
///    outranks the round trip on purpose: a fast path that drops one request
///    in three is worse than a slow one that drops none, and it outranks
///    recency because the newest endpoint is the one a moved peer just told us
///    about, which says nothing about whether it works.
/// 2. **What taught us the endpoint**, as [`source_rank`] ranks it. A proven
///    endpoint is tried before a claimed one, and this sits above the
///    measurements rather than below them because the measurements are taken
///    on paths this list chose: an endpoint a stranger's brief put on the row
///    is the weakest thing here, and the newest, and without this key it
///    sorted first on key 4 and was dialled before the address a completed
///    handshake proved.
/// 3. **Under the loss ceiling.** A path measured lossier than
///    [`PathsConfig::max_loss_pct`] goes behind every path under it. This
///    outranks the round trip on purpose: a fast path that drops one request
///    in three is worse than a slow one that drops none, and it outranks
///    recency because the newest endpoint is the one a moved peer just told us
///    about, which says nothing about whether it works.
/// 4. **Round trip**, smoothed. A path nobody has measured sorts after the
///    measured ones within its group, which is why a table with no samples at
///    all, every path `u32::MAX` and every comparison a tie, leaves the row in
///    exactly the order it had before this module existed.
/// 5. **Newest observation first**, the rule [`PeerRow::observe_endpoint`]
///    already writes the row in.
pub fn order_endpoints(row: &PeerRow, table: &PathTable) -> Vec<Endpoint> {
    let mut order = row.endpoints.clone();
    order.sort_by_key(|endpoint| {
        let stat = table.stat(&row.node, &endpoint.locator);
        let lossy =
            u8::from(stat.is_some_and(|stat| stat.loss_pct > f64::from(table.config.max_loss_pct)));
        let rtt = stat.map_or(u32::MAX, PathStat::rtt_key);
        (
            kind_rank(endpoint, &table.config),
            source_rank(endpoint),
            lossy,
            rtt,
            std::cmp::Reverse(endpoint.observed_at_ms),
        )
    });
    order
}

/// Key 2 of [`order_endpoints`]: how much this node's own evidence backs the
/// endpoint, in three bands.
///
/// [`EndpointSource::Paired`] and [`EndpointSource::Hello`] came out of a
/// completed handshake against the pinned static key, so they are the same
/// band: both are places this peer answered. [`EndpointSource::Mapping`] is
/// this node's own port-mapping request and [`EndpointSource::Beacon`] an
/// unauthenticated LAN announcement that matched a pinned instance id: two
/// hints nothing proved, but nothing a remote peer chose either.
/// [`EndpointSource::Brief`] is last and alone: it is a third machine's word
/// about where a fourth one answers, the only band whose contents an attacker
/// picks outright.
fn source_rank(endpoint: &Endpoint) -> u8 {
    match endpoint.source {
        EndpointSource::Paired | EndpointSource::Hello => 0,
        EndpointSource::Mapping | EndpointSource::Beacon => 1,
        EndpointSource::Brief => 2,
    }
}

/// Key 1 of [`order_endpoints`]: which kind of path this is, as the policy
/// ranks it.
fn kind_rank(endpoint: &Endpoint, config: &PathsConfig) -> u8 {
    if let Locator::Via { node } = endpoint.locator {
        if !config.via_allow.is_empty() && !config.via_allow.contains(&node) {
            return 2;
        }
    }
    match (config.prefer, endpoint.is_via()) {
        (PathPolicy::Direct, false) | (PathPolicy::Via, true) => 0,
        (PathPolicy::Direct, true) | (PathPolicy::Via, false) => 1,
    }
}

/// [`order_endpoints`] against the process-local table, which is what
/// [`crate::peer::serve::dial_order`] calls.
///
/// A poisoned lock falls back to the policy defaults and SAYS SO: the dial has
/// to produce an order, the default order is the one every build before this
/// module used, and a silent fallback is the thing this codebase does not do.
pub fn dial_order(row: &PeerRow) -> Vec<Endpoint> {
    match with_table(|table| order_endpoints(row, table)) {
        Ok(order) => order,
        Err(err) => {
            tracing::warn!(
                peer = %row.node.display(),
                error = %err,
                "peer probe: the path table could not be read, so this dial uses the \
                 default order (direct first, newest first) and no measurement"
            );
            order_endpoints(row, &PathTable::default())
        }
    }
}

/// The sort key for one Mac this node could ask to CARRY, as
/// `peer::egress::resolve_via` orders its candidates.
///
/// A key and not a sort, because the candidate type belongs to the carry path
/// and the measurements belong here: exporting the key lets one set of rules
/// order both an endpoint list and a carrier list without either file owning
/// the other's type.
///
/// Three parts, and the third is what makes it safe to adopt: **a carrier
/// nobody has probed keeps exactly the order it has today.** Loss above the
/// ceiling sorts last, measured round trip sorts next, and everything
/// unmeasured ties at `u32::MAX` and falls through to most-recently-seen first,
/// which is the rule `resolve_via` is written to.
pub fn carrier_key(
    peer: &PeerId,
    last_seen_ms: Option<i64>,
    table: &PathTable,
) -> (u8, u32, std::cmp::Reverse<i64>) {
    let best = table
        .stats
        .iter()
        .filter(|stat| &stat.peer == peer)
        .min_by_key(|stat| {
            (
                stat.loss_pct > f64::from(table.config.max_loss_pct),
                stat.rtt_key(),
            )
        });
    let lossy =
        u8::from(best.is_some_and(|stat| stat.loss_pct > f64::from(table.config.max_loss_pct)));
    let rtt = best.map_or(u32::MAX, PathStat::rtt_key);
    (
        lossy,
        rtt,
        std::cmp::Reverse(last_seen_ms.unwrap_or(i64::MIN)),
    )
}

/// [`carrier_key`] against the process-local table, for a caller that holds no
/// table of its own. A poisoned lock answers with the unmeasured key, which is
/// today's freshness-only order, and says so.
pub fn carrier_key_installed(
    peer: &PeerId,
    last_seen_ms: Option<i64>,
) -> (u8, u32, std::cmp::Reverse<i64>) {
    match with_table(|table| carrier_key(peer, last_seen_ms, table)) {
        Ok(key) => key,
        Err(err) => {
            tracing::warn!(
                peer = %peer.display(),
                error = %err,
                "peer probe: the path table could not be read, so this carrier is ordered \
                 by freshness alone"
            );
            carrier_key(peer, last_seen_ms, &PathTable::default())
        }
    }
}

/// The collapse hint for a peer, with the round trip this node actually
/// measured.
///
/// [`CollapseHint::observed_rtt_ms`] is `0` when no path to that peer has been
/// acked, which reads as "no measurement" and is what every build before this
/// module would have sent. A hint grants nothing, identity is re-proven by the
/// handshake against the pinned key, so the figure is advice about which path
/// is worth trying and never an authorization.
pub fn collapse_hint(node: PeerId, addrs: Vec<String>, table: &PathTable) -> CollapseHint {
    let observed_rtt_ms = match table.best_rtt_ms(&node) {
        Some(rtt) => rtt_key(Some(rtt)),
        None => 0,
    };
    CollapseHint {
        node,
        addrs,
        observed_rtt_ms,
    }
}

// ---------------------------------------------------------------------------
// The probe itself
// ---------------------------------------------------------------------------

/// What one probe learned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// The peer answered, this many milliseconds later by this node's own
    /// monotonic clock.
    Acked { rtt_ms: u32 },
    /// Nothing came back inside the timeout. One sample of loss, and nothing
    /// about whether the peer is alive.
    NoAnswer,
    /// The peer's build does not know what a probe is: it answered
    /// [`Control::Unknown`], or its listener closed the session the way an
    /// older build's refusal arm does. Recorded nowhere.
    NotSupported,
}

/// Why [`probe_session`] stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeStop {
    /// Every round asked for was sent.
    Completed,
    /// The peer does not probe, so no further probe is sent on this session.
    PeerDoesNotProbe,
}

/// What one session's probing did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeRun {
    /// How many `Probe` frames were written.
    pub sent: usize,
    /// Why it ended.
    pub stop: ProbeStop,
}

/// A nonce for one probe, from the same CSPRNG the handshake uses.
///
/// Random rather than a counter because a counter restarts at zero on every
/// session and a late ack from the previous one would match the new session's
/// first probe.
fn next_nonce() -> Result<u64> {
    let bytes = noise::random_secret()?;
    let Some(head) = bytes.get(..8) else {
        anyhow::bail!("peer probe: the CSPRNG returned fewer than eight bytes");
    };
    let mut nonce = [0_u8; 8];
    nonce.copy_from_slice(head);
    Ok(u64::from_be_bytes(nonce))
}

/// One probe on a live CONTROL session, measured by this node's clock.
///
/// Reads until the ack with THIS nonce arrives, so a late ack for an earlier
/// probe is skipped rather than counted: an ack that timed out is a sample of
/// loss already recorded, and counting it again as a fast round trip would make
/// a congested path look like the best one on the row.
pub async fn probe_once<S>(
    stream: &mut S,
    session: &mut PeerSession,
    nonce: u64,
    timeout: Duration,
) -> Result<ProbeOutcome>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let bytes = serde_json::to_vec(&Control::Probe {
        nonce,
        sent_ms: crate::now_ms(),
    })
    .context("peer probe: the Probe did not serialize")?;
    let started = Instant::now();
    noise::send_encrypted(stream, &mut session.transport, &bytes).await?;

    loop {
        let Some(remaining) = timeout.checked_sub(started.elapsed()) else {
            return Ok(ProbeOutcome::NoAnswer);
        };
        let read = tokio::time::timeout(
            remaining,
            noise::recv_encrypted(stream, &mut session.transport),
        )
        .await;
        let frame = match read {
            Err(_elapsed) => return Ok(ProbeOutcome::NoAnswer),
            Ok(Err(err)) => {
                // An older peer's listener refuses an unknown control frame by
                // closing the stream, so a read error here is the most common
                // way "this build does not probe" is said. Surfaced at debug
                // with the error, never swallowed, and never recorded as loss.
                tracing::debug!(
                    peer = %session.peer.display(),
                    error = %err,
                    "peer probe: this session closed on a probe; treating the peer as a \
                     build that does not answer probes"
                );
                return Ok(ProbeOutcome::NotSupported);
            }
            Ok(Ok(frame)) => frame,
        };
        let elapsed = started.elapsed().as_millis();
        let message: Control = serde_json::from_slice(&frame)
            .context("peer probe: this frame is not a control message")?;
        match message {
            Control::ProbeAck { nonce: echoed, .. } if echoed == nonce => {
                return Ok(ProbeOutcome::Acked {
                    rtt_ms: u32::try_from(elapsed).unwrap_or(u32::MAX),
                });
            }
            Control::ProbeAck { .. } => {
                tracing::debug!(
                    peer = %session.peer.display(),
                    "peer probe: an ack for an earlier probe arrived after its timeout; \
                     it is not counted as this probe's round trip"
                );
            }
            Control::Unknown => return Ok(ProbeOutcome::NotSupported),
            other => {
                tracing::debug!(
                    peer = %session.peer.display(),
                    frame = ?std::mem::discriminant(&other),
                    "peer probe: a control frame that is not this probe's answer arrived \
                     while waiting for it; still waiting"
                );
            }
        }
    }
}

/// Probe one peer on one session, folding each answer into `table`.
///
/// Stops at the FIRST [`ProbeOutcome::NotSupported`], for the reason the
/// module docs explain: one close is enough to know the
/// far end's build, and asking again costs another session.
pub async fn probe_session<S>(
    stream: &mut S,
    session: &mut PeerSession,
    table: &mut PathTable,
    locator: Locator,
    rounds: usize,
    interval: Duration,
) -> Result<ProbeRun>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut sent = 0_usize;
    for round in 0..rounds {
        if round > 0 && !interval.is_zero() {
            tokio::time::sleep(interval).await;
        }
        let nonce = next_nonce()?;
        let outcome = probe_once(stream, session, nonce, PROBE_TIMEOUT).await?;
        sent += 1;
        if outcome == ProbeOutcome::NotSupported {
            return Ok(ProbeRun {
                sent,
                stop: ProbeStop::PeerDoesNotProbe,
            });
        }
        table.record(session.peer, locator, outcome, crate::now_ms());
    }
    Ok(ProbeRun {
        sent,
        stop: ProbeStop::Completed,
    })
}
