//! The peer listener: a SECOND socket, with its own authorization function.
//!
//! # This socket does not reuse `local_endpoint_gate`, and that is deliberate
//!
//! `local_endpoint_gate` (`src/proxy.rs:1137`) authorizes the LOCAL `/_tcr/`
//! endpoints, and its own doc-comment says the thing that matters here: bind
//! scope is not authorization, because `127.0.0.1` is reachable by every
//! process and every container on this host. Behind it sits the one privileged
//! local mutation that ADDS A LIVE CREDENTIAL, whose doc says the gate plus a
//! content type is its entire authorization.
//!
//! So: a peer frame must never reach it. The peer listener is its own
//! `TcpListener`, bound where `tcr-peers.json` says and absent
//! entirely when it says nothing, and it has its own gate
//! ([`peer_stream_gate`]). `local_endpoint_gate` is not relaxed by one
//! character, and **neither arm of the existing listener is widened**, the
//! serving socket has two origins, an inherited descriptor
//! (`src/server.rs:1133-1143`) and the loopback bind below it, and a peer
//! listener bolted onto either would inherit a gate written for loopback.
//!
//! One implementation of the gate, for the same reason `local_endpoint_gate`'s
//! own doc gives for being one: two copies drift, and the copy that drifts is
//! the one on the route added later.
//!
//! # Speaks Noise, and nothing else
//!
//! The first bytes of every connection are one length-prefixed Noise message 1
//! of exactly the length its pattern fixes ([`crate::peer::noise`] pins both
//! numbers against `snow` in a test, rather than taking them from the spec).
//! Anything else closes the socket **with nothing written**: no banner, no
//! version string, no error frame. A stranger scanning this port learns that
//! something accepted a TCP connection and closed it.
//!
//! An `XX` message 1 is the one shape that is a well-formed Noise message and
//! still gets that silence by default: it has no prior key to check, so
//! answering it would hand this node's static key to whoever asked. It is
//! answered only inside a pairing window the operator opened
//! ([`crate::peer::pair::open_pairing_window`]), and the log a refused
//! connection writes is bounded to one line ([`RefusalLog`]), because a line
//! per connection is a disk-filling primitive handed to anyone who can reach
//! the port.
//!
//! # The policy half is re-read per connection
//!
//! `tcr-peers.json`'s socket half is boot-time and its policy half is
//! hot ([`crate::peer::config`]). This file takes the strictest reading of
//! "hot": the pinned rows and the outstanding invites are read from the file
//! for every connection and re-checked **before every frame**, which is what
//! makes `tcr peer forget` close a live session within one frame instead of at
//! the next handshake.
//!
//! # What this file decides about a SERVE, which is nothing
//!
//! [`serve_stream`] dispatches `StreamKind::Serve` to
//! [`crate::peer::serve::handle_serve_on`] and holds the three things a
//! listener cannot derive ([`LeaseServing`]: the ledger, this node's own proxy
//! base, a reader for its own quota). Every decision about the request, the
//! path refusal, the method, the lease, the debit, which account serves, is in
//! `src/peer/serve.rs`, and this file's only contribution is the gate that
//! already ran above.
//!
//! A build with no [`LeaseServing`] REFUSES a SERVE rather than accepting a
//! stream it cannot answer, which is the same shape every ungranted kind has.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tcr_peer_wire::{
    Caps, Control, Hello, InstanceId, Lendable, NeighborBrief, PeerId, StreamHeader, StreamKind,
    TunnelTarget, MAX_NEIGHBOR_BRIEFS, PROTO_VERSION,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;

use crate::peer::config::{
    self as config, ControlGrants, Endpoint, EndpointSource, NetworkKey, PeerFile, PeerRow,
    PeerStore,
};
use crate::peer::id::NodeKey;
use crate::peer::lease::Ledger;
use crate::peer::noise::{self, Handshake, PeerSession, PinRefusal, KEY_BYTES};
use crate::peer::serve::{self, WindowUtilization};
use crate::peer::state::{knock_address, PeerState};

/// How long a peer has to complete a handshake before the socket is closed.
/// A LAN round trip is milliseconds; this is a bound on a stalled connection,
/// not a latency budget.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// The caps, each one a number from `abuse-resistance.md` with its own gate
// ---------------------------------------------------------------------------

/// How often one address may knock: one per ten seconds, sustained.
///
/// `abuse-resistance.md`'s "knock flood from one address" row.
/// Over the bucket the socket is closed after message 1 with **zero bytes
/// written**, no queue entry and no UI row, so a flooder cannot tell a full
/// bucket from a machine that is simply not listening.
pub const KNOCK_INTERVAL_MS: i64 = 10_000;

/// How many knocks one address may make back to back before the interval binds.
/// Three, so a person pressing Trust, mistyping and pressing it again is never
/// rate-limited, and the fourth in ten seconds is.
pub const KNOCK_BURST: u32 = 3;

/// How many connections that have said NOTHING YET this node will hold at
/// once, across all addresses. `abuse-resistance.md`'s "resource exhaustion
/// before auth" row.
///
/// # What "yet" means here, because getting it wrong caps a real peer
///
/// The slot is held from `accept` to the moment message 1 has been read, and it
/// is released there, not when the handshake completes. That window is exactly
/// the threat the row names: a socket opened and left silent, which costs a
/// stranger one `connect` and costs this node a task and a buffer, bounded here
/// and by [`MESSAGE_1_TIMEOUT`].
///
/// **Holding it for the whole handshake is wrong, and it was measured wrong.**
/// The first version of this cap held the slot until the session authenticated,
/// and it silently limited a PINNED peer to two concurrent streams from one
/// address, so a lender serving a borrower's `max_inflight: 5` refused the
/// third with zero bytes and the borrower read it as "the handshake with the
/// lender failed". One TCP is one Noise session is one stream in this design
/// (`crate::peer::mod`), so concurrent streams from one peer are concurrent
/// connections from one address, and a pre-auth cap of two is a concurrency cap
/// of two. Caught by `borrowed_requests_are_paced_by_the_lenders_own_bucket`
/// (`tests/peer_lease.rs`), which fires three at once.
pub const MAX_UNAUTHENTICATED_SOCKETS: usize = 16;

/// How many of those one address may hold, **before the operator's own grants
/// are added**. Two, so a retry overlapping a stalled attempt still gets in and
/// a single host cannot take all sixteen.
///
/// The number a connection is really judged against is
/// [`unauthenticated_allowance`], which adds what this node has GRANTED. A
/// fixed two is right for a stranger and wrong for a peer the operator told
/// this node to serve five requests at once: three simultaneous `connect`s are
/// three accepted sockets before any of their tasks has read a byte, so a fixed
/// two refuses the third every time.
pub const MAX_UNAUTHENTICATED_PER_ADDRESS: usize = 2;

/// The two caps a connection is judged against: `(total, per address)`.
///
/// # Why these are derived and not constants
///
/// `abuse-resistance.md` gives 16 and 2, and both are right about a STRANGER.
/// Neither can bind a peer the operator configured, and a fixed 2 does: one TCP
/// is one Noise session is one stream (`crate::peer::mod`), so a borrower
/// granted `max_inflight: 5` opens five connections from one address, and every
/// one of them is "unauthenticated" for the microseconds between `accept` and
/// its task reading message 1. Three simultaneous `connect`s therefore collide
/// on a fixed cap of two **whatever the handshake costs**, which is why
/// narrowing the slot to the silent window was necessary and not sufficient.
///
/// So the floor is the doc's number and the LARGEST single grant is added on
/// top of both: with nothing granted this returns exactly `(16, 2)`, and the
/// numbers rise only because an operator asked for concurrency this node then
/// has to be able to accept. A stranger benefits from the raised ceiling too,
/// and that is the honest cost, bounded by the biggest burst one peer may
/// open, and a grant is the operator's act.
///
/// **The total was the SUM of every grant, and that was the review's L3**: a
/// node lending to six peers at five each carried a 46-socket pre-auth surface
/// instead of 16, so the ceiling an unrelated stranger met rose with grants
/// that had nothing to do with it. The largest grant is what the headroom is
/// for, which is what the per-address number always used.
///
/// Measured, not reasoned: with the fixed pair,
/// `borrowed_requests_are_paced_by_the_lenders_own_bucket`
/// (`tests/peer_lease.rs`) fails with the lender closing the third connection
/// with zero bytes and the borrower reporting "the handshake with the lender
/// failed"; it stays green on the largest-grant form.
pub fn unauthenticated_allowance(file: &PeerFile) -> (usize, usize) {
    // **The LARGEST grant, on both numbers**, the review's L3.
    //
    // The total used to be the SUM of every grant's `max_inflight`, so a node
    // lending to six peers at five each carried a 46-socket pre-auth surface
    // instead of 16, and every one of those thirty extra sockets was available
    // to an address with no grant at all: the ceiling rose with grants that had
    // nothing to do with the address connecting.
    //
    // The largest single grant is what the headroom is actually FOR. A slot is
    // held only until message 1 arrives (`serve_connection` drops it there, see
    // `MAX_UNAUTHENTICATED_SOCKETS`), so what has to fit is the burst one peer
    // can open at once, which is bounded by that peer's own cap, and the
    // per-address number has always been `MAX_UNAUTHENTICATED_PER_ADDRESS +
    // largest` for exactly this reason. Two numbers derived one way rather than
    // two.
    //
    // Measured, not reasoned, both before and after:
    // `borrowed_requests_are_paced_by_the_lenders_own_bucket`
    // (`tests/peer_lease.rs`) is the gate, with the bare constants it fails
    // with the lender closing the third connection with zero bytes, and it
    // stays green on the largest-grant form.
    let largest: usize = file
        .peers
        .iter()
        .flat_map(|row| row.lend.iter())
        .map(|grant| usize::from(grant.max_inflight))
        .max()
        .unwrap_or(0);
    (
        MAX_UNAUTHENTICATED_SOCKETS.saturating_add(largest),
        MAX_UNAUTHENTICATED_PER_ADDRESS.saturating_add(largest),
    )
}

/// How long a connection has to deliver message 1. Five seconds: a LAN round
/// trip is milliseconds, and a socket opened and left silent is the cheapest
/// thing a stranger can do.
pub const MESSAGE_1_TIMEOUT: Duration = Duration::from_secs(5);

/// The largest knock frame this node will read. A knock is an instance id, a
/// short name and a version number; 512 bytes is generous for that and far
/// under [`tcr_peer_wire::MAX_FRAME_BYTES`], which is what an unauthenticated
/// sender would otherwise get to allocate.
pub const MAX_KNOCK_FRAME_BYTES: usize = 512;

/// How long the 120-second window an Accept opens really is, in seconds.
///
/// The same number as [`crate::peer::pair::PAIRING_WINDOW_SECS`] and named
/// through it rather than re-typed: two spellings of one deadline is how the
/// CLI and the listener come to disagree about whether a window is open.
pub const ACCEPTED_WINDOW_SECS: i64 = crate::peer::pair::PAIRING_WINDOW_SECS;

/// How many served request ids are remembered, per connection here, and, by
/// [`crate::peer::lease::SERVED_CAPACITY`], per process in the ledger.
///
/// Sized from the design's own figure: the accounting backstop has to outlive a
/// burst rather than a session. The ledger's cache is the one that actually
/// catches a replayed debit ([`RequestDedup`] says why); this figure is named
/// through to it so the two layers cannot disagree about what a burst is.
pub const REQUEST_DEDUP_CAPACITY: usize = 4096;

/// How long a served request id is remembered.
pub const REQUEST_DEDUP_TTL_MS: i64 = 600_000;

/// Bind the peer listener and serve it until shutdown.
///
/// Returns immediately with `Ok(None)` when no listen address is configured,
/// which is the default: a fresh install opens no port. That is the whole of
/// the feature flag, there is no second switch to forget.
///
/// The `Ok(None)` arm is unreachable through this signature, which takes a
/// concrete [`SocketAddr`]: the caller that reads `listen: Option<SocketAddr>`
/// is the one that can answer "not configured". Reported to the lead rather
/// than re-signed here.
pub async fn serve(
    addr: SocketAddr,
    node: &NodeKey,
    store: &PeerStore,
    serving: Option<LeaseServing>,
) -> Result<Option<SocketAddr>> {
    let listener = bind(addr).await?;
    let local = listener
        .local_addr()
        .context("peer listener: the bound socket has no address")?;
    tracing::info!(
        peer_listen = %local,
        peers_file = %store.path().display(),
        "peer listener up (a second socket; the local /_tcr/ gate is untouched)"
    );
    // [`internet_admission`] decides, per connection, off the SOCKET this
    // listener is bound to, not off `peer.internet`, so a globally routable
    // bind is worth one line at boot even with the switch off: this line is
    // the only place an operator who typed `listen: 0.0.0.0:7755` learns that
    // a stranger's knock reaches this process at all.
    if !is_lan_scope(local.ip()) {
        tracing::warn!(
            peer_listen = %local,
            "peer listener: bound to a globally routable address; a knock or a first pairing \
             from off the LAN gets nothing back, only a return visit against a pinned key or an \
             enrolment reaches the pin check"
        );
    }

    // `peer.internet`, through the one function every serving process calls
    // for it ([`crate::peer::reach::start_peer_mapping`]): `server.rs` boots
    // through `bind` + `serve_on_with` rather than through here, so a mapping
    // started inline in this function was a mapping the shipped proxy never
    // started.
    //
    // The guard is held across the serve below so that a shutdown that DROPS
    // this future still takes the router mapping away: see
    // [`crate::peer::reach::MappingGuard`].
    let _mapping =
        crate::peer::reach::start_peer_mapping(store.path(), store.file().internet, local);

    serve_on(listener, node, store, serving).await?;
    Ok(Some(local))
}

/// Bind the peer socket, separately from serving it, so a test can learn the
/// kernel-assigned port before the first connection arrives.
pub async fn bind(addr: SocketAddr) -> Result<TcpListener> {
    TcpListener::bind(addr)
        .await
        .with_context(|| format!("peer listener: could not bind {addr}"))
}

/// Accept connections until the listener fails.
///
/// One task per connection, each holding its own Noise session: one TCP is one
/// Noise session is one stream, and there is no multiplexer anywhere in this
/// design.
pub async fn serve_on(
    listener: TcpListener,
    node: &NodeKey,
    store: &PeerStore,
    serving: Option<LeaseServing>,
) -> Result<()> {
    let context = SessionContext::new(node, store.path(), &crate::peer::state::default_path())
        .with_lease_serving(serving);
    serve_on_with(listener, context).await
}

/// [`serve_on`] with the context handed in, so the pairing-window file is the
/// CALLER's choice.
///
/// `serve_on` resolves that file with [`crate::peer::state::default_path`],
/// which is this machine's real runtime state. A two-process test must not read
/// it, and must not be able to be affected by an operator who happens to have
/// a pairing window open while the suite runs, so the test builds its own
/// [`SessionContext`] over a temp dir and calls this.
pub async fn serve_on_with(listener: TcpListener, context: SessionContext) -> Result<()> {
    // The ledger's charges are written HERE, off the relay path. See
    // `Ledger::dirty`. Started beside the accept loop rather than in
    // `server.rs` so every caller that serves peers gets it, including the
    // two-process tests, and stopped with the loop: the task holds nothing but
    // a clone of the ledger `Arc`, and it cannot be cancelled mid-write
    // because `Ledger::flush` has no await inside it.
    let ledger = context.lease_ledger();
    let flusher = ledger
        .clone()
        .map(|ledger| tokio::spawn(flush_ledger_periodically(ledger)));
    let outcome = accept_peer_connections(listener, context).await;
    if let Some(flusher) = flusher {
        flusher.abort();
    }
    // One last write on the way out, so an accept loop that ends between two
    // ticks does not take the charges of its final requests with it. (A caller
    // that DROPS this future instead, `server.rs`'s shutdown select, never
    // reaches here; the flusher it spawned outlives the drop and keeps writing
    // until the runtime goes down, which is the same bound either way.)
    if let Some(ledger) = ledger {
        let mut held = match ledger.lock() {
            Ok(held) => held,
            Err(poisoned) => poisoned.into_inner(),
        };
        held.flush();
    }
    outcome
}

/// How often the lender writes the charges its relays have recorded.
///
/// Short enough that a crash costs at most this much of the `spent` figure
/// `may_relay` bounds a lease by, long enough that a busy relay path is not
/// paying a locked file round trip per request. The write itself is skipped
/// entirely when nothing moved ([`Ledger::flush`]).
pub const LEDGER_FLUSH_INTERVAL: Duration = Duration::from_secs(2);

