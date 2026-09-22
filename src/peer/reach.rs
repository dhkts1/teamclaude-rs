//! What this Mac can be reached on from somewhere that is not this LAN.
//!
//! Nothing here decides trust. Every address and port below is ROUTING ADVICE,
//! exactly as [`crate::peer::config::PeerRow`] says of the addresses it stores:
//! identity stays the pinned static key and is re-proven by the handshake every
//! time, so a forged mapping or a guessed port buys an attacker a connection
//! that then fails the pin check.
//!
//! **A port on the router.** [`NatPmp`] asks the gateway for one, in the 12
//! bytes RFC 6886 defines. Hand-written rather than a crate: the whole protocol
//! is two request shapes and two response shapes, sent as UDP to one host on
//! the local link.
//!
//! **An address that needs no router at all.** [`global_v6_addresses`]. A
//! global IPv6 address is reachable directly, so two Macs that both have one
//! need no mapping, no relay and no hole punch.
//!
//! # Why not a STUN client
//!
//! A reflexive address (RFC 8489) tells this Mac what a third party sees it
//! from, which is the right answer to a different question and needs a third
//! party. Everything here is answerable from this machine and its own router,
//! so `tcr peer reach` works on a mesh of exactly two Macs with nothing
//! deployed anywhere.
//!
//! # The derived port is dial-only today
//!
//! [`accepted_ports`] and [`port_is_accepted`] answer "what port would the
//! other end of a completed handshake accept from us in this slot", which is
//! exactly the question a DIALLER asks before it tries a rendezvous port a
//! recorded endpoint stopped answering on ([`rendezvous_ports`], the caller
//! `tests/peer_reach.rs` exercises). Nothing in this repository BINDS on a
//! derived port or checks an inbound connection's source port against
//! [`port_is_accepted`]: [`crate::peer::listener::serve`] binds the one
//! configured `listen` address, and that is the only socket this node ever
//! answers on. A reader is not meant to infer a listener side from a
//! function's mere existence; there is none, and `port_is_accepted` is kept
//! for the day a listener-side check is added, not because one runs now.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context as _, Result};

/// The UDP port every NAT-PMP gateway listens on (RFC 6886 § 3).
pub const GATEWAY_PORT: u16 = 5351;

/// The only protocol version RFC 6886 defines, and the only one a gateway may
/// answer: a response carrying anything else is refused rather than guessed at,
/// because the layout of every field after the version byte is what the version
/// selects.
pub const VERSION: u8 = 0;

/// Opcode 0, "tell me my external address" (RFC 6886 § 3.2).
const OP_EXTERNAL: u8 = 0;
/// Opcode 1, map a UDP port (RFC 6886 § 3.3).
const OP_MAP_UDP: u8 = 1;
/// Opcode 2, map a TCP port (RFC 6886 § 3.3).
const OP_MAP_TCP: u8 = 2;
/// A response carries the request's opcode with the top bit set (RFC 6886 § 3).
const RESPONSE_BIT: u8 = 0x80;

/// RFC 6886 § 3.1: wait 250 ms for the first reply, then double.
const FIRST_TIMEOUT: Duration = Duration::from_millis(250);

/// How many times a request is sent before the gateway is called silent.
///
/// The RFC allows nine, doubling to about a minute. Three is the number for an
/// interactive verb: 250 ms + 500 ms + 1 s is 1.75 s worst case, and a gateway
/// that has not answered three times in 1.75 s on the local link is not a
/// gateway that speaks NAT-PMP. A silent gateway is a normal outcome, most
/// routers ship with this off, so it is an error value, never a panic.
pub const TRIES: u32 = 3;

/// The largest response any opcode here produces, so one buffer serves all of
/// them: 16 bytes for a mapping response (RFC 6886 § 3.3).
const MAX_RESPONSE: usize = 16;

/// Which transport a mapping is for.
///
/// A typed pair rather than the wire's opcode byte: the byte appears once, in
/// [`Self::request_opcode`], and a caller that had to remember "1 is UDP" is a
/// caller that maps TCP into the UDP table once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapProtocol {
    /// Opcode 1.
    Udp,
    /// Opcode 2.
    Tcp,
}

impl MapProtocol {
    /// The request opcode for this protocol.
    fn request_opcode(self) -> u8 {
        match self {
            Self::Udp => OP_MAP_UDP,
            Self::Tcp => OP_MAP_TCP,
        }
    }

    /// The name used in output a human reads.
    pub fn label(self) -> &'static str {
        match self {
            Self::Udp => "udp",
            Self::Tcp => "tcp",
        }
    }
}

/// The gateway's verdict on one request (RFC 6886 § 3.5).
///
/// [`Self::Unknown`] rather than a refusal on an unrecognised number: the RFC
/// reserves the space and a future gateway may use it, and "the router said 7"
/// is a more useful thing to print than "malformed".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultCode {
    /// 0.
    Success,
    /// 1: the gateway does not speak version 0.
    UnsupportedVersion,
    /// 2: NAT-PMP is off, or this host is not allowed to ask. **The common
    /// one**: most routers ship with port mapping disabled.
    NotAuthorized,
    /// 3: the gateway itself has no external address yet.
    NetworkFailure,
    /// 4: the gateway is out of mapping table entries.
    OutOfResources,
    /// 5: the gateway does not know this opcode.
    UnsupportedOpcode,
    /// Anything else the gateway sent.
    Unknown(u16),
}

impl ResultCode {
    /// Read a result code off the wire.
    fn from_wire(code: u16) -> Self {
        match code {
            0 => Self::Success,
            1 => Self::UnsupportedVersion,
            2 => Self::NotAuthorized,
            3 => Self::NetworkFailure,
            4 => Self::OutOfResources,
            5 => Self::UnsupportedOpcode,
            other => Self::Unknown(other),
        }
    }
}

impl std::fmt::Display for ResultCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Success => f.write_str("success"),
            Self::UnsupportedVersion => f.write_str("unsupported NAT-PMP version"),
            Self::NotAuthorized => {
                f.write_str("not authorized (port mapping is off on the router)")
            }
            Self::NetworkFailure => f.write_str("the router has no external address"),
            Self::OutOfResources => f.write_str("the router's mapping table is full"),
            Self::UnsupportedOpcode => f.write_str("the router does not know this request"),
            Self::Unknown(code) => write!(f, "result code {code}"),
        }
    }
}

/// Everything that can stop a reachability probe, each one a value a caller
/// prints rather than a condition it panics on.
#[derive(Debug, thiserror::Error)]
pub enum ReachError {
    /// No default route with a gateway address on it.
    #[error("peer reach: no default IPv4 gateway found ({0})")]
    NoGateway(String),
    /// The local socket would not open, bind, send or receive.
    #[error("peer reach: the local UDP socket failed: {0}")]
    Socket(#[source] std::io::Error),
    /// The gateway never answered. **Not a failure of this Mac**: a router with
    /// NAT-PMP off usually drops the packet rather than refusing it.
    #[error("peer reach: the gateway {gateway} did not answer after {tries} tries")]
    Silent {
        /// Who was asked.
        gateway: SocketAddr,
        /// How many times.
        tries: u32,
    },
    /// The gateway refused the packet rather than answering it: an ICMP port
    /// unreachable, which arrives here as `ConnectionRefused`, or the reset
    /// some gateways send instead.
    ///
    /// **Not a fault of the local socket**, which is what this used to be
    /// reported as, and what it cost: a router with no NAT-PMP service on port
    /// [`GATEWAY_PORT`] refuses the probe, the refusal came back as
    /// [`Self::Socket`], and [`MappingKeeper::map`] asks UPnP only about an
    /// answer that means "this router may not speak NAT-PMP at all". So a
    /// router that speaks UPnP and not NAT-PMP was never asked over UPnP, and
    /// `tcr peer reach` told its operator the local UDP socket had failed.
    #[error(
        "peer reach: the gateway {gateway} refused the NAT-PMP probe, so nothing there speaks \
         it ({source})"
    )]
    NoNatPmp {
        /// Who refused.
        gateway: SocketAddr,
        /// What the socket reported.
        #[source]
        source: std::io::Error,
    },
    /// The gateway answered and said no.
    #[error("peer reach: the gateway refused: {0}")]
    Refused(ResultCode),
    /// The gateway answered with something that is not a NAT-PMP response.
    #[error("peer reach: the gateway's answer was not readable: {0}")]
    Malformed(String),
}

impl ReachError {
    /// Does this answer mean "there is no NAT-PMP service on that router"?
    ///
    /// Two shapes, one meaning. A router with the service off usually drops the
    /// probe ([`Self::Silent`]); one with nothing bound on [`GATEWAY_PORT`]
    /// refuses it ([`Self::NoNatPmp`]). Both are answers about the ROUTER and
    /// neither is a decision the router made about this node, which is what
    /// separates them from [`Self::Refused`]: a gateway that read the request
    /// and said no has decided, and asking a second protocol would be routing
    /// around it.
    ///
    /// One predicate rather than a `matches!` at each site, because the two
    /// sites are the UPnP fallback and the retry ladder, and a router the
    /// fallback treats as mapless while the ladder treats it as broken is a
    /// router asked over UPnP every five seconds forever.
    pub fn no_natpmp_service(&self) -> bool {
        matches!(self, Self::Silent { .. } | Self::NoNatPmp { .. })
    }
}

/// The gateway's own view of the internet, as opcode 0 reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExternalAddress {
    /// The address the world sees this network as.
    pub addr: Ipv4Addr,
    /// The gateway's seconds-since-start-of-epoch counter. Kept because a
    /// DECREASE in it across two probes is how RFC 6886 § 3.6 says a client
    /// learns the router rebooted and dropped every mapping it holds.
    pub epoch_secs: u32,
}

/// One mapping, as the gateway granted it, never as it was asked for.
///
/// Every field is the gateway's answer and not the request's wish: RFC 6886
/// § 3.3 lets a gateway hand back a different external port and a shorter
/// lifetime than the one asked for, and a caller that assumed its own numbers
/// would advertise a port nothing is listening behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mapping {
    /// Which table the mapping is in.
    pub protocol: MapProtocol,
    /// The port on this Mac.
    pub internal_port: u16,
    /// The port on the router. **This is the one to advertise.**
    pub external_port: u16,
    /// How long the gateway says it will hold it, in seconds.
    pub lifetime_secs: u32,
    /// The gateway's epoch counter, for the reboot check in
    /// [`ExternalAddress::epoch_secs`].
    pub epoch_secs: u32,
}

/// A NAT-PMP client pointed at one gateway.
///
/// Synchronous on purpose, though the rest of this crate is async. The only
/// caller is a CLI verb that has nothing else in flight, and a blocking
/// [`UdpSocket`] with a read timeout is drivable from a plain thread, which is
/// what lets the test suite stand a fake gateway on loopback with no runtime
/// and no network.
#[derive(Debug, Clone, Copy)]
pub struct NatPmp {
    gateway: SocketAddr,
}

impl NatPmp {
    /// Point a client at a gateway address, for a test's fake responder or an
    /// operator who knows better than the route table.
    pub fn at(gateway: SocketAddr) -> Self {
        Self { gateway }
    }

    /// Point a client at this machine's default IPv4 gateway.
    pub fn on_default_gateway() -> Result<Self, ReachError> {
        let gateway = default_gateway()?;
        Ok(Self::at(SocketAddr::new(IpAddr::V4(gateway), GATEWAY_PORT)))
    }

    /// Who this client talks to.
    /// This Mac's own address on the LAN the gateway is on, which is what a
    /// UPnP `AddPortMapping` names as the internal client.
    ///
    /// The kernel's own source-address selection, read back off a connected UDP
    /// socket, the same trick and for the same reason as
    /// [`global_v6_addresses`]: `connect` on a datagram socket sends NOTHING,
    /// it records a destination and runs the route lookup, so this touches no
    /// network. NAT-PMP never needs it (the gateway reads the source address off
    /// the packet), which is why it arrives with the UPnP fallback and not
    /// before.
    pub fn internal_address(&self) -> Result<Ipv4Addr, ReachError> {
        let socket = std::net::UdpSocket::bind("0.0.0.0:0").map_err(ReachError::Socket)?;
        socket.connect(self.gateway).map_err(ReachError::Socket)?;
        match socket.local_addr().map_err(ReachError::Socket)? {
            SocketAddr::V4(local) => Ok(*local.ip()),
            SocketAddr::V6(local) => Err(ReachError::Malformed(format!(
                "peer internet: the route to {} is v6 ({local}) and a UPnP internal client                  is an IPv4 address",
                self.gateway
            ))),
        }
    }

    pub fn gateway(&self) -> SocketAddr {
        self.gateway
    }

    /// Opcode 0: ask the gateway what the world sees this network as.
    pub fn external_address(&self) -> Result<ExternalAddress, ReachError> {
        let response = self.exchange(&[VERSION, OP_EXTERNAL], OP_EXTERNAL, 12)?;
        // Bytes 8..12 are the external IPv4 address (RFC 6886 § 3.2).
        let octets: [u8; 4] = response[8..12]
            .try_into()
            .map_err(|_| ReachError::Malformed("the external address field was short".into()))?;
        Ok(ExternalAddress {
            addr: Ipv4Addr::from(octets),
            epoch_secs: epoch_secs(&response)?,
        })
    }

    /// Opcode 1 or 2: ask for a mapping from an external port to `internal_port`
    /// on this Mac, held for `lifetime_secs`.
    ///
    /// `suggested_external` is a wish, not a reservation. Pass `internal_port`
    /// for the RFC's recommended first attempt, or `0` to let the gateway pick;
    /// either way the port to advertise is the one in the returned
    /// [`Mapping::external_port`].
    pub fn map(
        &self,
        protocol: MapProtocol,
        internal_port: u16,
        suggested_external: u16,
        lifetime_secs: u32,
    ) -> Result<Mapping, ReachError> {
        let opcode = protocol.request_opcode();
        let mut request = Vec::with_capacity(12);
        request.push(VERSION);
        request.push(opcode);
        // Two reserved bytes, which RFC 6886 § 3.3 requires be zero.
        request.extend_from_slice(&0_u16.to_be_bytes());
        request.extend_from_slice(&internal_port.to_be_bytes());
        request.extend_from_slice(&suggested_external.to_be_bytes());
        request.extend_from_slice(&lifetime_secs.to_be_bytes());

        let response = self.exchange(&request, opcode, 16)?;
        let granted_internal = be_u16(&response, 8)?;
        let granted_external = be_u16(&response, 10)?;
        let granted_lifetime = be_u32(&response, 12)?;
        Ok(Mapping {
            protocol,
            internal_port: granted_internal,
            external_port: granted_external,
            lifetime_secs: granted_lifetime,
            epoch_secs: epoch_secs(&response)?,
        })
    }

    /// Re-ask for the mapping this Mac already holds, extending its lifetime.
    ///
    /// The same request as [`Self::map`], which is the whole renewal protocol:
    /// RFC 6886 § 3.3 says a repeated request for an existing mapping refreshes
    /// it rather than adding a second one. Re-asks for the external port the
    /// gateway actually granted, so a renewal cannot silently move the port
    /// every peer was told about.
    pub fn renew(&self, mapping: &Mapping, lifetime_secs: u32) -> Result<Mapping, ReachError> {
        self.map(
            mapping.protocol,
            mapping.internal_port,
            mapping.external_port,
            lifetime_secs,
        )
    }

