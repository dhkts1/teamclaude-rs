//! A userspace NAT, for the one thing in this tree that cannot be exercised
//! over loopback: a hole punch.
//!
//! # What it must do, written down before it was built
//!
//! 1. **A node behind it is undialable.** An inbound connection that matches
//!    no mapping is REFUSED, always, in both modes. That is the condition the
//!    punch exists for, and a simulator that relayed inbound-first traffic
//!    would let a punch "succeed" without punching anything.
//! 2. **A mapping is created by an outbound connect and outlives the flow**,
//!    exactly as a real box holds one past the packets that opened it.
//! 3. **Mapping behaviour, in RFC 4787's own two shapes**
//!    ([`Mapping`]):
//!    - `EndpointIndependent` (REQ-1's requirement, and what "a good NAT"
//!      means): the external port depends on the INTERNAL PORT alone, never on
//!      where the packet is going. It is also port preserving, so the external
//!      port IS the internal port when that port is free. This is the property
//!      the whole punch rests on: the far side can predict the port because it
//!      is the port the pair derived.
//!    - `AddressDependent` (what an operator calls a symmetric NAT): a FRESH
//!      external port per destination address. A port the peer learned from
//!      one flow is not the port the next flow uses, so a punch aimed at the
//!      derived port arrives at a port with no mapping behind it and is
//!      refused, in every slot, forever.
//! 4. **Filtering matches the mode.** Endpoint-independent filtering lets any
//!    external host use an existing mapping, which is what lets the punch's
//!    two halves cross. Address-dependent filtering only lets the destination
//!    that created the mapping back in.
//!
//! # Why the addresses are virtual
//!
//! This machine has one loopback address, `127.0.0.1`. Two port-preserving
//! routers would both have to hold the SAME external port on it at the same
//! instant, because the pair derives one port and both ends bind it. That
//! cannot be done: `127.0.0.2` is not assignable on macOS without changing the
//! interface, which is not a test's business. So the public addresses here are
//! virtual, from `203.0.113.0/24` (RFC 5737 TEST-NET-3, which is reserved for
//! documentation and is routed nowhere), and the bytes move over in-process
//! duplex pipes. Everything above the socket is the shipped code: the slot
//! waiting, the retry cadence, the race between the two halves and the
//! outcome names are all [`teamclaude_rs::peer::reach::punch`] itself, running
//! against [`teamclaude_rs::peer::reach::PunchNet`] exactly as it runs against
//! the kernel.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use teamclaude_rs::peer::reach::PunchNet;
use teamclaude_rs::peer::serve::PeerStream;
use tokio::sync::mpsc;

/// How many bytes one direction of a simulated connection buffers.
const PIPE_BYTES: usize = 64 * 1024;

/// The first external port the address-dependent mode hands out.
///
/// Deliberately far from the derived-port window
/// (`teamclaude_rs::peer::reach::PORT_FLOOR`), so a symmetric mapping can
/// never coincide with the port the pair derived and accidentally let a punch
/// through.
const SYMMETRIC_FIRST_PORT: u16 = 61_001;

/// How a box chooses external ports. See the module doc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mapping {
    /// One external port per internal port, whatever the destination, and
    /// equal to the internal port. RFC 4787 REQ-1's endpoint-independent
    /// mapping, plus port preservation.
    EndpointIndependent,
    /// A fresh external port per destination address. A symmetric NAT.
    AddressDependent,
}

/// Why the simulated network refused a connection.
///
/// Named rather than a bare `ConnectionRefused`, because the gate's claim is
/// about WHICH refusal: "no mapping" is the undialable state the punch exists
/// to beat, and "the mapping belongs to someone else" is the symmetric NAT
/// beating it back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Nothing owns that public address.
    NoRoute,
    /// The external port has no mapping: nobody behind this box has sent
    /// anything out from the port that would have opened one.
    NoMapping,
    /// The mapping exists but was opened towards a different address, and this
    /// box only lets that address back in.
    WrongSource,
    /// The mapping exists and points at an internal port nothing is listening
    /// on.
    NoListener,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoRoute => write!(f, "nat: no box owns that public address"),
            Self::NoMapping => write!(f, "nat: no mapping on that external port"),
            Self::WrongSource => write!(
                f,
                "nat: that mapping was opened towards another address and this box is \
                 address dependent"
            ),
            Self::NoListener => write!(f, "nat: the mapping points at a port nothing binds"),
        }
    }
}