/// Write the ledger's pending charges every [`LEDGER_FLUSH_INTERVAL`], for as
/// long as this lender is serving.
///
/// A poisoned ledger lock is recovered rather than ending the flusher: the
/// counters a panicking relay left behind are still the best record of what
/// was spent, and the alternative is a lender that silently stops persisting
/// for the rest of the process's life.
async fn flush_ledger_periodically(ledger: std::sync::Arc<std::sync::Mutex<Ledger>>) {
    let mut ticker = tokio::time::interval(LEDGER_FLUSH_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let mut held = match ledger.lock() {
            Ok(held) => held,
            Err(poisoned) => {
                tracing::warn!(
                    "peer lease: the ledger's lock is poisoned; flushing what it holds anyway"
                );
                poisoned.into_inner()
            }
        };
        if held.flush() {
            tracing::debug!("peer lease: the ledger's charges were written");
        }
    }
}

/// How long the accept loop waits after a transient accept error.
///
/// Long enough that an exhausted descriptor table is not a spin (EMFILE
/// answers immediately, so a bare retry loop would burn a core), short enough
/// that a peer dialling during the pause simply waits in the kernel's backlog.
pub const ACCEPT_RETRY_PAUSE: Duration = Duration::from_millis(100);

/// Whether an `accept` error ends this listener for good, or is about one
/// connection.
///
/// # What ending the loop cost
///
/// [`accept_peer_connections`] used to propagate every accept error. A peer
/// that hung up between the SYN and the accept (`ECONNABORTED`), or a process
/// momentarily out of descriptors (`EMFILE`, which a burst of leases or any
/// other part of the proxy can cause), therefore ended the mesh for the life
/// of the process: `server::supervise` supervises a task that has RETURNED, so
/// nothing starts it again, and the operator sees one warning line and a node
/// that answers nobody until the next restart.
///
/// Fatal is the short list, and it is the one where retrying would spin
/// forever on a socket that will never accept again: the listener's own
/// descriptor is gone or was never a socket.
pub fn accept_error_is_fatal(err: &std::io::Error) -> bool {
    /// `EBADF`, the one errno that says the listening descriptor itself is
    /// gone. Written as the number because this crate links no libc binding.
    const EBADF: i32 = 9;

    if err.raw_os_error() == Some(EBADF) {
        return true;
    }
    matches!(
        err.kind(),
        std::io::ErrorKind::NotConnected | std::io::ErrorKind::InvalidInput
    )
}

/// The accept loop itself. Split out of [`serve_on_with`] so that function has
/// one place to stop the flusher it started, on every exit path.
async fn accept_peer_connections(listener: TcpListener, context: SessionContext) -> Result<()> {
    // Read once, outside the loop: the socket a listener is bound to does not
    // change between accepts, and this is the value [`internet_admission`]
    // gates on for every connection this loop hands off.
    let bind = listener
        .local_addr()
        .context("peer listener: the bound socket has no address")?;
    loop {
        let (stream, from) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(err) => {
                if !accept_error_is_fatal(&err) {
                    // A transient accept error is about ONE connection: the
                    // peer hung up between the SYN and the accept, or this
                    // process is momentarily out of descriptors. Ending the
                    // loop over it takes the whole mesh down until the next
                    // restart, and nothing restarts it (`server::supervise`
                    // supervises the task, and this task has RETURNED).
                    //
                    // The pause is what keeps EMFILE from becoming a spin: an
                    // exhausted descriptor table answers instantly, so a bare
                    // `continue` would burn a core until something else closed
                    // a file.
                    tracing::warn!(
                        error = %err,
                        kind = ?err.kind(),
                        "peer listener: one accept failed and the loop carries on"
                    );
                    tokio::time::sleep(ACCEPT_RETRY_PAUSE).await;
                    continue;
                }
                return Err(anyhow::Error::new(err).context("peer listener: accept failed"));
            }
        };
        let context = context.clone();
        tokio::spawn(async move {
            let addr = knock_address(&from);
            if let Err(failure) = serve_connection(stream, &context, &from, bind).await {
                // Every refusal closes the connection and logs. There is no
                // arm that answers the peer: what this node will say to a
                // stranger is nothing at all.
                //
                // **Both paths are bounded, and by the same type from two
                // instances.** An earlier build logged every AUTHENTICATED failure
                // unconditionally on the reasoning that a pinned peer's stream
                // ending is always worth a line, which is true of the first
                // one and false of the thousandth: a pinned peer in a reconnect
                // loop is a pinned peer filling a disk, and `tcr peer forget`
                // is not the remedy an operator reaches for when the log is
                // what is broken. Two instances rather than one so that a flood
                // of anonymous refusals cannot silence a real peer's first
                // line.
                let mut guard = lock_admission(&context.admission);
                let log = if failure.authenticated {
                    &mut guard.authenticated_refusals
                } else {
                    &mut guard.unauthenticated_refusals
                };
                let admitted = log.admit(&addr, now_ms());
                drop(guard);
                let Some(suppressed) = admitted else {
                    return;
                };
                if failure.authenticated {
                    tracing::warn!(
                        peer_addr = %from,
                        error = %failure.error,
                        suppressed,
                        "peer connection closed"
                    );
                } else {
                    tracing::warn!(
                        peer_addr = %from,
                        error = %failure.error,
                        suppressed,
                        "peer connection refused before it authenticated"
                    );
                }
            }
        });
    }
}

/// Why one connection ended, and whether the peer had authenticated by then.
///
/// The flag is what lets [`serve_on`] bound the log: a refusal before the
/// handshake completed is something any host that can reach the port can
/// produce on demand, and a failure on an authenticated session is a pinned
/// peer's stream ending, which is always worth a line.
struct ConnectionFailure {
    authenticated: bool,
    error: anyhow::Error,
}

impl ConnectionFailure {
    /// A refusal from before the handshake completed.
    fn unauthenticated(error: anyhow::Error) -> Self {
        Self {
            authenticated: false,
            error,
        }
    }

    /// A failure on a session that had already proved who it was.
    fn authenticated(error: anyhow::Error) -> Self {
        Self {
            authenticated: true,
            error,
        }
    }
}

/// How long the listener stays silent, **per address**, after logging one
/// refusal.
///
/// One hour, which is `abuse-resistance.md`'s "log spam" row: "one log line per
/// address per hour for refusals". An earlier version was one line per MINUTE
/// for the whole listener, which is the wrong grain in both directions, a
/// single flooder silenced every other address's first refusal, and an hour of
/// slow scanning from a `/24` still wrote 254 lines a minute.
pub const REFUSAL_LOG_QUIET_MS: i64 = 3_600_000;

/// How many addresses the refusal log remembers. A map a stranger can grow
/// without bound is a memory bug with a security label, which is the same
/// sentence [`RequestDedup`] carries: an address that has not been refused
/// within [`REFUSAL_LOG_QUIET_MS`] is dropped, and above this cap the oldest
/// entry goes so the map cannot outgrow it even inside one hour.
pub const REFUSAL_LOG_ADDRESSES: usize = 1_024;

/// One log line per address per [`REFUSAL_LOG_QUIET_MS`], then silence, with
/// the count of what was silenced folded into the next line that is emitted, so
/// the bound never turns a flood into an absence of evidence.
///
/// Keyed by address, not global: see [`REFUSAL_LOG_QUIET_MS`]. The same type
/// serves the authenticated and the unauthenticated path, from two separate
/// instances, so a flood of anonymous refusals cannot silence a pinned peer's
/// stream ending, and neither one is unbounded.
#[derive(Debug, Default)]
pub struct RefusalLog {
    /// Per address: when a line was last emitted, and how many were silenced
    /// since.
    per_address: HashMap<String, (i64, u64)>,
}

impl RefusalLog {
    /// A log that has emitted nothing yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether this refusal should be logged. `Some(n)` means log it and that
    /// `n` refusals from THIS address were silenced since the previous line;
    /// `None` means stay silent.
    pub fn admit(&mut self, addr: &str, now_ms: i64) -> Option<u64> {
        // THIS address is resolved before anything is pruned. Pruning first was
        // a real defect and a quiet one: the entry for the address being
        // admitted aged out on the very call that was about to emit its line,
        // so the suppressed COUNT was dropped and the line said it stood for
        // nothing, a bound that turns a flood into an absence of evidence,
        // which is the failure this type exists to avoid.
        if let Some((last, suppressed)) = self.per_address.get_mut(addr) {
            if now_ms.saturating_sub(*last) < REFUSAL_LOG_QUIET_MS {
                *suppressed = suppressed.saturating_add(1);
                return None;
            }
            *last = now_ms;
            return Some(std::mem::take(suppressed));
        }
        // A new address, so now is the moment to drop the ones whose quiet hour
        // has passed: they carry no count worth keeping and their only cost is
        // the map entry.
        self.per_address
            .retain(|_, (last, _)| now_ms.saturating_sub(*last) < REFUSAL_LOG_QUIET_MS);
        while self.per_address.len() >= REFUSAL_LOG_ADDRESSES {
            // Bounded even inside one quiet window: drop the address whose last
            // line is oldest, which is the one a new refusal is least likely to
            // be silencing anything useful for.
            let Some(oldest) = self
                .per_address
                .iter()
                .min_by_key(|(_, (last, _))| *last)
                .map(|(addr, _)| addr.clone())
            else {
                break;
            };
            self.per_address.remove(&oldest);
        }
        self.per_address.insert(addr.to_string(), (now_ms, 0));
        Some(0)
    }

    /// How many addresses are remembered right now.
    pub fn len(&self) -> usize {
        self.per_address.len()
    }

    /// Whether nothing is remembered.
    pub fn is_empty(&self) -> bool {
        self.per_address.is_empty()
    }
}

/// A token bucket per source address: [`KNOCK_BURST`] tokens, one refilled
/// every [`KNOCK_INTERVAL_MS`].
///
/// Held as a last-refill instant plus a token count rather than a timer,
/// because a timer per address is a thing a flooder gets to create.
#[derive(Debug, Clone, Copy)]
struct KnockBucket {
    tokens: u32,
    last_ms: i64,
}

/// Everything the accept loop knows that is neither on disk nor in one
/// connection: the per-address knock buckets, the unauthenticated socket
/// counters, and the two refusal logs.
///
/// One instance per listener, shared by every connection task. Process state
/// rather than file state on purpose: a rate limit that survived a restart
/// would be a rate limit an operator could not clear, and a restart already
/// costs more than a knock does.
#[derive(Debug, Default)]
pub struct Admission {
    knocks: HashMap<String, KnockBucket>,
    live_per_address: HashMap<String, usize>,
    live_total: usize,
    unauthenticated_refusals: RefusalLog,
    authenticated_refusals: RefusalLog,
}

impl Admission {
    /// An empty one.
    pub fn new() -> Self {
        Self::default()
    }

    /// Take one knock token for `addr`, or report the bucket empty.
    ///
    /// Refills are computed from elapsed time rather than ticked, so an address
    /// that has been quiet for a minute finds a full bucket and one that is
    /// hammering finds an empty one, with no background task either way.
    pub fn take_knock_token(&mut self, addr: &str, now_ms: i64) -> bool {
        // An address whose bucket is full and idle is indistinguishable from an
        // address that has never knocked, so it is dropped: the map holds only
        // addresses that are currently spending.
        self.knocks.retain(|_, bucket| {
            let refilled = bucket
                .tokens
                .saturating_add(refill_tokens(bucket.last_ms, now_ms));
            refilled < KNOCK_BURST
        });
        let bucket = self.knocks.entry(addr.to_string()).or_insert(KnockBucket {
            tokens: KNOCK_BURST,
            last_ms: now_ms,
        });
        let refill = refill_tokens(bucket.last_ms, now_ms);
        if refill > 0 {
            bucket.tokens = bucket.tokens.saturating_add(refill).min(KNOCK_BURST);
            bucket.last_ms = bucket
                .last_ms
                .saturating_add(i64::from(refill).saturating_mul(KNOCK_INTERVAL_MS));
        }
        if bucket.tokens == 0 {
            return false;
        }
        bucket.tokens -= 1;
        true
    }

    /// How many connections from `addr` have not authenticated yet.
    ///
    /// Read by a gate, which is the only reason it is `pub`: the cap is
    /// enforced by `SocketSlot::acquire` against the same counter, so a test
    /// that inferred the count from timing would be measuring the scheduler.
    pub fn live_unauthenticated(&self, addr: &str) -> usize {
        self.live_per_address.get(addr).copied().unwrap_or(0)
    }

    /// How many connections across all addresses have not authenticated yet.
    pub fn live_unauthenticated_total(&self) -> usize {
        self.live_total
    }

    /// How many knock tokens `addr` has left. For a gate to assert on, and for
    /// the gauge `abuse-resistance.md` asks for instead of a log line.
    pub fn knock_tokens(&self, addr: &str, now_ms: i64) -> u32 {
        match self.knocks.get(addr) {
            None => KNOCK_BURST,
            Some(bucket) => bucket
                .tokens
                .saturating_add(refill_tokens(bucket.last_ms, now_ms))
                .min(KNOCK_BURST),
        }
    }
}

/// How many whole [`KNOCK_INTERVAL_MS`] intervals have passed. Saturating and
/// clamped to `u32`, because `now_ms` comes from a wall clock that can jump.
fn refill_tokens(last_ms: i64, now_ms: i64) -> u32 {
    let elapsed = now_ms.saturating_sub(last_ms).max(0);
    u32::try_from(elapsed / KNOCK_INTERVAL_MS).unwrap_or(u32::MAX)
}

/// One unauthenticated socket's slot in the two counters, released on drop.
///
/// RAII rather than a decrement at the end of the handler: the handler has a
/// dozen early returns (every refusal is one) and a counter that leaks on any
/// of them turns the cap into a permanent lockout after sixteen refused
/// connections, a denial of service built out of the defence against one.
struct SocketSlot {
    admission: Arc<Mutex<Admission>>,
    addr: String,
}

impl SocketSlot {
    /// Take a slot, or report which cap refused it.
    ///
    /// `allowance` is [`unauthenticated_allowance`]'s pair, read from the peers
    /// file the caller already holds. See that function for why neither number
    /// can be a constant.
    fn acquire(
        admission: &Arc<Mutex<Admission>>,
        addr: &str,
        allowance: (usize, usize),
    ) -> Result<Self, SocketRefusal> {
        let (total_allowed, per_address_allowed) = allowance;
        let mut guard = lock_admission(admission);
        if guard.live_total >= total_allowed {
            return Err(SocketRefusal::NodeFull {
                live: guard.live_total,
                allowed: total_allowed,
            });
        }
        let per_address = guard.live_per_address.get(addr).copied().unwrap_or(0);
        if per_address >= per_address_allowed {
            return Err(SocketRefusal::AddressFull {
                live: per_address,
                allowed: per_address_allowed,
            });
        }
        guard.live_total += 1;
        *guard.live_per_address.entry(addr.to_string()).or_insert(0) += 1;
        drop(guard);
        Ok(Self {
            admission: Arc::clone(admission),
            addr: addr.to_string(),
        })
    }
}

impl Drop for SocketSlot {
    fn drop(&mut self) {
        let mut guard = lock_admission(&self.admission);
        guard.live_total = guard.live_total.saturating_sub(1);
        if let Some(count) = guard.live_per_address.get_mut(&self.addr) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                guard.live_per_address.remove(&self.addr);
            }
        }
    }
}

/// Which pre-authentication cap refused a socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SocketRefusal {
    /// The node-wide allowance is spent.
    NodeFull { live: usize, allowed: usize },
    /// This address's allowance is spent.
    AddressFull { live: usize, allowed: usize },
}

impl std::fmt::Display for SocketRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NodeFull { live, allowed } => write!(
                f,
                "peer listener: {live} connections that have not delivered a message 1 are \
                 already open and the allowance is {allowed} \
                 (MAX_UNAUTHENTICATED_SOCKETS plus what this node has granted); closed \
                 with nothing written"
            ),
            Self::AddressFull { live, allowed } => write!(
                f,
                "peer listener: {live} connections from this address have not delivered a \
                 message 1 and the allowance is {allowed} \
                 (MAX_UNAUTHENTICATED_PER_ADDRESS plus the largest max_inflight this node \
                 has granted); closed with nothing written"
            ),
        }
    }
}

/// Take the admission lock, recovering a poisoned one.
///
/// A poisoned mutex means another connection task panicked while holding it.
/// Recovering is the safe side here: the counters may be one off, and the
/// alternative is a listener that refuses every connection for the rest of the
/// process's life over one panic.
fn lock_admission(admission: &Arc<Mutex<Admission>>) -> std::sync::MutexGuard<'_, Admission> {
    match admission.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// What the LENDER's half of a SERVE needs, which this file holds and does not