    /// Drop a mapping: RFC 6886 § 3.3's lifetime 0 with a suggested external
    /// port of 0.
    ///
    /// Returns the gateway's response so a caller can log what it confirmed.
    /// The RFC requires the external port in a delete response to be 0, and a
    /// gateway that answers otherwise has not deleted what was asked.
    pub fn delete(&self, protocol: MapProtocol, internal_port: u16) -> Result<Mapping, ReachError> {
        let confirmed = self.map(protocol, internal_port, 0, 0)?;
        if confirmed.external_port != 0 || confirmed.lifetime_secs != 0 {
            return Err(ReachError::Malformed(format!(
                "a delete was confirmed with external port {} and lifetime {}, and RFC 6886 \
                 requires both to be zero",
                confirmed.external_port, confirmed.lifetime_secs
            )));
        }
        Ok(confirmed)
    }

    /// Send `request` up to [`TRIES`] times and return the first well-formed
    /// response for `request_opcode`.
    ///
    /// Three things are checked before the bytes are handed back, and each one
    /// is a way a reply can look fine and mean something else: the version (the
    /// field layout depends on it), the opcode (a gateway answering an earlier
    /// request of ours would otherwise be read as answering this one), and the
    /// result code (a refusal carries a well-formed header and zeroes in every
    /// field a caller would go on to read).
    fn exchange(
        &self,
        request: &[u8],
        request_opcode: u8,
        expected_len: usize,
    ) -> Result<Vec<u8>, ReachError> {
        let bind: SocketAddr = match self.gateway {
            SocketAddr::V4(_) => "0.0.0.0:0".parse(),
            SocketAddr::V6(_) => "[::]:0".parse(),
        }
        .map_err(|err: std::net::AddrParseError| {
            ReachError::Malformed(format!("the bind address would not parse: {err}"))
        })?;
        let socket = UdpSocket::bind(bind).map_err(ReachError::Socket)?;
        socket.connect(self.gateway).map_err(ReachError::Socket)?;

        let mut timeout = FIRST_TIMEOUT;
        for _ in 0..TRIES {
            socket
                .set_read_timeout(Some(timeout))
                .map_err(ReachError::Socket)?;
            // Both halves are classified, because a refusal can land on
            // either: the ICMP port unreachable for one send is delivered to
            // the next operation on the connected socket, which is this recv
            // on the same pass and this send on the following one.
            socket.send(request).map_err(|err| self.probe_error(err))?;

            let mut buffer = [0_u8; MAX_RESPONSE];
            match socket.recv(&mut buffer) {
                Ok(read) => {
                    let response = &buffer[..read];
                    return interpret(response, request_opcode, expected_len);
                }
                Err(err)
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    // RFC 6886 § 3.1: double and try again.
                    timeout *= 2;
                }
                Err(err) => return Err(self.probe_error(err)),
            }
        }
        Err(ReachError::Silent {
            gateway: self.gateway,
            tries: TRIES,
        })
    }

    /// Name a socket error from the [`GATEWAY_PORT`] exchange for what it says
    /// about the ROUTER, where it says anything.
    ///
    /// A refused datagram and a reset are the gateway's answer, not this Mac's
    /// failure; everything else is the local socket and keeps its old name.
    fn probe_error(&self, err: std::io::Error) -> ReachError {
        match err.kind() {
            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::ConnectionReset => {
                ReachError::NoNatPmp {
                    gateway: self.gateway,
                    source: err,
                }
            }
            _ => ReachError::Socket(err),
        }
    }
}

/// Check a NAT-PMP response's header and length, and refuse it otherwise.
///
/// Free of the socket so the suite can drive every refusal shape, a short
/// frame, the wrong version, another opcode's answer, a non-zero result code,
/// without standing up a responder for each one.
pub fn interpret(
    response: &[u8],
    request_opcode: u8,
    expected_len: usize,
) -> Result<Vec<u8>, ReachError> {
    if response.len() < expected_len {
        return Err(ReachError::Malformed(format!(
            "expected at least {expected_len} bytes, got {}",
            response.len()
        )));
    }
    if response[0] != VERSION {
        return Err(ReachError::Malformed(format!(
            "expected version {VERSION}, got {}",
            response[0]
        )));
    }
    let expected_opcode = request_opcode | RESPONSE_BIT;
    if response[1] != expected_opcode {
        return Err(ReachError::Malformed(format!(
            "expected opcode {expected_opcode}, got {}",
            response[1]
        )));
    }
    let code = ResultCode::from_wire(be_u16(response, 2)?);
    if code != ResultCode::Success {
        return Err(ReachError::Refused(code));
    }
    Ok(response[..expected_len].to_vec())
}

/// Bytes 4..8 of every response: the gateway's epoch counter.
fn epoch_secs(response: &[u8]) -> Result<u32, ReachError> {
    be_u32(response, 4)
}

/// A big-endian `u16` at `offset`, or a refusal naming the offset.
fn be_u16(bytes: &[u8], offset: usize) -> Result<u16, ReachError> {
    let slice = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| ReachError::Malformed(format!("no u16 at byte {offset}")))?;
    let pair: [u8; 2] = slice
        .try_into()
        .map_err(|_| ReachError::Malformed(format!("no u16 at byte {offset}")))?;
    Ok(u16::from_be_bytes(pair))
}

/// A big-endian `u32` at `offset`, or a refusal naming the offset.
fn be_u32(bytes: &[u8], offset: usize) -> Result<u32, ReachError> {
    let slice = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| ReachError::Malformed(format!("no u32 at byte {offset}")))?;
    let quad: [u8; 4] = slice
        .try_into()
        .map_err(|_| ReachError::Malformed(format!("no u32 at byte {offset}")))?;
    Ok(u32::from_be_bytes(quad))
}

/// This machine's default IPv4 gateway.
///
/// Two sources, in this order, because on this box the first one alone finds
/// nothing. `route -n get default` reports the route the kernel would actually
/// take, which on a Mac with a VPN up is a `utun` interface, and a point-to-point
/// interface's route has NO gateway address at all, so the output carries
/// `interface: utun4` and no `gateway:` line. `netstat -rn -f inet` lists every
/// default route, including the physical one through the LAN router, which is the
/// only one a NAT-PMP request could ever be answered by.
fn default_gateway() -> Result<Ipv4Addr, ReachError> {
    #[cfg(target_os = "macos")]
    {
        if let Some(found) = run_route_tool(&["route", "-n", "get", "default"], parse_route_get) {
            return Ok(found);
        }
        if let Some(found) = run_route_tool(&["netstat", "-rn", "-f", "inet"], parse_netstat_inet) {
            return Ok(found);
        }
        Err(ReachError::NoGateway(
            "neither `route -n get default` nor `netstat -rn -f inet` named one".into(),
        ))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let table = std::fs::read_to_string("/proc/net/route")
            .map_err(|err| ReachError::NoGateway(format!("/proc/net/route: {err}")))?;
        parse_proc_net_route(&table)
            .ok_or_else(|| ReachError::NoGateway("/proc/net/route lists no default route".into()))
    }
}

/// Run one route-table tool and hand its stdout to `parse`.
///
/// A missing tool, a non-zero exit and unreadable output are all one outcome
/// here, [`None`], so the caller falls through to the next source, because
/// there is a second source and the operator gets one refusal naming both.
#[cfg(target_os = "macos")]
fn run_route_tool(argv: &[&str], parse: fn(&str) -> Option<Ipv4Addr>) -> Option<Ipv4Addr> {
    let (program, args) = argv.split_first()?;
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse(&String::from_utf8_lossy(&output.stdout))
}

/// The `gateway:` line of `route -n get default`.
///
/// Returns [`None`] when there is no such line, which is the normal shape for a
/// default route over a point-to-point interface. See [`default_gateway`].
pub fn parse_route_get(output: &str) -> Option<Ipv4Addr> {
    output
        .lines()
        .filter_map(|line| line.trim().strip_prefix("gateway:"))
        .filter_map(|value| value.trim().parse::<Ipv4Addr>().ok())
        .next()
}

/// The first `default` row of `netstat -rn -f inet` whose gateway is an address.
///
/// The rows whose gateway reads `link#24` are the point-to-point ones, and they
/// are skipped by the parse rather than by a special case: a gateway that is not
/// an IPv4 address is not somewhere a 12-byte UDP request can be sent.
pub fn parse_netstat_inet(output: &str) -> Option<Ipv4Addr> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            if fields.next() != Some("default") {
                return None;
            }
            fields.next()?.parse::<Ipv4Addr>().ok()
        })
        .next()
}

/// The gateway of the default route in a `/proc/net/route` table (Linux).
///
/// The columns are `Iface Destination Gateway Flags ...` and every address is
/// hex in the HOST's byte order, which on every platform this ships to is
/// little-endian, so `0101A8C0` is 192.168.1.1 and reading it big-endian would
/// give 1.1.168.192, a plausible-looking address pointing nowhere.
pub fn parse_proc_net_route(table: &str) -> Option<Ipv4Addr> {
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let _iface = fields.next()?;
            let destination = fields.next()?;
            let gateway = fields.next()?;
            if destination != "00000000" {
                return None;
            }
            let raw = u32::from_str_radix(gateway, 16).ok()?;
            let addr = Ipv4Addr::from(raw.to_le_bytes());
            if addr.is_unspecified() {
                return None;
            }
            Some(addr)
        })
        .next()
}

/// Whether an IPv6 address is one the open internet can route to this Mac on.
///
/// Stated as a list of exclusions taken from the registry rather than as
/// "starts with 2 or 3": `2000::/3` is today's global unicast assignment and
/// not a permanent definition, and the address that matters most to get right
/// here is the one a VPN hands out, a unique-local `fd00::/8`, which looks
/// like an address, answers a bind, and is unreachable from anywhere else.
pub fn is_global_v6(addr: &Ipv6Addr) -> bool {
    let segments = addr.segments();
    // Unspecified, loopback and multicast are not unicast addresses at all.
    if addr.is_unspecified() || addr.is_loopback() || addr.is_multicast() {
        return false;
    }
    // fe80::/10 link-local: scoped to one link, and the scope id is not
    // something another Mac can be told.
    if segments[0] & 0xffc0 == 0xfe80 {
        return false;
    }
    // fc00::/7 unique-local (RFC 4193). Tailscale and most VPNs hand these out.
    if segments[0] & 0xfe00 == 0xfc00 {
        return false;
    }
    // 2001:db8::/32, reserved for documentation (RFC 3849).
    if segments[0] == 0x2001 && segments[1] == 0x0db8 {
        return false;
    }
    // ::ffff:0:0/96 v4-mapped and ::/96 v4-compatible: an IPv4 address wearing
    // a v6 shape, whose reachability is the IPv4 question and not this one.
    if addr.to_ipv4_mapped().is_some() || addr.to_ipv4().is_some() {
        return false;
    }
    // 100::/64, discard-only (RFC 6666).
    if segments[0] == 0x0100 && segments[1..4] == [0, 0, 0] {
        return false;
    }
    true
}

/// The global IPv6 addresses this Mac would send from.
///
/// # What this returns and what it does not
///
/// The kernel's own source-address selection, read back off a connected UDP
/// socket. `connect` on a datagram socket sends NOTHING: it records a
/// destination and runs the route lookup, so this touches no network, and the
/// destinations asked about are `2001:db8::1` (reserved for documentation, RFC
/// 3849) and `2001:4860:4860::8888`, a public resolver, so a box with a default
/// v6 route but no route for the documentation prefix still answers.
///
/// It is therefore the address this Mac would be SEEN from, which is the one a
/// peer needs, and **not** the full interface list. A Mac with two global
/// addresses on two interfaces reports the preferred one. The full list needs
/// `getifaddrs`, which is C and cannot be called from this crate
/// (`#![forbid(unsafe_code)]`, `src/lib.rs:8`); the crate that wraps it,
/// `if-addrs`, is already resolved in `Cargo.lock` through `mdns-sd`, so it
/// would add a dependency EDGE and no package, an ask recorded for the lead
/// rather than taken.
///
/// An empty list is a normal, common answer: measured on this machine, whose
/// only non-link-local v6 addresses are two unique-local ones from VPN
/// interfaces.
pub fn global_v6_addresses() -> Vec<Ipv6Addr> {
    /// Destinations whose route lookup reveals a source address. Both are
    /// off-link on purpose: a link-local destination would select a link-local
    /// source, which is exactly the answer this function must not give.
    const PROBES: [&str; 2] = ["[2001:db8::1]:9", "[2001:4860:4860::8888]:9"];

    let mut found: Vec<Ipv6Addr> = Vec::new();
    for probe in PROBES {
        let Ok(destination) = probe.parse::<SocketAddr>() else {
            continue;
        };
        let Ok(socket) = UdpSocket::bind("[::]:0") else {
            continue;
        };
        if socket.connect(destination).is_err() {
            continue;
        }
        let Ok(SocketAddr::V6(local)) = socket.local_addr() else {
            continue;
        };
        let addr = *local.ip();
        if is_global_v6(&addr) && !found.contains(&addr) {
            found.push(addr);
        }
    }
    found
}

// ---------------------------------------------------------------------------
// The mapping this node holds open while `peer.internet` is on
// ---------------------------------------------------------------------------

/// How long a mapping is asked for, in seconds.
///
/// Two hours, renewed every [`MAPPING_RENEW_INTERVAL`], so a renewal that is
/// lost still leaves an hour and a half of mapping behind it. The pair of
/// numbers is the shape ruling here: the listener binds ONE stable
/// port and the router mapping is stable. The clock-derived port
/// ([`derived_port`]) is a rendezvous hint for the DIALLER and never something
/// this side rebinds or remaps.
pub const MAPPING_LIFETIME_SECS: u32 = 7_200;

/// How often the mapping is re-asked for: well inside
/// [`MAPPING_LIFETIME_SECS`], so three renewals in a row have to fail before a
/// peer loses the address it was told.
pub const MAPPING_RENEW_INTERVAL: Duration = Duration::from_secs(1_800);

/// How often the keeper wakes to see whether it has been asked to stop.
///
/// The renewal wait is slept in ticks of this rather than in one long sleep:
/// a shutdown must delete the mapping now, not up to half an hour from now.
pub const MAPPING_STOP_POLL: Duration = Duration::from_millis(250);

/// How long to wait before asking a router that would not map again, by how
/// many refusals in a row it has given.
///
/// The first wait is [`INTERNET_POLL_INTERVAL`], which is what every wait used
/// to be: a router with no mapping service on it was asked again every five
/// seconds, for as long as the process ran, and a day of that is thousands of
/// round trips and two log lines each. The last rung is the cap, so a router
/// that will never map is still re-asked often enough to notice the day it is
/// replaced or its settings change.
///
/// A ladder of fixed rungs rather than a doubling, so the wait an operator
/// sees is one of four numbers they can read here rather than an arithmetic
/// result, and so a test can assert the sequence.
pub const MAPPING_RETRY_LADDER: [Duration; 4] = [
    Duration::from_secs(5),
    Duration::from_secs(30),
    Duration::from_secs(120),
    Duration::from_secs(600),
];

/// How much of a wait passes before the default gateway is re-read, while a
/// refusal is being waited out.
///
/// Read on a clock of its own rather than at every wake, because reading it
/// runs a route-table tool: at the top of the ladder that is one subprocess a
/// half-minute instead of one every five seconds, and the whole point of the
/// ladder is to stop paying per wake for a router that has already answered.
pub const GATEWAY_RECHECK_INTERVAL: Duration = Duration::from_secs(30);

/// What the router last answered a mapping request with.
///
/// Two values rather than a `bool`, because both of them are logged and the
/// name is what the reader of the log sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MappingAnswer {
    /// A port was mapped.
    Mapped,
    /// No protocol would map one.
    Refused,
}

