//! The blind carry: the requester's own TLS bytes spliced through a trusted
//! gateway, which sees a peer, a host, two byte counts and a duration.
//!
//! # It is already written, once, for loopback
//!
//! `src/mitm.rs`'s `tunnel()` is a `copy_bidirectional` splice whose own
//! doc-comment is the security argument verbatim: TLS is never terminated, the
//! client's end-to-end encryption to the real host is untouched, and no
//! credentials are involved. [`splice`] is that body, extracted over two
//! generic halves rather than two `TcpStream`s, because neither half of a peer
//! carry is a bare socket on both ends.
//!
//! # The adapter, which is the piece a plan assumes is free
//!
//! `snow` exposes a FRAME API over byte slices, not a stream, while
//! `copy_bidirectional` needs `AsyncRead + AsyncWrite + Unpin`. So the splice
//! needs [`NoiseStream`], an adapter that turns one authenticated Noise session
//! into a byte stream.
//!
//! It is built out of a `tokio::io::duplex` pair plus one pump task rather than
//! a hand-written `poll_read`/`poll_write` state machine, and that choice is
//! the whole of its correctness argument. A `poll_` implementation has to hold
//! a half-read length prefix, a half-written frame and a partially drained
//! plaintext buffer across returns, and every one of those is a
//! silent-corruption bug if it is wrong. The pump is sequential code in two
//! tasks: a framer that only ever reads whole frames off the socket, and a
//! mixer that owns the session and therefore is the only thing that encrypts or
//! decrypts. Both `select!` arms in the mixer are cancel-safe
//! (`mpsc::Receiver::recv` and `DuplexStream::read`), which is why the framer
//! exists at all, `recv_encrypted` reads a prefix and then a body, so
//! cancelling it mid-frame would desynchronise the stream, and a `select!` on
//! it would do exactly that.
//!
//! # The second far end: another pinned Mac
//!
//! A TUNNEL's target is an origin or a peer, and the peer form is the reach
//! story for two Macs that both moved: the requester asks a pinned friend with
//! a reachable address to splice it onward.
//! [`handle_forward_on`] is that half, and the three things that keep it from
//! being an open relay are all in [`authorize_forward`]: the requester holds
//! `allow.relay`, the TARGET is a row in this Mac's own peers file (so no
//! address a peer chose is ever dialled), and one hop is spent per forward
//! with zero refused.
//!
//! The onward socket is spliced RAW, no handshake, no framing, because the
//! payload is already a fresh Noise session between the requester and the
//! target. The target therefore authenticates the REQUESTER, this node holds
//! ciphertext, and the meter is the same two byte counts a gateway has.
//!
//! [`open_forward_to`] is the REQUESTER's half of the same picture, and it did
//! not exist for a long time: every gate on the forwarding path was built
//! and nothing in the tree ever opened a TUNNEL whose target was a peer, so a
//! peer reachable only through a friend was reachable by nobody.
//!
//! Forwarded bytes are charged to the REQUESTER out of the same
//! [`TunnelBudget`] a carry draws on, through the same [`reserve_for`]: one
//! cap, one hour, one peer. That is what makes a transitive forward grant
//! bounded rather than merely honest, the node two hops out spends the
//! grantee's hour and never a fresh one.
//!
//! # What a blind hop may validate, and the ceiling of that
//!
//! A host and a port, and then one thing more: **the first ClientHello's SNI
//! must equal the CONNECTed host** ([`assert_sni_matches_target`]), so a
//! granted peer cannot domain-front to some other service behind the same
//! address. A gateway cannot do better than that without terminating TLS, at
//! which point it is not blind, so this is the ceiling, and the ceiling ships
//! with the floor rather than as a later optional phase.
//!
//! # What it meters
//!
//! Bytes, streams and wall time. **Never quota**, it cannot see a model, a
//! path, an account or a response header, and a design that let it meter quota
//! would be admitting it can see. The two counters `copy_bidirectional`
//! returns are the meter, plus a cap ([`TunnelBudget`]).
//!
//! The cap is enforced twice and neither is redundant. At admission, a peer
//! already over its hour is refused before a socket to the origin is opened
//! ([`TunnelBudget::admit`]); and during the splice, the peer half is wrapped
//! in [`Metered`] with this carry's allowance, so one stream that never ends
//! cannot spend an unbounded number of bytes under one admission. An
//! admission-only cap is a cap on stream count, which is not what the operator
//! asked for.
//!
//! **Admission counts the carries that are still OPEN, not only the ones that
//! closed.** A reservation is taken at admission and released at the end of the
//! carry, so `cap` bounds concurrent traffic too: without it, five carries
//! opened at once were each handed the whole remaining hour and could spend it
//! five times over. [`MAX_OPEN_CARRIES_PER_PEER`] is the concurrency bound that
//! falls out of that arithmetic.
//!
//! A gateway log line carries the peer, the target host, bytes up, bytes down
//! and milliseconds, and NOTHING else, asserted by a test
//! (`tests/peer_egress.rs`). A gateway is the one place where a lazy log line
//! would leak a peer's entire request stream.

use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use anyhow::{anyhow, bail, Context as _, Result};
use tcr_peer_wire::{PeerId, StreamHeader, StreamKind, TunnelTarget};
use tokio::io::{
    AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, DuplexStream, ReadBuf,
};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::peer::config::{Locator, PeerRow, PeerStore};
use crate::peer::noise::{self, PeerSession};

/// Plaintext chunk size for a splice: well under the frame ceiling, so one
/// large body cannot monopolise the stream.
pub const CHUNK_BYTES: usize = 16 * 1024;

/// How many bytes one peer may spend on carried traffic per rolling hour,
/// absent an operator setting.
///
/// Two gigabytes, which is large enough that no real request run notices it and
/// small enough that a peer that has gone wrong stops within one hour rather
/// than within one month. The operator's own value is `maxTunnelBytesPerHour`
/// in `tcr-peers.json`; that key is not yet in
/// [`crate::peer::config::PeerFile`], which is why this constant is here.
pub const DEFAULT_MAX_TUNNEL_BYTES_PER_HOUR: u64 = 2 * 1024 * 1024 * 1024;

/// The rolling window the byte cap is measured over, in milliseconds.
pub const BUDGET_WINDOW_MS: i64 = 60 * 60 * 1000;

/// How many carries one peer may hold OPEN against this gateway at once.
///
/// The cap is per peer per rolling hour, and it was once charged
/// only when a tunnel CLOSED, so N concurrent carries were each admitted with
/// the whole remaining hour as their allowance and could spend it N times over.
/// An open carry now reserves its slice up front
/// ([`TunnelBudget::admit`]), which makes this constant the concurrency bound
/// that falls out of the arithmetic: one peer gets at most four open carries,
/// each holding a quarter of what is left of its hour.
///
/// Four rather than one: a Claude Code session opens several requests at once
/// and a gateway that serialised them would look like a hang. Four rather than
/// sixteen: each reservation is a quarter of the hour's remainder, and a slice
/// too thin to carry one streamed response would fail carries mid-stream, which
/// is worse than refusing the fifth outright.
pub const MAX_OPEN_CARRIES_PER_PEER: u64 = 4;

/// The most bytes a gateway will buffer while it waits for a whole ClientHello.
///
/// A TLS handshake record is bounded at 16 KiB by the record layer, and the
/// ClientHello this checks is a few hundred bytes. The bound exists so a peer
/// that opens a tunnel and then dribbles bytes forever cannot make the gateway
/// grow a buffer.
pub const MAX_CLIENT_HELLO_BYTES: usize = 16 * 1024;

/// How long a requester has to deliver a complete first TLS record before the
/// gateway closes on it.
///
/// Five seconds, the number `abuse-resistance.md` already gives an
/// unauthenticated message 1, for the same reason one step later: a peer that
/// opens a carry and then goes quiet is holding a task and a socket on somebody
/// else's Mac, and being pinned does not make that free.
pub const FIRST_RECORD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

// ---------------------------------------------------------------------------
// The Noise session as a byte stream
// ---------------------------------------------------------------------------

/// A Noise session as an `AsyncRead + AsyncWrite` stream.
///
/// The adapter named in the module docs. It exists so the splice can be ONE
/// function over two generic halves instead of one function per pairing of
/// concrete socket types.
///
/// Reading yields the plaintext the peer sent; writing encrypts and frames.
/// A write shorter than [`CHUNK_BYTES`] is one frame, there is no coalescing,
/// because a splice that waited for a full chunk would stall a TLS handshake
/// that is a few hundred bytes long and then silent.
pub struct NoiseStream {
    /// This side of the plaintext duplex. The pump owns the other side.
    plaintext: DuplexStream,
    /// The pump, so its error can be collected by [`Self::finish`] rather than
    /// dropped on the floor.
    pump: tokio::task::JoinHandle<Result<()>>,
}