/// decide.
///
/// Three things, and each one is a thing a listener cannot derive: the ledger
/// that funds a relayed request (process state, persisted by the lender),
/// this node's OWN proxy base (the whole of "own picker, own Bearer, own
/// bucket", see [`crate::peer::serve::handle_serve_on`]), and a reader for
/// this node's own quota so the debit is the measured rise rather than
/// `MIN_DEBIT`.
///
/// `None` on [`SessionContext`] is a build with no lease serving wired, and the
/// dispatch arm then REFUSES a SERVE rather than accepting a stream it cannot
/// answer. That is the same shape as every other ungranted kind: a refusal,
/// never a silent accept.
#[derive(Clone)]
pub struct LeaseServing {
    /// The lender's ledger, shared with whatever else reads it (`tcr peer ls`,
    /// the panel row).
    pub ledger: std::sync::Arc<std::sync::Mutex<Ledger>>,
    /// This node's own proxy base, e.g. `http://127.0.0.1:3456`.
    pub upstream: String,
    /// This node's own quota, read either side of the relay.
    pub utilization: std::sync::Arc<dyn WindowUtilization>,
    /// The owner's manager, for `handoff_bearer`. A hand-mode grant is the one
    /// path on which the lender reads its own access token for another Mac.
    pub manager: std::sync::Arc<crate::manager::Manager>,
}

/// What one connection needs from this node, owned so the task can outlive the
/// borrow of [`NodeKey`] and [`PeerStore`].
#[derive(Clone)]
pub struct SessionContext {
    /// This node's static secret, for `snow`'s builder and nothing else.
    secret: [u8; KEY_BYTES],
    /// This node's own id, which is what check 4 compares `via` against.
    node: PeerId,
    /// Where the policy half is re-read from, per connection and per frame.
    peers_path: PathBuf,
    /// Where the pairing window is read from, per connection: the runtime-state
    /// file `tcr peer pair` writes its deadline into
    /// ([`crate::peer::pair::open_pairing_window`]). A separate path because it
    /// is a separate tier, operator INTENT is the peers file, and a two-minute
    /// deadline is something the process learned.
    state_path: PathBuf,
    /// This node's own per-boot instance id, which is what it ANNOUNCES and
    /// what it knocks under. Held here so the beacon and the knock cannot
    /// disagree about it: one value per process, minted once.
    instance_id: InstanceId,
    /// The process-lifetime caps: the per-address knock buckets, the
    /// unauthenticated socket counters and the two refusal logs. Shared across
    /// every connection task on this listener, which is what makes a cap a cap.
    admission: Arc<Mutex<Admission>>,
    /// The gateway's per-peer per-hour byte ledger for carried TUNNELs, shared
    /// across every connection task on this listener the way [`Self::admission`]
    /// is, which is what makes the cap a cap rather than a per-connection
    /// allowance a peer can reset by reconnecting.
    tunnels: Arc<Mutex<crate::peer::tunnel::TunnelBudget>>,
    /// What the lender's half of a SERVE needs, or `None` for a build that
    /// wired none. See [`LeaseServing`].
    serving: Option<LeaseServing>,
}

impl SessionContext {
    /// Build a context from this node's key, its peers file and its
    /// runtime-state file.
    ///
    /// Serves no lease: SERVE is the one stream kind that needs process state
    /// beyond these three, so it is added with [`Self::with_lease_serving`]
    /// rather than defaulted into existence.
    pub fn new(node: &NodeKey, peers_path: &Path, state_path: &Path) -> Self {
        Self {
            secret: *node.secret_bytes(),
            node: node.id(),
            peers_path: peers_path.to_path_buf(),
            state_path: state_path.to_path_buf(),
            instance_id: crate::peer::id::boot_instance_id(),
            admission: Arc::new(Mutex::new(Admission::new())),
            tunnels: Arc::new(Mutex::new(crate::peer::tunnel::TunnelBudget::new())),
            serving: None,
        }
    }

    /// Wire the lender's half of SERVE onto this context, or leave it unwired
    /// with `None`.
    #[must_use]
    pub fn with_lease_serving(mut self, serving: Option<LeaseServing>) -> Self {
        self.serving = serving;
        self
    }

    /// This node's per-boot instance id, what it announces and knocks under.
    pub fn instance_id(&self) -> InstanceId {
        self.instance_id
    }

    /// The shared caps, so a gate can read a token count or a live-socket
    /// count off the same state the accept loop decides on. Reading a copy
    /// would prove nothing about what the listener did.
    pub fn admission(&self) -> &Arc<Mutex<Admission>> {
        &self.admission
    }

    /// The lender's ledger, or `None` on a build with no lease serving wired.
    ///
    /// For [`flush_ledger_periodically`], which needs the `Arc` and nothing
    /// else out of [`LeaseServing`].
    fn lease_ledger(&self) -> Option<std::sync::Arc<std::sync::Mutex<Ledger>>> {
        self.serving.as_ref().map(|serving| serving.ledger.clone())
    }
}

// ---------------------------------------------------------------------------
// The accept gate `peer.internet` turns on
// ---------------------------------------------------------------------------

/// Whether `addr` is one this node would only ever be reached at from its own
/// network.
///
/// # An allow list, and never a negation
///
/// Written as "these prefixes are LAN" rather than "these prefixes are not the
/// internet" on purpose. A negation has to be complete to be safe, and the
/// address space it would have to enumerate is one the registries keep adding
/// to, so every future assignment would default to LAN. This way a prefix
/// nobody here thought of defaults to the internet, which is the side that
/// costs a refusal rather than an admission.
///
/// The list is `127.0.0.0/8`, `::1`, the three RFC 1918
/// ranges, `100.64.0.0/10` (RFC 6598, carrier-grade NAT, which is what a Mac
/// behind a mobile hotspot sits in), `169.254.0.0/16` (RFC 3927 link-local,
/// the address a Mac gives itself with no DHCP), `fe80::/10` and `fc00::/7`.
///
/// # This is not the switch, and it must not be read as one
///
/// `peer.internet` decides whether this node asks its router for a mapping and
/// dials another Mac's public address. It does not decide whether a stranger's
/// knock is answered: a socket bound to `0.0.0.0` is world-reachable whatever
/// the switch says, so [`internet_admission`] gates on the BIND class instead.
///
/// A LAN with native IPv6 is the case this function cannot decide alone. Two
/// Macs on one home network with an ISP-delegated `/64` see each other at
/// `2001:...` addresses, which this function correctly calls not-LAN: they are
/// globally routable and a stranger can dial them. What makes those two Macs
/// neighbours rather than strangers is that they share the prefix, and that
/// question is asked where the host's own addresses are known, in
/// [`internet_admission`] and [`shares_a_global_v6_prefix`].
pub fn is_lan_scope(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => is_lan_scope_v4(v4),
        // An IPv4 address wearing a v6 shape is the IPv4 question: a socket
        // accepted on a dual-stack listener reports `::ffff:10.0.0.7` for a
        // connection from `10.0.0.7`, and reading that as a global v6 address
        // would refuse the LAN this gate exists to leave alone.
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_lan_scope_v4(v4),
            None => is_lan_scope_v6(v6),
        },
    }
}

/// The IPv4 half of [`is_lan_scope`].
///
/// # Two prefixes this used to count as LAN, and what that admitted
///
/// `100.64.0.0/10` (RFC 6598) is the carrier-grade NAT range, and it is what
/// Tailscale numbers a tailnet out of. Counting it as LAN meant that every
/// node on a tailnet, anywhere in the world, reached the knock path and the
/// operator's pairing queue: [`internet_admission`] tests the SOURCE first and
/// answers a LAN-scope source whatever `bind` is and whatever `peer.internet`
/// says. A tailnet is not this LAN, it is a private path to machines that may
/// be on any network, so the switch has to be able to refuse it.
///
/// `169.254.0.0/16` (RFC 3927) is link-local: an address a host assigns itself
/// when nothing hands it one. It is not routed and it is not evidence of
/// anything the operator configured, and the same source class carries the
/// cloud metadata address this tree refuses elsewhere
/// ([`crate::peer::serve`]'s relay-path check).
///
/// Neither is refused outright by this change: both still reach an `IK` return
/// visit against a pinned key, which is the path a peer that really is on the
/// far side of a tailnet uses. What they no longer reach, on a listener bound
/// wide open, is the knock and the first pairing.
fn is_lan_scope_v4(addr: Ipv4Addr) -> bool {
    let [a, b, _, _] = addr.octets();
    // 127.0.0.0/8, loopback.
    if a == 127 {
        return true;
    }
    // 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16: RFC 1918.
    a == 10 || (a == 172 && (16..32).contains(&b)) || (a == 192 && b == 168)
}

/// The IPv6 half of [`is_lan_scope`].
fn is_lan_scope_v6(addr: Ipv6Addr) -> bool {
    if addr.is_loopback() {
        return true;
    }
    let segments = addr.segments();
    // fe80::/10, link-local.
    if segments[0] & 0xffc0 == 0xfe80 {
        return true;
    }
    // fc00::/7, unique-local (RFC 4193).
    segments[0] & 0xfe00 == 0xfc00
}

/// What the accept gate says about one connection, before a byte is written.
///
/// A typed answer rather than a `bool`, because the two outcomes are not
/// symmetric: [`Self::Refuse`] means the connection is closed with NOTHING
/// written, which is a different act from letting the handshake decide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InternetAdmission {
    /// Carry on to the pattern's own admission, unchanged.
    Answer,
    /// Close with zero bytes written.
    Refuse,
}

/// Whether a connection to `bind`, from `from`, offering `pattern`, is
/// answered at all.
///
/// The rule is: "over the internet only the `IK` handshake against a
/// pinned key is answered, never a knock". The bind scope decides whether
/// that row applies at all: `bind` is the address THIS listener is bound to,
/// and a source outside LAN scope only meets row 14 when the listener itself
/// is reachable from outside the LAN, i.e. when `bind` is not itself LAN
/// scope (`0.0.0.0`, a public interface address, or a routed IPv6 address,
/// never `127.0.0.1` or a private prefix). In that case a knock (`NN`,
/// `NNpsk0`) and a first pairing (`XX`) from off the LAN get nothing back,
/// and `IK` proceeds to the pin check that has always decided it. A stranger
/// on the internet therefore cannot reach the operator's pairing queue at
/// all, and the one thing it can reach refuses every key that is not already
/// pinned.
///
/// # The switch is read for ONE case, and it is the narrow one
///
/// `peer.internet` decides whether THIS node asks its own router for a mapping
/// and dials another Mac's public address
/// ([`crate::peer::reach::spawn_mapping_keeper`], read where `serve` starts
/// it). It does not decide whether a stranger's knock, arriving at a socket
/// this node chose to bind wide open, gets an answer: `listen: 0.0.0.0:7755`
/// with `peer.internet: false` is still a world-reachable socket, and reading
/// the switch as permission answered it anyway. So the bind class carries row
/// 14, and the switch narrows ONE case the bind class alone got wrong.
///
/// That case is a home LAN with ISP-delegated IPv6. Both Macs hold `2001:...`
/// addresses out of one `/64`, both bind `[::]`, and neither address is LAN
/// scope by [`is_lan_scope`] (they are globally routable, and correctly so).
/// Row 14 then refused the FIRST PAIRING and the knock between two Macs
/// sitting on one desk, which is not what row 14 is for and is a regression
/// against the same pair on IPv4.
///
/// The narrowing: with the switch OFF, a source that shares one of this host's
/// own global `/64` prefixes is answered like any LAN neighbour.
/// `own_global_v6` is this host's own globally routable addresses, and sharing
/// a `/64` with one of them means the source sits on the same delegated
/// prefix, which on a home network means the same link. It is not proof of
/// anything (a `/64` may be wider than one link, and nothing here is
/// authentication), which is why it only reopens the pattern's OWN admission,
/// the operator's pairing window and the knock bucket, exactly as on the LAN.
///
/// With the switch ON the operator has asked to be reachable from the
/// internet, the mapping is live, and row 14 applies in full: no knock, no
/// first pairing, whatever prefix the source is in.
///
/// A source inside LAN scope is always answered, whatever `bind` is: nothing
/// about row 14 restricts this node from its own network.
pub fn internet_admission(
    bind: IpAddr,
    from: IpAddr,
    pattern: Handshake,
    internet: bool,
    own_global_v6: &[Ipv6Addr],
) -> InternetAdmission {
    if is_lan_scope(from) || is_lan_scope(bind) {
        return InternetAdmission::Answer;
    }
    match pattern {
        // `IK` and `IKpsk1`: a pinned key, or a join token minted here. Both
        // prove something this node issued before a stream exists.
        Handshake::Return | Handshake::Enrol => InternetAdmission::Answer,
        // `XX` proves a key nobody has pinned yet, and `NN`/`NNpsk0` prove
        // nothing at all.
        Handshake::Pair | Handshake::Knock | Handshake::KnockPsk => {
            if !internet && shares_a_global_v6_prefix(from, own_global_v6) {
                InternetAdmission::Answer
            } else {
                InternetAdmission::Refuse
            }
        }
    }
}

/// Does `from` sit in the same global `/64` as one of this host's own
/// addresses?
///
/// # Why `/64` and not the whole address
///
/// A `/64` is the unit an ISP delegates and the unit SLAAC numbers hosts
/// inside, so "the same `/64` as one of mine" is the closest thing IPv6 has to
/// "the same home network" that a host can answer without asking the router.
/// The host part is the one thing it must NOT compare: every Mac on the link
/// has its own, and a privacy extension changes it by the hour.
///
/// Only global addresses on both sides. A link-local or unique-local source is
/// already LAN scope and never reaches here; an IPv4-mapped source is the IPv4
/// question and is answered before this. An empty `own_global_v6` (a Mac with
/// no global v6 at all, which is the common case measured on this machine)
/// makes this `false` for every source, which leaves row 14 exactly as it was.
pub fn shares_a_global_v6_prefix(from: IpAddr, own_global_v6: &[Ipv6Addr]) -> bool {
    let IpAddr::V6(from) = from else {
        return false;
    };
    if from.to_ipv4_mapped().is_some() {
        return false;
    }
    let prefix = &from.segments()[..4];
    own_global_v6
        .iter()
        .any(|own| own.segments()[..4] == *prefix)
}

/// [`internet_admission`], with this host's own global IPv6 addresses read off
/// the kernel.
///
/// The read costs two route lookups ([`crate::peer::reach::global_v6_addresses`]
/// binds a UDP socket and connects it, which sends nothing), so it is done
/// only when the pure answer would otherwise be a refusal. That is the rare
/// path by construction: every LAN connection, and every `IK` from anywhere,
/// has already been answered by the time this asks.
///
/// Read per connection rather than cached at boot for the reason the peers
/// file is: a Mac that moves between networks gets a new prefix, and a cached
/// one would answer about the network it used to be on.
fn admission_for_this_host(
    bind: IpAddr,
    from: IpAddr,
    pattern: Handshake,
    internet: bool,
) -> InternetAdmission {
    if internet_admission(bind, from, pattern, internet, &[]) == InternetAdmission::Answer {
        return InternetAdmission::Answer;
    }
    internet_admission(
        bind,
        from,
        pattern,
        internet,
        &crate::peer::reach::global_v6_addresses(),
    )
}