/// When a node whose router would not map may ask it again.
///
/// Pure, and driven by elapsed time handed in rather than by a clock it reads,
/// so the suite asserts the SEQUENCE of intervals rather than sleeping through
/// them.
///
/// # What it is for
///
/// `keep_internet_mapping` wakes every [`INTERNET_POLL_INTERVAL`] and starts a
/// keeper whenever none is running. A keeper whose router refuses dies within a
/// couple of seconds, so "none is running" was true at every single wake and
/// the router was asked forever, at five-second spacing, by a node that had
/// already been told no.
#[derive(Debug, Clone, Default)]
pub struct MappingRetry {
    /// How many times in a row the router would not map.
    refusals: u32,
    /// How long since the last request went out.
    waited: Duration,
    /// How long since the default gateway was last read.
    since_gateway_read: Duration,
    /// The gateway the last request went to, so a node that has moved to
    /// another network asks the new router straight away.
    gateway: Option<Ipv4Addr>,
}

impl MappingRetry {
    /// A ladder at its first rung, having asked nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// How long this node waits before asking again, given what it has been
    /// answered so far. Zero when nothing has been refused yet, which is the
    /// boot case: the first request goes out at once.
    pub fn interval(&self) -> Duration {
        if self.refusals == 0 {
            return Duration::ZERO;
        }
        let rung = usize::try_from(self.refusals)
            .unwrap_or(MAPPING_RETRY_LADDER.len())
            .min(MAPPING_RETRY_LADDER.len())
            .saturating_sub(1);
        MAPPING_RETRY_LADDER[rung]
    }

    /// Time has passed: `elapsed` more of it.
    pub fn tick(&mut self, elapsed: Duration) {
        self.waited = self.waited.saturating_add(elapsed);
        self.since_gateway_read = self.since_gateway_read.saturating_add(elapsed);
    }

    /// May the router be asked now?
    pub fn due(&self) -> bool {
        self.waited >= self.interval()
    }

    /// Is enough of a wait behind this node to be worth re-reading the default
    /// gateway? Always false while nothing has been refused, since a node that
    /// is being answered has no ladder to reset.
    pub fn gateway_read_due(&self) -> bool {
        self.refusals > 0 && self.since_gateway_read >= GATEWAY_RECHECK_INTERVAL
    }

    /// The default gateway reads as `gateway` now. Answers whether that is a
    /// different router than the one that refused, which puts the ladder back
    /// on its first rung: a Mac carried to another network is one request away
    /// from a mapping, and making it wait out the old router's ten minutes is
    /// making it wait for nothing.
    pub fn gateway_is_now(&mut self, gateway: Option<Ipv4Addr>) -> bool {
        self.since_gateway_read = Duration::ZERO;
        let moved = self.gateway.is_some() && self.gateway != gateway;
        self.gateway = gateway;
        if moved {
            self.reset();
        }
        moved
    }

    /// A request is going out now.
    pub fn asked(&mut self) {
        self.waited = Duration::ZERO;
    }

    /// The router answered `answer`, which is what moves the ladder: a refusal
    /// steps it up a rung and a mapping puts it back on the first one.
    ///
    /// Which LEVEL that answer is logged at is decided elsewhere, by
    /// [`mapping_answer_voice`], because the log is per gateway and per
    /// process while this ladder belongs to one loop.
    pub fn answered(&mut self, answer: MappingAnswer) {
        match answer {
            MappingAnswer::Mapped => self.refusals = 0,
            MappingAnswer::Refused => self.refusals = self.refusals.saturating_add(1),
        }
    }

    /// Back to the first rung: the switch was toggled, or this Mac is on
    /// another router.
    pub fn reset(&mut self) {
        self.refusals = 0;
        self.waited = Duration::ZERO;
    }

    /// How many refusals in a row are behind the current wait.
    pub fn refusals(&self) -> u32 {
        self.refusals
    }
}

/// The last answer each gateway gave, so the same answer twice is not the same
/// line twice.
///
/// Process-wide and keyed by gateway, the shape `peer state restored` uses
/// (`crate::peer::state`, `restore_counts_changed`): the two writers are the
/// keeper thread and the loop that starts it, which have no channel between
/// them, and a Mac that moves to another router gets its own first line there.
static LAST_MAPPING_ANSWER: OnceLock<Mutex<HashMap<SocketAddr, MappingAnswer>>> = OnceLock::new();

/// How loudly a router's answer is worth saying.
///
/// The decision, named and returned, rather than a `bool` each log site reads
/// its own way: the two sites are three hundred lines apart and one of them
/// used to write a warning every five seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerVoice {
    /// Something an operator has not been told: one line at WARN.
    News,
    /// The same answer as last time: kept at DEBUG, or at INFO for a mapping,
    /// where the line is the ordinary record of a keeper starting.
    Repeat,
}

/// Record what `gateway` answered and say how loudly to say it.
///
/// The two answers are not symmetric, and that is the whole of this function:
/// a refusal is news unless the last answer from this router was also a
/// refusal, while a mapping is news ONLY when the last answer was a refusal,
/// because the first mapping after a boot is the ordinary case and not a thing
/// to warn anybody about.
pub fn mapping_answer_voice(gateway: SocketAddr, answer: MappingAnswer) -> AnswerVoice {
    let previous = LAST_MAPPING_ANSWER
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|held| held.into_inner())
        .insert(gateway, answer);
    let news = match answer {
        MappingAnswer::Refused => previous != Some(MappingAnswer::Refused),
        MappingAnswer::Mapped => previous == Some(MappingAnswer::Refused),
    };
    if news {
        AnswerVoice::News
    } else {
        AnswerVoice::Repeat
    }
}

/// What a keeper did to the router, in the order it did it.
///
/// Recorded as a list rather than inferred from the router's state because the
/// ORDER is the contract: a renewal before the map, or a delete that never
/// ran, are both invisible in a snapshot of the mapping table and both are
/// exactly what this is here to catch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MappingStep {
    /// The first request, which created the mapping.
    Mapped,
    /// A refresh of the mapping this keeper already holds.
    Renewed,
    /// The lifetime-0 request that took it away again.
    Deleted,
    /// The first request, granted over UPnP IGD after NAT-PMP said nothing.
    ///
    /// Its own step and not a second `Mapped`, because which protocol answered
    /// is the fact an operator needs when a mapping appears on one router and
    /// not on another, and a step list that hid it would make the two routers
    /// look identical.
    MappedOverUpnp,
}

/// The UPnP search every production caller uses: the IGD multicast group.
///
/// Named here rather than written at each call site so `tcr peer reach` and the
/// mapping keeper cannot end up searching two different ways and reporting two
/// different answers about one router.
pub fn upnp_discoverer() -> crate::peer::reach_upnp::Discoverer {
    crate::peer::reach_upnp::Discoverer::multicast()
}

/// What a UPnP mapping this node asked for is called in a router's own table.
///
/// An operator reading the admin page has to be able to tell which row is
/// theirs, and "tcr" alone would not say which of the two protocols wrote it.
const UPNP_MAPPING_DESCRIPTION: &str = "tcr peer internet (upnp)";

/// The external socket this node is currently mapped at, for the one producer
/// of `Hello.addrs` to read.
///
/// A process-local register rather than a parameter, and the reason is the
/// shape of the two sides: the keeper runs on its own thread for the life of
/// the listener, and `Hello.addrs` is assembled per connection in
/// `crate::peer::listener::NodeFacts::listening`, three call sites away with
/// no channel between them. Threading it through would put an
/// `Option<SocketAddr>` in every signature between the accept loop and the
/// handshake, which is a wider change than the fact deserves.
///
/// `None` is the default and the common case: no mapping asked for, or the
/// router refused one.
fn mapped_external() -> &'static Mutex<Option<SocketAddr>> {
    static MAPPED: OnceLock<Mutex<Option<SocketAddr>>> = OnceLock::new();
    MAPPED.get_or_init(|| Mutex::new(None))
}

/// The external socket a peer off this LAN could reach this node at, or
/// [`None`] when nothing is mapped.
pub fn external_socket() -> Option<SocketAddr> {
    match mapped_external().lock() {
        Ok(held) => *held,
        // A poisoned lock here means a keeper panicked mid-write. The value
        // behind it is still the last address the router confirmed, and
        // dropping it would silently stop advertising a live mapping.
        Err(poisoned) => *poisoned.into_inner(),
    }
}

/// Where a keeper writes the mapping it holds, or [`None`] in a process that
/// never asked for one.
///
/// A register rather than a parameter for the same reason [`mapped_external`]
/// is one: the keeper runs on its own thread for the life of the listener, and
/// the state file it should write to is known at boot, three call sites away.
/// Set by [`record_mappings_at`], and a process that never calls it records
/// nothing at all, which is what keeps every test that drives a keeper against
/// a fake gateway from writing to a real state file.
fn mapping_record_path() -> &'static Mutex<Option<std::path::PathBuf>> {
    static RECORD_AT: OnceLock<Mutex<Option<std::path::PathBuf>>> = OnceLock::new();
    RECORD_AT.get_or_init(|| Mutex::new(None))
}

/// Record every mapping this process holds in the state file at `path`, so
/// `tcr peer reach` can report it without asking the router.
///
/// Called once, at listener boot, beside the keeper it belongs to.
pub fn record_mappings_at(path: std::path::PathBuf) {
    let mut held = match mapping_record_path().lock() {
        Ok(held) => held,
        Err(poisoned) => poisoned.into_inner(),
    };
    *held = Some(path);
}

/// The external socket a live, unexpired mapping in the state file at
/// `state_path` records, read cold rather than off [`external_socket`].
///
/// `tcr peer invite` and `tcr peer moved mint` both run as their own,
/// short-lived process: neither spawns the keeper that fills
/// [`mapped_external`], so [`external_socket`] would read `None` in either
/// one every time, even with a mapping alive on the router right now. The
/// state file is the one place that mapping is written down for a process
/// that is not the one holding it, `Manager`'s own keeper saves it there on
/// every renewal, so this is the single place both verbs read it back from.
///
/// The second element is the sentence to say when a record exists but yields
/// no socket to dial, in the caller's own voice: this function only ever
/// returns the fact, never prints it, so `mint`'s wording and `invite`'s stay
/// two calls of the one function, not two copies of the one sentence.
pub fn recorded_external_socket(
    state_path: &Path,
    now_ms: i64,
) -> (Option<SocketAddr>, Option<String>) {
    let Some(record) = crate::peer::state::load(state_path, now_ms)
        .ok()
        .and_then(|state| state.mapping)
        .filter(|record| record.expires_at_ms > now_ms)
    else {
        return (None, None);
    };

    match record.external_address.as_deref() {
        Some(text) => match text.parse::<SocketAddr>() {
            Ok(addr) => (Some(addr), None),
            Err(why) => (
                None,
                Some(format!(
                    "the held mapping's external address did not parse ({why})"
                )),
            ),
        },
        None => (
            None,
            Some(
                "the router mapped a port and would not name its own external address".to_string(),
            ),
        ),
    }
}

/// The gateway's view of this Mac's address on the internet, NAT-PMP first
/// and UPnP IGD when NAT-PMP refuses: the same fallback order
/// [`MappingKeeper::map`] runs for the mapping itself, so `tcr peer reach`
/// never claims a router answers nothing when it answers over the one
/// protocol NAT-PMP is not.
///
/// The label says which protocol answered, because a router that answers
/// only one of the two is exactly the case an operator runs this verb to
/// find out about.
pub fn external_address_report(
    client: &NatPmp,
    discoverer: &crate::peer::reach_upnp::Discoverer,
) -> String {
    let nat_pmp_err = match client.external_address() {
        Ok(found) => return format!("{} (nat-pmp)", found.addr),
        Err(err) => err,
    };
    match crate::peer::reach_upnp::UpnpClient::discover(discoverer)
        .and_then(|upnp| upnp.get_external_address())
    {
        Ok(addr) => format!("{addr} (upnp)"),
        Err(upnp_err) => format!("unavailable: {nat_pmp_err} (nat-pmp), {upnp_err} (upnp)"),
    }
}

/// Publish (or withdraw) the external socket [`external_socket`] reports, and
/// record it where a CLI can read it.
///
/// The two go together on purpose: an advertised address and a recorded one
/// that disagree would have `tcr peer reach` describe a mapping no `Hello`
/// carries, which is worse than either alone.
///
/// A failed record is logged and never propagated. The mapping itself is
/// granted and in use by then, and refusing to serve because a report could
/// not be written would trade a working listener for a status line.
fn publish_external(addr: Option<SocketAddr>, held_mapping: Option<&Mapping>) {
    let mut held = match mapped_external().lock() {
        Ok(held) => held,
        Err(poisoned) => poisoned.into_inner(),
    };
    *held = addr;
    drop(held);

    let path = match mapping_record_path().lock() {
        Ok(path) => path.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    let Some(path) = path else {
        return;
    };
    let record = held_mapping.map(|mapping| crate::peer::state::MappingRecord {
        external_address: addr.map(|addr| addr.to_string()),
        external_port: mapping.external_port,
        internal_port: mapping.internal_port,
        expires_at_ms: crate::now_ms()
            .saturating_add(i64::from(mapping.lifetime_secs).saturating_mul(1_000)),
    });
    if let Err(err) = crate::peer::state::save_mapping(&path, record) {
        tracing::warn!(
            error = %err,
            "peer internet: the mapping is held, but it could not be recorded, so \
             `tcr peer reach` will report none until the next renewal"
        );
    }
}

/// A NAT-PMP client holding one mapping open, with the steps it took.
///
/// Synchronous, like [`NatPmp`] itself, so the suite can drive a whole
/// map/renew/delete cycle against a fake gateway on loopback with no runtime
/// and no network.
#[derive(Debug)]
pub struct MappingKeeper {
    client: NatPmp,
    internal_port: u16,
    lifetime_secs: u32,
    held: Option<Mapping>,
    steps: Vec<MappingStep>,
    /// Where to look for a UPnP IGD gateway when NAT-PMP says nothing, or
    /// [`None`] for a keeper that only speaks NAT-PMP.
    ///
    /// Injected rather than built here so the suite can point it at a fake on
    /// loopback: the alternative is a multicast search in a test, which is the
    /// one thing this whole module is arranged to avoid.
    upnp: Option<crate::peer::reach_upnp::Discoverer>,
    /// The UPnP client that granted the held mapping, kept for the renewal and
    /// the delete.
    ///
    /// Held rather than rediscovered, because a second search could land on a
    /// different device and then the renewal would extend a mapping on one
    /// router while the delete took one away on another.
    upnp_client: Option<crate::peer::reach_upnp::UpnpClient>,
}

/// Which protocol granted the mapping a [`MappingKeeper`] holds.
///
/// # Why the keeper has to remember, and what it cost not to
///
/// A mapping is not a fact about a port, it is a fact about a CONVERSATION
/// with one gateway: the renewal and the delete have to go back to the
/// protocol that granted it. Before this existed, `renew` and `delete` spoke
/// NAT-PMP unconditionally, so a UPnP mapping was renewed by asking a silent
/// NAT-PMP gateway (which fails, is logged, and the loop carries on) and
/// deleted the same way. The visible consequences were a mapping that lapsed
/// after [`MAPPING_LIFETIME_SECS`] with peers still holding the address, and a
/// forward left standing on the router at shutdown, pointing at a listener
/// that had stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MappedOver {
    /// NAT-PMP answered the first request.
    NatPmp,
    /// The router had no NAT-PMP service, by silence or by refusing the probe,
    /// and UPnP IGD answered.
    Upnp,
}

impl MappingKeeper {
    /// A keeper for `internal_port` against `client`, holding nothing yet.
    pub fn new(client: NatPmp, internal_port: u16, lifetime_secs: u32) -> Self {
        Self {
            client,
            internal_port,
            lifetime_secs,
            held: None,
            steps: Vec::new(),
            upnp: None,
            upnp_client: None,
        }
    }