impl NoiseStream {
    /// Start pumping `session` over `socket` and return the plaintext side.
    ///
    /// Two tasks, for the cancel-safety reason in the module docs: a framer
    /// that reads whole frames off the socket's read half and nothing else, and
    /// a mixer that owns the session and is therefore the only thing that
    /// touches the cipher.
    pub fn start<S>(socket: S, mut session: PeerSession) -> Self
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (mine, theirs) = tokio::io::duplex(CHUNK_BYTES * 4);
        let pump = tokio::spawn(async move {
            let (mut read_half, mut write_half) = tokio::io::split(socket);
            // One frame in flight plus one queued: enough that the framer is
            // never the bottleneck, bounded so a fast peer cannot make this
            // node buffer its stream in memory.
            let (frames_tx, mut frames_rx) = mpsc::channel::<Vec<u8>>(2);
            let framer = tokio::spawn(async move {
                loop {
                    match noise::read_frame(&mut read_half).await {
                        Ok(frame) => {
                            if frames_tx.send(frame).await.is_err() {
                                return;
                            }
                        }
                        // EOF and a torn frame are the same fact here, the
                        // session is over. The mixer reports it, because the
                        // mixer is the half with the context.
                        Err(_) => return,
                    }
                }
            });

            let mut plain = theirs;
            let mut from_local = vec![0_u8; CHUNK_BYTES];
            let outcome = loop {
                tokio::select! {
                    frame = frames_rx.recv() => {
                        let Some(frame) = frame else { break Ok(()) };
                        let mut out = vec![0_u8; tcr_peer_wire::MAX_FRAME_BYTES];
                        let len = match session.transport.read_message(&frame, &mut out) {
                            Ok(len) => len,
                            Err(err) => break Err(anyhow!(
                                "peer tunnel: a carried frame did not decrypt under the \
                                 session key: {err}"
                            )),
                        };
                        out.truncate(len);
                        if plain.write_all(&out).await.is_err() {
                            // The local end went away. Not an error: it is how
                            // a finished request looks.
                            break Ok(());
                        }
                    }
                    read = plain.read(&mut from_local) => {
                        match read {
                            Ok(0) => break Ok(()),
                            Ok(n) => {
                                if let Err(err) = noise::send_encrypted(
                                    &mut write_half,
                                    &mut session.transport,
                                    &from_local[..n],
                                )
                                .await
                                {
                                    break Err(err.context(
                                        "peer tunnel: the carried stream could not be written",
                                    ));
                                }
                            }
                            Err(err) => break Err(anyhow::Error::new(err)
                                .context("peer tunnel: the local end of the carry failed")),
                        }
                    }
                }
            };
            // Half-close the socket so the far end sees the end of the stream
            // rather than a hang, then stop the framer.
            let _shutdown = write_half.shutdown().await;
            framer.abort();
            outcome
        });
        Self {
            plaintext: mine,
            pump,
        }
    }

    /// Wait for the pump to stop and surface whatever ended it.
    ///
    /// The one place a carried stream's failure becomes visible. A caller that
    /// drops a [`NoiseStream`] instead gets the pump's log line and nothing
    /// else, which is why the splice callers here all call this.
    pub async fn finish(self) -> Result<()> {
        // Dropping our half first is what tells the pump to stop; without it a
        // pump whose peer is silent would be joined forever.
        drop(self.plaintext);
        self.pump
            .await
            .context("peer tunnel: the carry task did not finish cleanly")?
    }
}

impl AsyncRead for NoiseStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.plaintext).poll_read(cx, buf)
    }
}

impl AsyncWrite for NoiseStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.plaintext).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.plaintext).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.plaintext).poll_shutdown(cx)
    }
}

// ---------------------------------------------------------------------------
// The cap, as a stream wrapper
// ---------------------------------------------------------------------------

/// A stream with a byte allowance: once the allowance is spent, reads and
/// writes fail instead of continuing.
///
/// The second half of the byte cap. [`TunnelBudget::admit`] answers "may this
/// peer open another tunnel"; this answers "may this tunnel keep going", which
/// is the question an admission check cannot ask. Both directions draw on one
/// allowance because both cost the gateway's uplink.
pub struct Metered<S> {
    inner: S,
    remaining: Arc<AtomicU64>,
}

impl<S> Metered<S> {
    /// Wrap `inner` with `allowance` bytes, counting both directions.
    pub fn new(inner: S, allowance: u64) -> Self {
        Self {
            inner,
            remaining: Arc::new(AtomicU64::new(allowance)),
        }
    }

    /// A handle on the same allowance, for a caller that wants to read how much
    /// is left after the splice returns.
    pub fn remaining(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.remaining)
    }

    /// Spend `n` bytes, reporting whether the allowance covered them.
    fn spend(&self, n: usize) -> bool {
        let n = n as u64;
        loop {
            let left = self.remaining.load(Ordering::Acquire);
            if left < n {
                self.remaining.store(0, Ordering::Release);
                return false;
            }
            if self
                .remaining
                .compare_exchange(left, left - n, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return true;
            }
        }
    }
}

/// The error a spent allowance produces. Its own function so the gateway's
/// refusal and the test that drives it agree on one string.
fn over_budget() -> io::Error {
    io::Error::other("peer tunnel: this peer's carried-byte allowance for the hour is spent")
}

impl<S: AsyncRead + Unpin> AsyncRead for Metered<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let poll = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &poll {
            let read = buf.filled().len().saturating_sub(before);
            if read > 0 && !self.spend(read) {
                return Poll::Ready(Err(over_budget()));
            }
        }
        poll
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Metered<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let poll = Pin::new(&mut self.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(written)) = &poll {
            let written = *written;
            if written > 0 && !self.spend(written) {
                return Poll::Ready(Err(over_budget()));
            }
        }
        poll
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

// ---------------------------------------------------------------------------
// The splice
// ---------------------------------------------------------------------------

/// Splice two streams until either closes, returning `(bytes_a_to_b,
/// bytes_b_to_a)`, the two figures the gateway meters and the only two it has.
///
/// `src/mitm.rs:805`'s body, over two generic halves. **Two `TcpStream`
/// parameters would not do**: neither end
/// of a peer carry is a bare socket on both sides (the gateway splices a
/// [`NoiseStream`] against a `TcpStream`, the requester splices a loopback
/// `TcpStream` against a [`NoiseStream`]), so a `TcpStream`-concrete splice has
/// no caller. Widening it keeps every `TcpStream` call site compiling.
pub async fn splice<A, B>(mut a: A, mut b: B) -> Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    tokio::io::copy_bidirectional_with_sizes(&mut a, &mut b, CHUNK_BYTES, CHUNK_BYTES)
        .await
        .context("peer tunnel: the splice failed mid-stream")
}

// ---------------------------------------------------------------------------
// The one thing a blind hop may check
// ---------------------------------------------------------------------------

/// Refuse a tunnel whose first ClientHello names a host other than the one it
/// asked to CONNECT to.
///
/// Reads the SNI out of the first handshake record without terminating
/// anything.
///
/// A ClientHello with no SNI at all is refused as well: the gateway's whole
/// claim is that it carried bytes to the host it was asked for, and it cannot
/// make that claim about a handshake that names nobody.
pub fn assert_sni_matches_target(first_bytes: &[u8], target_host: &str) -> Result<()> {
    let Some(sni) = client_hello_sni(first_bytes)? else {
        bail!(
            "peer tunnel: this ClientHello carries no server name, so nothing here can say it \
             is bound for {target_host}; closing"
        );
    };
    // ASCII case-insensitive: a DNS name is case-insensitive and a peer that
    // sends `API.Anthropic.com` is not domain-fronting.
    if !sni.eq_ignore_ascii_case(target_host) {
        bail!(
            "peer tunnel: this stream asked to be carried to {target_host} and its ClientHello \
             names {sni}; closing rather than carrying bytes to a host this gateway never \
             allowed"
        );
    }
    Ok(())
}