/// Handshake, gate, and then serve one stream until it closes.
///
/// # The order, which is the security of this file
///
/// 1. the ban list, read from `peer-state.json`, a blocked address gets zero
///    bytes before this node allocates a buffer, let alone reads a frame;
/// 2. the unauthenticated socket caps ([`SocketSlot`]), so sixteen strangers
///    cannot make this node hold a seventeenth;
/// 3. message 1, under [`MESSAGE_1_TIMEOUT`] and bounded by
///    [`crate::peer::noise::MAX_MESSAGE_1_BYTES`] **before the allocation**;
/// 4. which pattern it is, decided by length in one place
///    ([`Handshake::from_message_1_len`]);
/// 5. the internet gate ([`internet_admission`]), which is the only check
///    here that reads the SOURCE address: a knock or a first pairing from
///    outside LAN scope, arriving on a listener bound to a globally routable
///    address, ends here, whatever `peer.internet` says;
/// 6. the pattern's own admission, meaning the knock bucket for a knock, the
///    operator's accepted window for a first pairing, the pin check for a
///    return visit, an outstanding invite for an enrolment;
/// 7. only then a byte is written.
async fn serve_connection<S>(
    mut stream: S,
    context: &SessionContext,
    from: &SocketAddr,
    bind: SocketAddr,
) -> std::result::Result<(), ConnectionFailure>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let addr = knock_address(from);
    let now = now_ms();

    // ONE store per accepted connection, and the review's L2 is why it is a
    // store rather than two reads: everything after this point that needs the
    // peers file or the runtime state asks this value
    // ([`PeerStore::file`], [`PeerStore::state`]), so an accepted connection
    // opens each file once. It used to open the peers file twice, here, and
    // again in `serve_stream` for the same bytes, which `tests/peer_lease.rs`
    // now counts through `config::peers_file_opens`.
    //
    // Before `SocketSlot::acquire`, because the ban list and the cap the slot
    // is taken against are both read out of these two values.
    let store = PeerStore::open(&context.peers_path)
        .map_err(ConnectionFailure::unauthenticated)?
        .with_state_path(context.state_path.clone());
    let file = store.file();
    let peer_state = store
        .state(now)
        .map_err(ConnectionFailure::unauthenticated)?;

    // Check 1. A banned address is refused before anything else happens, which
    // is the cheapest refusal this file has and the one an operator asked for
    // explicitly.
    if peer_state.is_address_banned(&addr) {
        return Err(ConnectionFailure::unauthenticated(anyhow::anyhow!(
            "peer listener: {addr} is blocked (`tcr peer unblock {addr}` lifts it); \
             closed with nothing written"
        )));
    }

    // Check 2, held for the whole of the unauthenticated phase and released on
    // every exit path by `Drop`. See [`SocketSlot`].
    let slot = SocketSlot::acquire(&context.admission, &addr, unauthenticated_allowance(&file))
        .map_err(|refusal| ConnectionFailure::unauthenticated(anyhow::anyhow!("{refusal}")))?;

    // Check 3. A socket opened and left silent costs five seconds, and the
    // length prefix cannot reserve more than the largest message 1 any pattern
    // here has.
    let message_1 = match tokio::time::timeout(
        MESSAGE_1_TIMEOUT,
        noise::read_frame_bounded(&mut stream, noise::MAX_MESSAGE_1_BYTES),
    )
    .await
    {
        Ok(frame) => frame.map_err(ConnectionFailure::unauthenticated)?,
        Err(elapsed) => {
            return Err(ConnectionFailure::unauthenticated(
                anyhow::Error::new(elapsed).context(
                    "peer listener: no message 1 within the pre-authentication timeout; \
                     closed with nothing written",
                ),
            ))
        }
    };

    // The slot is released as soon as message 1 is in hand for every pattern
    // that goes on to AUTHENTICATE: holding it through the handshake would cap
    // a pinned peer's concurrent streams at two. See
    // [`MAX_UNAUTHENTICATED_SOCKETS`] for the measurement.
    //
    // A KNOCK never authenticates, so it keeps its slot to the end (the arm
    // below). Dropping it here left the knock path with no in-flight bound at
    // all: message 1 is cheap to produce, and everything after it is a state
    // file write under a lock, so a LAN host could hold as many knocks in
    // flight as it could open sockets. Bounded now by the same two counters
    // that bound the silent window, and by `MESSAGE_1_TIMEOUT` on the
    // handshake that follows.

    // Check 4, in one place: see [`Handshake::from_message_1_len`].
    let Some(pattern) = Handshake::from_message_1_len(message_1.len()) else {
        return Err(ConnectionFailure::unauthenticated(anyhow::anyhow!(
            "peer listener: {} first bytes are not a Noise message 1 ({} for a knock, {} for \
             a knock under the network key, {} for a first pairing, {} for a return or an \
             enrolment); closing with nothing written",
            message_1.len(),
            noise::KNOCK_MESSAGE_1_LEN,
            noise::KNOCK_PSK_MESSAGE_1_LEN,
            noise::XX_MESSAGE_1_LEN,
            noise::IK_MESSAGE_1_LEN
        )));
    };

    // Check 5, and the only one that reads where the connection came from.
    // Before the knock arm, so a knock from off the LAN never reaches the
    // bucket, the queue or the operator's screen. Gated on `bind`, the socket
    // this listener is bound to, and on `file.internet` for the one case the
    // bind class alone got wrong: two Macs on one home LAN with ISP-delegated
    // IPv6. See [`internet_admission`]'s own doc for both halves.
    if admission_for_this_host(bind.ip(), from.ip(), pattern, file.internet)
        == InternetAdmission::Refuse
    {
        return Err(ConnectionFailure::unauthenticated(anyhow::anyhow!(
            "peer listener: this listener is bound to a globally routable address and {addr} \
             is neither on this LAN nor in one of this Mac's own IPv6 /64 prefixes, so a {} \
             gets nothing back; only a return visit against a pinned key is answered from off \
             the LAN",
            pattern.pattern()
        )));
    }

    // Check 6, the knock arm. Everything about a knock ends here: it never
    // reaches a stream header, a gate or a handler, because the only thing it
    // can produce is a row an operator reads.
    if matches!(pattern, Handshake::Knock | Handshake::KnockPsk) {
        let outcome = serve_knock(&mut stream, context, &addr, pattern, &message_1, &file, now)
            .await
            .map_err(ConnectionFailure::unauthenticated);
        drop(slot);
        return outcome;
    }
    drop(slot);

    let psks = outstanding_secrets(&file);
    let session = match tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        accept_pairing_or_return(
            &mut stream,
            context,
            &addr,
            pattern,
            &message_1,
            &file.peers,
            &psks,
            &peer_state,
            now,
        ),
    )
    .await
    {
        Ok(accepted) => accepted.map_err(ConnectionFailure::unauthenticated)?,
        Err(elapsed) => {
            return Err(ConnectionFailure::unauthenticated(
                anyhow::Error::new(elapsed)
                    .context("peer handshake: timed out before it completed"),
            ))
        }
    };

    if session.session.handshake == Handshake::Pair {
        // The responder's half of the six-digit compare: the operator in front
        // of THIS Mac has to be able to read the digits the dialling Mac is
        // showing, or the compare has one screen and proves nothing.
        tracing::info!(
            code = %session.session.code,
            peer_addr = %from,
            "peer pairing: compare these six digits with the other Mac, then trust it there"
        );
        // A first pairing ENDS here, and it writes no pin. The `XX` handshake
        // has proved a static key to both machines and bound it with the six
        // digits; what turns that into a row is the operator confirming on each
        // side (`tcr peer pair <host:port> <code>`), which is a separate act by
        // design. An earlier build let this fall through into the stream gate,
        // where it was refused for want of the row it had not created yet, a
        // successful pairing that logged a refusal.
        //
        // The learned static key is recorded against the accepted window so
        // that `tcr peer block` can ban the KEY and not only the address: this
        // is the one moment a knock-then-pair sequence has one in hand.
        if let Err(err) = record_pairing_key(context, &addr, session.session.peer, now).await {
            tracing::warn!(
                error = %err,
                peer_addr = %from,
                "peer pairing: could not record the static key this pairing learned; a later \
                 `tcr peer block` will ban the address only"
            );
        }
        return Ok(());
    }

    // BY VALUE, not `&mut`: a TUNNEL spawns a Noise pump that outlives this
    // frame's borrow, so `tunnel::handle_tunnel_on` needs the stream itself
    // (`Send + 'static`). Every other kind still works on a `&mut` taken
    // inside `serve_stream`.
    serve_stream(stream, session, context, &store, *from)
        .await
        .map_err(ConnectionFailure::authenticated)
}

/// [`serve_connection`] for a caller that already has the stream and the
/// address it came from.
///
/// The accept loop is not the only thing that knows a source address, and on
/// this machine it is not the thing that can produce an interesting one:
/// macOS puts only `127.0.0.1`, `::1` and one link-local address on `lo0`, so
/// `10.0.0.7` and `2001:db8::1` cannot be dialled from here at all
/// (`tests/peer_abuse.rs` measured it, and adding an alias needs root on a
/// machine that is also serving a live proxy). This takes the address as the
/// value it already is inside [`serve_connection`], so the refusal a peer off
/// the LAN gets can be observed as BYTES on a real stream rather than inferred
/// from a verdict.
///
/// Loses the authenticated/unauthenticated split the accept loop uses to bound
/// its own logging, which is why the loop still calls [`serve_connection`]
/// directly.
///
/// `bind` is the socket [`internet_admission`] gates on, the same way the
/// accept loop reads it off the real `TcpListener`: a caller here can name
/// `0.0.0.0:7755` without actually binding it, which is how the globally
/// routable case is exercised without opening a socket this box does not own.
pub async fn serve_accepted<S>(
    stream: S,
    context: &SessionContext,
    from: &SocketAddr,
    bind: SocketAddr,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    serve_connection(stream, context, from, bind)
        .await
        .map_err(|failure| failure.error)
}

/// Serve a punched connection, with the recursion stated rather than inferred.
///
/// [`serve_accepted`] leads back to `serve_control`, which is where a punch is
/// spawned, so a punch that serves its own result is an async cycle and the
/// compiler cannot decide by inference whether the future is `Send`. Boxing it
/// as `dyn Future + Send` erases the cycle and asserts the fact, which is the
/// standard shape for recursive async and the only reason this function is not
/// one line at the call site.
fn serve_punched(
    stream: crate::peer::serve::PeerStream,
    context: SessionContext,
    from: SocketAddr,
    bind: SocketAddr,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>> {
    Box::pin(async move { serve_accepted(stream, &context, &from, bind).await })
}

/// Answer one knock: bucket, queue, one ack byte, close.
///
/// Every refusal returns before a byte is written. That is the whole reason the
/// bucket check sits between the handshake and the ack rather than after the
/// knock is read: a rate-limited address must not be able to tell a full bucket
/// from a machine that is not there.
async fn serve_knock<S>(
    stream: &mut S,
    context: &SessionContext,
    addr: &str,
    pattern: Handshake,
    message_1: &[u8],
    file: &PeerFile,
    now_ms: i64,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // The network key decides WHICH knock pattern this node answers, and a
    // mismatch is refused inside message 1, a Mac without the key cannot
    // reach the queue, and a Mac with one cannot knock at a node that has
    // none. Checked here, before `snow` is handed anything, so the refusal
    // names the cause instead of surfacing as a decryption failure.
    match (file.network_key.as_ref(), pattern) {
        (Some(_), Handshake::Knock) => bail!(
            "peer knock: this Mac has a network key set, so a knock must run {} with it; \
             the one that arrived carries no key and gets nothing",
            noise::PATTERN_KNOCK_PSK
        ),
        (None, Handshake::KnockPsk) => bail!(
            "peer knock: this Mac has no network key, so there is nothing to verify a {} \
             knock against; it gets nothing",
            noise::PATTERN_KNOCK_PSK
        ),
        _ => {}
    }

    // The bucket, before the handshake completes: over it, the socket closes
    // after message 1 with zero bytes written, which is what
    // `abuse-resistance.md` asks for verbatim.
    {
        let mut guard = lock_admission(&context.admission);
        if !guard.take_knock_token(addr, now_ms) {
            bail!(
                "peer knock: {addr} is over its rate limit ({} per {}ms, burst {}); closed \
                 after message 1 with nothing written",
                1,
                KNOCK_INTERVAL_MS,
                KNOCK_BURST
            );
        }
    }

    // Banned, muted or over the queue cap: refused here, before `snow` has
    // written a single byte of message 2 back. These three depend only on
    // `addr`, never on the knock's own payload, which is what makes checking
    // them possible before the handshake that reveals that payload even
    // finishes. See `PeerState::refusal_for_knock_source`. Checking this
    // only after `finish_responder` (as before) let a muted or over-cap
    // source earn message 2 on the wire before being refused two reads
    // later; that is no longer possible.
    //
    // The check AND the reservation happen under the same lock
    // `record_knock` re-acquires below, via `reserve_knock_slot`. A read-only
    // check here, even a locked one, would still leave a window between
    // "checked" and "the handshake that spends minutes on the wire" in which
    // another concurrent knock could read the same "there is room" state,
    // so this reserves the row for `addr` right here, not just checks it: a
    // caller that finds no room is refused before message 1 is ever
    // answered, and one that finds room has already claimed its slot, so a
    // sibling connection racing it a moment later sees the reservation and is
    // refused in turn.
    // **MESSAGE 1 IS VALIDATED BEFORE ANYTHING IS RESERVED**, and the ordering
    // defect that forced it: the reservation was taken first,
    // so a stranger sending the right NUMBER of wrong bytes claimed one of the
    // eight slots and made this node write its state file twice (the
    // reservation, then the release) before the cryptography had said a word
    // about the sender.
    //
    // `read_message_1_matching` is pure, it decrypts and matches against the
    // network key and writes NOTHING to the socket, so moving it above the
    // reservation costs nothing and keeps both properties the old order was
    // protecting: the reservation is still taken before message 2 is written
    // (`finish_responder`, below), so a caller that finds no room is refused
    // before this node answers a byte, and a sibling racing it a moment later
    // sees the reservation rather than the same "there is room" snapshot.
    let psk = file.network_key.as_ref().map(NetworkKey::as_bytes);
    let psks: &[[u8; KEY_BYTES]] = match psk {
        Some(key) => std::slice::from_ref(key),
        None => &[],
    };
    let mut scratch = vec![0_u8; noise::HANDSHAKE_SCRATCH_BYTES];
    // A knock has no static key on either side, so the secret handed to the
    // builder is an ephemeral throwaway and never this node's identity, the
    // same reasoning [`crate::peer::noise::send_knock`] gives on the dialling
    // side.
    let throwaway = noise::random_secret()?;
    let read = noise::read_message_1_matching(&throwaway, pattern, psks, message_1, &mut scratch)?;

    // **On a blocking thread, not this runtime's.** `FileLock::acquire` waits
    // for a contended lock with `std::thread::sleep`, up to
    // [`crate::peer::config::LOCK_WAIT_MS`], and the load and the save are
    // synchronous file IO under it. Run inline, on an unauthenticated path any
    // LAN host can reach, a handful of simultaneous knocks park that many of
    // the proxy's worker threads for seconds each, and what stops answering is
    // the LOCAL proxy: the thing the mesh is not allowed to cost anything.
    let reservation_created = on_blocking_thread({
        let state_path = context.state_path.clone();
        let addr = addr.to_string();
        move || {
            let _lock = crate::peer::config::FileLock::acquire(&state_path)?;
            let mut peer_state = crate::peer::state::load(&state_path, now_ms)?;
            let created = match peer_state.reserve_knock_slot(&addr, now_ms) {
                Ok(created) => created,
                Err(refusal) => {
                    bail!("peer knock: {refusal}; closed after message 1 with nothing written")
                }
            };
            crate::peer::state::save(&state_path, &peer_state)?;
            Ok(created)
        }
    })
    .await?;

    // A wrong or missing network key, a malformed message, a dropped
    // connection, any of these must still reach NO row, exactly as before
    // the reservation existed. The reservation above only proves this
    // address is not banned, muted or over the cap; it says nothing about
    // whether the handshake itself will succeed, so a failure here has to
    // release it rather than leave a placeholder for a knock that never
    // actually completed.
    //
    // **And it is BOUNDED**. Message 1 has had a
    // five-second deadline for a long time (`MESSAGE_1_TIMEOUT`) and the two
    // writes after it had none, so a sender that delivered a valid message 1
    // and then went quiet held its reserved slot for as long as the TCP
    // connection lived: one eighth of this node's pairing queue, per stalled
    // connection, with no operator-visible cause. The same deadline, because
    // the remaining work is one message 2 and one small frame on a LAN, which
    // is the identical cost profile message 1 already has.
    let outcome = match tokio::time::timeout(
        MESSAGE_1_TIMEOUT,
        finish_knock_handshake(stream, read, pattern),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(elapsed) => Err(anyhow::Error::new(elapsed).context(
            "peer knock: the knock handshake did not complete within the pre-authentication \
             timeout; the reservation is released",
        )),
    };
    let (mut session, knock) = match outcome {
        Ok(pair) => pair,
        Err(err) => {
            if reservation_created {
                release_reserved_knock_slot(context, addr, now_ms).await;
            }
            return Err(err);
        }
    };

    // The claimed name is attacker-chosen text that lands in a panel row, a
    // `tcr peer pending` line and a log line. A name that fails the whitelist
    // drops the NAME and keeps the row, which is the same rule
    // `crate::peer::discovery::discovered_row` follows: the row is still a
    // machine the operator may want to accept, and it shows its address.
    let proposed_name = knock
        .proposed_name
        .as_deref()
        .and_then(|name| tcr_peer_wire::sanitize_label(name).ok());

    // One read-mutate-write of the state file, under the same lock the peers
    // file uses, because two knocks landing together would otherwise each write
    // a queue built on a stale read, and the eight-row cap would not be a cap.
    //
    // This always coalesces rather than adding a row: the reservation above
    // already holds this address's slot (or, if it coalesced onto an
    // existing pending row instead of creating one, that row already
    // existed). `record_knock`'s own `Ok(bool)` therefore no longer means
    // "is this address new" the way it used to, `reservation_created`
    // above is that signal now.
    //
    // On a blocking thread, for the reason the reservation above gives: the
    // lock waits with `std::thread::sleep` and everything under it is
    // synchronous file IO, on a path no caller has authenticated.
    let pending = on_blocking_thread({
        let state_path = context.state_path.clone();
        let addr = addr.to_string();
        let instance_id = knock.instance_id;
        let wire_version = knock.wire_version;
        move || {
            let _lock = crate::peer::config::FileLock::acquire(&state_path)?;
            let mut peer_state = crate::peer::state::load(&state_path, now_ms)?;
            peer_state
                .record_knock(&addr, instance_id, proposed_name, wire_version, now_ms)
                .map_err(anyhow::Error::new)?;
            crate::peer::state::save(&state_path, &peer_state)?;
            Ok(peer_state.pending.len())
        }
    })
    .await?;

    // The ack is written only now: after the ban, the mute, the bucket and the
    // queue cap have all said yes.
    noise::write_knock_ack(stream, &mut session).await?;

    tracing::info!(
        peer_addr = %addr,
        instance = %knock.instance_id,
        wire_version = knock.wire_version,
        fresh = reservation_created,
        pending,
        "peer knock: a Mac wants to pair (`tcr peer pending` lists it; accept, ignore or \
         block it, nothing else has happened)"
    );
    Ok(())
}

/// The part of a knock that runs AFTER a slot is reserved, and the only part
/// that writes to the socket: message 2, then the knock frame.
///
/// Pulled out of [`serve_knock`] so its caller has one place to catch a failure
/// and release the reservation [`PeerState::reserve_knock_slot`] made, rather
/// than duplicating that release at every `?`, and one place to put a deadline
/// on. Message 1's own validation is NOT in here
/// any more: it writes nothing, so it belongs above the reservation.
async fn finish_knock_handshake<S>(
    stream: &mut S,
    // The whole [`noise::Message1`] rather than its `state` field: snow's
    // `HandshakeState` is not nameable from this file (it is a private import
    // in `crate::peer::noise`), and taking the struct keeps the "what reading
    // message 1 produced" unit intact rather than splitting it at a signature.
    read: noise::Message1,
    pattern: Handshake,
) -> Result<(noise::PeerSession, tcr_peer_wire::Knock)>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // **The first byte this node writes in answer to a knock is written
    // here**, which is why message 1's own validation sits above the
    // reservation in the caller and this does not: everything before this
    // function has written nothing.
    let mut session = noise::finish_responder(stream, read.state, pattern, |_| {
        Ok(PeerId([0_u8; KEY_BYTES]))
    })
    .await?;

    let knock = noise::read_knock(stream, &mut session, MAX_KNOCK_FRAME_BYTES).await?;
    Ok((session, knock))
}