    /// Fall back to UPnP IGD, through `discoverer`, when the router has no
    /// NAT-PMP service: [`ReachError::no_natpmp_service`] decides which
    /// answers those are.
    ///
    /// Silence and a refused probe, and nothing else. A gateway that READ the
    /// request and answered no has made a decision this node does not get to
    /// route around by asking a second protocol; a gateway that says nothing,
    /// or refuses the packet outright, is one that may not speak NAT-PMP at
    /// all, which is the case UPnP exists for here.
    #[must_use]
    pub fn with_upnp(mut self, discoverer: crate::peer::reach_upnp::Discoverer) -> Self {
        self.upnp = Some(discoverer);
        self
    }

    /// Ask the router for the mapping.
    ///
    /// The RFC's recommended first attempt: suggest the internal port, and
    /// advertise whatever the gateway hands back instead.
    pub fn map(&mut self) -> Result<Mapping, ReachError> {
        let asked = self.client.map(
            MapProtocol::Tcp,
            self.internal_port,
            self.internal_port,
            self.lifetime_secs,
        );
        let mapping = match asked {
            Ok(mapping) => {
                self.steps.push(MappingStep::Mapped);
                mapping
            }
            // A router with no NAT-PMP service on it, by silence or by
            // refusal, is the only thing UPnP is asked about.
            Err(err) if err.no_natpmp_service() && self.upnp.is_some() => {
                let mapping = self.map_over_upnp()?;
                self.steps.push(MappingStep::MappedOverUpnp);
                mapping
            }
            Err(err) => return Err(err),
        };
        self.held = Some(mapping);
        Ok(mapping)
    }

    /// Ask a UPnP IGD gateway for the same mapping NAT-PMP would not answer
    /// about, and record the external address it names.
    ///
    /// Every refusal keeps its own name on the way out
    /// ([`crate::peer::reach_upnp::UpnpError`] is `Display`), because the three
    /// this client invents rather than reads off the wire,
    /// `UntrustedLocation`, `ControlUrlElsewhere` and `Redirected`, are the
    /// ones an operator cannot diagnose from "no mapping": each says a device
    /// answered and was refused BY THIS NODE, and for a reason worth reading.
    fn map_over_upnp(&mut self) -> Result<Mapping, ReachError> {
        use crate::peer::reach_upnp::UpnpClient;

        let discoverer = self
            .upnp
            .as_ref()
            .ok_or_else(|| ReachError::Malformed("peer internet: no UPnP discoverer".into()))?;
        // A UPnP silence stays a SILENCE on the way out, not a "the gateway's
        // answer was not readable": both protocols saying nothing is the one
        // outcome an operator fixes by looking at the router, and `tcr peer
        // reach` reads this variant to print exactly that. Every other refusal
        // keeps its own words, `UntrustedLocation`, `ControlUrlElsewhere` and
        // `Redirected` included, because each of those says a device answered
        // and THIS NODE refused it.
        let gateway = discoverer.destination();
        let named = move |err: crate::peer::reach_upnp::UpnpError| match err {
            crate::peer::reach_upnp::UpnpError::Silent { .. } => {
                ReachError::Silent { gateway, tries: 1 }
            }
            other => ReachError::Malformed(other.to_string()),
        };

        let client = UpnpClient::discover(discoverer).map_err(named)?;
        let internal = self.client.internal_address().map_err(|err| {
            ReachError::Malformed(format!(
                "peer internet: UPnP needs this Mac's address on the LAN and it could not                  be read: {err}"
            ))
        })?;
        client
            .add_port_mapping(
                MapProtocol::Tcp,
                self.internal_port,
                self.internal_port,
                internal,
                UPNP_MAPPING_DESCRIPTION,
                self.lifetime_secs,
            )
            .map_err(named)?;
        let external = client.get_external_address().map_err(named)?;

        tracing::info!(
            external = %external,
            port = self.internal_port,
            "peer internet: this router has no NAT-PMP and UPnP IGD mapped this node's listener \
             port"
        );
        // UPnP IGD confirms the port asked for or refuses, so there is no
        // granted-port field to read back the way NAT-PMP has one.
        let mapping = Mapping {
            protocol: MapProtocol::Tcp,
            internal_port: self.internal_port,
            external_port: self.internal_port,
            lifetime_secs: self.lifetime_secs,
            epoch_secs: 0,
        };
        // Published with the mapping beside it rather than the address alone:
        // the record and the advertised address are written together, so a
        // reader can never see one without the other.
        publish_external(
            Some(SocketAddr::new(IpAddr::V4(external), self.internal_port)),
            Some(&mapping),
        );
        // Kept, so the renewal and the delete go back to the gateway that
        // granted this, over the protocol that granted it.
        self.upnp_client = Some(client);
        Ok(mapping)
    }

    /// Which protocol granted the mapping this keeper holds, or [`None`] when
    /// it holds none.
    pub fn granted_by(&self) -> Option<MappedOver> {
        self.held?;
        match self.upnp_client {
            Some(_) => Some(MappedOver::Upnp),
            None => Some(MappedOver::NatPmp),
        }
    }

    /// The external socket to advertise for the mapping this keeper holds,
    /// asked of the gateway that granted it.
    ///
    /// [`None`] when nothing is held, or when the gateway mapped the port and
    /// would not name its external address: an external port with no address
    /// is not something a peer can dial, so nothing is published rather than
    /// half of one.
    pub fn external_socket(&self) -> Option<SocketAddr> {
        let held = self.held?;
        match (&self.upnp_client, self.granted_by()?) {
            (Some(client), MappedOver::Upnp) => match client.get_external_address() {
                Ok(external) => Some(SocketAddr::new(IpAddr::V4(external), held.external_port)),
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "peer internet: the UPnP gateway mapped a port but would not name its \
                         external address, so no mapped endpoint is advertised"
                    );
                    None
                }
            },
            _ => external_socket_for(&self.client, &held),
        }
    }

    /// Re-ask for the mapping this keeper holds, extending its lifetime.
    ///
    /// Refuses rather than silently mapping when there is nothing held: a
    /// renewal that quietly became a first map would hide a keeper whose
    /// initial request never ran.
    ///
    /// Routed by [`MappedOver`]: a UPnP mapping is renewed by re-issuing
    /// `AddPortMapping` with the same arguments, which is how IGD extends a
    /// lease, and asking NAT-PMP to renew it instead would be a request to a
    /// gateway that never answered in the first place.
    pub fn renew(&mut self) -> Result<Mapping, ReachError> {
        let Some(held) = self.held else {
            return Err(ReachError::Malformed(
                "peer internet: a renewal was asked for before anything was mapped".into(),
            ));
        };
        if let Some(client) = self.upnp_client.clone() {
            return self.renew_over_upnp(&client, held);
        }
        let mapping = self.client.renew(&held, self.lifetime_secs)?;
        self.held = Some(mapping);
        self.steps.push(MappingStep::Renewed);
        Ok(mapping)
    }

    /// Take the mapping away, and record that it went.
    ///
    /// A keeper holding nothing deletes nothing and says so with `Ok(None)`:
    /// at shutdown that is the normal shape for a node whose router refused
    /// the first request.
    ///
    /// Routed by [`MappedOver`], for the reason that enum exists: a UPnP
    /// mapping deleted over NAT-PMP is a forward left on the router, pointing
    /// at a listener this process has just stopped running.
    pub fn delete(&mut self) -> Result<Option<Mapping>, ReachError> {
        let Some(held) = self.held else {
            return Ok(None);
        };
        if let Some(client) = &self.upnp_client {
            client
                .delete_port_mapping(MapProtocol::Tcp, held.external_port)
                .map_err(|err| ReachError::Malformed(err.to_string()))?;
            self.held = None;
            self.upnp_client = None;
            self.steps.push(MappingStep::Deleted);
            return Ok(Some(held));
        }
        let confirmed = self.client.delete(MapProtocol::Tcp, self.internal_port)?;
        self.held = None;
        self.steps.push(MappingStep::Deleted);
        Ok(Some(confirmed))
    }

    /// Re-issue the UPnP `AddPortMapping` that granted the held mapping.
    ///
    /// Split out so [`Self::renew`] reads as the routing decision it is. The
    /// arguments are the ones the first grant used, which is what makes this a
    /// lease extension rather than a second mapping: IGD keys a mapping on
    /// (remote host, external port, protocol), and all three are unchanged.
    fn renew_over_upnp(
        &mut self,
        client: &crate::peer::reach_upnp::UpnpClient,
        held: Mapping,
    ) -> Result<Mapping, ReachError> {
        let internal = self.client.internal_address().map_err(|err| {
            ReachError::Malformed(format!(
                "peer internet: a UPnP renewal needs this Mac's address on the LAN and it \
                 could not be read: {err}"
            ))
        })?;
        client
            .add_port_mapping(
                MapProtocol::Tcp,
                held.external_port,
                held.internal_port,
                internal,
                UPNP_MAPPING_DESCRIPTION,
                self.lifetime_secs,
            )
            .map_err(|err| ReachError::Malformed(err.to_string()))?;
        let renewed = Mapping {
            lifetime_secs: self.lifetime_secs,
            ..held
        };
        self.held = Some(renewed);
        self.steps.push(MappingStep::Renewed);
        Ok(renewed)
    }

    /// The mapping this keeper holds, as the gateway granted it.
    pub fn held(&self) -> Option<Mapping> {
        self.held
    }

    /// What this keeper did, in order.
    pub fn steps(&self) -> &[MappingStep] {
        &self.steps
    }
}

/// Hold a mapping open until `stop` is set, then take it away.
///
/// Blocking on purpose: the caller runs this on its own thread
/// ([`spawn_mapping_keeper`]), and every socket under it is a blocking
/// [`UdpSocket`] with a read timeout.
///
/// Returns the steps it took. A gateway that refuses the FIRST request is an
/// error the caller reports, because nothing was ever mapped and there is
/// nothing to keep; a refusal on a later renewal is logged and the loop stays
/// up, since the mapping already granted outlives several missed renewals and
/// giving up would throw away an address peers are already using.
pub fn run_mapping(
    client: NatPmp,
    internal_port: u16,
    lifetime_secs: u32,
    renew_every: Duration,
    stop: &AtomicBool,
) -> Result<Vec<MappingStep>, ReachError> {
    run_mapping_with_upnp(
        client,
        None,
        internal_port,
        lifetime_secs,
        renew_every,
        stop,
    )
}

/// [`run_mapping`], with a UPnP IGD gateway to fall back on when NAT-PMP says
/// nothing.
///
/// Separate entry point rather than a sixth parameter on `run_mapping`, so the
/// NAT-PMP-only tests that drive the keeper against a fake gateway keep their
/// call unchanged and cannot accidentally acquire a multicast search.
pub fn run_mapping_with_upnp(
    client: NatPmp,
    upnp: Option<crate::peer::reach_upnp::Discoverer>,
    internal_port: u16,
    lifetime_secs: u32,
    renew_every: Duration,
    stop: &AtomicBool,
) -> Result<Vec<MappingStep>, ReachError> {
    run_mapping_while(
        client,
        upnp,
        internal_port,
        lifetime_secs,
        renew_every,
        stop,
        &|| true,
    )
}

/// [`run_mapping_with_upnp`], with a second way to end the loop: a predicate
/// the keeper asks between renewals.
///
/// # Why a keeper has to be able to end itself
///
/// `tcr peer internet off` runs in the CLI process and writes the flag; the
/// keeper lives in the SERVING process and used to read `peer.internet`
/// exactly once, at boot. So the CLI deleted the mapping over NAT-PMP and the
/// keeper's next renewal created it again, up to half an hour later: the
/// operator turned the switch off, saw `tcr peer reach` confirm it, and this
/// Mac went on being reachable from the internet until it was restarted. A
/// UPnP-granted mapping was worse, since the CLI's NAT-PMP delete never took
/// that one away at all.
///
/// Ending the loop here is also what deletes over the RIGHT protocol:
/// [`MappingKeeper::delete`] routes by [`MappedOver`], and it runs on every
/// exit path below.
///
/// The predicate is asked on the stop-poll tick, not once per renewal, so the
/// switch takes effect in about a second rather than in up to
/// [`MAPPING_RENEW_INTERVAL`].
pub fn run_mapping_while(
    client: NatPmp,
    upnp: Option<crate::peer::reach_upnp::Discoverer>,
    internal_port: u16,
    lifetime_secs: u32,
    renew_every: Duration,
    stop: &AtomicBool,
    keep_going: &dyn Fn() -> bool,
) -> Result<Vec<MappingStep>, ReachError> {
    let gateway = client.gateway();
    let mut keeper = MappingKeeper::new(client, internal_port, lifetime_secs);
    if let Some(discoverer) = upnp {
        keeper = keeper.with_upnp(discoverer);
    }
    let mapping = keeper.map()?;
    // Published through the keeper, which asks the gateway that granted the
    // mapping. The UPnP path has already published the address it was given
    // inside `map_over_upnp`, so re-publishing here would ask the router for
    // its external address a second time in the same second; worse, the line
    // this replaces asked NAT-PMP, which had just been silent, and wrote the
    // `None` that answer produces straight over the real address.
    if keeper.granted_by() != Some(MappedOver::Upnp) {
        publish_external(keeper.external_socket(), Some(&mapping));
    }
    // A router that was refusing and now maps is the one mapping event worth a
    // warning: the operator changed something, or this Mac is somewhere else,
    // and either way what they were told last is no longer true.
    if mapping_answer_voice(gateway, MappingAnswer::Mapped) == AnswerVoice::News {
        tracing::warn!(
            external_port = mapping.external_port,
            internal_port = mapping.internal_port,
            lifetime_secs = mapping.lifetime_secs,
            "peer internet: the router mapped this node's listener port"
        );
    } else {
        tracing::info!(
            external_port = mapping.external_port,
            internal_port = mapping.internal_port,
            lifetime_secs = mapping.lifetime_secs,
            "peer internet: the router mapped this node's listener port"
        );
    }

    while !stop.load(Ordering::SeqCst) {
        if !keep_going() {
            tracing::info!(
                "peer internet: the switch is off, so the mapping this node holds is deleted \
                 and the keeper stops"
            );
            break;
        }
        if !sleep_until_while(renew_every, stop, keep_going) {
            break;
        }
        match keeper.renew() {
            Ok(renewed) => {
                publish_external(keeper.external_socket(), Some(&renewed));
            }
            Err(err) => tracing::warn!(
                error = %err,
                "peer internet: a mapping renewal failed; the mapping already granted stands \
                 until its lifetime runs out and the next renewal is tried anyway"
            ),
        }
    }

    // The delete runs on every exit path, including the one where a renewal
    // has been failing: leaving a mapping behind is a port on the router that
    // forwards to a listener this node has just stopped running.
    publish_external(None, None);
    if let Err(err) = keeper.delete() {
        tracing::warn!(
            error = %err,
            "peer internet: the router did not confirm the mapping was deleted; it expires \
             on its own within {lifetime_secs}s"
        );
    }
    Ok(keeper.steps().to_vec())
}

/// Sleep `wait` in [`MAPPING_STOP_POLL`] ticks, answering whether the full
/// wait elapsed (`true`) or it was cut short (`false`), by `stop` being set or
/// by `keep_going` answering no.
fn sleep_until_while(wait: Duration, stop: &AtomicBool, keep_going: &dyn Fn() -> bool) -> bool {
    let deadline = std::time::Instant::now() + wait;
    let mut ticks: u32 = 0;
    loop {
        if stop.load(Ordering::SeqCst) {
            return false;
        }
        // Every [`FLAG_POLL_TICKS`] ticks rather than every tick: the
        // predicate reads a file, and a quarter-second file read for half an
        // hour is a cost with no reader.
        if ticks.is_multiple_of(FLAG_POLL_TICKS) && !keep_going() {
            return false;
        }
        ticks = ticks.saturating_add(1);
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return true;
        }
        std::thread::sleep(left.min(MAPPING_STOP_POLL));
    }
}