/// The SNI out of a TLS ClientHello, or `None` when the handshake carries no
/// server-name extension.
///
/// Hand-parsed rather than handed to rustls, and the reason is the whole point
/// of this file: a gateway that built a TLS acceptor would be terminating the
/// session it promises not to read. This walks the record and the extension
/// vector with bounds checks and copies out one string.
///
/// `Err` is "these bytes are not a ClientHello", which is a different fact from
/// `Ok(None)` ("a ClientHello with no server name") and gets a different
/// message at the call site.
pub fn client_hello_sni(bytes: &[u8]) -> Result<Option<String>> {
    /// A field read that says which field ran off the end.
    fn take<'a>(bytes: &'a [u8], at: usize, len: usize, what: &str) -> Result<&'a [u8]> {
        bytes
            .get(at..at + len)
            .ok_or_else(|| anyhow!("peer tunnel: this is not a ClientHello ({what} is truncated)"))
    }
    fn be16(bytes: &[u8], at: usize, what: &str) -> Result<usize> {
        let field = take(bytes, at, 2, what)?;
        Ok(usize::from(u16::from_be_bytes([field[0], field[1]])))
    }

    // TLS record header: content type 22 (handshake), version, length.
    let header = take(bytes, 0, 5, "the record header")?;
    if header[0] != 0x16 {
        bail!(
            "peer tunnel: the first carried bytes are record type {}, not a TLS handshake (22)",
            header[0]
        );
    }
    let record_len = usize::from(u16::from_be_bytes([header[3], header[4]]));
    let body = bytes
        .get(5..5 + record_len)
        .ok_or_else(|| anyhow!("peer tunnel: the ClientHello record is not complete yet"))?;

    // Handshake header: type 1 (client_hello), u24 length.
    if body.first() != Some(&0x01) {
        bail!("peer tunnel: the first handshake message is not a ClientHello");
    }
    // legacy_version (2) + random (32)
    let mut at = 4 + 2 + 32;
    let session_id_len = usize::from(
        *take(body, at, 1, "the session id length")?
            .first()
            .unwrap_or(&0),
    );
    at += 1 + session_id_len;
    let cipher_suites_len = be16(body, at, "the cipher suite length")?;
    at += 2 + cipher_suites_len;
    let compression_len = usize::from(
        *take(body, at, 1, "the compression method length")?
            .first()
            .unwrap_or(&0),
    );
    at += 1 + compression_len;
    if body.len() <= at {
        // A ClientHello with no extension vector at all. Legal TLS 1.2, and it
        // names no server.
        return Ok(None);
    }
    let extensions_len = be16(body, at, "the extension vector length")?;
    at += 2;
    let extensions = take(body, at, extensions_len, "the extension vector")?;

    let mut cursor = 0_usize;
    while cursor + 4 <= extensions.len() {
        let kind = u16::from_be_bytes([extensions[cursor], extensions[cursor + 1]]);
        let len = usize::from(u16::from_be_bytes([
            extensions[cursor + 2],
            extensions[cursor + 3],
        ]));
        let value = take(extensions, cursor + 4, len, "an extension body")?;
        cursor += 4 + len;
        // 0 is server_name.
        if kind != 0 {
            continue;
        }
        // server_name_list: u16 length, then entries of {u8 type, u16 length}.
        let list_len = be16(value, 0, "the server name list length")?;
        let list = take(value, 2, list_len, "the server name list")?;
        let mut entry = 0_usize;
        while entry + 3 <= list.len() {
            let name_type = list[entry];
            let name_len = usize::from(u16::from_be_bytes([list[entry + 1], list[entry + 2]]));
            let name = take(list, entry + 3, name_len, "a server name")?;
            entry += 3 + name_len;
            // 0 is host_name, the only type ever assigned.
            if name_type != 0 {
                continue;
            }
            let host = std::str::from_utf8(name)
                .context("peer tunnel: the server name in this ClientHello is not UTF-8")?;
            return Ok(Some(host.to_string()));
        }
        return Ok(None);
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// The byte cap
// ---------------------------------------------------------------------------

/// What this node has carried and forwarded for each peer inside the rolling
/// hour.
///
/// Both kinds of stream draw on one ledger and one cap, charged to the peer
/// whose handshake this node checked: a carry's bytes and a forward's bytes
/// cost the same uplink, and a peer that could spend its hour twice by asking
/// for the other one would have no cap at all.
///
/// In memory and per process, deliberately: a byte cap exists to stop a peer
/// that has gone wrong inside one hour, and a restart of `tcr` on this machine
/// is the operator's own act, not the peer's. Persisting it would put a
/// per-peer traffic history on disk, which is a record of when somebody else's
/// machine was working and is exactly the kind of thing this feature promises
/// not to keep.
/// # Open carries count against the hour too
///
/// A closed tunnel's bytes are in [`Self::spent`]; an OPEN one's reservation is
/// in [`Self::open`], and [`Self::admit`] subtracts both. Without the second
/// half the cap was only ever a cap on FINISHED traffic: five carries opened at
/// once were each handed the whole remaining hour as their allowance, and five
/// streams could spend it five times over before any of them closed. The
/// reservation is released (and the real spend charged) by
/// [`Self::close`].
#[derive(Debug, Default)]
pub struct TunnelBudget {
    /// One entry per admitted tunnel: when it closed, and what it spent.
    spent: VecDeque<(i64, PeerId, u64)>,
    /// One entry per tunnel that is open right now: its id, whose it is, and
    /// the slice of the hour it holds.
    open: Vec<(OpenCarryId, PeerId, u64)>,
    /// The next open-carry id. Monotonic per process, never reused, so a
    /// release cannot free somebody else's reservation.
    next_open: u64,
}

/// A handle on one open carry's reservation, handed out by
/// [`TunnelBudget::admit`] and spent by [`TunnelBudget::close`].
///
/// Opaque and not constructible outside this module: a release is only ever
/// the release of a reservation this ledger actually made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenCarryId(u64);

/// A gateway's answer to "may this peer open another tunnel".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Carry it, with this many bytes of allowance.
    Carry {
        /// This carry's slice of the hour, in bytes: one
        /// `cap / MAX_OPEN_CARRIES_PER_PEER` slice, or whatever is left of the
        /// hour if that is less.
        allowance: u64,
        /// The reservation this admission took. [`TunnelBudget::close`] must
        /// be given it, or the slice stays held until the process restarts.
        open: OpenCarryId,
    },
    /// Refuse it: the hour's cap is spent or reserved.
    OverBudget {
        /// What the peer has already spent inside the window, plus what its
        /// open carries hold in reserve.
        spent: u64,
        /// The cap it is measured against.
        cap: u64,
    },
}

impl TunnelBudget {
    /// An empty ledger.
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop everything older than one window, so the hour rolls.
    fn trim(&mut self, now_ms: i64) {
        let floor = now_ms - BUDGET_WINDOW_MS;
        while self.spent.front().is_some_and(|(at, _, _)| *at < floor) {
            self.spent.pop_front();
        }
    }

    /// What `peer` has spent inside the rolling hour ending at `now_ms`.
    pub fn spent_by(&mut self, peer: &PeerId, now_ms: i64) -> u64 {
        self.trim(now_ms);
        self.spent
            .iter()
            .filter(|(_, who, _)| who == peer)
            .map(|(_, _, bytes)| *bytes)
            .sum()
    }

    /// What `peer`'s currently-open carries hold in reserve.
    pub fn reserved_by(&self, peer: &PeerId) -> u64 {
        self.open
            .iter()
            .filter(|(_, who, _)| who == peer)
            .map(|(_, _, bytes)| *bytes)
            .sum()
    }

    /// The slice of `cap` one open carry holds.
    ///
    /// A fixed fraction of the CAP rather than of what is left, which is what
    /// makes [`MAX_OPEN_CARRIES_PER_PEER`] a real bound: slicing the remainder
    /// instead would halve forever and admit an unbounded number of carries,
    /// each thinner than the last.
    fn slice(cap: u64) -> u64 {
        (cap / MAX_OPEN_CARRIES_PER_PEER).max(1)
    }

    /// May `peer` open another tunnel, and with how much allowance.
    ///
    /// Spent bytes AND open reservations are both subtracted, so the answer is
    /// about the hour and not merely about the carries that happen to have
    /// finished. An `Admission::Carry` has taken a reservation by the time it
    /// is returned: pass its [`OpenCarryId`] to [`Self::close`].
    pub fn admit(&mut self, peer: &PeerId, cap: u64, now_ms: i64) -> Admission {
        let spent = self.spent_by(peer, now_ms);
        let held = spent.saturating_add(self.reserved_by(peer));
        let allowance = match cap.checked_sub(held) {
            Some(0) | None => return Admission::OverBudget { spent: held, cap },
            Some(left) => left.min(Self::slice(cap)),
        };
        let open = OpenCarryId(self.next_open);
        self.next_open = self.next_open.saturating_add(1);
        self.open.push((open, *peer, allowance));
        Admission::Carry { allowance, open }
    }

    /// Record what a closed tunnel spent.
    ///
    /// Callers that took a reservation use [`Self::close`] instead; this is the
    /// charge half on its own, kept public for a caller that is accounting for
    /// bytes it never admitted.
    pub fn charge(&mut self, peer: &PeerId, bytes: u64, now_ms: i64) {
        self.trim(now_ms);
        self.spent.push_back((now_ms, *peer, bytes));
    }

    /// Release `open`'s reservation and charge what its carry actually spent.
    ///
    /// One method rather than a `release` beside a `charge`, because the two
    /// must happen together: a release without a charge gives the hour back to
    /// a peer that spent it, and a charge without a release holds a slice for
    /// ever.
    ///
    /// An id that is not open is a no-op charge of `bytes` against `peer`,
    /// double-closing cannot free a second reservation.
    pub fn close(&mut self, open: OpenCarryId, peer: &PeerId, bytes: u64, now_ms: i64) {
        self.open.retain(|(id, _, _)| *id != open);
        self.charge(peer, bytes, now_ms);
    }
}

// ---------------------------------------------------------------------------
// The per-path meter
// ---------------------------------------------------------------------------

/// One charge against one path, as the meter holds it.
///
/// Bytes and tokens on ONE row rather than two windows, because they are two
/// measurements of the same event, this Mac used that path, and two windows
/// would roll off at two instants and disagree about which hour a carry was in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PathCharge {
    at_ms: i64,
    peer: PeerId,
    path: Locator,
    bytes: u64,
    tokens: u64,
}

/// What each PATH to each peer carried inside the rolling hour, beside the
/// per-peer [`TunnelBudget`] that caps it.
///
/// # Why it is beside the budget and not inside it
///
/// The budget answers "may this peer open another tunnel", and the only key
/// that question has is the peer whose handshake this node checked: a cap that
/// could be spent once per path would be no cap at all, which is the sentence
/// [`TunnelBudget`]'s own doc already makes about carries and forwards. This
/// answers a different question, which WAY the bytes went, and it is a
/// measurement with no authority over anything. Two keys, two types; folding
/// the second key into the ledger would make the cap depend on an attribution
/// that is sometimes unavailable.
///
/// # What is charged, and what is honestly not
///
/// A charge needs a locator this Mac can compare against a row's endpoints, so
/// the meter is fed from the places that CHOSE a path: the forwarder, which
/// dialled the target's own address ([`handle_forward_on`]), and the ledger,
/// for a lease whose serving path was noted. A gateway carry is not charged
/// here at all: the requester dialled US, the source socket's ephemeral port
/// is no endpoint of any row, and attributing it by guess is the exact thing
/// [`crate::status::PathStatus::bytes_per_hour`] refused to do while it was
/// `None`.
///
/// # In memory, per process, like the budget
///
/// For the same reason, said once there: a per-path traffic history on disk is
/// a record of when somebody else's machine was working. What reaches the state
/// file is [`Self::rows`]' two totals and one timestamp.
#[derive(Debug, Default)]
pub struct PathMeter {
    /// Newest last, so the roll-off is a pop from the front.
    charges: VecDeque<PathCharge>,
    /// Every (peer, path) this meter has ever charged, kept after its charges
    /// roll off.
    ///
    /// Without it a path that carried nothing THIS hour is indistinguishable
    /// from a path nothing has ever measured, because both hold no charges,
    /// and those are the two answers
    /// [`crate::status::PathStatus::bytes_per_hour`] exists to keep apart. The
    /// charge is what rolls; the fact that this Mac meters this path does not.
    ///
    /// Bounded by the operator's own peers file: a key is a row's endpoint, and
    /// a row holds at most [`crate::peer::config::MAX_ENDPOINTS_PER_PEER`] of
    /// them. Nothing a peer sends can add one.
    seen: Vec<(PeerId, Locator)>,
}