/// Undo a reservation [`PeerState::reserve_knock_slot`] made for `addr` at
/// `reserved_at_ms`, because the handshake that would have earned it real
/// details never completed. Logged rather than propagated: the connection is
/// already being torn down for its own error, and a state-file write failure
/// here must not shadow that original cause, it costs one placeholder row
/// that ages out after [`crate::peer::state::KNOCK_TTL_MS`] at worst.
async fn release_reserved_knock_slot(context: &SessionContext, addr: &str, reserved_at_ms: i64) {
    let released = on_blocking_thread({
        let state_path = context.state_path.clone();
        let addr = addr.to_string();
        move || {
            let _lock = crate::peer::config::FileLock::acquire(&state_path)?;
            let mut peer_state = crate::peer::state::load(&state_path, reserved_at_ms)?;
            peer_state.release_knock_reservation(&addr, reserved_at_ms);
            crate::peer::state::save(&state_path, &peer_state)?;
            Ok(())
        }
    })
    .await;
    if let Err(err) = released {
        tracing::warn!(
            peer_addr = %addr,
            error = %err,
            "peer knock: failed to release a reservation after the handshake did not \
             complete; the row ages out on its own"
        );
    }
}

/// Record the static key an `XX` handshake learned against the accepted window
/// for that address, so `tcr peer block` can ban the key as well.
async fn record_pairing_key(
    context: &SessionContext,
    addr: &str,
    key: PeerId,
    now_ms: i64,
) -> Result<()> {
    on_blocking_thread({
        let state_path = context.state_path.clone();
        let addr = addr.to_string();
        move || {
            let _lock = crate::peer::config::FileLock::acquire(&state_path)?;
            let mut peer_state = crate::peer::state::load(&state_path, now_ms)?;
            let mut touched = false;
            for window in &mut peer_state.accepted {
                if window.addr == addr {
                    window.learned_key = Some(key);
                    touched = true;
                }
            }
            if !touched {
                return Ok(());
            }
            crate::peer::state::save(&state_path, &peer_state)
        }
    })
    .await
}

/// Run one lock-load-save on a blocking thread and wait for it there.
///
/// Every state-file write on the listener's own paths goes through this.
/// [`crate::peer::config::FileLock::acquire`] waits for a contended lock with
/// `std::thread::sleep` for up to [`crate::peer::config::LOCK_WAIT_MS`], and
/// the load and the save under it are synchronous file IO: run inline on the
/// proxy's runtime, a handful of simultaneous knocks park that many worker
/// threads for seconds each, and the local proxy stops answering. The knock
/// path is unauthenticated, so who arranges that handful is not this node's
/// choice.
///
/// A panic inside the closure comes back as the `JoinError`, reported as this
/// function's own error rather than resumed: the caller is one connection, and
/// a panicking state write must not take the accept loop's task with it.
async fn on_blocking_thread<T, F>(work: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(outcome) => outcome,
        Err(err) => Err(anyhow::Error::new(err)
            .context("peer listener: a state-file write did not finish on its own thread")),
    }
}

/// An authenticated session, plus the one thing about HOW it authenticated
/// that the stream handler cannot re-derive.
///
/// The extra field is the enrolment secret. A join token carries no invite id,
/// so the PSK that decrypted message 1 is the only thing that identifies which
/// invite is being spent, and [`crate::peer::pair::accept_enrolment`] needs it
/// to retire the right row. See the ordering rules in
/// [`crate::peer::noise`]'s module docs. There was no way to carry it out
/// of the handshake, which is exactly why `accept_enrolment` had no production
/// caller and the headless join path pinned nothing on the registrar's side.
pub struct AcceptedSession {
    /// The authenticated session.
    pub session: PeerSession,
    /// The outstanding invite's secret, on [`Handshake::Enrol`] only.
    pub enrol_secret: Option<[u8; KEY_BYTES]>,
}

/// Run the responder's half for a first pairing, a return visit or an
/// enrolment, from the message 1 the caller already holds.
///
/// Knocks do not come here: they are [`serve_knock`], which shares no code with
/// this function because it shares no decision either, a knock has no static
/// key, no pin, no grant and no stream.
///
/// The order for the two 96-byte patterns is the common case first:
/// [`Handshake::Return`] is tried, and only a message that does not
/// authenticate under it is offered to the outstanding invites as an
/// enrolment. Both orders are safe, an `IKpsk1` message 1 never decrypts under
/// plain `IK`, because the PSK is mixed in before it, and this one keeps a
/// returning peer from paying the trial cost.
///
/// A pin refusal is therefore final: it happens only on a message that already
/// authenticated as `IK`, so there is no path where a refused peer is then
/// offered a second pattern.
///
/// # What authorizes an `XX` message 1, which is the operator and nothing else
///
/// Answering an `XX` message 1 discloses this node's static key in message 2.
/// There is no pin to check, that is what "first pairing" means, so the only
/// thing that can stand in for one is the operator having approved THIS
/// machine: [`crate::peer::state::PeerState::accepted_window`] must hold a live
/// window for this source address AND the instance id the message 1 payload
/// carries.
///
/// **A node-wide window was replaced with that pair**, and the
/// difference is the whole point: the older `tcr peer pair` opened two
/// minutes in which ANY host that could reach this port got message 2, so a
/// stranger who happened to be scanning during a legitimate pairing harvested
/// this node's static key. Now a window exists only because the operator
/// pressed Accept on a knock they could see, and it admits one id at one
/// address.
///
/// A banned static key is refused here too, inside the pin callback, which is
/// the earliest moment a key is known. See
/// [`crate::peer::state::PeerState::is_key_banned`] for why the address half of
/// a ban is not enough on its own.
#[allow(clippy::too_many_arguments)]
async fn accept_pairing_or_return<S>(
    stream: &mut S,
    context: &SessionContext,
    addr: &str,
    pattern: Handshake,
    message_1: &[u8],
    rows: &[PeerRow],
    psks: &[[u8; KEY_BYTES]],
    peer_state: &PeerState,
    now_ms: i64,
) -> Result<AcceptedSession>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let secret = &context.secret;
    let mut scratch = vec![0_u8; noise::HANDSHAKE_SCRATCH_BYTES];

    if pattern == Handshake::Pair {
        let read = noise::read_message_1(secret, Handshake::Pair, &[], message_1, &mut scratch)?;
        // The payload is read out of the message this node already holds, so
        // the check below costs no round trip and happens before message 2.
        let claimed = noise::Message1 {
            state: read,
            psk: None,
            payload: message_1[32..].to_vec(),
        };
        let instance_id = claimed.instance_id()?;
        if !peer_state.accepted_window(addr, &instance_id, now_ms) {
            bail!(
                "peer listener: a first pairing arrived from {addr} as instance {instance_id} \
                 and no window is open for it, so nothing was answered. A window exists only \
                 after the operator accepts a pairing request on THIS Mac \
                 (`tcr peer pending`, then `tcr peer accept <instance|addr>`), it lasts {} \
                 seconds, and it admits that one instance at that one address",
                ACCEPTED_WINDOW_SECS
            );
        }
        let session = noise::finish_responder(stream, claimed.state, Handshake::Pair, |_| {
            Ok(PeerId([0_u8; KEY_BYTES]))
        })
        .await?;
        if peer_state.is_key_banned(&session.peer) {
            bail!(
                "peer listener: the static key this pairing revealed is blocked, so it is \
                 refused from this new address too (`tcr peer unblock` lifts it)"
            );
        }
        return Ok(AcceptedSession {
            session,
            enrol_secret: None,
        });
    }

    let returning = noise::read_message_1(secret, Handshake::Return, &[], message_1, &mut scratch);
    match returning {
        Ok(state) => {
            let rows = rows.to_vec();
            let banned = peer_state.banned.clone();
            let session =
                noise::finish_responder(stream, state, Handshake::Return, move |remote| {
                    let pinned = noise::pin_check_rows(remote, &rows)?;
                    // A pinned peer whose KEY is banned is refused, and the refusal
                    // is here rather than after the handshake for the same reason
                    // the pin check is: nothing has been written yet.
                    if banned.iter().any(|ban| ban.key.as_ref() == Some(&pinned)) {
                        return Err(PinRefusal::NotPinned {
                            offered: pinned.display(),
                        });
                    }
                    Ok(pinned)
                })
                .await?;
            Ok(AcceptedSession {
                session,
                enrol_secret: None,
            })
        }
        Err(return_error) => {
            if psks.is_empty() {
                return Err(return_error.context(
                    "peer listener: no outstanding invite to try this as an enrolment \
                     (`tcr peer invite` mints one)",
                ));
            }
            let read = noise::read_message_1_matching(
                secret,
                Handshake::Enrol,
                psks,
                message_1,
                &mut scratch,
            )?;
            // Enrolment's authorization is the PSK, and it was proved inside
            // message 1 before this line ran. The row is written by
            // `crate::peer::pair`, which owns both halves of enrolment.
            let session = noise::finish_responder(stream, read.state, Handshake::Enrol, |remote| {
                match <[u8; KEY_BYTES]>::try_from(remote) {
                    Ok(key) => Ok(PeerId(key)),
                    Err(_) => Err(PinRefusal::Malformed { len: remote.len() }),
                }
            })
            .await?;
            Ok(AcceptedSession {
                session,
                enrol_secret: read.psk,
            })
        }
    }
}

/// Open ONE carrier at `friend` and serve whatever rides back over it.
///
/// The lender's half of the reverse path, and the production `run_one` that
/// [`crate::peer::tunnel::keep_reverse_carrier`]'s doc says lives outside that
/// function because it needs a stream header the wire only just learned.
///
/// # Three steps, and the third is the surprising one
///
/// Dial the friend; run the handshake so it knows WHICH pinned Mac is asking;
/// send a [`StreamKind::Park`] header. Then hand the raw socket to this node's
/// OWN accept path and await it. That is not a trick: the socket's far end is
/// on the friend's carrier desk, and what the friend eventually writes into it
/// is a forward, which is a fresh connection to this Mac in every respect
/// except who opened the TCP. Serving it through [`serve_connection`] is what
/// makes it one: the same ban list, the same caps, the same per-frame gate, on
/// a socket that happens to point the other way.
///
/// Returns when the carrier is finished, however it finished, which is the
/// contract `keep_reverse_carrier` loops on.
///
/// `from` is the friend's own address, because that is who is at the far end
/// of this socket. A forward arriving over it was requested by a third Mac,
/// and that Mac proves who it is inside its own Noise session, exactly as it
/// would on a forward the friend dialled.
pub async fn park_one_carrier(friend: &PeerRow, context: &SessionContext) -> Result<()> {
    let Some((addr, mut stream)) = serve::dial_peer_with_endpoint(friend).await else {
        bail!(
            "peer reverse: none of {}'s addresses answered, so no carrier could be parked there",
            friend.node.display()
        );
    };
    let mut session = noise::dial_handshake(
        &mut stream,
        &context.secret,
        Handshake::Return,
        Some(&friend.node.0),
        None,
    )
    .await
    .context("peer reverse: the handshake with the friend failed")?;
    let header = StreamHeader {
        kind: StreamKind::Park,
        target: None,
        via: Vec::new(),
        hops_remaining: 1,
        request_id: crate::peer::lease::random_id()
            .context("peer reverse: no request id for the carrier")?,
    };
    let bytes =
        serde_json::to_vec(&header).context("peer reverse: the header did not serialize")?;
    noise::send_encrypted(&mut stream, &mut session.transport, &bytes).await?;
    // The session's whole job is done: it said who is parking. Dropped here so
    // nothing can be tempted to write a keepalive into a socket somebody
    // else's carried session is about to own.
    drop(session);
    tracing::debug!(
        friend = %friend.node.display(),
        friend_addr = %addr,
        "peer reverse: a carrier is parked at this friend; serving whatever rides back"
    );

    // `bind` is the friend's address for the same reason `from` is: this
    // socket has no local listener behind it, and the value is only read by
    // the off-LAN admission gate, which asks whether the FAR end is off this
    // network. Answering it with the address the far end is actually at is the
    // honest reading.
    serve_connection(stream, context, &addr, addr)
        .await
        .map_err(|failure| failure.error)
}