/// How many [`MAPPING_STOP_POLL`] ticks pass between two reads of
/// `peer.internet`. Eight ticks is two seconds, which is how long
/// `tcr peer internet off` takes to be believed by a running server.
const FLAG_POLL_TICKS: u32 = 8;

/// The external socket to advertise for one mapping: the gateway's own
/// external address, with the port the gateway granted.
///
/// [`None`] when the gateway will not say what its external address is, which
/// is a normal answer from a router that mapped the port anyway: an external
/// port with no address is not something a peer can dial, so nothing is
/// published rather than half of one.
fn external_socket_for(client: &NatPmp, mapping: &Mapping) -> Option<SocketAddr> {
    match client.external_address() {
        Ok(external) => Some(SocketAddr::new(
            IpAddr::V4(external.addr),
            mapping.external_port,
        )),
        Err(err) => {
            tracing::warn!(
                error = %err,
                "peer internet: the router mapped a port but would not name its external \
                 address, so no mapped endpoint is advertised"
            );
            None
        }
    }
}

/// A running keeper. Dropping it stops the thread, which deletes the mapping.
///
/// The guard is what makes "deletes the mapping at shutdown" true on the path
/// that actually happens: the listener's future is DROPPED at shutdown rather
/// than run to completion (`src/peer/listener.rs` says so of its ledger
/// flusher), so a delete written after the accept loop would never run.
#[derive(Debug)]
pub struct MappingGuard {
    stop: Arc<AtomicBool>,
    /// Cleared by the keeper thread on its way out, so a holder can tell a
    /// keeper it stopped itself from one it is still holding.
    ///
    /// The keeper has a second way to end: `run_mapping_while`'s own re-read
    /// of `peer.internet`, which is what `tcr peer internet off` reaches it
    /// through. A holder that assumed a guard meant a running keeper would
    /// answer [`KeeperStep::Renew`] for a thread that had already gone, and
    /// the switch turned back on would start nothing.
    alive: Arc<AtomicBool>,
}

impl MappingGuard {
    /// Stop the keeper without waiting for it, for a caller that wants the
    /// delete to start before it drops the rest of its state.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// A handle on a running mapping keeper, which stops its keeper when it is
/// dropped.
///
/// A trait rather than [`MappingGuard`] itself so [`keep_internet_mapping`]
/// can be driven with a fake in a test: the production starter asks this
/// machine's real router for a mapping, which is the one thing the suite here
/// may never do.
pub trait KeeperHandle {
    /// Is the keeper behind this handle still running?
    fn is_running(&self) -> bool;
}

impl KeeperHandle for MappingGuard {
    fn is_running(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
}

impl Drop for MappingGuard {
    fn drop(&mut self) {
        self.stop();
    }
}

/// What a serving process decided about asking its router for a mapping.
///
/// A typed answer, built once and read by the boot line, the log and the gate,
/// rather than a `bool` each caller re-derives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MappingBoot {
    /// `peer.internet` is off: nothing is asked for.
    SwitchOff,
    /// The listener is bound to loopback, so a router mapping would forward a
    /// public port at a socket no packet from outside this Mac can reach.
    LoopbackOnly,
    /// A mapping is wanted for this port.
    Wanted {
        /// The port the listener is really bound on, which is the one mapped:
        /// a listener configured on port 0 maps what the kernel gave it.
        internal_port: u16,
    },
}

/// How many times each [`MappingBoot`] has been decided in this process.
///
/// Counters rather than "the last one", because several listeners can boot in
/// one process (every test in a suite binary does) and a gate has to be able
/// to say "a serving process asked for this one" without racing a sibling.
///
/// The decision is what a boot gate can read without a router in the room: the
/// alternative is a test that lets a keeper talk to whatever gateway the
/// machine running the suite happens to sit behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MappingBootCounts {
    /// `peer.internet` was off.
    pub switch_off: u64,
    /// The listener was bound to loopback.
    pub loopback_only: u64,
    /// A keeper was started.
    pub wanted: u64,
}

static BOOTS_SWITCH_OFF: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static BOOTS_LOOPBACK_ONLY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static BOOTS_WANTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Every mapping decision this process has taken, counted by kind.
pub fn mapping_boot_counts() -> MappingBootCounts {
    use std::sync::atomic::Ordering as AtomicOrdering;
    MappingBootCounts {
        switch_off: BOOTS_SWITCH_OFF.load(AtomicOrdering::SeqCst),
        loopback_only: BOOTS_LOOPBACK_ONLY.load(AtomicOrdering::SeqCst),
        wanted: BOOTS_WANTED.load(AtomicOrdering::SeqCst),
    }
}

/// Count one decision.
fn count_mapping_boot(decision: MappingBoot) {
    use std::sync::atomic::Ordering as AtomicOrdering;
    match decision {
        MappingBoot::SwitchOff => &BOOTS_SWITCH_OFF,
        MappingBoot::LoopbackOnly => &BOOTS_LOOPBACK_ONLY,
        MappingBoot::Wanted { .. } => &BOOTS_WANTED,
    }
    .fetch_add(1, AtomicOrdering::SeqCst);
}

/// Whether a serving process bound at `local` should ask its router for a
/// mapping, given `peer.internet`.
///
/// Pure, so both arms are testable without a router.
pub fn mapping_boot(internet: bool, local: SocketAddr) -> MappingBoot {
    if !internet {
        return MappingBoot::SwitchOff;
    }
    if local.ip().is_loopback() {
        return MappingBoot::LoopbackOnly;
    }
    MappingBoot::Wanted {
        internal_port: local.port(),
    }
}

/// Ask this machine's router to map the peer listener's port, when
/// `peer.internet` says so, and hold it open until the returned guard drops.
///
/// **The one function a serving process calls for the internet half**, and the
/// reason it exists: `listener::serve` did all of this inline and
/// `server::boot_peer_listener` boots through `bind` + `serve_on_with`
/// instead, so the shipped proxy read `peer.internet` nowhere, mapped no port,
/// and `external_socket()` was `None` on every Mac. What that cost was not
/// only a node nobody outside the LAN could dial: it is also the input to
/// `tunnel::reverse_carry_is_wanted`, so every Mac decided it could not be
/// dialled and parked a carrier at every friend.
pub fn start_peer_mapping(
    peers_path: &Path,
    internet: bool,
    local: SocketAddr,
) -> Option<MappingGuard> {
    let decision = mapping_boot(internet, local);
    count_mapping_boot(decision);
    match decision {
        MappingBoot::SwitchOff => {
            tracing::debug!(
                "peer internet: off, so this node asks its router for nothing (`tcr peer \
                 internet on` turns it on)"
            );
            None
        }
        MappingBoot::LoopbackOnly => {
            tracing::info!(
                peer_listen = %local,
                "peer internet: on, but this listener is bound to loopback, so a router \
                 mapping would point at a socket nothing off this Mac can reach; none is \
                 asked for"
            );
            None
        }
        MappingBoot::Wanted { internal_port } => {
            // Where the keeper records what it holds, set before it starts:
            // the state file beside THIS peers file. A CLI cannot read a
            // thread's memory, so this record is the only way `tcr peer reach`
            // reports the live mapping without asking the router and changing
            // the table it is reporting on.
            record_mappings_at(crate::peer::serve::peer_state_path(peers_path));
            spawn_mapping_keeper(peers_path, internal_port)
        }
    }
}

/// Start a keeper for `internal_port` on this machine's default gateway.
///
/// Returns [`None`] when there is no gateway to ask, which is a normal
/// outcome and not a failure of this node: the listener goes on serving the
/// LAN exactly as before, and the operator sees the reason in the log.
pub fn spawn_mapping_keeper(peers_path: &Path, internal_port: u16) -> Option<MappingGuard> {
    let client = match NatPmp::on_default_gateway() {
        Ok(client) => client,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "peer internet: on, but this Mac has no NAT-PMP gateway to ask for a mapping; \
                 a peer off this LAN can still reach it over IPv6 if both ends have one"
            );
            return None;
        }
    };
    let gateway = client.gateway();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let alive = Arc::new(AtomicBool::new(true));
    let thread_alive = Arc::clone(&alive);
    let peers_path = peers_path.to_path_buf();
    std::thread::Builder::new()
        .name("tcr-peer-mapping".to_string())
        .spawn(move || {
            // Cleared however this thread leaves, including the panic path:
            // a guard that still said "running" after its keeper died would
            // make the switch unstartable for the life of the process.
            let _running = RunningUntilDropped(thread_alive);
            if let Err(err) = run_mapping_while(
                client,
                Some(upnp_discoverer()),
                internal_port,
                MAPPING_LIFETIME_SECS,
                MAPPING_RENEW_INTERVAL,
                &thread_stop,
                &|| internet_is_on(&peers_path),
            ) {
                // The same answer from the same router is a line nobody reads
                // twice, and this one used to be written every five seconds
                // for as long as the process ran.
                if mapping_answer_voice(gateway, MappingAnswer::Refused) == AnswerVoice::Repeat {
                    tracing::debug!(
                        error = %err,
                        "peer internet: the router would not map this node's listener port"
                    );
                } else {
                    tracing::warn!(
                        error = %err,
                        "peer internet: the router would not map this node's listener port"
                    );
                }
            }
        })
        .map_err(|err| {
            tracing::warn!(
                error = %err,
                "peer internet: could not start the mapping keeper thread"
            );
            err
        })
        .ok()?;
    Some(MappingGuard { stop, alive })
}

/// Clears an "is it running" flag however the thread holding it leaves.
struct RunningUntilDropped(Arc<AtomicBool>);

impl Drop for RunningUntilDropped {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// How often a serving process re-reads `peer.internet`.
///
/// The same shape the beacon reads `peer.find` with, and a shorter interval
/// because this one waits on a router round trip afterwards rather than on a
/// multicast announce. `tcr peer internet off` is not waiting on this: the CLI
/// deletes the mapping itself and the keeper's own poll notices in about a
/// second. What waits on this is `on`, which nothing else can act on.
pub const INTERNET_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// What a serving process should do with its mapping keeper this wake.
///
/// A typed answer over the two facts, the switch and whether a keeper is
/// running, so both can be decided without a router, a thread or a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeeperStep {
    /// A mapping is wanted and nothing is holding one: start a keeper.
    Start,
    /// A mapping is wanted and a keeper holds one: it renews on its own.
    Renew,
    /// A mapping is not wanted and a keeper holds one: stop it, which deletes
    /// the mapping the router granted.
    Stop,
    /// Nothing is wanted and nothing is running.
    Idle,
}

/// Decide [`KeeperStep`] from the boot decision as the file states it NOW and
/// whether a keeper is running.
///
/// # Why a live server has to keep asking
///
/// `tcr peer internet off` reached a running server (the keeper re-reads the
/// switch between renewals, and the CLI deletes the mapping itself), and
/// `internet on` reached nothing at all: `set_internet` writes the flag and
/// `start_peer_mapping` had only boot callers, so the mapping came back at the
/// next restart and not before. `docs/cli.md` said a running server honours
/// both without one.
pub fn keeper_step(decision: MappingBoot, running: bool) -> KeeperStep {
    match (decision, running) {
        (MappingBoot::Wanted { .. }, false) => KeeperStep::Start,
        (MappingBoot::Wanted { .. }, true) => KeeperStep::Renew,
        // A listener bound to loopback is the same answer as the switch being
        // off: a mapping would forward a public port at a socket nothing off
        // this Mac can reach.
        (MappingBoot::SwitchOff | MappingBoot::LoopbackOnly, true) => KeeperStep::Stop,
        (MappingBoot::SwitchOff | MappingBoot::LoopbackOnly, false) => KeeperStep::Idle,
    }
}

/// What one run of [`keep_internet_mapping`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InternetWatch {
    /// Keepers started, not counting one the caller handed in.
    pub started: usize,
    /// Keepers stopped because the switch went off.
    pub stopped: usize,
    /// Wakes where a keeper was wanted, none was running, and the router that
    /// refused the last one was left alone: the ladder's whole effect, counted
    /// so a test can see it without a clock.
    pub deferred: usize,
}

/// Keep this process's mapping keeper in step with `peer.internet`, for as
/// long as the process serves.
///
/// `held` is the keeper the caller already started at boot, or [`None`]. The
/// loop takes ownership of it: dropping the handle is what stops a keeper, so
/// the guard has to live here rather than beside the accept loop.
///
/// `start` is a parameter for the reason [`KeeperHandle`] is a trait: the
/// production one asks this machine's real router, and a test may not.
///
/// `rounds` bounds the loop so a test can drive it; [`None`] is the production
/// shape, which is forever.
///
/// # A router that says no is asked less often
///
/// A keeper whose router refuses the first request ends within a couple of
/// seconds, so "a mapping is wanted and none is running" was true at every
/// wake and this loop asked the same router every five seconds for the life of
/// the process: a day of that is one pair of log lines every five seconds and
/// nothing gained. [`MappingRetry`] spaces the asks out instead, along
/// [`MAPPING_RETRY_LADDER`], and puts them back at five seconds the moment the
/// answer could have changed: the switch toggled, this Mac moved to another
/// router, or a request was granted.
pub async fn keep_internet_mapping<H, S>(
    peers_path: std::path::PathBuf,
    local: SocketAddr,
    every: Duration,
    rounds: Option<usize>,
    mut start: S,
    mut held: Option<H>,
) -> InternetWatch
where
    H: KeeperHandle,
    S: FnMut() -> Option<H>,
{
    let mut tally = InternetWatch::default();
    // The ladder that decides when a router which would not map is asked
    // again. A keeper handed in is one already asked for, so its death is this
    // router's answer and not this loop's first question.
    let mut retry = MappingRetry::new();
    let mut expecting = held.is_some();
    let mut ticker = tokio::time::interval(every);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The first tick is immediate and the boot decision has already been
    // taken by the caller, so it is spent here rather than re-deciding it.
    ticker.tick().await;
    let mut round = 0_usize;
    loop {
        if let Some(limit) = rounds {
            if round >= limit {
                return tally;
            }
        }
        round += 1;
        ticker.tick().await;
        retry.tick(every);

        // A peers file that does not read for a moment is not the same fact as
        // the switch being off, the rule `internet_is_on` states: the mapping
        // this node holds is kept and the switch is re-read at the next wake.
        let internet = match crate::peer::config::read_or_default(&peers_path) {
            Ok(file) => file.internet,
            Err(err) => {
                tracing::warn!(
                    path = %peers_path.display(),
                    error = %err,
                    "peer internet: the peers file did not read, so the mapping this node \
                     holds is left as it is until the next wake"
                );
                continue;
            }
        };
        let decision = mapping_boot(internet, local);
        match keeper_step(
            decision,
            held.as_ref().is_some_and(KeeperHandle::is_running),
        ) {
            KeeperStep::Idle => {}
            // A keeper that is still running is a router that mapped, which
            // puts the ladder back on its first rung: the next refusal, weeks
            // from now, is not made to wait ten minutes for one that has been
            // answering ever since.
            KeeperStep::Renew => retry.answered(MappingAnswer::Mapped),
            KeeperStep::Start => {
                if expecting {
                    // A keeper this loop is responsible for is gone. It only
                    // ends on its own when the first request was refused, so
                    // this is the router's answer, and the line naming it is
                    // written by the keeper thread itself.
                    retry.answered(MappingAnswer::Refused);
                    expecting = false;
                }
                // A Mac carried to another network is behind another router,
                // and this one may map on the first ask.
                if retry.gateway_read_due() && retry.gateway_is_now(default_gateway().ok()) {
                    tracing::info!(
                        "peer internet: this Mac is behind a different router than the one that \
                         would not map, so it is asked straight away"
                    );
                }
                if !retry.due() {
                    tally.deferred += 1;
                    tracing::debug!(
                        wait_secs = retry.interval().as_secs(),
                        refusals = retry.refusals(),
                        "peer internet: this router would not map, so the next request waits"
                    );
                    continue;
                }
                held = start();
                retry.asked();
                expecting = held.is_some();
                match &held {
                    Some(_) => {
                        tally.started += 1;
                        tracing::info!(
                            peer_listen = %local,
                            "peer internet: the switch is on, so this node is asking its router \
                             for a mapping (`tcr peer reach` reports what it gets)"
                        );
                    }
                    // No keeper at all: there is no gateway to ask, and the
                    // line saying so is written where that is decided. It
                    // counts as a refusal here, or this loop re-asks a Mac
                    // with no router every five seconds for as long as it
                    // runs.
                    None => retry.answered(MappingAnswer::Refused),
                }
            }
            KeeperStep::Stop => {
                // Dropping the handle stops the keeper, which deletes the
                // mapping over the protocol that granted it.
                held = None;
                tally.stopped += 1;
                // The switch being toggled is an operator act, and the next
                // `on` asks the router at once rather than waiting out a
                // ladder built before they touched anything.
                expecting = false;
                retry.reset();
                tracing::info!(
                    "peer internet: the switch is off, so the mapping this node holds is \
                     deleted and no keeper runs"
                );
            }
        }
    }
}