impl From<Refusal> for io::Error {
    fn from(refusal: Refusal) -> Self {
        io::Error::new(io::ErrorKind::ConnectionRefused, refusal.to_string())
    }
}

/// One external port's state.
#[derive(Debug, Clone, Copy)]
struct Row {
    /// The port behind the box this maps to.
    internal_port: u16,
    /// Where the outbound connect that opened it was going.
    towards: SocketAddr,
}

#[derive(Default)]
struct BoxState {
    /// External port to mapping.
    table: HashMap<u16, Row>,
    /// Internal port to the queue an accept reads from.
    listeners: HashMap<u16, mpsc::UnboundedSender<PeerStream>>,
    /// The next port the address-dependent mode hands out.
    next_symmetric: u16,
    /// Every inbound connection this box turned away, with its reason, so a
    /// test can say WHY a punch failed rather than only that it did.
    refused: Vec<Refusal>,
}

/// One simulated router, and the node behind it.
///
/// One struct rather than a box and a host, because every box here has exactly
/// one machine behind it and a second one would need an internal address space
/// this simulator has no use for.
pub struct NatBox {
    public: Ipv4Addr,
    mapping: Mapping,
    internet: Internet,
    state: Mutex<BoxState>,
}

impl NatBox {
    /// This node's public address, which is what a peer punches at.
    pub fn public_ip(&self) -> IpAddr {
        IpAddr::V4(self.public)
    }

    /// Every (external port, internal port) pair this box holds right now.
    ///
    /// The gate reads it to check port preservation directly rather than
    /// inferring it from a punch that worked: a simulator that quietly handed
    /// out a different external port would still let a punch through if the
    /// test only looked at the outcome.
    pub fn mappings(&self) -> Vec<(u16, u16)> {
        let held = self.locked();
        let mut rows: Vec<_> = held
            .table
            .iter()
            .map(|(external, row)| (*external, row.internal_port))
            .collect();
        rows.sort_unstable();
        rows
    }