/// Serve one authenticated stream: the header, the gate, then the handler.
///
/// The row set is re-read and the gate re-run **before every frame**, so
/// `tcr peer forget` drops a live session within one frame. Cheap by design:
/// the file is small and a peer stream is not a hot loop.
async fn serve_stream<S>(
    stream: S,
    accepted: AcceptedSession,
    context: &SessionContext,
    store: &PeerStore,
    from: SocketAddr,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // Taken by value because the TUNNEL arm hands the stream to a pump that
    // outlives this function's frame; every other arm borrows it from here, so
    // nothing else changes shape.
    let mut stream = stream;
    let AcceptedSession {
        mut session,
        enrol_secret,
    } = accepted;
    let mut dedup = RequestDedup::new();
    let frame = noise::recv_encrypted(&mut stream, &mut session.transport).await?;
    let header: StreamHeader =
        serde_json::from_slice(&frame).context("peer stream: the first frame is not a header")?;

    // **The enrolment exemption, and the only thing it admits.**
    //
    // A joiner is not pinned, that is what enrolling means, so the gate below
    // would refuse the very frame that creates its row. The rules are stated in
    // full in `crate::peer::noise`'s module docs; this is where they are
    // enforced, and for a long time nothing enforced them: `accept_enrolment`
    // had no production caller, so `tcr peer join` completed its handshake,
    // sent its `Control::Enroll`, was refused for want of a row, and the
    // registrar wrote nothing. The joiner then waited for a `Hello` that never
    // came, which is at least an honest failure, but the headless path, the
    // one a machine with no screen has, did not work at all.
    //
    // The exemption is exactly one frame wide: the header must be CONTROL, the
    // next frame must be a `Control::Enroll`, and the handshake must have been
    // `IKpsk1` under a secret that matched an OUTSTANDING invite (proved inside
    // message 1, before this node wrote a byte). Everything after it meets the
    // ordinary gate against the row it just created.
    if session.handshake == Handshake::Enrol {
        let Some(secret) = enrol_secret else {
            bail!(
                "peer stream: an enrolment session arrived with no invite secret attached, \
                 which is a bug in this listener rather than anything the joiner did; \
                 nothing was pinned"
            );
        };
        if header.kind != StreamKind::Control {
            bail!(
                "peer stream: an enrolment may open a CONTROL stream and nothing else, and \
                 this one declared {:?}; nothing was pinned",
                header.kind
            );
        }
        return serve_enrolment(&mut stream, &mut session, context, &secret, store, from).await;
    }

    // Read BEFORE the session can be moved into a carry: the TUNNEL arm hands
    // `session` to the pump, and the peer id is what every line after that
    // names.
    let accepted_peer = session.peer;
    // The bytes this connection already read, unless the operator edited the
    // file while the handshake ran, `reload_if_changed` stats and re-reads
    // only then, so the gate is as fresh as it ever was and the ordinary
    // connection opens the file once (the review's L2).
    store.reload_if_changed();
    let file = store.file();
    let row = file.peers.iter().find(|row| row.node == session.peer);
    peer_stream_gate_rows(&header, row).map_err(anyhow::Error::new)?;
    peer_stream_gate_hop(&context.node, &header, &mut dedup, now_ms())
        .map_err(anyhow::Error::new)?;

    match header.kind {
        StreamKind::Control => serve_control(&mut stream, &mut session, context, store, from).await,
        // The lender's half. The gate above has already agreed this peer holds
        // `inspect`; `handle_serve_on` asks the same question again through
        // this store, which is the belt-and-braces the SERVE module documents
        // rather than a duplicate, the row can be revoked between the header
        // frame and the request frame, and one frame is the revocation latency
        // this file promises. It stays true through the connection's own store
        // because `handle_serve` opens with `store.reload_if_changed()`
        // (`src/peer/serve.rs:675`): a re-read when the file moved, and no open
        // at all when it did not, which is the review's L2.
        StreamKind::Serve => {
            let Some(serving) = &context.serving else {
                bail!(
                    "peer stream: SERVE is granted for this peer but this build wired no lease \
                     ledger, so there is nothing to serve it against (a lender needs \
                     `listener::serve` to be given a `LeaseServing`)"
                );
            };
            serve::handle_serve_on(
                &mut stream,
                &mut session,
                store,
                serving,
                header.request_id,
                now_ms(),
                from,
            )
            .await
        }
        // The gateway's half of blind egress. The gate above has already
        // agreed this peer holds `allow.gateway` for an origin target
        // (`peer_stream_gate_rows`); everything else a carry needs, the
        // allow-listed origins, the per-peer hourly byte cap and the SNI check,
        // is `tunnel::handle_tunnel_on`'s, and it answers the first two
        // before a socket to the origin exists.
        //
        // The stream goes in BY VALUE: the carry spawns a pump over it that
        // outlives this frame, which is the whole reason `serve_stream` takes
        // it owned.
        StreamKind::Tunnel => {
            let Some(target) = header.target.as_ref() else {
                bail!(
                    "peer stream: a TUNNEL with no target is not something this node can \
                     carry"
                );
            };
            // **Which of the two handlers, decided by the TARGET and by
            // nothing else.** An origin target is a gateway carry and a peer
            // target is a forward, and the two differ in what they are handed
            // rather than only in where they dial: a forward needs this Mac's
            // peers file (the requester's `allow.relay` and the TARGET's own
            // row), which `handle_tunnel_on` is deliberately not given, so a
            // build that routed both through it would either be an open relay
            // or, as before, refuse every forward.
            //
            // The route is this node's own DECISION and is built here from the
            // target the gate already admitted, never from a second field a
            // peer could disagree with: `authorize_forward` refuses the two
            // when they differ, and this is the only production caller that
            // sets them.
            let carry = crate::peer::tunnel::Carry {
                route: match target {
                    TunnelTarget::Peer { node } => crate::peer::tunnel::OriginRoute::Peer(*node),
                    TunnelTarget::Origin { .. } => crate::peer::tunnel::OriginRoute::Resolve,
                },
                peer: accepted_peer,
                target,
                hosts: crate::peer::egress::PEER_EGRESS_HOSTS,
                cap_bytes: crate::peer::tunnel::DEFAULT_MAX_TUNNEL_BYTES_PER_HOUR,
                budget: &context.tunnels,
                now_ms: now_ms(),
            };
            let (kind, (up, down)) = match target {
                TunnelTarget::Peer { .. } => (
                    "FORWARD",
                    crate::peer::tunnel::handle_forward_on(
                        stream,
                        session,
                        carry,
                        crate::peer::tunnel::Forward {
                            store,
                            node: context.node,
                            hops_remaining: header.hops_remaining,
                            via: &header.via,
                        },
                    )
                    .await?,
                ),
                TunnelTarget::Origin { .. } => (
                    "TUNNEL",
                    crate::peer::tunnel::handle_tunnel_on(stream, session, carry).await?,
                ),
            };
            // What this carry cost, against the path it arrived over, so the
            // per-path "carried this hour" figure is a measurement rather than
            // a total split by guesswork. Unattributable (no recorded endpoint
            // with this source's host, or a hop whose socket is the
            // forwarder's) charges nothing: `locator_from` answers `None`
            // rather than naming the first locator on the row.
            if let Some(path) = store
                .row(&accepted_peer)
                .and_then(|row| row.locator_from(from))
            {
                match crate::peer::tunnel::path_meter().lock() {
                    Ok(mut meter) => {
                        meter.charge_bytes(&accepted_peer, path, up + down, now_ms());
                    }
                    Err(_) => tracing::warn!(
                        peer = %accepted_peer.display(),
                        "peer stream: the path meter's lock is poisoned, so this carry is                          not on any path's hour"
                    ),
                }
            }
            tracing::debug!(
                bytes_up = up,
                bytes_down = down,
                peer = %accepted_peer.display(),
                "peer stream: {kind} closed"
            );
            Ok(())
        }
        // The carrier desk's half of the reverse path. The stream goes in BY
        // VALUE and never comes back: from here it is a raw socket on the desk
        // waiting for a forward to spend it, and this function returning is
        // what LEAVES it open rather than what closes it.
        //
        // The Noise session this arrived over is dropped right here, unused.
        // It proved who is parking, which is all it is for: whatever rides
        // this socket next is a fresh session the far Mac's own listener
        // answers, and a byte written here to keep the socket warm would land
        // in the middle of somebody's carried session.
        StreamKind::Park => {
            let outcome = crate::peer::tunnel::park_reverse_carry(
                store,
                session.peer,
                Box::new(stream),
                now_ms(),
            );
            match outcome {
                crate::peer::tunnel::ParkOutcome::Parked { open } => {
                    tracing::info!(
                        peer = %session.peer.display(),
                        peer_addr = %from,
                        open,
                        "peer reverse: holding a carrier for a Mac that cannot be dialled"
                    );
                    Ok(())
                }
                // A refusal is the end of this connection and nothing else:
                // the socket is dropped with the outcome, and the far Mac
                // learns it has no carrier here by the same means it learns
                // anything, its next attempt.
                crate::peer::tunnel::ParkOutcome::Refused(refusal) => {
                    bail!("peer stream: this carrier was not parked ({refusal:?})")
                }
            }
        }
        StreamKind::Unknown(raw) => bail!("peer stream: stream kind {raw} is unknown"),
    }
}

/// The registrar's half of an enrolment: read the one `Control::Enroll`, write
/// the pinned row, answer the `Hello` that is the joiner's acknowledgement,
/// then fall into the ordinary CONTROL loop against the row just created.
///
/// The write is [`crate::peer::pair::accept_enrolment`], which spends the
/// invite and pins the joiner in ONE locked read-modify-write. See its docs
/// for why two writes cannot be correct here, and
/// [`crate::peer::config::FileLock`] for why the lock is what makes a one-use
/// key admit one machine.
///
/// The `Hello` is the ordinary message any pinned peer may receive, so the
/// acknowledgement needs no new wire type; [`crate::peer::pair::join_as`] pins
/// the registrar and reports success only when it arrives.
async fn serve_enrolment<S>(
    stream: &mut S,
    session: &mut PeerSession,
    context: &SessionContext,
    secret: &[u8; KEY_BYTES],
    store: &PeerStore,
    from: SocketAddr,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let frame = noise::recv_encrypted(stream, &mut session.transport).await?;
    let message: Control = serde_json::from_slice(&frame)
        .context("peer enrol: this frame is not a control message")?;
    let Control::Enroll(enroll) = message else {
        bail!(
            "peer enrol: the one frame an unpinned enrolment may send is a Control::Enroll, \
             and this one is {message:?}; nothing was pinned"
        );
    };

    let row = crate::peer::pair::accept_enrolment(
        &context.peers_path,
        session.peer,
        &enroll,
        secret,
        now_ms(),
        from,
    )?;

    let mut facts = NodeFacts::listening(context.node, store.file().listen);
    facts.briefs = crate::peer::discovery::neighbor_briefs(&store.file().peers, session.peer);
    let hello = hello_for_peer(&facts, &row.allow.control);
    let bytes = serde_json::to_vec(&Control::Hello(hello))
        .context("peer enrol: the acknowledging Hello did not serialize")?;
    noise::send_encrypted(stream, &mut session.transport, &bytes).await?;

    // The row exists now, so every later frame on this stream is an ordinary
    // pinned peer's frame and meets the ordinary gate. `serve_control` opens
    // with `store.reload_if_changed()`, and `accept_enrolment` has just
    // written the file, so the row it wrote is the row the next frame is
    // gated against.
    serve_control(stream, session, context, store, from).await
}

/// The CONTROL handler: answer `Hello` with a `Hello` assembled from this
/// peer's grants, answer `Ping`, refuse everything phase 4 owns.
/// How often an idle CONTROL session wakes to ask whether a handed bearer is
/// due for renewal.
///
/// A choice, not a measurement, and the two things it is bounded by are named
/// rather than left implicit. It has to be well under
/// [`crate::peer::lease::HANDOFF_RENEW_LEAD_MS`], or the lead time this wakes
/// to honour would elapse between wakes; and it has to be long enough that an
/// idle session is not a busy loop. One second on a per-connection timer that
/// does nothing but compare two integers when nothing is handed.
const HANDOFF_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Push a fresh `Handoff` for every hand-mode lease on this session whose
/// bearer is close to expiring.
///
/// The owner renews rather than the borrower asking, because the borrower has
/// nothing to ask WITH: the token is the only thing it holds and a request for
/// a new one carries no proof the old one is still legitimate.
///
/// Revocation is the absence of this call, which is what
/// [`crate::peer::lease::handoff_for`]'s doc means by "revocation IS stop
/// renewing": a lease dropped from the ledger, past its `until`, or expired is
/// simply not in the list below, and no recall frame exists because a token on
/// another machine cannot be taken back.
async fn push_due_handoffs<S>(
    stream: &mut S,
    session: &mut PeerSession,
    context: &SessionContext,
    store: &PeerStore,
    handed: &mut std::collections::HashMap<u128, crate::peer::lease::HandedBearer>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let Some(serving) = &context.serving else {
        return Ok(());
    };
    if !handoff_is_permitted(session.handshake) {
        return Ok(());
    }
    if handed.is_empty() {
        return Ok(());
    }
    let now = now_ms();

    // Only leases this session ALREADY handed a bearer for, which is what makes
    // the mode question disappear: a lease in this map was funded by a hand
    // grant, because the grant arm is the only thing that puts one here and it
    // reads the mode off the grant the ledger itself picked
    // (`lease::Granted::mode`).
    //
    // Re-deriving the mode here by window would pick an ENDED hand grant beside
    // the live serve grant that actually funded the lease, which is the review's
    // first HIGH and has its own gate
    // (`an_ended_hand_grant_beside_a_live_serve_grant_hands_nothing_over`). The
    // first draft of this function did exactly that and that gate caught it.
    //
    // A lease nobody handed needs no renewal: its bearer went out with the
    // grant, on this same session.
    //
    // The ledger's lock is taken and dropped before any await: a guard held
    // across a send is how one slow borrower stops every other lease on this
    // Mac, which is the rule the grant arm already follows.
    let live: Vec<tcr_peer_wire::Lease> = {
        let Ok(held) = serving.ledger.lock() else {
            return Ok(());
        };
        held.live(now)
            .into_iter()
            .filter(|lease| held.grantee_of(lease.lease_id) == Some(session.peer))
            .filter(|lease| handed.contains_key(&lease.lease_id))
            .collect()
    };
    // Revocation IS the absence of a renewal: a lease dropped from the ledger,
    // past its `until`, or expired is simply not in `live`, and it leaves this
    // map with it. There is no recall frame, because a token already on another
    // machine cannot be taken back.
    handed.retain(|id, _| live.iter().any(|lease| lease.lease_id == *id));

    store.reload_if_changed();
    let file = store.file();
    let Some(row) = file.peers.iter().find(|row| row.node == session.peer) else {
        // Forgotten mid-session. The next frame closes the stream; this one
        // just does not renew, which IS the revoke.
        return Ok(());
    };

    for lease in live {
        let last = handed.get(&lease.lease_id).copied();
        if !crate::peer::lease::handoff_renewal_due(last.map(|sent| sent.expires_at_ms), now) {
            continue;
        }
        // Whether the bearer this poll would send is worth sending is decided
        // below, once it is known, by `handoff_push_is_due`. This first check
        // is only here to keep an undue lease from reading the owner's token
        // out of the config at all.
        // The funding grant, re-read per renewal and through the SAME selector
        // `Ledger::grant` uses. Two things ride on it. An operator who revokes
        // the grant stops the renewals within one poll, which is what
        // `handoff_for`'s "revocation IS stop renewing" means when there is no
        // recall frame and no ledger removal; and the selector skips ENDED
        // grants, so this cannot pick the ended `hand` grant beside a live
        // `serve` one, which is the review's first HIGH.
        let Some(grant) = row.grant_for(
            lease.window,
            u64::try_from(now / 1_000).unwrap_or_default(),
            &|scope| {
                serving.utilization.scope_restriction(scope)
                    != crate::peer::serve::ScopeRestriction::Unenforceable
            },
        ) else {
            continue;
        };
        if grant.mode != crate::peer::config::LendMode::Hand {
            continue;
        }
        let scope = match serving.ledger.lock() {
            Ok(held) => held.scope_of(lease.lease_id),
            Err(_) => continue,
        };
        let Some(frame) = crate::peer::lease::handoff_for(
            grant.mode,
            &lease,
            serving.manager.handoff_bearer(&scope),
            now,
        ) else {
            continue;
        };
        if let Control::Handoff {
            lease_id,
            expires_at_ms,
            access_token,
        } = &frame
        {
            let next = crate::peer::lease::HandedBearer {
                expires_at_ms: *expires_at_ms,
                fingerprint: crate::peer::lease::bearer_fingerprint(access_token.reveal()),
            };
            // The bearer the borrower already holds is not a renewal. The
            // record is left as it is when this skips, so the moment the
            // owner's credential IS refreshed the next poll sends the new one.
            if !crate::peer::lease::handoff_push_is_due(last, next, now) {
                continue;
            }
            handed.insert(*lease_id, next);
        }
        let bytes = serde_json::to_vec(&frame)
            .context("peer control: the renewing Handoff did not serialize")?;
        noise::send_encrypted(stream, &mut session.transport, &bytes).await?;
        tracing::debug!(
            peer = %session.peer.display(),
            lease = %crate::peer::config::lease_id_string(lease.lease_id),
            "peer control: pushed a fresh bearer before the last one expires"
        );
    }
    Ok(())
}