/// Is `peer.internet` still on in the peers file at `path`?
///
/// The keeper's own re-read of the switch, asked between renewals. A file that
/// does not read is answered `true`, with a line: a mapping is a thing an
/// operator asked for, and tearing it down because a read failed for a moment
/// would take the node off the internet on a transient error. The switch being
/// OFF is a fact the file has to state.
pub fn internet_is_on(path: &Path) -> bool {
    match crate::peer::config::read_or_default(path) {
        Ok(file) => file.internet,
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "peer internet: the peers file did not read, so the mapping this node holds \
                 is kept; the switch is re-read at the next tick"
            );
            true
        }
    }
}

/// What `tcr peer internet on|off` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InternetSwitch {
    /// Turned on. The mapping itself is asked for by the listener at boot, so
    /// this reports the port that will be mapped, or [`None`] when no listener
    /// is configured yet.
    On {
        /// The listener's configured port, which is what gets mapped.
        listen_port: Option<u16>,
    },
    /// Turned off, and the mapping this node held was deleted.
    Off {
        /// What the router confirmed, or [`None`] when there was no listener
        /// port to unmap or no gateway to ask.
        deleted: Option<Mapping>,
    },
}

/// Set `peer.internet` and, when turning it OFF, take the router mapping away.
///
/// The delete runs here rather than in the serving process because the two are
/// different processes: `tcr peer internet off` is a CLI run with no way to
/// reach the keeper thread inside a running `tcr` server. A NAT-PMP delete is
/// addressed by internal port, not by who asked, so the CLI can unmap what the
/// server mapped. The server's own keeper notices at its next renewal.
pub fn set_internet(peers_path: &Path, on: bool) -> Result<InternetSwitch> {
    set_internet_via(peers_path, on, None)
}

/// [`set_internet`] with the gateway handed in, so a test can point it at a
/// fake responder on loopback instead of this machine's real router.
pub fn set_internet_via(
    peers_path: &Path,
    on: bool,
    gateway: Option<NatPmp>,
) -> Result<InternetSwitch> {
    use crate::peer::config;

    let _lock = config::FileLock::acquire(peers_path)?;
    let mut file = config::read_or_default(peers_path)?;
    file.internet = on;
    config::save(peers_path, &file)
        .with_context(|| format!("peer internet: could not write {}", peers_path.display()))?;

    let listen_port = file.listen.map(|addr| addr.port());
    if on {
        return Ok(InternetSwitch::On { listen_port });
    }

    publish_external(None, None);
    let Some(port) = listen_port else {
        return Ok(InternetSwitch::Off { deleted: None });
    };
    let client = match gateway {
        Some(client) => client,
        None => match NatPmp::on_default_gateway() {
            Ok(client) => client,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "peer internet: off, and there is no NAT-PMP gateway to unmap at; any \
                     mapping this node holds expires on its own"
                );
                return Ok(InternetSwitch::Off { deleted: None });
            }
        },
    };
    match client.delete(MapProtocol::Tcp, port) {
        Ok(confirmed) => Ok(InternetSwitch::Off {
            deleted: Some(confirmed),
        }),
        Err(err) => {
            tracing::warn!(
                error = %err,
                "peer internet: off, and the router did not confirm the mapping was deleted; \
                 it expires on its own"
            );
            Ok(InternetSwitch::Off { deleted: None })
        }
    }
}

// ---------------------------------------------------------------------------
// A port both ends compute, and nobody else can guess
// ---------------------------------------------------------------------------

/// How long one port slot lasts, in seconds.
///
/// Thirty seconds, with the two adjacent slots also accepted, so the window a
/// dial may land in is 90 seconds wide. That is the number the pair of clocks
/// has to agree inside, and two Macs syncing to the same time servers are
/// nowhere near a second apart, the width is for the dial's own flight time
/// and for a slot boundary crossed between the derive and the connect, not for
/// clock skew.
pub const SLOT_SECONDS: u64 = 30;

/// The lowest port a derivation may return.
///
/// Above the registered range and above every port this program binds on
/// purpose, so a derived port never collides with a service somebody chose.
pub const PORT_FLOOR: u16 = 20_000;

/// One past the highest port a derivation may return.
///
/// Below 61000, where macOS starts handing out ephemeral source ports
/// (`sysctl net.inet.ip.portrange.first`), so a derived listener is not
/// competing with the kernel's own allocator for its own number.
pub const PORT_CEILING: u16 = 60_000;

/// How many ports the derivation chooses between.
const PORT_SPAN: u64 = (PORT_CEILING - PORT_FLOOR) as u64;

// The reduction below adds a value under PORT_SPAN to PORT_FLOOR and casts the
// sum to u16. That is exact only while the sum fits, so the condition is
// checked at COMPILE time rather than trusted: a widened span is otherwise a
// debug panic in one build and a silently wrapped port number -- a port outside
// the window, which nothing is listening on -- in the release build that ships.
// Found by mutation: raising the span to 65535 panicked with "attempt to add
// with overflow" in three tests and would have wrapped in release.
const _: () = assert!(PORT_FLOOR as u64 + PORT_SPAN <= u16::MAX as u64);

/// The label that separates this derivation from every other use of the same
/// handshake hash.
///
/// HKDF's `info` field, and the reason it is not empty: the six-digit pairing
/// code (`noise::six_digit_code`) is also derived from the handshake hash, and
/// two derivations from one secret with no domain separator are one derivation
/// whose outputs are related. The version suffix is here so a change to the
/// scheme changes every port rather than half of them.
const PORT_INFO: &[u8] = b"tcr peer reach derived-port v1";

/// Which slot a unix timestamp falls in.
pub fn current_slot(unix_seconds: u64) -> u64 {
    unix_seconds / SLOT_SECONDS
}

/// The per-pair secret the port derivation runs on, from the pair's completed
/// Noise handshake hash.
///
/// HKDF-SHA256 (RFC 5869) over the tree's own `hmac_sha256`
/// (`src/peer/config.rs`), which is checked against RFC 4231 rather than
/// against itself. Extract with an all-zero salt, as the RFC says to when
/// there is none, then one expand round, because 32 bytes is one block and the
/// counter never reaches two.
///
/// # Why a derivation and not the hash itself
///
/// The handshake hash is the input to the six-digit compare an operator reads
/// off two screens. A port computed as a slice of that same hash would leak a
/// little of a value the operators speak out loud; more importantly it would
/// tie two unrelated schemes together, so changing either one changes both.
/// This is the Noise handshake's own key material, re-derived: it is **not** a
/// second key exchange, and no new one may be invented here.
///
/// Takes the hash as bytes rather than a handshake object on purpose: the
/// handshake type belongs to `snow`, which by this module tree's own rule is
/// named in `src/peer/noise.rs` and nowhere else.
pub fn port_secret(handshake_hash: &[u8]) -> [u8; 32] {
    use crate::peer::config::hmac_sha256;

    // RFC 5869 § 2.2: with no salt, the salt is HashLen zero bytes.
    let pseudorandom_key = hmac_sha256(&[0_u8; 32], handshake_hash);

    // RFC 5869 § 2.3, one round: T(1) = HMAC(PRK, info | 0x01).
    let mut block = Vec::with_capacity(PORT_INFO.len() + 1);
    block.extend_from_slice(PORT_INFO);
    block.push(0x01);
    hmac_sha256(&pseudorandom_key, &block)
}

/// The port this pair listens on, and dials, during one slot.
///
/// Both ends run this over the same secret and the same slot number and get
/// the same answer, so neither has to tell the other a port: the port IS the
/// shared secret plus the clock. A scanner without the secret sees one port in
/// forty thousand move every thirty seconds.
///
/// # The modulo, stated rather than hidden
///
/// Eight bytes of MAC reduced into a 40000-wide range is biased, by the
/// remainder of 2^64 over 40000, about one part in 4.6e14. That is not a
/// number any attacker can stand on, and rejection sampling here would make
/// the two ends disagree the moment their loop counts differed, so the bias is
/// taken deliberately.
pub fn derived_port(secret: &[u8; 32], slot: u64) -> u16 {
    use crate::peer::config::hmac_sha256;

    let mut message = [0_u8; 12];
    message[..4].copy_from_slice(b"slot");
    message[4..].copy_from_slice(&slot.to_be_bytes());
    let mac = hmac_sha256(secret, &message);

    let mut head = [0_u8; 8];
    head.copy_from_slice(&mac[..8]);
    let choice = u64::from_be_bytes(head) % PORT_SPAN;

    // The cast is exact: `choice` is below PORT_SPAN, so the sum is below
    // PORT_CEILING, which is a u16.
    PORT_FLOOR + choice as u16
}

/// Every port a dial in slot `slot` may legitimately arrive on: the slot
/// before, the slot itself, and the slot after.
///
/// Three rather than one because a dial crosses a slot boundary for free, the
/// deriving end may compute its port at 11:59:59.9 and connect at 12:00:00.1,
/// and because the listener has to be up on the port before the dialler picks
/// it. Ordered current, previous, next, which is the order a listener should
/// bind in and a test should check in.
pub fn accepted_ports(secret: &[u8; 32], slot: u64) -> [u16; 3] {
    [
        derived_port(secret, slot),
        derived_port(secret, slot.saturating_sub(1)),
        derived_port(secret, slot.saturating_add(1)),
    ]
}

/// The port secrets this process has learned, one per pair.
///
/// # What is in memory, what is on disk, and what is on neither
///
/// The secret is the pair's completed Noise handshake hash run through
/// [`port_secret`], so it exists only where a handshake has completed. This
/// register is the hot copy, filled by every session as it completes.
///
/// It is ALSO written to `tcr-peers.json`, as
/// [`crate::peer::config::PeerRow::rendezvous_secret`], and
/// [`restore_from_peers`] reads it back at boot. A decision of 2026-09-18
/// took that decision and drew the line where this doc used to refuse the file
/// outright: what the row holds is this derived value and never the handshake
/// hash it came from. One keyed hash separates them, under `PORT_INFO`'s own
/// domain string, and the hash is what the six-digit pairing compare is built
/// from, so it stays in memory only.
///
/// Two places it still is not. The wire, because both ends compute it and
/// telling each other a port is the thing a derived port replaces. And any row
/// this node does not pin, because a secret for a peer with no row has nothing
/// to be for.
///
/// Before row 17 a restarted node had no secret for any pair until the next
/// completed session, and a dialler had the recorded endpoints and nothing
/// else, which is exactly the case a peer that MOVED is not covered by.
fn port_secrets() -> &'static Mutex<HashMap<tcr_peer_wire::PeerId, [u8; 32]>> {
    static SECRETS: OnceLock<Mutex<HashMap<tcr_peer_wire::PeerId, [u8; 32]>>> = OnceLock::new();
    SECRETS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record the port secret a completed handshake with `peer` derived.
///
/// Takes the SECRET and not the handshake hash, so the one derivation lives in
/// [`port_secret`] and a caller cannot accidentally register the hash itself,
/// which is the value the six-digit compare is also built from.
pub fn remember_port_secret(peer: tcr_peer_wire::PeerId, secret: [u8; 32]) {
    let mut held = match port_secrets().lock() {
        Ok(held) => held,
        Err(poisoned) => poisoned.into_inner(),
    };
    held.insert(peer, secret);
}

/// Derive and record the port secret for a completed handshake with `peer`.
///
/// The entry point a session driver calls: it takes the handshake hash the
/// session ended with and runs the one derivation, so no caller outside this
/// module handles the hash as if it were the secret.
pub fn remember_pair_hash(peer: tcr_peer_wire::PeerId, handshake_hash: &[u8]) {
    remember_port_secret(peer, port_secret(handshake_hash));
}

/// Fill the register from the peers file, at boot.
///
/// Returns how many rows carried a secret, for the one log line the caller
/// prints: a count is what tells an operator whether a restart kept the fleet's
/// rendezvous reach or started from nothing, and "restored=0" on a file that
/// should have had rows is a real signal.
///
/// A row without the key is skipped and is not an error: it predates the key,
/// or nothing has completed a session with that peer yet.
pub fn restore_from_peers(file: &crate::peer::config::PeerFile) -> usize {
    let mut restored = 0;
    for row in &file.peers {
        if let Some(secret) = row.rendezvous_secret {
            remember_port_secret(row.node, secret);
            restored += 1;
        }
        // The reflexive half, on the same pass and counted separately below by
        // nothing: a row may carry either key, both, or neither, and a boot
        // that restored one and not the other would report a number that is
        // true of neither. What the count means is "rows that had something to
        // restore", which is what the boot line says.
        if let Some((seen, _)) = row.sees_us_at.as_ref() {
            match seen.parse::<SocketAddr>() {
                Ok(addr) => remember_observed_self(row.node, addr),
                Err(err) => tracing::debug!(
                    peer = %row.node.display(),
                    seen = %seen,
                    error = %err,
                    "peer reach: the address on this row is not a socket address; ignoring it"
                ),
            }
        }
    }
    restored
}

/// Drop the port secret this process holds for `peer`.
///
/// Called where `tcr peer forget` drops the row. Forgetting a Mac has to take
/// away every way of reaching it, and the rendezvous secret is a way of
/// reaching it that survives the row: the peers file no longer names the peer,
/// and a register nobody cleared would still hand a dialler that pair's three
/// ports for as long as the process lived. The row on disk is cleared by the
/// same act, because a forgotten peer has no row to hold it.
///
/// Answers whether anything was held, for the caller's log line: "nothing to
/// clear" and "cleared" are different facts and neither is a failure.
///
/// It clears the reflexive observations too ([`forget_observations`]), on the
/// same sentence: the address a forgotten Mac saw this one at is another way
/// the two could still meet, and a caller that had to remember a second call
/// is a caller that forgets it.
pub fn forget_port_secret(peer: &tcr_peer_wire::PeerId) -> bool {
    let mut held = match port_secrets().lock() {
        Ok(held) => held,
        Err(poisoned) => poisoned.into_inner(),
    };
    let had_secret = held.remove(peer).is_some();
    let had_observation = forget_observations(peer);
    had_secret || had_observation
}