    /// Every refusal this box has made, in order.
    pub fn refusals(&self) -> Vec<Refusal> {
        self.locked().refused.clone()
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, BoxState> {
        match self.state.lock() {
            Ok(held) => held,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Open (or reuse) the mapping an outbound connect from `internal_port`
    /// towards `target` gets, and answer the external port it leaves from.
    fn open_mapping(&self, internal_port: u16, target: SocketAddr) -> u16 {
        let mut held = self.locked();
        match self.mapping {
            Mapping::EndpointIndependent => {
                if let Some((external, _)) = held
                    .table
                    .iter()
                    .find(|(_, row)| row.internal_port == internal_port)
                {
                    return *external;
                }
                // Port preserving: the external port IS the internal one.
                held.table.insert(
                    internal_port,
                    Row {
                        internal_port,
                        towards: target,
                    },
                );
                internal_port
            }
            Mapping::AddressDependent => {
                if let Some((external, _)) = held.table.iter().find(|(_, row)| {
                    row.internal_port == internal_port && row.towards.ip() == target.ip()
                }) {
                    return *external;
                }
                let external = if held.next_symmetric == 0 {
                    SYMMETRIC_FIRST_PORT
                } else {
                    held.next_symmetric
                };
                held.next_symmetric = external.saturating_add(1);
                held.table.insert(
                    external,
                    Row {
                        internal_port,
                        towards: target,
                    },
                );
                external
            }
        }
    }

    /// An inbound connection arriving from `from` at this box's `external_port`.
    fn inbound(&self, from: SocketAddr, external_port: u16) -> Result<PeerStream, Refusal> {
        let mut held = self.locked();
        let Some(row) = held.table.get(&external_port).copied() else {
            held.refused.push(Refusal::NoMapping);
            return Err(Refusal::NoMapping);
        };
        if self.mapping == Mapping::AddressDependent && row.towards.ip() != from.ip() {
            held.refused.push(Refusal::WrongSource);
            return Err(Refusal::WrongSource);
        }
        let Some(listener) = held.listeners.get(&row.internal_port) else {
            held.refused.push(Refusal::NoListener);
            return Err(Refusal::NoListener);
        };
        let (theirs, ours) = tokio::io::duplex(PIPE_BYTES);
        if listener.send(Box::new(ours) as PeerStream).is_err() {
            held.listeners.remove(&row.internal_port);
            held.refused.push(Refusal::NoListener);
            return Err(Refusal::NoListener);
        }
        Ok(Box::new(theirs) as PeerStream)
    }

    /// Bind `internal_port` and answer the queue arrivals land in.
    fn bind(&self, internal_port: u16) -> mpsc::UnboundedReceiver<PeerStream> {
        let (sender, receiver) = mpsc::unbounded_channel();
        self.locked().listeners.insert(internal_port, sender);
        receiver
    }
}

/// The virtual network every box is attached to.
#[derive(Clone, Default)]
pub struct Internet {
    boxes: Arc<Mutex<HashMap<Ipv4Addr, Arc<NatBox>>>>,
}

impl Internet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach a box at `public`, with one node behind it.
    pub fn attach(&self, public: Ipv4Addr, mapping: Mapping) -> Node {
        let router = Arc::new(NatBox {
            public,
            mapping,
            internet: self.clone(),
            state: Mutex::new(BoxState::default()),
        });
        self.locked().insert(public, Arc::clone(&router));
        Node { router }
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, HashMap<Ipv4Addr, Arc<NatBox>>> {
        match self.boxes.lock() {
            Ok(held) => held,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Carry one connection from `from` to `target`.
    fn deliver(&self, from: SocketAddr, target: SocketAddr) -> Result<PeerStream, Refusal> {
        let IpAddr::V4(destination) = target.ip() else {
            return Err(Refusal::NoRoute);
        };
        let router = {
            let held = self.locked();
            held.get(&destination).map(Arc::clone)
        };
        let Some(router) = router else {
            return Err(Refusal::NoRoute);
        };
        router.inbound(from, target.port())
    }
}

/// One simulated Mac, as the thing a punch is handed.
///
/// A named type rather than the `Arc<NatBox>` itself because a trait from
/// another crate cannot be implemented on `Arc`. Clone is cheap and shares one
/// box, which is what a second task on the same Mac should see.
#[derive(Clone)]
pub struct Node {
    router: Arc<NatBox>,
}

impl Node {
    /// This node's public address, which is what a peer punches at.
    pub fn public_ip(&self) -> IpAddr {
        self.router.public_ip()
    }

    /// Every (external port, internal port) pair its box holds. See
    /// [`NatBox::mappings`].
    pub fn mappings(&self) -> Vec<(u16, u16)> {
        self.router.mappings()
    }

    /// Every refusal its box has made, in order.
    pub fn refusals(&self) -> Vec<Refusal> {
        self.router.refusals()
    }
}

impl PunchNet for Node {
    /// Bind the internal port and wait for something to arrive on it.
    ///
    /// Binding does NOT open a mapping, which is the simulator's whole point:
    /// a node that only listens stays undialable until something it sent out
    /// opened a hole.
    async fn accept_on(&self, port: u16, within: Duration) -> io::Result<PeerStream> {
        let mut inbox = self.router.bind(port);
        match tokio::time::timeout(within, inbox.recv()).await {
            Ok(Some(stream)) => Ok(stream),
            Ok(None) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "nat: the box dropped this listener",
            )),
            Err(_elapsed) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("nat: nothing arrived on internal port {port} inside the slot"),
            )),
        }
    }

    /// Leave from `port`, through this box, towards `target`.
    ///
    /// `within` is unused: delivery here is a function call, so there is no
    /// flight time to bound. The shipped implementation bounds a real connect
    /// with it, and the punch's own patience comes from the slot window rather
    /// than from this argument.
    async fn connect_from(
        &self,
        port: u16,
        target: SocketAddr,
        _within: Duration,
    ) -> io::Result<PeerStream> {
        let external = self.router.open_mapping(port, target);
        let from = SocketAddr::new(IpAddr::V4(self.router.public), external);
        self.router
            .internet
            .deliver(from, target)
            .map_err(io::Error::from)
    }
}