impl PathMeter {
    /// An empty meter.
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop everything older than one window, so the hour rolls.
    ///
    /// [`BUDGET_WINDOW_MS`] and not a window of its own: an operator reading
    /// "carried this hour" beside "may carry this hour" must not be reading two
    /// different hours.
    fn trim(&mut self, now_ms: i64) {
        let floor = now_ms - BUDGET_WINDOW_MS;
        while self.charges.front().is_some_and(|row| row.at_ms < floor) {
            self.charges.pop_front();
        }
    }

    /// Charge bytes carried over one path.
    pub fn charge_bytes(&mut self, peer: &PeerId, path: Locator, bytes: u64, now_ms: i64) {
        self.charge(peer, path, bytes, 0, now_ms);
    }

    /// Charge tokens drawn over one path.
    pub fn charge_tokens(&mut self, peer: &PeerId, path: Locator, tokens: u64, now_ms: i64) {
        self.charge(peer, path, 0, tokens, now_ms);
    }

    fn charge(&mut self, peer: &PeerId, path: Locator, bytes: u64, tokens: u64, now_ms: i64) {
        self.trim(now_ms);
        if !self.seen.contains(&(*peer, path)) {
            self.seen.push((*peer, path));
        }
        self.charges.push_back(PathCharge {
            at_ms: now_ms,
            peer: *peer,
            path,
            bytes,
            tokens,
        });
    }

    /// Bytes carried over one path inside the hour ending at `now_ms`.
    pub fn bytes_last_hour(&mut self, peer: &PeerId, path: &Locator, now_ms: i64) -> u64 {
        self.trim(now_ms);
        self.sum(peer, path, |row| row.bytes)
    }

    /// Tokens drawn over one path inside the hour ending at `now_ms`.
    pub fn tokens_last_hour(&mut self, peer: &PeerId, path: &Locator, now_ms: i64) -> u64 {
        self.trim(now_ms);
        self.sum(peer, path, |row| row.tokens)
    }

    fn sum(&self, peer: &PeerId, path: &Locator, of: impl Fn(&PathCharge) -> u64) -> u64 {
        self.charges
            .iter()
            .filter(|row| row.peer == *peer && row.path == *path)
            .fold(0_u64, |total, row| total.saturating_add(of(row)))
    }

    /// One row per (peer, path) this meter has charged inside the hour, in the
    /// shape the state file keeps.
    ///
    /// A path that was charged and has since rolled off comes back with ZEROES
    /// rather than being dropped, for the reason
    /// [`crate::peer::state::PathTraffic`] gives: a reader must be able to tell
    /// "carried nothing this hour" from "nothing measures this path". Only the
    /// charges are trimmed; [`Self::seen`] is what the meter keeps answering
    /// about.
    pub fn rows(&mut self, now_ms: i64) -> Vec<crate::peer::state::PathTraffic> {
        self.trim(now_ms);
        self.seen
            .clone()
            .into_iter()
            .map(|(peer, path)| crate::peer::state::PathTraffic {
                peer,
                locator: path,
                bytes_last_hour: self.sum(&peer, &path, |row| row.bytes),
                tokens_last_hour: self.sum(&peer, &path, |row| row.tokens),
                updated_at_ms: now_ms,
            })
            .collect()
    }
}

/// The one meter this process charges, for the reason
/// [`crate::peer::lease::handed_tokens`] is a process-local store: the
/// alternative is a handle threaded from boot through the forwarder and the
/// ledger, and a handle the ledger can reach is a handle
/// [`crate::config::Config`] can reach.
///
/// Nothing here writes the state file. Summing the meter into
/// [`crate::peer::state::save_path_traffic`] is the serving process's act, on
/// its own schedule, exactly as the prober's costs reach
/// [`crate::peer::state::save_paths`].
pub fn path_meter() -> &'static std::sync::Mutex<PathMeter> {
    static METER: std::sync::OnceLock<std::sync::Mutex<PathMeter>> = std::sync::OnceLock::new();
    METER.get_or_init(|| std::sync::Mutex::new(PathMeter::new()))
}

// ---------------------------------------------------------------------------
// The gateway's half
// ---------------------------------------------------------------------------

/// How a gateway turns the target host into a socket.
///
/// The two-function seam this file needs, and the reason it exists is that the
/// allow-list is by NAME: an origin a gateway will carry to is
/// `api.anthropic.com`, so a test cannot stand one up without either editing
/// the machine's resolver or being allowed to say where the name lives for the
/// length of one test. Production is always [`Self::Resolve`]; nothing in
/// `src/` constructs the other arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginRoute {
    /// Resolve the target host and dial it. The only arm production uses.
    Resolve,
    /// Dial this address instead, still carrying only the allow-listed host and
    /// still requiring the ClientHello to name it.
    Fixed(std::net::SocketAddr),
    /// This carry's far end is not the internet at all: it is a peer THIS Mac
    /// has pinned, dialled on the addresses its own row carries
    /// ([`dial_pinned_peer`]), and the stream is [`handle_forward_on`]'s.
    ///
    /// The id is here as well as in [`TunnelTarget::Peer`] on purpose: a route
    /// is what this node DECIDED, a target is what a peer ASKED for, and
    /// [`authorize_forward`] refuses the two when they disagree rather than
    /// letting either one alone pick the socket. There is no arm that carries
    /// an address a peer chose, that is the difference between a forwarder and
    /// an open relay.
    Peer(PeerId),
}

/// What a gateway needs besides the stream: who asked, where for, and what it
/// may spend.
pub struct Carry<'a> {
    /// How the target host becomes a socket. [`OriginRoute::Resolve`] in
    /// production.
    pub route: OriginRoute,
    /// The peer that asked, as its handshake proved it.
    pub peer: PeerId,
    /// The target the header declared.
    pub target: &'a TunnelTarget,
    /// The origins this gateway will carry to.
    pub hosts: &'a [&'a str],
    /// The per-peer per-hour byte cap.
    pub cap_bytes: u64,
    /// The gateway's own ledger for the hour.
    pub budget: &'a std::sync::Mutex<TunnelBudget>,
    /// Now, in Unix milliseconds.
    pub now_ms: i64,
}

/// Take this stream's slice of the requester's hour, or refuse it before
/// anything is dialled.
///
/// Shared by the gateway and the forwarder, because the cap is ONE cap: a
/// forwarded byte and a carried byte are the same byte off the same peer's
/// hour, charged to the peer whose handshake this node checked. A second copy
/// of this arithmetic would be a second answer to one question, and the answer
/// that matters, how much is left, is the one an operator set a number for.
fn reserve_for(carry: &Carry<'_>) -> Result<(u64, OpenCarryId)> {
    let mut budget = carry
        .budget
        .lock()
        .map_err(|_| anyhow!("peer tunnel: the byte ledger lock is poisoned"))?;
    match budget.admit(&carry.peer, carry.cap_bytes, carry.now_ms) {
        Admission::Carry { allowance, open } => Ok((allowance, open)),
        Admission::OverBudget { spent, cap } => {
            // One line, and it names no bytes of the peer's stream, there is
            // no stream yet.
            tracing::warn!(
                peer = %carry.peer.display(),
                spent_bytes = spent,
                cap_bytes = cap,
                "peer tunnel: refused, this peer's carried-byte cap for the hour is spent"
            );
            bail!(
                "peer tunnel: {} has spent {spent} of {cap} carried bytes this hour; refused",
                carry.peer.display()
            );
        }
    }
}

/// One open carry's reservation, released on every exit path.
///
/// A guard rather than a `close` call at the end of the happy path, because
/// most of the exits between admission and the splice are `?` and `bail!`: a
/// mismatched SNI, an origin that cannot be reached, a requester that goes
/// quiet. Every one of those used to leave nothing behind because nothing was
/// reserved; now each would strand a slice of the peer's hour until the process
/// restarted.
///
/// [`Self::spent`] is what will be charged, and the splice sets it just before
/// this guard goes out of scope. A refusal charges zero, which is right: no
/// bytes were carried.
struct CarryReservation<'a> {
    budget: &'a std::sync::Mutex<TunnelBudget>,
    peer: PeerId,
    id: OpenCarryId,
    now_ms: i64,
    /// Bytes to charge on release.
    spent: u64,
}

impl Drop for CarryReservation<'_> {
    fn drop(&mut self) {
        match self.budget.lock() {
            Ok(mut budget) => budget.close(self.id, &self.peer, self.spent, self.now_ms),
            // Never silent: a poisoned ledger means this peer's slice stays
            // held, which an operator sees as a gateway that refuses carries,
            // and this line is the only thing that explains it.
            Err(_) => tracing::error!(
                peer = %self.peer.display(),
                "peer tunnel: the byte ledger lock is poisoned, so this carry's reservation \
                 could not be released; carries for this peer will be refused until restart"
            ),
        }
    }
}

