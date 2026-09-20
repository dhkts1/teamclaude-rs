//! The LAN peer mesh: `tcr peer`.
//!
//! One operator's own Macs, over one Noise-authenticated TCP stream per
//! purpose, moving two different scarce things that are NOT the same shape:
//!
//! - **egress**: reach Anthropic at all. A peer carries it BLIND, as pure
//!   bytes, with the requester's own credential sealed inside the requester's
//!   own TLS. No lease, no ledger, no plaintext leaving any host. This is also
//!   the only thing that keeps an offline machine's own tokens alive, because
//!   `platform.claude.com` (the host Claude Code's OAuth refresh targets) is
//!   on the same two-host allow-list (`src/mitm.rs:66`).
//! - **quota**: someone else's account serves. This FORCES the lender to see
//!   plaintext, because substituting a Bearer on ciphertext is not possible.
//!
//! **Blindness is therefore not a permission bit anywhere in this module, and
//! must never become one.** A forwarding node is handed a byte pipe and has
//! nothing to parse; a lending node is handed an HTTP request it must re-sign
//! and has no way not to read it. The property is WHICH STREAM KIND RAN, and
//! there is no fourth state reachable by flipping a flag.
//!
//! # Vocabulary, stated once
//!
//! A **node** is this machine. A **peer** is another machine as seen from here.
//! So `tcr-node.key` and `PeerId` are both right and neither renames the other.
//!
//! # The four invariants every file here inherits
//!
//! 1. **No credential on the wire, with one named exemption.** A lender
//!    attaches its OWN Bearer locally; a borrower scrubs its own
//!    `authorization` and `x-api-key` BEFORE a SERVE frame is written
//!    ([`serve`]) and refuses the client-credential paths outright. The
//!    exemption is `Control::Handoff`, which IS `hand` mode: the owner hands
//!    over its short-lived access token, never the refresh token, on a CONTROL
//!    stream that nests end to end, so the borrowed request can leave the
//!    borrower's own machine. No other type may gain a token field, and the
//!    gate that holds that line names this one variant and nothing else
//!    (`tests/peer_wire.rs`).
//! 2. **Pin before you answer.** `snow` has no pin store: an `IK` responder
//!    learns the initiator's static key after reading message 1, and if the
//!    comparison against the pinned set is missing, the handshake completes and
//!    NOTHING ERRORS. Those few lines are the whole of authorization
//!    ([`noise::pin_check`]).
//! 3. **Everything except TUNNEL nests.** A SERVE or a CONTROL stream through a
//!    forwarder is a FRESH end-to-end Noise session inside a TUNNEL, so the
//!    forwarder holds ciphertext for a session it has no key to. Stated
//!    negatively on purpose: an enumeration of which kinds nest is a list with
//!    an item missing, and the item missed last time was CONTROL, whose lease
//!    grant a reading forwarder could forge.
//! 4. **This socket is not the local one.** The peer listener is a SECOND
//!    `TcpListener` with its own gate ([`listener::peer_stream_gate`]).
//!    `local_endpoint_gate` (`src/proxy.rs:1137`) is not relaxed by one
//!    character and neither arm of the existing listener is widened, because
//!    behind that gate sits the one privileged local mutation that adds a live
//!    credential.
//!
//! # Scope
//!
//! Machines whose operators paired by hand or by a join key, each seeing only
//! what its own peers file says; a hop may belong to a friend, and a friend
//! carries bytes it cannot read. Not a public relay, not a service, not
//! multi-tenant.
//!
//! This paragraph said "every hop is a machine whose operator is the same
//! person" until the forwarder landed, and that has been false since: a
//! `Locator::Via` names a peer that carries a nested Noise session it holds no
//! key to. The sentence above is the one the code actually enforces.
//!
//! # This module was built against an internal design blueprint
//!
//! That document is NOT tracked in this repository. Every contract it
//! imposed is restated in full beside the item it constrains, so a reader
//! here never needs it.
//!
//! # Phase order
//!
//! 1. wire crate and the node key: [`id`], `tcr_peer_wire`
//! 2. Noise, enrolment, the pin check: [`noise`], [`listener`], [`pair`]
//! 3. discovery: [`discovery`]
//! 4. the lease and SERVE: [`lease`], [`serve`], `crate::fallback`
//! 5. the Peers tab (Swift)
//! 6. blind egress: [`tunnel`], [`egress`]
//! 7. chains, deferred until a third machine exists
//! 8. `tcr peer move`, out of the mesh
//!
//! Each `#[allow(clippy::todo, unused_variables)]` below is a phase's own
//! marker: the phase that fills that file deletes its line, and the crate-level
//! `warn(clippy::todo)` in `src/lib.rs` then holds the file to the same
//! standard as the rest of the tree. Nothing else in this module suppresses a
//! lint.
//!
//! **Nine of them are gone, which is what a filled phase looks like.**
//! [`id`], [`config`], [`noise`], [`listener`], [`pair`], [`discovery`],
//! [`state`], [`lease`] and [`serve`] carry no `todo!()` body any more, so
//! their markers were deleted in the same change that emptied them rather than
//! left behind, a suppression that outlives its reason is a suppression the
//! next reader trusts.
//!
//! Two remain, and both are honest: [`tunnel`] and [`egress`] are phase 6 and
//! still stubs.

/// Phase 1: the node key, `tcr-node.key` in the config directory.
pub mod id;

/// Phase 1: operator intent on disk, `tcr-peers.json` in the config directory.
pub mod config;

/// Phase 2: the handshake patterns and the pin check.
pub mod noise;

/// Phase 2: the second listener and the one authorization function.
pub mod listener;

/// Phase 2: enrolment, an invite, a join token, a six-digit compare.
pub mod pair;

/// Phase 3: discovery, opt-in, behind two functions.
pub mod discovery;

/// Phase 4: the lease ledger and the guard band.
pub mod lease;

/// Phase 4: when a lend grant is open for borrowing.
pub mod schedule;

/// Phase 4: the SERVE stream, both halves of it.
pub mod serve;

/// Phase 6: the blind splice. Stub only in this skeleton.
#[allow(clippy::todo, unused_variables)]
pub mod tunnel;

/// Phase 6: the loopback egress splice. Stub only in this skeleton.
#[allow(clippy::todo, unused_variables)]
pub mod egress;

/// Phase 1: runtime state, `teamclaude/peer-state.json` in the cache directory.
pub mod state;

/// What this Mac can be reached on from off this LAN: a NAT-PMP mapping, a
/// global IPv6 address, and a port both ends of a pair can compute.
pub mod reach;

/// A second router client beside [`reach`]'s NAT-PMP one, for gateways that
/// speak only UPnP IGD.
pub mod reach_upnp;

/// What each path to a peer costs: RTT and loss, measured on a live session.
pub mod probe;

/// Where two Macs that both moved leave each other an address: the per-pair
/// name and the sealed record. Crypto and naming only, called by nothing yet.
pub mod drop;

/// What one Mac sends one friend after it changed networks: a sealed record
/// saying where it is now. The seal and the refusals only, called by nothing
/// yet.
pub mod moved;

/// One codec for a list of dial addresses, shared by the v3 join key and the
/// sealed reply so the same list is never spelled into bytes twice.
pub mod dialaddrs;