/// The port secret this process holds for `peer`, or [`None`] when no session
/// with it has completed since boot and the peers file held none either.
pub fn port_secret_for(peer: &tcr_peer_wire::PeerId) -> Option<[u8; 32]> {
    let held = match port_secrets().lock() {
        Ok(held) => held,
        Err(poisoned) => poisoned.into_inner(),
    };
    held.get(peer).copied()
}

/// The ports a dialler should try for `peer` when the recorded one stopped
/// answering: the pair's three accepted ports for the current slot, or an
/// empty list when this process holds no secret for it.
///
/// Current, previous, next, which is [`accepted_ports`]'s own order: the two
/// neighbours are what make one slot of clock skew, or a slot boundary crossed
/// between the derive and the connect, still meet.
pub fn rendezvous_ports(peer: &tcr_peer_wire::PeerId, unix_seconds: u64) -> Vec<u16> {
    match port_secret_for(peer) {
        Some(secret) => accepted_ports(&secret, current_slot(unix_seconds)).to_vec(),
        None => Vec::new(),
    }
}

/// Whether `port` is one this pair could be reaching each other on in `slot`.
pub fn port_is_accepted(secret: &[u8; 32], slot: u64, port: u16) -> bool {
    accepted_ports(secret, slot).contains(&port)
}

// ---------------------------------------------------------------------------
// The address the far side of a NAT sees this Mac at
// ---------------------------------------------------------------------------

/// Where each pinned peer last saw THIS Mac from, and where this Mac last saw
/// each peer from.
///
/// # Why a second register and not the peers file
///
/// An endpoint on a row is an address a peer ANSWERS on, written by
/// `observe_endpoints` and dialled by `dial_order`. What is here is a
/// different fact: the source address a connection ARRIVED from, which for a
/// Mac behind a NAT is the router's address and not anything that Mac listens
/// on. Writing it as an endpoint would put an address nothing accepts on into
/// the dial order, where it would cost a connect timeout on every dial.
///
/// It is the reflexive fact [`crate::peer::listener::NodeFacts::listening`]
/// says it does not carry, learned without a STUN server: the other end of an
/// authenticated session is the third party, and it already knows the answer.
///
/// In memory only, and per boot, exactly as the port-secret register was
/// before that gave it a file. Persisting it needs a field on
/// `PeerRow` (`src/peer/config.rs`), and it has none.
#[derive(Default)]
struct Observations {
    /// Keyed by peer: the address that peer reported seeing this Mac at.
    self_at: HashMap<tcr_peer_wire::PeerId, SocketAddr>,
    /// Keyed by peer: the source address a connection from that peer arrived
    /// from.
    peer_at: HashMap<tcr_peer_wire::PeerId, SocketAddr>,
}

fn observations() -> &'static Mutex<Observations> {
    static SEEN: OnceLock<Mutex<Observations>> = OnceLock::new();
    SEEN.get_or_init(|| Mutex::new(Observations::default()))
}

/// Take the register, recovering a poisoned lock rather than panicking: a
/// routing hint is never worth taking a process down for.
fn observations_locked() -> std::sync::MutexGuard<'static, Observations> {
    match observations().lock() {
        Ok(held) => held,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Record that `peer` told this Mac it sees us at `addr`.
///
/// The value is ROUTING ADVICE and the session that carried it had already
/// proved the pinned static key, so it is recorded as told. A peer that lies
/// costs this node one failed punch attempt against an address nobody is
/// behind, which is the same cost as a stale endpoint.
pub fn remember_observed_self(peer: tcr_peer_wire::PeerId, addr: SocketAddr) {
    observations_locked().self_at.insert(peer, addr);
}

/// The address `peer` last said it sees this Mac at.
pub fn observed_self_for(peer: &tcr_peer_wire::PeerId) -> Option<SocketAddr> {
    observations_locked().self_at.get(peer).copied()
}

/// Record the source address a connection from `peer` arrived from.
pub fn remember_observed_peer(peer: tcr_peer_wire::PeerId, addr: SocketAddr) {
    observations_locked().peer_at.insert(peer, addr);
}

/// The address this Mac last saw `peer` arrive from.
pub fn observed_peer_for(peer: &tcr_peer_wire::PeerId) -> Option<SocketAddr> {
    observations_locked().peer_at.get(peer).copied()
}

/// Every peer this Mac has been told an address by, for the verb that prints
/// them. Sorted by peer id so two runs print the same order.
pub fn observed_self_addresses() -> Vec<(tcr_peer_wire::PeerId, SocketAddr)> {
    let mut rows: Vec<_> = observations_locked()
        .self_at
        .iter()
        .map(|(peer, addr)| (*peer, *addr))
        .collect();
    rows.sort_by_key(|(peer, _)| peer.0);
    rows
}

/// Drop both observations for `peer`. Answers whether anything was held.
pub fn forget_observations(peer: &tcr_peer_wire::PeerId) -> bool {
    let mut held = observations_locked();
    let had_self = held.self_at.remove(peer).is_some();
    let had_peer = held.peer_at.remove(peer).is_some();
    had_self || had_peer
}

// ---------------------------------------------------------------------------
// The punch: two Macs dialling each other at a moment they both computed
// ---------------------------------------------------------------------------

/// How many slots a punch spends before it gives up.
///
/// Three, the number the dial order names, and the same three the
/// derivation already accepts on either side of a boundary. Each one costs a
/// slot's width of waiting, so a fourth buys a case the first three did not:
/// a NAT that refused the first two will refuse the fourth for the same
/// reason.
pub const PUNCH_SLOTS: u32 = 3;

/// One attempt of a punch: when to start, and on which port.
///
/// Both ends compute this from the same pair secret and the same clock and get
/// the same list, which is the whole reason a punch needs no coordination
/// message beyond "start at slot N".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PunchSlot {
    /// The slot number, which is what one side names to the other.
    pub slot: u64,
    /// The port both ends bind and dial in this slot ([`derived_port`]).
    pub port: u16,
    /// When this slot opens, in unix milliseconds. Absolute, because the two
    /// sides are agreeing on an instant and a duration from "now" is two
    /// different instants.
    pub opens_at_unix_ms: i64,
}

/// The next `slots` slots, each with the port this pair punches on.
///
/// # Why it never starts in the slot that is already open
///
/// The slot in progress is part spent: one side may compute the plan with 29
/// seconds left in it and the other with 200 milliseconds left, and the second
/// one would bind, dial once and move on before the first had started. The
/// next boundary is an instant both sides can name without knowing how far
/// through the current slot the other one is. It costs up to one slot of
/// waiting, which is the cost of the two of them being in the same slot at
/// all.
pub fn punch_plan(secret: &[u8; 32], now_unix_ms: i64, slots: u32) -> Vec<PunchSlot> {
    let now_seconds = u64::try_from(now_unix_ms.max(0)).unwrap_or(0) / 1_000;
    let first = current_slot(now_seconds).saturating_add(1);
    (0..u64::from(slots))
        .map(|step| {
            let slot = first.saturating_add(step);
            let opens_at_unix_ms = slot
                .saturating_mul(SLOT_SECONDS)
                .saturating_mul(1_000)
                .try_into()
                .unwrap_or(i64::MAX);
            PunchSlot {
                slot,
                port: derived_port(secret, slot),
                opens_at_unix_ms,
            }
        })
        .collect()
}

/// The plan the other side computes when it is told "punch at slot N".
///
/// Named separately from [`punch_plan`] because the receiving side must not
/// re-derive the starting slot from its own clock: the point of the number on
/// the wire is that one side chose it. A slot already past is not extended
/// into the future here either; the caller gets the slots it was told about
/// and [`punch`] skips the ones whose instant has gone.
pub fn punch_plan_from_slot(secret: &[u8; 32], first: u64, slots: u32) -> Vec<PunchSlot> {
    (0..u64::from(slots))
        .map(|step| {
            let slot = first.saturating_add(step);
            let opens_at_unix_ms = slot
                .saturating_mul(SLOT_SECONDS)
                .saturating_mul(1_000)
                .try_into()
                .unwrap_or(i64::MAX);
            PunchSlot {
                slot,
                port: derived_port(secret, slot),
                opens_at_unix_ms,
            }
        })
        .collect()
}

/// Why a punch did not happen, or did not work.
///
/// Every arm is a NAME a dial can print, which is the half of the gate that
/// says "never hangs": a punch that fails has to say which of these it was, so
/// an operator reading one line knows whether to fix an address, pair the two
/// Macs again, or stop expecting a direct path from this network at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PunchFailure {
    /// No peer has ever told this Mac where it sees that peer, and the row
    /// holds no dialable endpoint either, so there is no address to aim at.
    /// The ordinary answer for a pair that has only ever met on one LAN with
    /// no direct locator recorded.
    PeerAddressUnknown,
    /// No completed handshake with this peer since boot and no secret on its
    /// row, so the pair cannot compute a port. The pair still shares a key:
    /// one session fixes it.
    NoRendezvousSecret,
    /// Every slot opened, both sides dialled, and nothing connected.
    ///
    /// **This is what a symmetric NAT looks like from in here.** A NAT that
    /// assigns a fresh external port per destination (RFC 4787 calls the
    /// mapping address dependent) never maps the port the pair derived, so the
    /// far side's dial arrives at a port nothing is behind, in every slot,
    /// forever. The punch cannot tell that from a Mac that is simply asleep,
    /// so it names what it observed and how many times.
    NoSlotConnected {
        /// How many slots were actually waited out.
        slots_tried: u32,
        /// The port each of those slots was on, so a reader can check it
        /// against a packet capture.
        ports: Vec<u16>,
    },
    /// Nothing has ever told this Mac what address IT is reachable at, so a
    /// punch could ask the peer to aim at nothing. One session with any pinned
    /// peer on this build fixes it.
    SelfAddressUnknown,
    /// No pinned Mac could carry the ask to the peer, so the peer will not be
    /// in the slot. The punch is not attempted rather than spent on an empty
    /// slot.
    NotAnnounced,
    /// A peer asked for a punch and gave an address that is not one.
    ///
    /// Kept, rather than dropped as a malformed frame: the session it arrived
    /// on is authenticated, so this is a pinned Mac on a build that disagrees
    /// with this one about a format, which is worth a line naming what it sent.
    AddressNotUnderstood {
        /// What the peer sent, as it sent it.
        told: String,
    },
}

impl std::fmt::Display for PunchFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PeerAddressUnknown => write!(
                f,
                "punch: no peer has told this Mac an address for that one, and the row \
                 holds no dialable endpoint either, so there is nothing to punch at"
            ),
            Self::NoRendezvousSecret => write!(
                f,
                "punch: this process holds no rendezvous secret for that peer, so the two \
                 cannot compute a port; one completed session records it"
            ),
            Self::SelfAddressUnknown => write!(
                f,
                "punch: no peer has told this Mac its own public address yet, so there is                  nothing to ask that peer to aim at"
            ),
            Self::NotAnnounced => write!(
                f,
                "punch: no pinned Mac could carry the ask to that peer, so it would not be                  in the slot"
            ),
            Self::AddressNotUnderstood { told } => write!(
                f,
                "punch: that peer asked for a punch from `{told}`, which is not a socket                  address"
            ),
            Self::NoSlotConnected { slots_tried, ports } => write!(
                f,
                "punch: {slots_tried} slots opened on ports {ports:?} and neither side got \
                 through; a NAT that hands out a fresh port per destination cannot be \
                 punched through and a sleeping Mac looks the same from here"
            ),
        }
    }
}

impl std::error::Error for PunchFailure {}

/// The address to aim a punch at from a row's own endpoints, when the
/// observation register has nothing.
///
/// Three rules and nothing else: only a locator this node can dial itself
/// ([`crate::peer::config::Endpoint::direct_addr`]), a preference for one
/// whose IP is not LAN scope over one that is
/// ([`crate::peer::listener::is_lan_scope`] reads a tailnet address as
/// internet scope, which is correct here: a tailnet address is directly
/// dialable), and otherwise the row's own order, which is newest observation
/// first. Every direct locator on the row qualifies whatever its band: each
/// got there under a rule that already proved it.
pub fn aimable_address(endpoints: &[crate::peer::config::Endpoint]) -> Option<SocketAddr> {
    let dialable: Vec<SocketAddr> = endpoints
        .iter()
        .filter_map(crate::peer::config::Endpoint::direct_addr)
        .collect();
    dialable
        .iter()
        .find(|addr| !crate::peer::listener::is_lan_scope(addr.ip()))
        .or_else(|| dialable.first())
        .copied()
}

/// What a punch at `peer` needs before it starts: an address to aim at and the
/// pair's secret.
///
/// One function rather than two lookups at the call site, because "can these
/// two punch" is one question with two named answers, and a caller that asked
/// it in two places would print two different sentences for the same state.
///
/// The address comes from either of two places. The per boot observation
/// register first, because a source address a connection just arrived from is
/// the freshest fact there is; the row's own endpoints when the register has
/// nothing, because the register is never persisted and is empty for every
/// peer after any restart, move or no move, while the row may already hold
/// that peer's current public address from a moved link or a fetched dead
/// drop.
pub fn punch_target(
    peer: &tcr_peer_wire::PeerId,
    endpoints: &[crate::peer::config::Endpoint],
) -> Result<(IpAddr, [u8; 32]), PunchFailure> {
    let seen = observed_peer_for(peer)
        .map(|addr| addr.ip())
        .or_else(|| aimable_address(endpoints).map(|addr| addr.ip()));
    let Some(ip) = seen else {
        return Err(PunchFailure::PeerAddressUnknown);
    };
    let Some(secret) = port_secret_for(peer) else {
        return Err(PunchFailure::NoRendezvousSecret);
    };
    Ok((ip, secret))
}

/// Whether a punch at `peer` could be attempted right now.
///
/// The question `tcr peer reach` prints an answer to, stated as one call so
/// the verb and the dial cannot disagree about what "possible" means.
pub fn punch_is_possible(
    peer: &tcr_peer_wire::PeerId,
    endpoints: &[crate::peer::config::Endpoint],
) -> bool {
    punch_target(peer, endpoints).is_ok()
}

/// The two sockets a punch needs, as the one seam a test can stand behind.
///
/// # Why a trait and not two free functions
///
/// A punch is the one thing in this tree that cannot be exercised over
/// loopback as it ships. It needs two Macs whose PUBLIC ports are equal to the
/// ports they bound, and this machine has exactly one loopback address, so two
/// simulated routers would have to hold the same external port on
/// `127.0.0.1` at the same time and cannot. The seam is the socket layer and
/// nothing above it: [`punch`] itself, the slot waiting, the retry cadence and
/// the race between the two halves are the shipped code either way, and the
/// test swaps only what a router would have been.
///
/// Both methods take the LOCAL port, because that is the whole trick: the
/// outbound connect has to leave from the port the other side is dialling, or
/// the mapping it opens is for a port nobody will aim at.
pub trait PunchNet {
    /// Accept one connection on `port`, within `within`.
    fn accept_on(
        &self,
        port: u16,
        within: Duration,
    ) -> impl std::future::Future<Output = std::io::Result<crate::peer::serve::PeerStream>> + Send;

    /// Connect to `target` FROM local `port`, within `within`.
    fn connect_from(
        &self,
        port: u16,
        target: SocketAddr,
        within: Duration,
    ) -> impl std::future::Future<Output = std::io::Result<crate::peer::serve::PeerStream>> + Send;
}