async fn serve_control<S>(
    stream: &mut S,
    session: &mut PeerSession,
    context: &SessionContext,
    store: &PeerStore,
    from: SocketAddr,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // What this session last handed over, per lease, so the pusher below knows
    // whether a fresh bearer is due. Per SESSION and not global: a Handoff can
    // only travel on the `IK`-authenticated stream to its own grantee, so the
    // one that would send it is the one that has to remember.
    let mut handed: std::collections::HashMap<u128, crate::peer::lease::HandedBearer> =
        std::collections::HashMap::new();

    // The frame reader lives as long as the session, and that is the whole
    // point of it. The timeout below cancels the read once a second on every
    // control session, so the bytes already taken off the socket have to
    // survive in something the loop owns rather than in the cancelled future:
    // a frame whose halves straddle a tick is otherwise torn, and the next read
    // reads ciphertext as a length prefix. `FrameReader`'s doc has the failure
    // in full.
    let mut frames = noise::FrameReader::new();

    loop {
        // A bounded wait rather than a bare read, so a borrower that sits idle
        // mid-lease still gets its bearer renewed. Without it the renewal could
        // only ride a frame the borrower happened to send, and a borrower with
        // nothing to say is exactly the one whose token quietly expires.
        //
        // No spawned task: the Handoff has to go out on THIS stream, and a task
        // holding the stream would be a second writer on one Noise transport.
        let frame = match tokio::time::timeout(
            HANDOFF_POLL_INTERVAL,
            frames.recv_encrypted(stream, &mut session.transport),
        )
        .await
        {
            Ok(frame) => frame?,
            Err(_elapsed) => {
                push_due_handoffs(stream, session, context, store, &mut handed).await?;
                continue;
            }
        };
        let message: Control = serde_json::from_slice(&frame)
            .context("peer control: this frame is not a control message")?;

        // Re-read per frame: a grant revoked while this stream is open takes
        // effect on the next message, not at the next handshake. Through the
        // store's mtime check rather than an unconditional open, a revoke
        // moves the mtime, so the frame after it still reads the new file,
        // and a stream that sits there pinging stops re-reading bytes that did
        // not change (the review's L2).
        store.reload_if_changed();
        let file = store.file();
        let Some(row) = file.peers.iter().find(|row| row.node == session.peer) else {
            bail!(
                "peer control: this peer is no longer pinned; closing the live session \
                 (`tcr peer forget` takes effect within one frame)"
            );
        };

        match message {
            // **The refresh rule, and the reason a peer that moved is still
            // reachable.** Two facts land here and they are different kinds of
            // fact: the addresses the peer CLAIMS it listens on, and the
            // address this node SAW it from. Neither is identity: the session
            // that carried them already proved the pinned static key, so
            // both are recorded as advice, and the timestamp on each is this
            // node's own clock rather than anything off the wire.
            //
            // WireGuard's roaming rule, stated in its own words: a peer's
            // address is a side effect of a correctly authenticated packet
            // arriving, never a configured fact.
            Control::Hello(incoming) => {
                let mut learned = config::endpoints_from_hello(&incoming.addrs, now_ms());
                learned.push(Endpoint::direct(from, now_ms(), EndpointSource::Hello));
                // The pair's rendezvous secret, recorded on the frame that
                // already writes this file rather than on every accepted
                // connection: a SERVE must still open the peers file exactly
                // once (`an_accepted_connection_opens_the_peers_file_once`,
                // `tests/peer_lease.rs`), and a Hello is the exchange that
                // exists to answer "where and how do we meet next time".
                //
                // Surfaced and never fatal: a file that would not take the
                // secret costs this pair the derived-port fallback after a
                // restart, which is the reach it had before the fallback
                // existed, and it must not cost it the session open right now.
                if let Err(err) = crate::peer::config::observe_rendezvous_secret(
                    &context.peers_path,
                    &session.peer,
                    crate::peer::reach::port_secret(&session.handshake_hash),
                ) {
                    tracing::warn!(
                        peer = %session.peer.display(),
                        error = %err,
                        "peer control: could not record this pair's rendezvous secret",
                    );
                }
                // The sender goes with the briefs: `observe_neighbor_briefs`
                // applies them only for a peer this node would brief in
                // return (`allow.control.briefs`), which is the same flag
                // `hello_for_peer` reads when it builds the outgoing side.
                if let Some(briefs) = incoming.briefs.as_deref() {
                    if let Err(err) = crate::peer::discovery::observe_neighbor_briefs(
                        &context.peers_path,
                        &session.peer,
                        briefs,
                        now_ms(),
                    ) {
                        tracing::warn!(
                            peer = %session.peer.display(),
                            error = %err,
                            "peer control: could not record a neighbor brief from this peer",
                        );
                    }
                }
                if let Err(err) =
                    config::observe_endpoints(&context.peers_path, &session.peer, &learned)
                {
                    // Surfaced and not swallowed, and not fatal to the stream:
                    // the session is authenticated and useful, and what failed
                    // is a routing hint the next Hello offers again.
                    tracing::warn!(
                        peer = %session.peer.display(),
                        peer_addr = %from,
                        error = %err,
                        "peer control: could not record where this peer answers; it stays \
                         reachable at the endpoints already on its row"
                    );
                }
                // The two reflexive halves of this frame, recorded in memory
                // and never onto the row: `from` is the address a NAT in front
                // of that peer rewrote its packets to, which is not an address
                // it listens on, so it is advice for a punch and never an
                // endpoint to dial.
                crate::peer::reach::remember_observed_peer(session.peer, from);
                if let Some(told) = incoming.observed_you_at.as_deref() {
                    match told.parse::<SocketAddr>() {
                        Ok(addr) => {
                            crate::peer::reach::remember_observed_self(session.peer, addr);
                            // And onto the row, so a restart still knows where
                            // this Mac is seen. Surfaced and never fatal, the
                            // rule the rendezvous secret beside it follows: a
                            // file that would not take the address costs this
                            // pair a punch target after a restart and must not
                            // cost them the session open right now.
                            if let Err(err) = crate::peer::config::observe_seen_address(
                                &context.peers_path,
                                &session.peer,
                                addr,
                                u64::try_from(now_ms().max(0)).unwrap_or_default(),
                            ) {
                                tracing::warn!(
                                    peer = %session.peer.display(),
                                    error = %err,
                                    "peer control: could not record where this peer sees us",
                                );
                            }
                        }
                        Err(err) => tracing::debug!(
                            peer = %session.peer.display(),
                            told = %told,
                            error = %err,
                            "peer control: this peer said it sees us at something that is not                              a socket address; ignoring the hint"
                        ),
                    }
                }
                let mut facts = NodeFacts::listening(context.node, store.file().listen);
                facts.briefs =
                    crate::peer::discovery::neighbor_briefs(&store.file().peers, session.peer);
                facts.observed_peer_at = Some(from);
                let hello = hello_for_peer(&facts, &row.allow.control);
                let bytes = serde_json::to_vec(&Control::Hello(hello))
                    .context("peer control: the Hello did not serialize")?;
                noise::send_encrypted(stream, &mut session.transport, &bytes).await?;
            }
            // **The punch's one message.** A Mac nobody can dial cannot be
            // told anything directly, so this frame usually arrives through a
            // mutual friend, blind, inside a TUNNEL. Everything it asks for is
            // decided by `reach::punch_request`, and what happens here is only
            // the spawn: the punch waits for a slot boundary, which is up to
            // thirty seconds, and this loop has other frames to read.
            //
            // A punched socket is served exactly as an accepted one is. The
            // roles are the protocol's and not TCP's: the side that ASKED for
            // the punch dials the handshake, so this side responds to it
            // whichever of the two sockets completed, and a stranger who
            // guessed the port meets the same pin check as anybody else.
            Control::PunchAt { slot, public_addr } => {
                match crate::peer::reach::punch_request(session.peer, slot, &public_addr) {
                    Ok((peer_ip, plan)) => {
                        let context = context.clone();
                        let peer = session.peer;
                        tokio::spawn(async move {
                            let outcome = crate::peer::reach::punch(
                                &crate::peer::reach::KernelPunchNet,
                                &plan,
                                peer_ip,
                                crate::peer::reach::slot_window(),
                            )
                            .await;
                            match outcome {
                                Ok(punched) => {
                                    let from = SocketAddr::new(peer_ip, punched.port);
                                    let bind = SocketAddr::new(
                                        IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                                        punched.port,
                                    );
                                    if let Err(err) =
                                        serve_punched(punched.stream, context, from, bind).await
                                    {
                                        tracing::warn!(
                                            peer = %peer.display(),
                                            peer_addr = %from,
                                            error = %err,
                                            "peer punch: a punched connection did not serve"
                                        );
                                    }
                                }
                                Err(failure) => tracing::info!(
                                    peer = %peer.display(),
                                    failure = %failure,
                                    "peer punch: this peer asked for a punch and none of the                                      slots got through"
                                ),
                            }
                        });
                    }
                    Err(failure) => tracing::info!(
                        peer = %session.peer.display(),
                        peer_addr = %from,
                        failure = %failure,
                        "peer punch: this peer asked for a punch this node cannot join"
                    ),
                }
            }
            Control::Ping => {
                let bytes = serde_json::to_vec(&Control::Ping)
                    .context("peer control: the Ping did not serialize")?;
                noise::send_encrypted(stream, &mut session.transport, &bytes).await?;
            }
            // **The echo is the whole of the responder's half.** Both fields go
            // back untouched: the nonce is what pairs an answer with its
            // question, and `sent_ms` is the ASKER's clock, so reading it here,
            // comparing it to this Mac's clock, or replacing it with this Mac's
            // clock would each turn a round trip into a measurement of how far
            // apart two NTP daemons are. The asker times its own probe against
            // its own monotonic clock (`crate::peer::probe`).
            //
            // Answered on the same authorization every other frame on this
            // stream has: a pinned static key, re-checked against a re-read row
            // at the top of this loop. A probe grants nothing and discloses
            // nothing this session has not already proved.
            Control::Probe { nonce, sent_ms } => {
                let bytes = serde_json::to_vec(&Control::ProbeAck { nonce, sent_ms })
                    .context("peer control: the ProbeAck did not serialize")?;
                noise::send_encrypted(stream, &mut session.transport, &bytes).await?;
            }
            // An ack nobody on this side asked for. Logged and NOT refused:
            // probing runs on the dialling side, which reads its own acks
            // inline, so one arriving here is a peer's stray frame and closing
            // a working session over it would cost a cold prefix to say
            // nothing. It is not silence: the line names the peer, and no
            // measurement is recorded from a probe this node never sent.
            Control::ProbeAck { .. } => {
                tracing::debug!(
                    peer = %session.peer.display(),
                    peer_addr = %from,
                    "peer control: a probe ack arrived on a stream this node never probed on; \
                     ignoring it"
                );
            }
            // **THE ARM THAT JOINS THE TWO HALVES OF LEASING.**
            //
            // Both halves existed for a long time and nothing connected
            // them: the borrower's `lease::request_lease` sends this frame and
            // `Ledger::grant` answers it, and in between sat the `other =>`
            // refusal below. So a borrowed request reached the lender, was
            // refused here, and the borrower fell through to its own honest 429,
            // which from the outside is indistinguishable from a lender that
            // was never wired at all. `tests/peer_e2e.rs` asserted exactly that
            // gap.
            //
            // **Both opt-ins are checked, and neither is checked here.** The
            // lender's half is `row.allow.inspect`, asked inside
            // `clamp_to_grant` (it refuses `InspectNotGranted`), against a row
            // re-read by `Ledger::grant`'s own `reload_if_changed`, so a grant
            // revoked a second ago refuses without a restart. The borrower's
            // half is `allow.disclose` and it is checked ON THE BORROWER before
            // it ever asks (`lease::request_lease`), because a lender that
            // enforced both would be pretending it can see the other Mac's
            // config. The scope and the `until` are read off
            // the operator's own `LendGrant` by the same function.
            //
            // A build with no `LeaseServing` REFUSES rather than answering a
            // grant it could not fund, the same shape every other ungranted
            // kind has.
            Control::LeaseRequest(ask) => {
                let Some(serving) = &context.serving else {
                    bail!(
                        "peer control: this peer asked for a lease and this build wired no \
                         lease ledger, so there is nothing to grant it against (a lender \
                         needs `listener::serve` to be given a `LeaseServing`)"
                    );
                };
                // The guard is dropped before the await below: a `MutexGuard`
                // held across a send is how one slow borrower stops every other
                // lease on this Mac.
                let granted = {
                    let mut held = serving.ledger.lock().map_err(|_| {
                        anyhow::anyhow!("peer control: the lease ledger's lock is poisoned")
                    })?;
                    held.grant(&session.peer, &ask, store, serving.utilization.as_ref())
                };
                // The MODE this lease was funded under, read off the grant the
                // ledger itself picked. Never looked up again here: the review's
                // first HIGH was this arm re-finding the grant by window alone,
                // which picked an ENDED `hand` grant beside the live `serve` one
                // that actually minted the lease. See [`lease::Granted`].
                let mode = granted.mode();
                // And the SCOPE the same call recorded beside the lease. Read
                // here for the review's M2: this arm used to ask the ledger
                // again through a second lock and read a poisoned one as
                // `LendScope::All`, which is a silent widening of the
                // operator's own grant to every account on this Mac. The
                // second lock is gone, so the fallback has nowhere to live.
                let scope = granted.scope;
                let grant = granted.answer;
                // One log line, and it names the peer, the window and the
                // outcome, never an account, never a scope. A scope names the
                // lender's own accounts and this line is read beside a borrower's
                // name; the scope is in `tcr peer ls --json`, which is the
                // lender's own surface.
                tracing::info!(
                    peer = %session.peer.display(),
                    window = ?ask.window,
                    granted = grant.lease.is_some(),
                    refusal = ?grant.refusal,
                    "peer control: answered a lease request"
                );
                // Copied before the grant moves into the frame: the Handoff
                // below is about the lease this answer just minted.
                // `Option<Lease>` is `Copy`, so this is a read and not a clone.
                let minted = grant.lease;
                let bytes = serde_json::to_vec(&Control::LeaseGrant(grant))
                    .context("peer control: the LeaseGrant did not serialize")?;
                noise::send_encrypted(stream, &mut session.transport, &bytes).await?;
                // A HAND grant follows its lease with the owner's
                // short-lived bearer, on THIS session, which is the only
                // `IK`-authenticated one the grantee has. Every refusal about
                // the LEASE is in `lease::handoff_for` (serve mode, an ended
                // lease, no bearer the scope covers); the one about the SOCKET
                // is [`handoff_is_permitted`] and it is asked here, because the
                // pattern that ran is a property of this session and of
                // nothing the lease knows.
                if let Some(minted) = minted {
                    if mode == crate::peer::config::LendMode::Hand
                        && !handoff_is_permitted(session.handshake)
                    {
                        // Asked BEFORE the manager is, so a session that may
                        // not receive the bearer never causes it to be read
                        // out of the config at all. The lease itself stands:
                        // it was granted on its own merits and the borrower
                        // can spend it the serve way.
                        tracing::warn!(
                            peer = %session.peer.display(),
                            peer_addr = %from,
                            handshake = ?session.handshake,
                            "peer control: a hand grant was answered on a session that is not \
                             a return visit to a pinned key, so the owner's bearer stays on \
                             this Mac"
                        );
                    } else if let Some(frame) = crate::peer::lease::handoff_for(
                        mode,
                        &minted,
                        serving.manager.handoff_bearer(&scope),
                        now_ms(),
                    ) {
                        if let Control::Handoff {
                            lease_id,
                            expires_at_ms,
                            access_token,
                        } = &frame
                        {
                            handed.insert(
                                *lease_id,
                                crate::peer::lease::HandedBearer {
                                    expires_at_ms: *expires_at_ms,
                                    fingerprint: crate::peer::lease::bearer_fingerprint(
                                        access_token.reveal(),
                                    ),
                                },
                            );
                        }
                        let bytes = serde_json::to_vec(&frame)
                            .context("peer control: the Handoff did not serialize")?;
                        noise::send_encrypted(stream, &mut session.transport, &bytes).await?;
                    }
                }
            }
            // The borrower's own report of what a hand-mode response cost. It
            // can only raise `spent`; every refusal is in the ledger.
            Control::UsageHint { lease_id, spent } => {
                let Some(serving) = &context.serving else {
                    bail!("peer control: a usage hint arrived and this build wired no ledger");
                };
                let mut held = serving.ledger.lock().map_err(|_| {
                    anyhow::anyhow!("peer control: the lease ledger's lock is poisoned")
                })?;
                // Only the grantee of that lease may report against it.
                if held.grantee_of(lease_id) != Some(session.peer) {
                    bail!("peer control: a usage hint for a lease this peer does not hold");
                }
                let charged = held.apply_usage_hint(lease_id, spent);
                tracing::info!(
                    peer = %session.peer.display(),
                    charged,
                    "peer control: applied a borrower's usage hint"
                );
            }
            other => bail!(
                "peer control: {other:?} is not answered by this build (the lease lifecycle \
                 is phase 4)"
            ),
        }
    }
}