/// Carry one TUNNEL stream: the gateway's half of blind egress.
///
/// The order of the four checks is the design. The allow-list and the byte cap
/// are answered before a socket to the origin exists, so a refused peer costs
/// this gateway one connection and no outbound traffic at all; the SNI check
/// needs the requester's first bytes and therefore comes after them, and still
/// before the origin is dialled, so a domain-fronting attempt never reaches the
/// network either.
///
/// `TunnelTarget::Peer` is refused here rather than silently treated as an
/// origin, and it still is now that forwarding exists: a forward needs the
/// peers file, the requester's `allow.relay` and the target's own row, and
/// this entry point is handed none, so it could not answer either question. The
/// forwarder is [`handle_forward_on`], which takes a [`Forward`] carrying that
/// file. A gateway that quietly relayed would be an open relay with a gateway's
/// grant.
pub async fn handle_tunnel_on<S>(
    stream: S,
    session: PeerSession,
    carry: Carry<'_>,
) -> Result<(u64, u64)>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let TunnelTarget::Origin { host, port } = carry.target else {
        bail!(
            "peer tunnel: {} asked this Mac to relay to another peer, and this entry point holds \
             no peers file, so it can check neither the relay grant nor the pin; closing (a \
             forward is handle_forward_on's)",
            carry.peer.display()
        );
    };
    if !carry
        .hosts
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(host))
    {
        bail!(
            "peer tunnel: {} asked for {host}:{port}, which is not on this gateway's origin \
             allow-list; refused before anything was dialled",
            carry.peer.display()
        );
    }

    let (allowance, open) = reserve_for(&carry)?;

    // From here on every exit (refusal, timeout, `?`) releases the slice.
    let mut reservation = CarryReservation {
        budget: carry.budget,
        peer: carry.peer,
        id: open,
        now_ms: carry.now_ms,
        spent: 0,
    };

    let mut peer_side = NoiseStream::start(stream, session);

    // The requester's first plaintext: its own ClientHello. Read before the
    // origin is dialled so a mismatched SNI costs no outbound connection, and
    // read under a deadline so a peer that opens a tunnel and then says nothing
    // does not pin a gateway task for as long as it likes.
    //
    // **The deadline was found by a mutation, not by review.** Disabling the
    // origin allow-list and re-running the gate turned a fast refusal into a
    // 590-second hang, which is the same shape `abuse-resistance.md` gives
    // message 1 its five seconds for, the difference being that this peer is
    // authenticated and therefore costs a task rather than a socket.
    let mut hello = Vec::with_capacity(1024);
    let deadline = tokio::time::Instant::now() + FIRST_RECORD_TIMEOUT;
    loop {
        let mut chunk = vec![0_u8; CHUNK_BYTES];
        let read = tokio::time::timeout_at(deadline, peer_side.read(&mut chunk))
            .await
            .map_err(|_| {
                anyhow!(
                    "peer tunnel: {} opened a carry and sent no complete ClientHello within \
                     {}s; closing",
                    carry.peer.display(),
                    FIRST_RECORD_TIMEOUT.as_secs()
                )
            })?
            .context("peer tunnel: the requester sent no ClientHello")?;
        if read == 0 {
            bail!("peer tunnel: the requester closed before sending a ClientHello");
        }
        hello.extend_from_slice(&chunk[..read]);
        match assert_sni_matches_target(&hello, host) {
            Ok(()) => break,
            Err(err) if hello.len() < MAX_CLIENT_HELLO_BYTES && incomplete_hello(&err) => continue,
            Err(err) => {
                tracing::warn!(
                    peer = %carry.peer.display(),
                    host = %host,
                    "peer tunnel: refused, the first record does not name this target"
                );
                return Err(err);
            }
        }
    }

    let started = std::time::Instant::now();
    let mut origin = match carry.route {
        OriginRoute::Resolve => TcpStream::connect((host.as_str(), *port)).await,
        OriginRoute::Fixed(addr) => TcpStream::connect(addr).await,
        // An origin target with a peer route is a caller that mixed the two
        // paths up. Refused rather than resolved: the route is the half that
        // says which socket, and this one says "a peer".
        OriginRoute::Peer(onward) => bail!(
            "peer tunnel: {} asked for {host}:{port} and this carry's route names peer {}; a \
             forward is handle_forward_on's, so nothing here dials anything",
            carry.peer.display(),
            onward.display()
        ),
    }
    .with_context(|| format!("peer tunnel: this gateway could not reach {host}:{port}"))?;
    origin
        .write_all(&hello)
        .await
        .context("peer tunnel: the origin refused the first record")?;

    // The peer half is the metered one: it is the peer's allowance being
    // spent, and metering the origin half would charge a peer for bytes the
    // origin chose to send after the allowance ran out.
    let metered = Metered::new(peer_side, allowance);
    let remaining = metered.remaining();
    let spliced = splice(metered, origin).await;
    let spent = allowance.saturating_sub(remaining.load(Ordering::Acquire));
    let carried = u64::try_from(hello.len()).unwrap_or(u64::MAX);
    // What the reservation turns into when it is released, which happens on
    // the way out of this function whether the splice ended well or not.
    reservation.spent = spent.saturating_add(carried);

    let (up, down) = spliced?;
    let up = up.saturating_add(carried);
    // The gateway's whole record of a carried stream: who, where, how much,
    // how long. Nothing derived from the bytes themselves, because there is
    // nothing here that can read them.
    tracing::info!(
        peer = %carry.peer.display(),
        host = %host,
        port = *port,
        bytes_up = up,
        bytes_down = down,
        ms = started.elapsed().as_millis(),
        "peer tunnel: carried"
    );
    Ok((up, down))
}

/// Whether a [`client_hello_sni`] failure means "not all of it has arrived
/// yet".
///
/// Matched on the message rather than on a variant because the parser's errors
/// are `anyhow` contexts, which is what every other refusal in this module
/// produces. The two strings it looks for are both written in this file.
fn incomplete_hello(err: &anyhow::Error) -> bool {
    let text = format!("{err}");
    text.contains("is truncated") || text.contains("is not complete yet")
}

// ---------------------------------------------------------------------------
// The forwarder's half
// ---------------------------------------------------------------------------

/// What a forwarder needs besides [`Carry`]: the file that says who may ask for
/// what, this node's own id, and the two header fields that bound a chain.
///
/// A separate struct rather than four more [`Carry`] fields, because a gateway
/// carry needs none of them: a `Carry` is "who asked, where for, what it may
/// spend", and every field here exists only because the far end is a peer.
pub struct Forward<'a> {
    /// This Mac's peers file. BOTH sides of a forward are read from it, the
    /// requester's [`crate::peer::config::Allow::relay`] grant and the target's
    /// own row, which is the whole of "never to an arbitrary address": an
    /// address this Mac did not already pin has no row to be dialled from.
    pub store: &'a PeerStore,
    /// This node's own id, compared against [`Self::via`]. Not in
    /// [`crate::peer::config::PeerStore`] (a peers file lists other Macs), so
    /// the caller hands it over the way
    /// [`crate::peer::listener::peer_stream_gate_hop`] is handed it.
    pub node: PeerId,
    /// [`tcr_peer_wire::StreamHeader::hops_remaining`] as the requester wrote
    /// it. One hop is spent here, and zero is a refusal.
    pub hops_remaining: u8,
    /// [`tcr_peer_wire::StreamHeader::via`] as the requester wrote it. A frame
    /// that already passed through this Mac is a cycle.
    pub via: &'a [PeerId],
}

/// A forward this node has agreed to: which pinned Mac, and how much hop budget
/// is left after this one is spent.
#[derive(Debug, Clone)]
pub struct ForwardPlan {
    /// The target's own row, whose addresses are the only ones that will be
    /// dialled.
    pub target: PeerRow,
    /// `hops_remaining - 1`, clamped by this Mac's own
    /// [`crate::peer::config::PeerFile::max_hops`].
    ///
    /// # It is enforced here and it does not travel, and that is stated rather than implied
    ///
    /// A blind forward writes no header of its own, the payload is a nested
    /// Noise session between the requester and the target, and a node that
    /// framed anything around it would be holding a key it must not have. So
    /// the decrement bounds the frame THIS node was handed, and what refuses
    /// the next forward is the next node's own grant set and its own
    /// `maxHops`, read against the frame the requester nested for it. That is
    /// mesh-v2's "the hop count bounds a FRAME, not a request", and a reader
    /// who expects a counter to ride along the chain would otherwise look for
    /// one that cannot exist.
    pub onward_hops: u8,
}