/// Which half of the punch got through.
///
/// Recorded and reported because the two are not the same event on the wire
/// and an operator reading a packet capture needs to know which one to look
/// for. It decides NOTHING about roles: the side that asked for the punch runs
/// the dialling handshake and the side that was told runs the responding one,
/// whichever socket completed, because TCP's idea of who called whom is not
/// this protocol's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PunchArrival {
    /// This node's own connect completed.
    OurConnect,
    /// A connection arrived on the port this node bound.
    TheirConnect,
}

/// A punch that got through.
pub struct Punched {
    /// The slot it landed in.
    pub slot: u64,
    /// The port it landed on.
    pub port: u16,
    /// Which half completed.
    pub arrival: PunchArrival,
    /// The stream, with nothing sent on it yet: the caller runs the handshake
    /// its role calls for, and a stranger who guessed the port gets no further
    /// than that handshake.
    pub stream: crate::peer::serve::PeerStream,
}

impl std::fmt::Debug for Punched {
    /// Without the stream, which has no useful debug form.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Punched")
            .field("slot", &self.slot)
            .field("port", &self.port)
            .field("arrival", &self.arrival)
            .finish_non_exhaustive()
    }
}

/// How often a slot re-tries its outbound connect.
///
/// A punch is a race between two sides opening their mappings, and the loser
/// of that race is refused rather than queued: whoever connects first arrives
/// before the other end's router has a mapping, and gets a closed port. So the
/// connect is repeated for the width of the slot. A quarter second is short
/// enough that a 30 second slot holds about a hundred attempts and long enough
/// that two Macs are not spinning.
pub const PUNCH_RETRY: Duration = Duration::from_millis(250);

/// Run a punch: for each slot, bind the pair's port, and race an inbound
/// accept against a repeated outbound connect to the peer's public address on
/// the same port.
///
/// # Why both halves, on both sides
///
/// The outbound connect is not how this node reaches the peer. It is how this
/// node's own router is made to hold a mapping on the punched port, so the
/// peer's connect has somewhere to arrive. Both sides do both things and
/// whichever socket completes first is the connection; the other is dropped
/// when this function returns, and a peer that sees it close reads exactly
/// what it reads from any abandoned dial.
///
/// A slot whose instant has already passed is skipped rather than run late:
/// the far side is not there any more, and dialling into an empty slot is how
/// a plan that drifted turns into three connects nobody answers.
pub async fn punch<N: PunchNet + Sync>(
    net: &N,
    plan: &[PunchSlot],
    peer_ip: IpAddr,
    slot_window: Duration,
) -> Result<Punched, PunchFailure> {
    let mut tried = 0_u32;
    let mut ports = Vec::new();

    for entry in plan {
        let now = crate::now_ms();
        let window_ms = i64::try_from(slot_window.as_millis()).unwrap_or(i64::MAX);
        if entry.opens_at_unix_ms.saturating_add(window_ms) <= now {
            tracing::debug!(
                slot = entry.slot,
                port = entry.port,
                "peer punch: this slot is already over; the far side is not in it"
            );
            continue;
        }
        let wait_ms = entry.opens_at_unix_ms.saturating_sub(now).max(0);
        tokio::time::sleep(Duration::from_millis(u64::try_from(wait_ms).unwrap_or(0))).await;

        tried = tried.saturating_add(1);
        ports.push(entry.port);
        let target = SocketAddr::new(peer_ip, entry.port);

        let accepted = net.accept_on(entry.port, slot_window);
        let dialled = punch_connect_loop(net, entry.port, target, slot_window);
        tokio::pin!(accepted);
        tokio::pin!(dialled);

        let landed = tokio::select! {
            inbound = &mut accepted => inbound.map(|stream| (PunchArrival::TheirConnect, stream)),
            outbound = &mut dialled => outbound.map(|stream| (PunchArrival::OurConnect, stream)),
        };

        match landed {
            Ok((arrival, stream)) => {
                tracing::info!(
                    slot = entry.slot,
                    port = entry.port,
                    peer_addr = %target,
                    arrival = ?arrival,
                    "peer punch: a slot got through"
                );
                return Ok(Punched {
                    slot: entry.slot,
                    port: entry.port,
                    arrival,
                    stream,
                });
            }
            Err(err) => tracing::debug!(
                slot = entry.slot,
                port = entry.port,
                peer_addr = %target,
                error = %err,
                "peer punch: this slot came and went with nothing through; trying the next"
            ),
        }
    }

    Err(PunchFailure::NoSlotConnected {
        slots_tried: tried,
        ports,
    })
}

/// Connect to `target` from `port`, over and over, until `within` is spent.
///
/// The repeat is the point: see [`PUNCH_RETRY`]. Every refusal is the ordinary
/// outcome of being the first of the two to dial, so they are counted and the
/// last one is what the caller is told about, rather than each being an error
/// in its own right.
async fn punch_connect_loop<N: PunchNet + Sync>(
    net: &N,
    port: u16,
    target: SocketAddr,
    within: Duration,
) -> std::io::Result<crate::peer::serve::PeerStream> {
    let deadline = tokio::time::Instant::now() + within;
    let mut last = std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "the slot ended before any connect was attempted",
    );
    let mut attempts = 0_u32;
    while tokio::time::Instant::now() < deadline {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        attempts = attempts.saturating_add(1);
        match net.connect_from(port, target, left.min(within)).await {
            Ok(stream) => return Ok(stream),
            Err(err) => last = err,
        }
        tokio::time::sleep(
            PUNCH_RETRY.min(deadline.saturating_duration_since(tokio::time::Instant::now())),
        )
        .await;
    }
    Err(std::io::Error::new(
        last.kind(),
        format!("{attempts} connects to {target} from port {port} went unanswered: {last}"),
    ))
}

/// The [`PunchNet`] the program ships with: real sockets on this Mac.
///
/// `SO_REUSEADDR` and `SO_REUSEPORT` on both, because the punch binds one port
/// twice at once, a listener and a connecting socket, which is the whole shape
/// of a TCP simultaneous open and is refused outright without them.
#[derive(Debug, Clone, Copy, Default)]
pub struct KernelPunchNet;

impl KernelPunchNet {
    /// A socket bound to `port` on every interface, with both reuse options
    /// set. Errors surface: a bind that failed is the difference between a
    /// punch that cannot work and one that did not this time.
    fn bound(port: u16) -> std::io::Result<tokio::net::TcpSocket> {
        let socket = tokio::net::TcpSocket::new_v4()?;
        socket.set_reuseaddr(true)?;
        socket.set_reuseport(true)?;
        socket.bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port))?;
        Ok(socket)
    }
}

impl PunchNet for KernelPunchNet {
    async fn accept_on(
        &self,
        port: u16,
        within: Duration,
    ) -> std::io::Result<crate::peer::serve::PeerStream> {
        let listener = Self::bound(port)?.listen(8)?;
        match tokio::time::timeout(within, listener.accept()).await {
            Ok(Ok((stream, _from))) => Ok(Box::new(stream) as crate::peer::serve::PeerStream),
            Ok(Err(err)) => Err(err),
            Err(_elapsed) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("nothing arrived on punched port {port} inside the slot"),
            )),
        }
    }

    async fn connect_from(
        &self,
        port: u16,
        target: SocketAddr,
        within: Duration,
    ) -> std::io::Result<crate::peer::serve::PeerStream> {
        let socket = Self::bound(port)?;
        match tokio::time::timeout(within, socket.connect(target)).await {
            Ok(Ok(stream)) => Ok(Box::new(stream) as crate::peer::serve::PeerStream),
            Ok(Err(err)) => Err(err),
            Err(_elapsed) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("the connect to {target} from port {port} did not finish in time"),
            )),
        }
    }
}

/// What a peer's "meet me at slot N, I am at this address" frame asks for.
///
/// The whole of the listener's punch arm, here rather than there, so the
/// decision is a function a test can call: the arm itself only spawns what
/// this returns. Recording the address is part of the decision and not a step
/// the caller could forget, because an address a peer just told us is the
/// freshest one there is and the punch is about to aim at it.
pub fn punch_request(
    peer: tcr_peer_wire::PeerId,
    slot: u64,
    public_addr: &str,
    endpoints: &[crate::peer::config::Endpoint],
) -> Result<(IpAddr, Vec<PunchSlot>), PunchFailure> {
    let Ok(addr) = public_addr.parse::<SocketAddr>() else {
        return Err(PunchFailure::AddressNotUnderstood {
            told: public_addr.to_string(),
        });
    };
    remember_observed_peer(peer, addr);
    let (peer_ip, secret) = punch_target(&peer, endpoints)?;
    Ok((peer_ip, punch_plan_from_slot(&secret, slot, PUNCH_SLOTS)))
}

/// How long one side spends inside a slot: the slot's own width.
///
/// Stated as a [`Duration`] once, because three callers need it and a second
/// spelling of "a slot is 30 seconds" is a second answer.
pub fn slot_window() -> Duration {
    Duration::from_secs(SLOT_SECONDS)
}

/// The address to tell a peer to aim at: what THAT peer last saw this Mac as,
/// and failing that whatever any peer last saw it as.
///
/// The peer's own observation first, because two peers on different networks
/// may see this Mac as two different addresses and the one that is right for a
/// punch is the one the punching peer itself measured. Any other observation
/// is the fallback, and it is a fallback rather than nothing because a Mac
/// behind one router looks the same from everywhere outside it.
pub fn self_address_for(peer: &tcr_peer_wire::PeerId) -> Option<SocketAddr> {
    observed_self_for(peer).or_else(|| observed_self_addresses().first().map(|(_, addr)| *addr))
}

/// The slots of `plan` that open inside `budget` from now.
///
/// A dial has a deadline, the borrow timeout the operator set, and a punch
/// whose first slot opens after it would spend the whole budget waiting for a
/// boundary and then report the peer unreachable. So the punch takes the slots
/// that FIT, which is usually one and is sometimes none: a borrow with ten
/// seconds of patience meets a thirty second slot boundary a third of the
/// time. A caller with a longer budget gets all three.
pub fn punch_slots_within(
    plan: &[PunchSlot],
    now_unix_ms: i64,
    budget: Duration,
) -> Vec<PunchSlot> {
    let budget_ms = i64::try_from(budget.as_millis()).unwrap_or(i64::MAX);
    let deadline = now_unix_ms.saturating_add(budget_ms);
    plan.iter()
        .filter(|entry| entry.opens_at_unix_ms <= deadline)
        .copied()
        .collect()
}

/// The two lines `tcr peer reach` prints per pinned Mac: the address that Mac
/// last saw this one at, and whether a punch between them is possible.
///
/// A function here rather than formatting inside the verb, so the verb and the
/// dial cannot disagree about what "possible" means, and so the reading ships
/// without editing `src/main.rs`. The verb calls it with the row's label and
/// id.
///
/// Greppable, in the shape the rest of that verb already prints.
pub fn reach_punch_line(
    label: &str,
    peer: &tcr_peer_wire::PeerId,
    endpoints: &[crate::peer::config::Endpoint],
) -> String {
    let seen = match observed_self_for(peer) {
        Some(addr) => addr.to_string(),
        None => "not told (that Mac has not greeted this one since boot)".to_string(),
    };
    let punch = match punch_target(peer, endpoints) {
        Ok(_) => "yes".to_string(),
        Err(failure) => format!("no: {failure}"),
    };
    format!(
        "reach: peer: {label} {}: sees-us-at: {seen}: punch-possible: {punch}",
        peer.display()
    )
}

/// The last line `tcr peer reach` prints when no protocol will map a port:
/// what an operator can still do about it, in one sentence.
///
/// [`Some`] only for the outcome it is about, a router with no mapping service
/// this node can talk to ([`ReachError::no_natpmp_service`] names it, and the
/// keeper's answer carries the UPnP half too). Every other refusal is a device
/// that ANSWERED and was refused by name, where the thing to do is read that
/// name rather than reach for a workaround, so this stays [`None`] and the
/// verb prints nothing extra.
///
/// A function here rather than a `println!` in the verb, for the reason
/// [`reach_punch_line`] gives: the wording ships next to the decision that
/// selects it.
///
/// No new state, and nothing is asked of the router to produce it.
pub fn reach_no_mapping_line(failure: &ReachError, listen_port: Option<u16>) -> Option<String> {
    if !failure.no_natpmp_service() {
        return None;
    }
    let port = match listen_port {
        Some(port) => format!("tcp port {port}"),
        None => "the peer listener's port".to_string(),
    };
    Some(format!(
        "reach: no-mapping: this router will not open a port over either protocol, so two ways \
         are left: forward {port} to this Mac by hand in the router's own settings, or put both \
         Macs on one private network (Tailscale, or any VPN they both join) and pair over that"
    ))
}

#[cfg(test)]
mod recorded_external_socket_tests {
    use super::*;

    fn state_path() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::TempDir::new().expect("a scratch dir");
        let path = dir.path().join("peer-state.json");
        (dir, path)
    }

    fn record(
        external_address: Option<&str>,
        expires_at_ms: i64,
    ) -> crate::peer::state::MappingRecord {
        crate::peer::state::MappingRecord {
            external_address: external_address.map(str::to_string),
            external_port: 7_755,
            internal_port: 41_234,
            expires_at_ms,
        }
    }

    /// A live record yields the socket, and nothing to say about it.
    #[test]
    fn a_live_record_yields_the_socket() {
        let (_dir, path) = state_path();
        let now = 1_000;
        crate::peer::state::save_mapping(
            &path,
            Some(record(Some("198.51.100.9:7755"), now + 120_000)),
        )
        .expect("the record writes");
        let (socket, reason) = recorded_external_socket(&path, now);
        assert_eq!(socket, Some("198.51.100.9:7755".parse().expect("a socket")));
        assert_eq!(
            reason, None,
            "a live, well-formed record has nothing to explain"
        );
    }

    /// A record whose deadline has passed is no mapping, the same rule
    /// `tcr peer moved mint` already enforced before this function existed.
    #[test]
    fn an_expired_record_yields_neither_socket_nor_reason() {
        let (_dir, path) = state_path();
        let now = 1_000;
        crate::peer::state::save_mapping(&path, Some(record(Some("198.51.100.9:7755"), now - 1)))
            .expect("the record writes");
        let (socket, reason) = recorded_external_socket(&path, now);
        assert_eq!(
            socket, None,
            "a lapsed deadline is not a mapping to advertise"
        );
        assert_eq!(
            reason, None,
            "an expired record is silently no mapping, not a mapping this Mac failed to read"
        );
    }

    /// The router mapped a port but would not name its own external address:
    /// nothing to dial, and a sentence saying so.
    #[test]
    fn a_record_with_no_external_address_yields_a_reason() {
        let (_dir, path) = state_path();
        let now = 1_000;
        crate::peer::state::save_mapping(&path, Some(record(None, now + 120_000)))
            .expect("the record writes");
        let (socket, reason) = recorded_external_socket(&path, now);
        assert_eq!(socket, None);
        assert_eq!(
            reason.as_deref(),
            Some("the router mapped a port and would not name its own external address")
        );
    }

    /// A recorded string that does not parse as a socket is said out loud,
    /// never silently dropped: it is the difference between a link a friend
    /// off the LAN can act on and one they cannot.
    #[test]
    fn a_record_that_does_not_parse_yields_a_reason() {
        let (_dir, path) = state_path();
        let now = 1_000;
        crate::peer::state::save_mapping(&path, Some(record(Some("not-a-socket"), now + 120_000)))
            .expect("the record writes");
        let (socket, reason) = recorded_external_socket(&path, now);
        assert_eq!(socket, None);
        let reason = reason.expect("an unparseable record explains itself");
        assert!(
            reason.starts_with("the held mapping's external address did not parse ("),
            "the reason names what went wrong: {reason}"
        );
    }
}