/// May the owner's bearer go out on a session this handshake produced?
///
/// The `Handoff` frame travels on an `IK` session and nothing
/// else, and until a review asked, nothing checked it. `IK` ([`Handshake::Return`])
/// is the only pattern that proves the dialler holds the private half of a key
/// this Mac has PINNED, which is the whole of "the grantee and nobody else":
///
/// - `XX` ([`Handshake::Pair`]) proves a static key that this Mac has not
///   pinned yet; the six digits are compared by a human afterwards.
/// - `IKpsk1` ([`Handshake::Enrol`]) proves an invite secret, which is a
///   one-use admission to the mesh and not an identity this Mac lends to.
/// - `NN` and `NNpsk0` ([`Handshake::Knock`], [`Handshake::KnockPsk`]) carry no
///   static key at all.
///
/// # This is a second lock on a door that is already shut
///
/// `serve_stream` returns a `Pair` session at the six-digit line and an `Enrol`
/// session at `serve_enrolment`, so no pattern other than `Return` reaches
/// `serve_control` in this build, and no integration test can drive one there.
/// That is exactly why the rule is written as a value a test can ask about:
/// the day a pattern is added, or one of those early returns becomes a
/// fall-through, this answers the same way it does today.
pub fn handoff_is_permitted(handshake: Handshake) -> bool {
    match handshake {
        Handshake::Return => true,
        Handshake::Pair | Handshake::Enrol | Handshake::Knock | Handshake::KnockPsk => false,
    }
}

/// The ONE authorization function for an inbound peer stream, checked in this
/// order:
///
/// 1. the handshake has been read to the point where the initiator's static is
///    known, and [`crate::peer::noise::pin_check`] agrees, **before message 2
///    is written**;
/// 2. [`StreamHeader::kind`] is granted for that peer, per its row's `allow`;
/// 3. [`StreamHeader::hops_remaining`] is above zero;
/// 4. this node's own id is not already in [`StreamHeader::via`];
/// 5. [`StreamHeader::request_id`] is not in THIS CONNECTION's dedup cache.
///
/// Order matters: every later check reads a field a peer controls, so the pin
/// check has to answer first.
///
/// **Check 5 cannot fire as the serving path is wired**, and the review's H2b
/// is that saying otherwise sends the next reader hunting for a defence that is
/// somewhere else. [`RequestDedup::new`] is called per `serve_stream`, one
/// stream carries one request, and [`peer_stream_gate_hop`] is asked exactly
/// once against a cache that is empty every time. The cross-connection
/// defence (a diamond, or a replay on a second connection) is the ledger's
/// `served`/`debited` caches ([`crate::peer::lease::Ledger`]), which are
/// process-lifetime and are what actually makes one request one debit. This
/// check is the per-connection half and is kept as one: it costs nothing and
/// it is the half that survives the day a stream carries two requests.
///
/// **Checks 4 and 5 are not reachable through this signature**, and that is
/// reported to the lead rather than papered over: check 4 compares `via`
/// against THIS NODE'S OWN ID, which a [`PeerStore`] does not carry, and check
/// 5 needs the request-id cache, which is process state and not file state.
/// They are [`peer_stream_gate_hop`], the single implementation of those two,
/// and the serving path calls it immediately after this function. The two
/// functions share no check, so there is nothing here that can drift out of
/// step with a copy.
pub fn peer_stream_gate(
    peer: &PeerId,
    header: &StreamHeader,
    store: &PeerStore,
) -> Result<(), StreamRefusal> {
    let row = store.row(peer);
    peer_stream_gate_rows(header, row.as_ref())
}

/// Checks 1 to 3 against the pinned row itself: `None` is an unpinned peer,
/// which is the same answer as "not authorized for anything".
pub fn peer_stream_gate_rows(
    header: &StreamHeader,
    row: Option<&PeerRow>,
) -> Result<(), StreamRefusal> {
    // Check 2 runs on the kind before the grant is read, because an unknown
    // kind has no grant to read: refusal is the only safe answer, never a
    // degrade to a kind this build does know.
    if let StreamKind::Unknown(raw) = header.kind {
        return Err(StreamRefusal::UnknownKind(raw));
    }
    let Some(row) = row else {
        return Err(StreamRefusal::NotGranted(header.kind));
    };
    let granted = match (header.kind, &header.target) {
        // A pinned peer may always open CONTROL. What it is TOLD there is
        // assembled from its own `allow.control` grants by
        // [`hello_for_peer`], so an ungranted control stream learns booleans
        // and addresses and nothing else.
        (StreamKind::Control, _) => true,
        // A pinned Mac may park a carrier, and being pinned is the whole
        // admission. There is no grant to read because parking asks this node
        // for nothing it has not already agreed to: the socket is the far
        // Mac's, it is spent only by a forward that `allow.relay` has to admit
        // on its own terms, and what it costs here is a desk slot bounded by
        // `tunnel::MAX_PARKED_PER_PEER`. `park_reverse_carry` asks the same
        // question again against its own store, the belt and braces SERVE
        // documents, because a row can be forgotten between this frame and the
        // park.
        (StreamKind::Park, _) => true,
        (StreamKind::Serve, _) => row.allow.inspect,
        (StreamKind::Tunnel, Some(TunnelTarget::Origin { .. })) => row.allow.gateway,
        (StreamKind::Tunnel, Some(TunnelTarget::Peer { .. })) => row.allow.relay,
        // A tunnel with no target is not something this node can carry.
        (StreamKind::Tunnel, None) => false,
        (StreamKind::Unknown(_), _) => false,
    };
    if !granted {
        return Err(StreamRefusal::NotGranted(header.kind));
    }
    if header.hops_remaining == 0 {
        return Err(StreamRefusal::HopsExhausted);
    }
    Ok(())
}

/// Checks 4 and 5: the loop stamp and the dedup cache.
///
/// Separate from [`peer_stream_gate_rows`] only because the two pieces of state
/// they read (this node's own id, and a process-lifetime cache) are not in a
/// [`PeerStore`]. Called immediately after it, so the documented order holds.
pub fn peer_stream_gate_hop(
    node: &PeerId,
    header: &StreamHeader,
    dedup: &mut RequestDedup,
    now_ms: i64,
) -> Result<(), StreamRefusal> {
    if header.via.contains(node) {
        return Err(StreamRefusal::LoopDetected);
    }
    if !dedup.admit(header.request_id, now_ms) {
        return Err(StreamRefusal::Duplicate);
    }
    Ok(())
}

/// The request ids already served **on one connection**, for check 5.
///
/// Exists for ACCOUNTING, never for the replay defence: a `via` stamp cannot
/// catch a diamond, two disjoint paths converging on one terminal, and a
/// diamond served twice is two debits for one request. Bounded by
/// [`REQUEST_DEDUP_CAPACITY`] and [`REQUEST_DEDUP_TTL_MS`], because a cache a
/// peer can grow without bound is a memory bug with a security label.
///
/// # This cache is per connection, and a diamond does not arrive on one
///
/// Said plainly here rather than left for a third reader to
/// re-derive: [`Self::new`] is called inside `serve_stream`, so each accepted
/// stream gets an EMPTY cache, one stream carries one request, and
/// [`peer_stream_gate_hop`] asks [`Self::admit`] once, which therefore always
/// answers `true`. The two-debits-for-one-request hole this was filed against
/// is closed one layer down and process-wide, at the ledger's `served` and
/// `debited` caches ([`crate::peer::lease::Ledger::enter_relay`] and
/// [`crate::peer::lease::Ledger::debit`]), whose bounds are this type's own
/// ([`crate::peer::lease::SERVED_CAPACITY`] names them through it, so the two
/// layers cannot disagree about what a burst is).
///
/// So what lives here is the per-connection half: real the day a stream carries
/// more than one request, and not the thing standing between this lender and a
/// replayed debit today.
#[derive(Debug, Default)]
pub struct RequestDedup {
    seen: VecDeque<(u128, i64)>,
}

impl RequestDedup {
    /// An empty cache.
    pub fn new() -> Self {
        Self {
            seen: VecDeque::new(),
        }
    }

    /// Record `id` and report whether it is new. `false` means this request has
    /// already been served here.
    pub fn admit(&mut self, id: u128, now_ms: i64) -> bool {
        self.seen
            .retain(|(_, at)| now_ms.saturating_sub(*at) < REQUEST_DEDUP_TTL_MS);
        if self.seen.iter().any(|(seen, _)| *seen == id) {
            return false;
        }
        while self.seen.len() >= REQUEST_DEDUP_CAPACITY {
            self.seen.pop_front();
        }
        self.seen.push_back((id, now_ms));
        true
    }

    /// How many ids are remembered right now.
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    /// Whether nothing is remembered.
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

/// Everything this node could tell a peer about itself, before any grant is
/// consulted.
///
/// A struct rather than nine parameters, and deliberately **not** the `Hello`
/// itself: the optional blocks are assembled into a `Hello` by
/// [`hello_for_peer`] from the grants, so an ungranted figure is never
/// constructed into a message and then filtered out on the way past. A filter
/// is something a later edit forgets to apply.
pub struct NodeFacts {
    /// This node's id.
    pub node: PeerId,
    /// The operator's display name for this machine, already sanitized.
    pub label: String,
    /// Monotonic per sender.
    pub seq: u64,
    /// The three booleans every pinned peer is told.
    pub caps: Caps,
    /// Addresses this node can be reached on.
    pub addrs: Vec<String>,
    /// How long this advert may be believed.
    pub ttl_s: u32,
    /// Per-window lendable amounts. Needs `control.lendable`.
    pub lendable: Vec<Lendable>,
    /// Hops from here to egress. Needs `control.lendable`.
    pub hops_to_egress: Option<u8>,
    /// One level of neighbour briefs. Needs `control.briefs`.
    pub briefs: Vec<NeighborBrief>,
    /// This build's sha. Needs `control.diag`.
    pub build_sha: String,
    /// This boot's id. Needs `control.diag`.
    pub boot_id: u64,
    /// The source address this node saw the peer these facts are being
    /// assembled FOR arrive from, when it has seen one.
    ///
    /// The one field here that is about the reader. It is what makes
    /// [`tcr_peer_wire::Hello::observed_you_at`] answerable, and it is per
    /// peer, so it is set by the caller that knows which peer this Hello is
    /// for rather than read from a register inside the assembly.
    pub observed_peer_at: Option<std::net::SocketAddr>,
}

impl NodeFacts {
    /// The facts a node has before any of phase 4's accounting exists: an id,
    /// no capabilities, nothing countable.
    ///
    /// Announces no address either, which is why it is not what a listener
    /// sends: see [`Self::listening`].
    pub fn minimal(node: PeerId) -> Self {
        Self {
            node,
            label: String::new(),
            seq: 0,
            caps: Caps::default(),
            addrs: Vec::new(),
            ttl_s: HELLO_TTL_S,
            lendable: Vec::new(),
            hops_to_egress: None,
            briefs: Vec::new(),
            build_sha: String::new(),
            boot_id: 0,
            observed_peer_at: None,
        }
    }
}

impl NodeFacts {
    /// [`Self::minimal`], plus the socket this node actually listens on.
    ///
    /// **This is `Hello.addrs`'s producer, and for a long time there was
    /// none.** The field has been on the wire since the skeleton with nothing
    /// filling it, so a peer that moved had no way to say so and the far side
    /// went on dialling the address it was paired over until that failed.
    ///
    /// Two addresses at most, and never every address of every interface: the
    /// configured listen socket, which is what a peer on this LAN acts on, and
    /// the router mapping this node holds when `peer.internet` is on
    /// ([`crate::peer::reach::external_socket`]), which is what a peer off it
    /// acts on. Both are sockets a peer can dial. Reflexive discovery, meaning
    /// what a peer SEES this node as from the far side of a NAT it did not map
    /// itself, is still a different mechanism and is not here.
    pub fn listening(node: PeerId, listen: Option<std::net::SocketAddr>) -> Self {
        let mut facts = Self::minimal(node);
        facts.addrs = listen
            .map(|addr| vec![addr.to_string()])
            .unwrap_or_default();
        // The router mapping, when this node holds one. It goes AFTER the
        // configured socket rather than before it: a peer on the same LAN
        // should reach the LAN address first, and the mapped one is what is
        // left for a peer that is not.
        if let Some(mapped) = crate::peer::reach::external_socket() {
            let mapped = mapped.to_string();
            if !facts.addrs.contains(&mapped) {
                facts.addrs.push(mapped);
            }
        }
        facts
    }
}

/// How long a `Hello` may be believed, in seconds.
pub const HELLO_TTL_S: u32 = 60;

/// Assemble the `Hello` one peer is entitled to.
///
/// Assembled from the grant set rather than filtered on the way out. Without
/// the three `allow.control` booleans, pinning a peer would silently subscribe
/// it to per-window lendable amounts with account counts, this build's sha, its
/// boot id, and a list of this node's other peers.
///
/// **No credential and nothing countable can reach this message by accident**:
/// `tcr_peer_wire::Hello` has no token field and must never gain one, and every
/// countable block here is behind its own grant.
pub fn hello_for_peer(facts: &NodeFacts, grants: &ControlGrants) -> Hello {
    Hello {
        proto: PROTO_VERSION,
        node: facts.node,
        label: facts.label.clone(),
        seq: facts.seq,
        caps: facts.caps,
        addrs: facts.addrs.clone(),
        ttl_s: facts.ttl_s,
        lendable: grants.lendable.then(|| facts.lendable.clone()),
        hops_to_egress: grants.lendable.then_some(facts.hops_to_egress).flatten(),
        briefs: grants.briefs.then(|| {
            facts
                .briefs
                .iter()
                .take(MAX_NEIGHBOR_BRIEFS)
                .cloned()
                .collect()
        }),
        build_sha: grants.diag.then(|| facts.build_sha.clone()),
        boot_id: grants.diag.then_some(facts.boot_id),
        // Behind no grant: it is the reader's own public address, told back to
        // it, and a Mac that is told nothing here cannot punch at all.
        observed_you_at: facts.observed_peer_at.map(|addr| addr.to_string()),
    }
}

/// The secrets of every invite that is still outstanding, for the `IKpsk1`
/// trial. An expired row is not offered: its deadline is absolute, so a clock
/// that moved is not a second chance.
pub fn outstanding_secrets(file: &PeerFile) -> Vec<[u8; KEY_BYTES]> {
    let now = now_ms();
    file.pending_invites
        .iter()
        .filter(|invite| invite.uses_left > 0 && invite.expires_at_ms > now)
        .map(|invite| invite.secret)
        .collect()
}

/// Unix milliseconds, or 0 if the clock is before the epoch.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

/// Why a stream was refused. **Every arm closes the connection and logs.**
///
/// [`Self::UnknownKind`] is the one that must never degrade to anything else: a
/// newer peer on the same LAN will speak a kind this build does not know, the
/// PARSE deliberately survives that ([`StreamKind::Unknown`]), and a handler
/// that then treated it as a tunnel would be an open relay. Refusal is the only
/// safe answer here, the inverse of `ProxyHost::Unknown`
/// (`src/singleton.rs:105-125`), where degrading is what keeps a live proxy
/// from being signalled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamRefusal {
    /// The kind is not granted for this peer. The default for every grant is
    /// false, so this is what an unconfigured mesh answers to everything.
    NotGranted(StreamKind),
    /// A kind this build does not know.
    UnknownKind(u16),
    /// Hop budget spent.
    HopsExhausted,
    /// Our own id is already in `via`, a cycle, killed outright.
    LoopDetected,
    /// This request id has already been served **on this connection**. Exists
    /// for ACCOUNTING: a via stamp cannot catch a diamond (two disjoint paths
    /// converging on one terminal), and a diamond served twice is two debits
    /// for one request.
    ///
    /// Unreachable as the serving path is wired, one stream, one request, one
    /// empty cache, and the diamond is caught process-wide by the ledger
    /// instead. See [`RequestDedup`], which carries the whole of the review's
    /// H2b.
    Duplicate,
}

impl std::fmt::Display for StreamRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotGranted(kind) => write!(f, "peer stream refused: {kind:?} is not granted"),
            Self::UnknownKind(raw) => write!(
                f,
                "peer stream refused: stream kind {raw} is unknown to this build"
            ),
            Self::HopsExhausted => write!(f, "peer stream refused: hop budget spent"),
            Self::LoopDetected => write!(f, "peer stream refused: this node is already in `via`"),
            Self::Duplicate => write!(f, "peer stream refused: request id already served here"),
        }
    }
}

impl std::error::Error for StreamRefusal {}