/// May this forward happen, and to which row.
///
/// Every refusal here happens before a socket to the target exists, so a
/// refused requester costs the target nothing at all, not a connection, not a
/// handshake, not a log line. The order is the design, and it is the same
/// order [`crate::peer::listener::peer_stream_gate_rows`] uses: what the
/// handshake proved and what the file grants first, then the fields the
/// requester itself chose.
///
/// The grant is re-read here rather than trusted from the listener's gate, and
/// the two are not a duplicated fact: the gate answers "may this peer open a
/// stream of this kind", this answers "may this peer be dialled onward to that
/// row", and the second question has an input the first one does not have ,
/// the target's row.
pub fn authorize_forward(carry: &Carry<'_>, forward: &Forward<'_>) -> Result<ForwardPlan> {
    let TunnelTarget::Peer { node: target } = carry.target else {
        bail!(
            "peer forward: {} asked for an origin on the forwarding path; an origin carry is \
             handle_tunnel_on's and nothing here dials a host",
            carry.peer.display()
        );
    };
    let OriginRoute::Peer(routed) = carry.route else {
        bail!(
            "peer forward: this carry's route is not a peer route, so nothing here knows which \
             Mac was chosen; closing"
        );
    };
    if routed != *target {
        bail!(
            "peer forward: the route names {} and the header names {}; closing rather than \
             dialling a Mac one half of this decision never chose",
            routed.display(),
            target.display()
        );
    }
    let Some(requester) = forward.store.row(&carry.peer) else {
        bail!(
            "peer forward: {} is not pinned by this Mac, so it holds no grant at all; closing",
            carry.peer.display()
        );
    };
    if !requester.allow.relay {
        bail!(
            "peer forward: {} does not hold `allow.relay` on this Mac; a gateway grant is not a \
             relay grant; closing",
            carry.peer.display()
        );
    }
    let Some(row) = forward.store.row(target) else {
        bail!(
            "peer forward: {} asked this Mac to forward to a Mac it has not pinned; closing \
             rather than opening a socket to an address a peer chose",
            carry.peer.display()
        );
    };
    if *target == forward.node {
        bail!(
            "peer forward: {} asked this Mac to forward to itself; closing",
            carry.peer.display()
        );
    }
    if *target == carry.peer {
        bail!(
            "peer forward: {} asked this Mac to forward back out the link the frame arrived on; \
             closing",
            carry.peer.display()
        );
    }
    if forward.via.contains(&forward.node) {
        bail!(
            "peer forward: this frame has already passed through this Mac, so forwarding it \
             would complete a cycle; closing"
        );
    }
    let max_hops = forward.store.file().max_hops;
    if max_hops == 0 {
        bail!(
            "peer forward: this Mac's `maxHops` is 0, which disables forwarding entirely; \
             closing"
        );
    }
    let Some(onward_hops) = forward.hops_remaining.checked_sub(1) else {
        bail!(
            "peer forward: this frame has no hops left, so {} is asking for one forward past \
             the budget it arrived with; closing",
            carry.peer.display()
        );
    };
    Ok(ForwardPlan {
        target: row,
        onward_hops: onward_hops.min(max_hops.saturating_sub(1)),
    })
}

/// Dial a pinned row's own addresses, and nothing else.
///
/// One function around [`crate::peer::serve::dial_peer`] so that widening
/// that return type, a `TcpStream` today, a boxed
/// `AsyncRead + AsyncWrite` once endpoints land, is one line of merge here
/// instead of one per use: an `impl Trait` return already accepts either.
/// Which address answered comes back with the stream, because it is the one
/// fact that turns these bytes into traffic over a PATH: the row's endpoints
/// are [`Locator`]s and the address dialled is one of them, so the meter can
/// key on the same value the peers file spells.
/// The return is [`crate::peer::serve::PeerStream`], the boxed form of that
/// same widening: [`reach_target`] chooses at runtime between this and a
/// carrier off the desk, and two transports are two types no caller can be
/// generic over.
async fn dial_pinned_peer(
    row: &PeerRow,
) -> Option<(std::net::SocketAddr, crate::peer::serve::PeerStream)> {
    crate::peer::serve::dial_peer_with_endpoint(row).await
}

/// Forward one TUNNEL stream to a pinned peer: the forwarder's half of reach.
///
/// # What this node holds, and it is only ciphertext
///
/// The onward socket is spliced RAW. This node runs no handshake on it and
/// frames nothing around what it carries, because the payload is already a
/// fresh Noise session between the requester and the target: the target
/// authenticates the REQUESTER's static key, not this one's, and a forwarder
/// that terminated anything would be holding a key that lets it read a
/// `LeaseGrant` it could then forge (mesh-v2 §3's nesting rule, stated
/// negatively there for exactly that reason).
///
/// So the meter is the same two figures a gateway has, bytes up, bytes down ,
/// and there is nothing else here to see.
///
/// # Why the first chunk is read before the target is dialled
///
/// The same reason [`handle_tunnel_on`] reads the ClientHello first: a peer
/// that opens a forward and then goes quiet must not cost the TARGET a
/// connection. There is nothing to validate in those bytes (they are the first
/// message of a session this node has no key for, and a forwarder that
/// insisted on a shape would be a forwarder that parses), so the deadline is
/// the whole of the check.
pub async fn handle_forward_on<S>(
    stream: S,
    session: PeerSession,
    carry: Carry<'_>,
    forward: Forward<'_>,
) -> Result<(u64, u64)>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let plan = authorize_forward(&carry, &forward)?;

    // Charged to the REQUESTER, exactly like a carry, and for the reason
    // `Allow::relay`'s doc-comment gives: a forward grant is transitive, so the
    // node two hops out spends the grantee's hour and never a fresh one. This
    // is the only hard ceiling on what transitivity costs, which is why it is
    // taken before the target is dialled rather than charged afterwards.
    let (allowance, open) = reserve_for(&carry)?;
    // From here on every exit, a target that does not answer, a requester that
    // goes quiet, any `?`, releases the slice.
    let mut reservation = CarryReservation {
        budget: carry.budget,
        peer: carry.peer,
        id: open,
        now_ms: carry.now_ms,
        spent: 0,
    };

    let mut peer_side = NoiseStream::start(stream, session);
    let mut first = vec![0_u8; CHUNK_BYTES];
    let read = tokio::time::timeout(FIRST_RECORD_TIMEOUT, peer_side.read(&mut first))
        .await
        .map_err(|_| {
            anyhow!(
                "peer forward: {} opened a forward and sent nothing within {}s; closing before \
                 anything was dialled",
                carry.peer.display(),
                FIRST_RECORD_TIMEOUT.as_secs()
            )
        })?
        .context("peer forward: the requester sent nothing to forward")?;
    if read == 0 {
        bail!("peer forward: the requester closed before it sent anything to forward");
    }
    first.truncate(read);

    let started = std::time::Instant::now();
    let (reached, mut onward) =
        reach_target(&plan.target, carry.now_ms)
            .await
            .ok_or_else(|| {
                anyhow!(
                    "peer forward: {} has parked no carrier here and none of its pinned addresses \
                 answered, so there is nothing to forward to",
                    plan.target.node.display()
                )
            })?;
    onward
        .write_all(&first)
        .await
        .context("peer forward: the target refused the first bytes of the carried session")?;

    // The peer half is the metered one, for the same reason a gateway meters
    // that half: it is the requester's allowance being spent, and metering the
    // onward half would charge it for bytes the TARGET chose to send after the
    // allowance ran out.
    let metered = Metered::new(peer_side, allowance);
    let remaining = metered.remaining();
    let spliced = splice(metered, onward).await;
    let carried = u64::try_from(first.len()).unwrap_or(u64::MAX);
    reservation.spent = allowance
        .saturating_sub(remaining.load(Ordering::Acquire))
        .saturating_add(carried);

    let (up, down) = spliced?;
    let up = up.saturating_add(carried);
    // The per-path half of the meter, and it is keyed on the TARGET and the
    // address that answered, not on the requester, who is already charged by
    // the hour above. These bytes went out over one of the target's own
    // endpoints, which is the only path in this whole file this Mac chose
    // rather than was handed, and `Locator::Direct` is how its row spells it.
    //
    // A poisoned meter is a lost measurement and never a failed forward: the
    // bytes are already carried by the time this runs, and a gateway that
    // answered an error because a counter is unreadable would be trading the
    // service for the statistic.
    match reached {
        ReachedTarget::Direct(addr) => match path_meter().lock() {
            Ok(mut meter) => meter.charge_bytes(
                &plan.target.node,
                Locator::Direct { addr },
                up.saturating_add(down),
                carry.now_ms,
            ),
            Err(_) => tracing::error!(
                onward = %plan.target.node.display(),
                "peer forward: the per-path meter's lock is poisoned, so this forward's bytes \
                 are missing from every per-path figure until restart"
            ),
        },
        // A carrier the target parked here is not one of ITS endpoints, and
        // the meter keys on a [`Locator`], which has two arms, an address and
        // a Mac that carried. Neither is this path, so these bytes are charged
        // to the requester's hour like every other forwarded byte and are
        // missing from the per-path figures alone. A third `Locator` arm and a
        // panel key for it would close that gap.
        ReachedTarget::Reverse => tracing::debug!(
            onward = %plan.target.node.display(),
            bytes = up.saturating_add(down),
            "peer forward: these bytes rode a carrier the target parked here, a path no \
             `Locator` arm spells yet, so the per-path meter has no row to charge"
        ),
    }
    // A forwarder's whole record of a forwarded stream: who asked, who it went
    // to, how much, how deep it may still go, how long. Nothing derived from
    // the bytes, because there is nothing here that can read them.
    tracing::info!(
        peer = %carry.peer.display(),
        onward = %plan.target.node.display(),
        path = reached.label(),
        hops_left = plan.onward_hops,
        bytes_up = up,
        bytes_down = down,
        ms = started.elapsed().as_millis(),
        "peer forward: carried"
    );
    Ok((up, down))
}

// ---------------------------------------------------------------------------
// The requester's half
// ---------------------------------------------------------------------------

/// How many hops a client asks a forwarder for.
///
/// One. The requester spends it at the forwarder and the target is the far
/// end, so a second hop is a chain nothing in this build constructs, and a
/// budget that is not spent is a budget an intermediate could spend on this
/// node's behalf. [`authorize_forward`] clamps it again by the FORWARDER's own
/// `maxHops`, so this number can only ever ask for less than the operator
/// there allows.
pub const CLIENT_FORWARD_HOPS: u8 = 1;

/// Ask `forwarder` to carry a stream to `target`, and return that stream.
///
/// # This is the half that did not exist, and what its absence meant
///
/// Everything else on the forwarded path was built and gated
/// ([`authorize_forward`], [`handle_forward_on`]) and nothing could reach it:
/// no code anywhere opened a `StreamKind::Tunnel` whose target was a peer, so
/// a row whose only endpoint was a [`crate::peer::config::Locator::Via`] was
/// dialled by nobody and `serve.rs`'s dial loop logged it and moved on. This
/// function is that opening, and [`crate::peer::serve::dial_peer_reaching`] is
/// the caller that decides when to use it.
///
/// # What comes back, and why the caller cannot tell it apart
///
/// A [`crate::peer::serve::PeerStream`], exactly what a direct dial returns.
/// The forwarder splices the onward socket RAW ([`handle_forward_on`]), so
/// the bytes this stream carries are the target's own socket, and the caller
/// runs its ordinary handshake over it against the TARGET's pinned key. That
/// is the whole of the blindness argument from this side: this node holds a
/// session with the forwarder that carries a second session the forwarder has
/// no key for.
///
/// # Ordering, which is not an accident
///
/// The header is written before this returns, so the forwarder is already
/// waiting on the requester's first bytes ([`handle_forward_on`]'s deadline)
/// when the caller starts its handshake with the target. A caller that wrote
/// nothing would be closed by that deadline rather than pinning a task on the
/// forwarder, which is why nothing here waits for an acknowledgement: a
/// forward has none to give: the next thing the forwarder does is dial, and
/// its refusals all happen before any byte of the nested session moves.
///
/// The dial to the forwarder is [`crate::peer::serve::dial_peer_within`],
/// which follows DIRECT endpoints only. That is what bounds this: a forwarder
/// reached through a forwarder would be a chain this node builds one hop at a
/// time with no budget of its own, and the hop count is the forwarder's bound,
/// never the requester's.
pub async fn open_forward_to(
    forwarder: &PeerRow,
    node_secret: &[u8; noise::KEY_BYTES],
    target: &PeerId,
    via: &[PeerId],
    borrow_timeout_ms: u64,
) -> Result<crate::peer::serve::PeerStream> {
    if forwarder.node == *target {
        bail!(
            "peer forward: {} cannot carry a stream to itself; a direct dial is what that \
             asks for",
            target.display()
        );
    }
    let (_reached, mut stream) = crate::peer::serve::dial_peer_within(forwarder, borrow_timeout_ms)
        .await
        .ok_or_else(|| {
            anyhow!(
                "peer forward: none of {}'s own addresses answered, so it cannot carry \
                     anything to {}",
                forwarder.label,
                target.display()
            )
        })?;
    let mut session = noise::dial_handshake(
        &mut stream,
        node_secret,
        noise::Handshake::Return,
        Some(&forwarder.node.0),
        None,
    )
    .await
    .context("peer forward: the handshake with the forwarder failed")?;

    let header = StreamHeader {
        kind: StreamKind::Tunnel,
        target: Some(TunnelTarget::Peer { node: *target }),
        via: via.to_vec(),
        hops_remaining: CLIENT_FORWARD_HOPS,
        request_id: crate::peer::lease::random_id()?,
    };
    let bytes = serde_json::to_vec(&header)
        .context("peer forward: the forwarding header did not serialize")?;
    noise::send_encrypted(&mut stream, &mut session.transport, &bytes)
        .await
        .context("peer forward: the forwarder did not take the stream header")?;

    tracing::debug!(
        forwarder = %forwarder.node.display(),
        target = %target.display(),
        "peer forward: asked a pinned Mac to carry a stream to another pinned Mac"
    );
    Ok(Box::new(NoiseStream::start(stream, session)))
}

// ---------------------------------------------------------------------------
// The reverse carry: reaching a Mac nothing can dial
// ---------------------------------------------------------------------------

/// How long a parked carrier is treated as usable.
///
/// Five minutes, which is five of the prober's own rounds
/// ([`crate::peer::probe::PROBE_INTERVAL`]): a keeper that stops opening
/// carriers loses its desk slot within a few probe windows of going quiet, and
/// a socket whose far end died without a FIN is dropped rather than spliced
/// into a requester's stream. It is a ceiling and never a liveness claim: the
/// only proof a parked socket still works is the splice itself, which is why
/// [`ReverseDesk::take`] hands out one carrier and forgets it.
pub const REVERSE_PARK_TTL_MS: i64 = 5 * 60 * 1000;

/// How many carriers one peer may keep parked here at once.
///
/// One TCP connection is one Noise session is one stream
/// ([`tcr_peer_wire::StreamKind`]'s own doc says why there is no multiplexer),
/// so a parked carrier serves exactly ONE forward and is then gone. This
/// number is therefore the concurrency an undialable friend gets on this Mac,
/// and it is [`MAX_OPEN_CARRIES_PER_PEER`] for that constant's own reason: a
/// session opens several requests at once, and a friend held to one would look
/// like a hang rather than a refusal.
pub const MAX_PARKED_PER_PEER: usize = MAX_OPEN_CARRIES_PER_PEER as usize;

/// One carrier a peer opened TO this node, waiting for a forward to ride back
/// out over it.
struct ParkedCarry {
    stream: crate::peer::serve::PeerStream,
    parked_at_ms: i64,
}

/// Why a carrier was not parked.
///
/// A typed answer rather than a bool, because the two cases are the operator's
/// two different problems: a Mac this node never pinned is a pairing that did
/// not happen, and a full desk is a friend opening carriers faster than
/// anything uses them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParkRefusal {
    /// The peer is not a row in this Mac's peers file. The same rule
    /// [`authorize_forward`] enforces on the other side of the same forward:
    /// nothing here holds a socket for a Mac this node has not pinned.
    NotPinned,
    /// This peer already holds [`MAX_PARKED_PER_PEER`] carriers here.
    DeskFull { open: usize },
}

/// What [`park_reverse_carry`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParkOutcome {
    /// Parked, with how many this peer now holds.
    Parked { open: usize },
    /// Not parked, and the socket was dropped rather than held.
    Refused(ParkRefusal),
}

/// The carriers pinned Macs have opened to this node, one queue per peer.
///
/// # Why a desk and not a dial
///
/// Everything else in this file reaches a peer by opening a socket to an
/// address its row carries. A Mac behind a NAT that maps by destination has no
/// such address: nothing this node writes down can be dialled, and the row is
/// not stale, it is unusable by construction. What that Mac CAN do is open a
/// socket outwards, and a socket is bidirectional, so the one it opened is the
/// way in. This is where those sockets wait.
///
/// # It holds ciphertext and it is not a session
///
/// A parked carrier is a raw socket, spliced by [`handle_forward_on`] exactly
/// as a dialled one is: the payload is a fresh Noise session between the
/// REQUESTER and the peer that parked it, and this node holds no key for it.
/// So nothing here reads a byte, and a carrier that is used is used once,
/// because there is no multiplexer in this design and framing one here would
/// be inventing the component the wire crate's doc says does not exist.
pub struct ReverseDesk {
    open: std::collections::HashMap<PeerId, std::collections::VecDeque<ParkedCarry>>,
}

impl Default for ReverseDesk {
    fn default() -> Self {
        Self::new()
    }
}

impl ReverseDesk {
    /// An empty desk.
    pub fn new() -> Self {
        Self {
            open: std::collections::HashMap::new(),
        }
    }

    /// Park one carrier, or say why not.
    ///
    /// The pinned check is the CALLER's ([`park_reverse_carry`]), because the
    /// peers file is the caller's to read and a desk that opened one would be
    /// a second reader of the same fact.
    fn park(
        &mut self,
        peer: PeerId,
        stream: crate::peer::serve::PeerStream,
        now_ms: i64,
    ) -> ParkOutcome {
        let queue = self.open.entry(peer).or_default();
        drop_expired(queue, now_ms);
        if queue.len() >= MAX_PARKED_PER_PEER {
            return ParkOutcome::Refused(ParkRefusal::DeskFull { open: queue.len() });
        }
        queue.push_back(ParkedCarry {
            stream,
            parked_at_ms: now_ms,
        });
        ParkOutcome::Parked { open: queue.len() }
    }

    /// Take the carrier this peer parked longest ago, if one is still inside
    /// [`REVERSE_PARK_TTL_MS`].
    ///
    /// Oldest first, which is the opposite of what a cache would do and right
    /// here: every carrier is equally able to carry, and the oldest is the one
    /// closest to its TTL, so spending it first is what keeps a desk from
    /// holding a socket until it expires unused.
    pub fn take(&mut self, peer: &PeerId, now_ms: i64) -> Option<crate::peer::serve::PeerStream> {
        let queue = self.open.get_mut(peer)?;
        drop_expired(queue, now_ms);
        queue.pop_front().map(|carry| carry.stream)
    }

    /// How many usable carriers this peer holds here, expired ones dropped.
    pub fn open_for(&mut self, peer: &PeerId, now_ms: i64) -> usize {
        match self.open.get_mut(peer) {
            Some(queue) => {
                drop_expired(queue, now_ms);
                queue.len()
            }
            None => 0,
        }
    }

    /// Drop every expired carrier on the desk, and report how many went.
    pub fn prune(&mut self, now_ms: i64) -> usize {
        let mut dropped = 0;
        for queue in self.open.values_mut() {
            let before = queue.len();
            drop_expired(queue, now_ms);
            dropped += before.saturating_sub(queue.len());
        }
        dropped
    }
}

/// Drop the carriers at the front of `queue` that are past their TTL.
///
/// A queue is parked in time order, so the expired ones are a prefix and this
/// is a `pop_front` loop rather than a scan of the whole queue.
fn drop_expired(queue: &mut std::collections::VecDeque<ParkedCarry>, now_ms: i64) {
    while let Some(front) = queue.front() {
        if now_ms.saturating_sub(front.parked_at_ms) < REVERSE_PARK_TTL_MS {
            return;
        }
        queue.pop_front();
    }
}

/// The one desk this process keeps, for [`path_meter`]'s reason: the
/// alternative is a handle threaded from boot through the listener and the
/// forwarder, and both ends of a parked carrier are handled by code that was
/// never given one.
pub fn reverse_desk() -> &'static std::sync::Mutex<ReverseDesk> {
    static DESK: std::sync::OnceLock<std::sync::Mutex<ReverseDesk>> = std::sync::OnceLock::new();
    DESK.get_or_init(|| std::sync::Mutex::new(ReverseDesk::new()))
}

/// Hold a carrier a pinned peer opened to this node, so a forward for that
/// peer has a way back out.
///
/// The admission is one question and it is the same one
/// [`authorize_forward`] asks about a forward's target: is this Mac in the
/// peers file. A carrier from a stranger is dropped here, before it can take a
/// desk slot from a friend.
pub fn park_reverse_carry(
    store: &PeerStore,
    peer: PeerId,
    stream: crate::peer::serve::PeerStream,
    now_ms: i64,
) -> ParkOutcome {
    if store.row(&peer).is_none() {
        tracing::warn!(
            peer = %peer.display(),
            "peer reverse: a Mac this node has not pinned offered to keep a carrier here; dropping it"
        );
        return ParkOutcome::Refused(ParkRefusal::NotPinned);
    }
    let outcome = match reverse_desk().lock() {
        Ok(mut desk) => desk.park(peer, stream, now_ms),
        Err(_) => {
            tracing::error!(
                peer = %peer.display(),
                "peer reverse: the carrier desk's lock is poisoned, so this carrier is dropped \
                 and that peer stays unreachable from here until restart"
            );
            ParkOutcome::Refused(ParkRefusal::DeskFull { open: 0 })
        }
    };
    match outcome {
        ParkOutcome::Parked { open } => tracing::debug!(
            peer = %peer.display(),
            open,
            "peer reverse: holding a carrier for a Mac that cannot be dialled"
        ),
        ParkOutcome::Refused(ParkRefusal::DeskFull { open }) => tracing::warn!(
            peer = %peer.display(),
            open,
            "peer reverse: this peer already holds every carrier slot here; dropping the offer"
        ),
        ParkOutcome::Refused(ParkRefusal::NotPinned) => {}
    }
    outcome
}

/// How a forward got to its target.
///
/// Reported for the same reason [`crate::peer::serve::Reached`] is: the meter
/// and the log line both need to name the path, and a boxed stream cannot be
/// asked afterwards which one it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReachedTarget {
    /// A socket this node opened to one of the row's own addresses.
    Direct(std::net::SocketAddr),
    /// A carrier the target itself opened to this node and parked.
    Reverse,
}

impl ReachedTarget {
    /// The one word a log line names this path by.
    pub fn label(self) -> &'static str {
        match self {
            Self::Direct(_) => "direct",
            Self::Reverse => "reverse",
        }
    }
}

/// A parked carrier for this target, or a dial of its own addresses, in that
/// order.
///
/// # The order is the whole of the reverse story
///
/// A parked carrier is proof the target was alive and able to reach this Mac
/// as recently as [`REVERSE_PARK_TTL_MS`] ago, and for the Mac it exists for,
/// the one nothing can dial, the row's addresses are not stale but unusable.
/// Dialling first would spend the borrow's whole timeout proving that before
/// reaching for the socket that was already open. A target that is dialable
/// parks nothing, so this costs it one lock and no round trip.
async fn reach_target(
    row: &PeerRow,
    now_ms: i64,
) -> Option<(ReachedTarget, crate::peer::serve::PeerStream)> {
    match reverse_desk().lock() {
        Ok(mut desk) => {
            if let Some(parked) = desk.take(&row.node, now_ms) {
                tracing::debug!(
                    peer = %row.node.display(),
                    "peer forward: this target parked a carrier here, so the forward rides back \
                     out over it and nothing is dialled"
                );
                return Some((ReachedTarget::Reverse, parked));
            }
        }
        Err(_) => tracing::error!(
            peer = %row.node.display(),
            "peer forward: the carrier desk's lock is poisoned, so a parked carrier cannot be \
             read and this forward falls back to dialling the row"
        ),
    }
    let (addr, stream) = dial_pinned_peer(row).await?;
    Some((ReachedTarget::Direct(addr), stream))
}

/// Whether this node needs pinned friends to carry for it at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReverseNeed {
    /// Nothing dialable was found, so a friend's open socket is the way in.
    Wanted,
    /// This node can be dialled, and says how.
    NotWanted(ReverseNotWanted),
}

/// The reason a node needs no carrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReverseNotWanted {
    /// A router mapping is held, so the public address in it is dialable.
    HoldsMapping,
    /// This Mac has a global IPv6 address, which needs no mapping at all.
    HasGlobalV6,
}

/// Does this node need a friend to hold a carrier for it.
///
/// A pure function over the two facts `reach` already measures, and it takes
/// them rather than reading them, so the answer can be tested against both
/// states without a router, a socket or a NAT. The caller is the process that
/// holds [`crate::peer::reach::MappingKeeper`], which is the only thing that
/// knows whether the mapping it asked for exists this minute.
///
/// A mapping outranks IPv6 in this answer only in which reason is reported:
/// either one alone means a peer can open a socket to this Mac, and a node
/// that asked a friend to carry anyway would be spending a third Mac's bytes
/// on a path it does not need.
pub fn reverse_carry_is_wanted(
    held: Option<&crate::peer::reach::Mapping>,
    global_v6: &[std::net::Ipv6Addr],
) -> ReverseNeed {
    if held.is_some() {
        return ReverseNeed::NotWanted(ReverseNotWanted::HoldsMapping);
    }
    if !global_v6.is_empty() {
        return ReverseNeed::NotWanted(ReverseNotWanted::HasGlobalV6);
    }
    ReverseNeed::Wanted
}

/// The pinned Macs this node may ask to hold a carrier for it, in the order to
/// ask them.
///
/// [`crate::peer::serve::forwarders_for`] with this node's own id as the
/// target, and that is not a trick: "who may carry a stream to X" with X set
/// to self IS "who may carry for me", it already excludes this Mac and any
/// candidate with no direct address of its own, and its order is the prober's.
/// A second list here would be a second answer to one question, and the two
/// would drift the first time `paths.viaAllow` changed.
pub fn reverse_carriers(store: &PeerStore, node: &PeerId) -> Vec<PeerRow> {
    crate::peer::serve::forwarders_for(node, store)
        .into_iter()
        .filter_map(|peer| store.row(&peer))
        .collect()
}

/// What one friend's keeper loop did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReverseKeeperRun {
    /// Carriers that were opened, parked, and ran to completion.
    pub carried: usize,
    /// Attempts that never became a parked carrier.
    pub failed: usize,
}

/// Keep one friend supplied with a carrier: open one, hold it until the friend
/// is done with it, open the next.
///
/// # Why this is a loop and not a keepalive
///
/// A parked carrier is a raw socket the friend splices; a byte this side wrote
/// into it to prove liveness would land in the middle of somebody's carried
/// session. So nothing is sent on a parked carrier at all: it is opened, it
/// waits, it is spent by one forward, and this loop opens the next. That is
/// what "keeps it alive" means here, and the probe cadence
/// ([`crate::peer::probe::PROBE_INTERVAL`]) is the caller's `retry` value, the
/// spacing between attempts when the friend cannot be reached at all.
///
/// # The seam
///
/// `run_one` opens one carrier to `friend` and returns when that carrier is
/// finished, however it finished. It is a parameter because the production
/// version writes a stream header the peer wire does not carry yet (the
/// variant and the listener arm that reads it are both missing), while
/// everything this function decides, how many, how often, what a failure
/// costs, is testable with a closure and no socket at all.
///
/// `runs` bounds the loop so a test can drive it; `None` is the production
/// shape, which is forever.
pub async fn keep_reverse_carrier<F, Fut>(
    friend: PeerRow,
    retry: std::time::Duration,
    runs: Option<usize>,
    run_one: F,
) -> ReverseKeeperRun
where
    F: Fn(PeerRow) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let mut tally = ReverseKeeperRun::default();
    let mut round = 0_usize;
    loop {
        if let Some(limit) = runs {
            if round >= limit {
                return tally;
            }
        }
        round += 1;
        match run_one(friend.clone()).await {
            Ok(()) => {
                tally.carried += 1;
                tracing::debug!(
                    friend = %friend.node.display(),
                    carried = tally.carried,
                    "peer reverse: a carrier this node parked at a friend was spent; opening the next"
                );
            }
            Err(err) => {
                tally.failed += 1;
                // Surfaced with the friend's own refusal text, because every
                // way this fails, an unreachable friend, a full desk, a peers
                // file that does not pin this node, is a sentence the operator
                // has nowhere else to read.
                tracing::warn!(
                    friend = %friend.node.display(),
                    error = %err,
                    "peer reverse: this friend is not holding a carrier for us"
                );
                if !retry.is_zero() {
                    tokio::time::sleep(retry).await;
                }
            }
        }
    }
}
