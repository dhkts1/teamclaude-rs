//! A second router client beside [`crate::peer::reach::NatPmp`]: the same
//! question, "get me a port on the router", answered over UPnP IGD instead
//! of NAT-PMP, for the gateways that speak only the older protocol.
//!
//! Same shape, same reasons, restated once rather than re-derived:
//!
//! - **Routing advice, not trust.** Exactly as [`crate::peer::reach`] says of
//!   NAT-PMP: identity stays the pinned static key, re-proven by the
//!   handshake every time, so a forged mapping buys nothing.
//! - **Synchronous on purpose, from a caller with nothing else in flight.**
//!   [`Discoverer::discover`] drives a plain [`std::net::UdpSocket`] with a
//!   read timeout; [`UpnpClient`]'s HTTP calls run on a DEDICATED thread with
//!   its own current-thread `tokio` runtime, the same pattern as
//!   `update::fetch_bytes` (`src/update.rs`), for the same reason: a caller
//!   already inside `#[tokio::main]` cannot `block_on` on its own thread, and
//!   a fresh thread has no runtime to collide with.
//! - **The discovery destination is injectable**, exactly as
//!   [`crate::peer::reach::NatPmp::at`] takes a gateway address: a test points
//!   [`Discoverer::at`] at a loopback fake and nothing here ever sends a
//!   packet to a real multicast group in the test suite.
//!
//! # Why a hand-rolled XML reader and not a crate
//!
//! The device description and every SOAP body this module reads are small,
//! shallow documents: elements with text or child elements, no namespaces
//! that matter beyond the `u:` prefix on an action name (never parsed, only
//! written), no processing instructions worth interpreting, no DTD. A crate
//! earns its keep on a document nobody controls the shape of; the shape here
//! is one page of UPnP IGD's own spec. [`parse_xml`] is a few dozen lines and
//! builds a plain tree; [`XmlNode::child`] and [`XmlNode::find_all`] are the
//! entire reading API, and neither cares what order sibling elements arrive
//! in: see the shuffled-order proptest in `tests/peer_reach_upnp.rs`.
//!
//! # The four calls, and the one invariant that binds them
//!
//! [`Discoverer::discover`] finds a gateway over SSDP; [`UpnpClient::discover`]
//! fetches its device description and locates the WANIPConnection control
//! URL (version 1 or 2, a gateway is asked for both in one search, and
//! either answers the same [`UpnpClient`]). From there,
//! [`UpnpClient::add_port_mapping`], [`UpnpClient::get_external_address`] and
//! [`UpnpClient::delete_port_mapping`] are three SOAP actions against the one
//! control URL. A refusal, most commonly UPnP error 718,
//! `ConflictInMappingEntry`, is [`UpnpError::Refused`], never a panic and
//! never silently swallowed: [`UpnpClient::call`] treats any SOAP fault as a
//! value to return, not a shape to retry past.
//!
//! # An SSDP answer is an unauthenticated datagram from anyone on the link
//!
//! Every host on the LAN can answer an `M-SEARCH`, and the first answer wins.
//! So the four calls above are driven by a URL a stranger chose, they are
//! re-run whenever the mapping is renewed, and their answer becomes the
//! address this node advertises to every pinned peer. Four rules keep that
//! from being a request forgery with a publishing side effect, and each one
//! is a named refusal with a test:
//!
//! 1. the `LOCATION` header must live on the very address the datagram came
//!    from ([`check_location`]) and that address must be a private IPv4, so a
//!    LAN host cannot aim this client at loopback, at another host's admin
//!    page, or at an address off the link. [`LocationPolicy`] is derived from
//!    the search destination, never from the answer;
//! 2. the resolved `controlURL` must share the description's own authority
//!    ([`locate_control_url`]), so neither an absolute `controlURL` nor a
//!    `URLBase` can move the SOAP calls to a third host;
//! 3. redirects are never followed and never silently treated as a body: a
//!    3xx is [`UpnpError::Redirected`], because following one would hand rule
//!    2 back to the description's author;
//! 4. the HTTP client is built with `.no_proxy()`, for the reason
//!    `peer::serve::serve_on_own_account` and `peer::lease` give: we ARE a
//!    proxy, and an ambient `HTTP_PROXY` would route a router call through
//!    the endpoint it is about.
//!
//! **What the four rules do NOT decide is who answers.** They bound where this
//! client can be sent; the race for the first `M-SEARCH` answer is still won by
//! whoever on the link replies fastest, and that host then serves the bytes.
//! So every answer is read under a ceiling as well ([`MAX_BODY_BYTES`]): a LAN
//! host that wins the race cannot make this node hold an unbounded body just
//! by promising one.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use crate::peer::reach::MapProtocol;

// ---------------------------------------------------------------------------
// SSDP discovery
// ---------------------------------------------------------------------------

/// The two WANIPConnection service URNs a gateway may advertise, checked in
/// this order. IGD 2 gateways still answer to a search for IGD 1's URN in
/// practice, but the search asks for both so a gateway that only recognises
/// its own version's `ST` still answers.
pub const SERVICE_TYPES: [&str; 2] = [
    "urn:schemas-upnp-org:service:WANIPConnection:1",
    "urn:schemas-upnp-org:service:WANIPConnection:2",
];

/// SSDP's `MX` header: the top of the random delay window a gateway is
/// allowed to answer within (UPnP Device Architecture § 1.2.2).
///
/// Two seconds, not the spec's ceiling of five: this is an interactive verb
/// on the local link, and RFC 6886's own NAT-PMP client in this module tree
/// (`reach.rs`) calls a gateway silent after 1.75 s of nothing. A search that
/// waited five seconds for a router that will never answer would make UPnP
/// look five times slower than NAT-PMP for the identical outcome.
pub const MX_SECONDS: u32 = 2;

/// The largest SSDP response this client reads. Real gateways answer in a few
/// hundred bytes of headers; this is headroom, not a measured maximum.
const MAX_SSDP_RESPONSE: usize = 2048;

/// Everything that can stop a UPnP reachability call, each one a value a
/// caller prints rather than a condition it panics on, the same contract as
/// [`crate::peer::reach::ReachError`], restated in this file's own type
/// because the two protocols' failure shapes do not line up: NAT-PMP has one
/// numeric result code, UPnP has an HTTP status plus an optional SOAP fault
/// carrying its own code and a free-text description.
#[derive(Debug, thiserror::Error)]
pub enum UpnpError {
    /// The local UDP socket would not open, bind, send or receive.
    #[error("peer reach (upnp): the local UDP socket failed: {0}")]
    Socket(#[source] std::io::Error),
    /// No gateway answered the SSDP search within [`MX_SECONDS`]. **Not a
    /// failure of this Mac**: most gateways that do not speak UPnP simply say
    /// nothing, the same way most NAT-PMP-less routers do.
    #[error("peer reach (upnp): no gateway answered the SSDP search for {searched} within {mx}s")]
    Silent {
        /// The service types that were searched for, joined for one log line.
        searched: String,
        /// How long the search waited.
        mx: u32,
    },
    /// An SSDP response, a device description, or a SOAP body did not parse.
    #[error("peer reach (upnp): {0}")]
    Malformed(String),
    /// An HTTP call, fetching the device description, or a SOAP POST, did
    /// not complete.
    #[error("peer reach (upnp): the HTTP call to {url} failed: {source}")]
    Http {
        /// The URL that was called.
        url: String,
        /// The underlying transport error.
        #[source]
        source: reqwest::Error,
    },
    /// An HTTP call completed but its body was not the shape this reader
    /// expects.
    #[error("peer reach (upnp): {url} answered with a body this reader could not use: {reason}")]
    UnreadableResponse {
        /// The URL that answered.
        url: String,
        /// What was wrong with the body.
        reason: String,
    },
    /// The device description at `location` named no WANIPConnection service
    /// in either version.
    #[error("peer reach (upnp): {location} advertises no WANIPConnection service")]
    NoWanService {
        /// Where the description was fetched from.
        location: String,
    },
    /// The gateway answered a SOAP call with a fault: RFC-shape, gateway
    /// error 718 (`ConflictInMappingEntry`) most commonly among them.
    #[error(
        "peer reach (upnp): the gateway refused {action} with UPnP error {code}: {description}"
    )]
    Refused {
        /// Which SOAP action was refused.
        action: String,
        /// The UPnP error code from `<errorCode>`.
        code: u16,
        /// The UPnP error text from `<errorDescription>`, empty if the fault
        /// carried none.
        description: String,
    },
    /// An SSDP answer advertised a device description this client will not
    /// fetch: on a host other than the one that answered, or in an address
    /// class the search was not aimed at.
    #[error(
        "peer reach (upnp): refusing the device description at {location} advertised from {from}: {reason}"
    )]
    UntrustedLocation {
        /// The `LOCATION` header exactly as it arrived.
        location: String,
        /// The source address of the datagram that carried it.
        from: IpAddr,
        /// Which of [`check_location`]'s rules refused it.
        reason: String,
    },
    /// A device description named a `controlURL` that resolves onto an
    /// authority other than its own, whether written absolute or moved there
    /// by a `URLBase`.
    #[error(
        "peer reach (upnp): the description at {location} points its controlURL at {control_url}, which is not on {location}'s own host"
    )]
    ControlUrlElsewhere {
        /// Where the description was fetched from.
        location: String,
        /// The resolved control URL that was refused.
        control_url: String,
    },
    /// An HTTP call was answered with a redirect. Never followed: a gateway
    /// that redirects a description fetch is pointing this client at a host
    /// that answered no search, which is the same move the `controlURL` rule
    /// refuses one hop later.
    #[error("peer reach (upnp): {url} answered HTTP {status} redirecting to {to}, which is never followed")]
    Redirected {
        /// The URL that was called.
        url: String,
        /// The redirect status it answered with.
        status: u16,
        /// The `Location` header it offered, or `(none)` when it sent none.
        to: String,
    },
    /// The thread driving an HTTP call panicked rather than returning.
    #[error("peer reach (upnp): the worker thread driving an HTTP call panicked")]
    WorkerPanicked,
}

/// Where a gateway said its device description lives, which service version
/// answered, and the address the answer actually came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredGateway {
    /// The `LOCATION` header: an absolute URL to the device description.
    pub location: String,
    /// The `ST` header that matched, `SERVICE_TYPES[0]` or `[1]`.
    pub service_type: String,
    /// The source address of the datagram that carried this answer. Kept
    /// rather than discarded because it is the only thing in an SSDP reply
    /// this node did not take on the responder's word: see [`check_location`].
    pub from: IpAddr,
}

/// The real SSDP multicast group (UPnP Device Architecture § 1.2.2): all
/// UPnP control points and devices, port 1900.
fn multicast_destination() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(239, 255, 255, 250)), 1900)
}

/// An SSDP search pointed at one destination.
///
/// The destination is the whole seam: production points it at the real
/// multicast group with [`Self::multicast`], and a test points it at a
/// loopback fake with [`Self::at`], exactly the shape
/// [`crate::peer::reach::NatPmp::at`] uses, so nothing in this module ever
/// needs to multicast on a real interface to be tested.
#[derive(Debug, Clone, Copy)]
pub struct Discoverer {
    destination: SocketAddr,
}

impl Discoverer {
    /// Point a search at an explicit destination: a test's fake responder, or
    /// an operator who knows a gateway's unicast address.
    pub fn at(destination: SocketAddr) -> Self {
        Self { destination }
    }

    /// Point a search at the real SSDP multicast group.
    pub fn multicast() -> Self {
        Self::at(multicast_destination())
    }

    /// Who this search is sent to.
    pub fn destination(&self) -> SocketAddr {
        self.destination
    }

    /// Which address classes an answer to THIS search may advertise a
    /// description in, derived from the destination and never from the
    /// answer: a search sent to the multicast group or to a LAN address
    /// accepts only a private IPv4 description host, and only a search
    /// deliberately aimed at loopback accepts a loopback one.
    pub fn location_policy(&self) -> LocationPolicy {
        if self.destination.ip().is_loopback() {
            LocationPolicy::Loopback
        } else {
            LocationPolicy::PrivateLan
        }
    }

    /// Send an `M-SEARCH` for both [`SERVICE_TYPES`] and return the first
    /// gateway that answers with one of them.
    ///
    /// One socket, two sends, one read loop bounded by [`MX_SECONDS`], a
    /// gateway that answers only the version it advertises still gets a
    /// question it recognises, and a gateway that answers both is read once,
    /// on whichever reply arrives first.
    pub fn discover(&self) -> Result<DiscoveredGateway, UpnpError> {
        let bind: SocketAddr = match self.destination {
            SocketAddr::V4(_) => "0.0.0.0:0".parse(),
            SocketAddr::V6(_) => "[::]:0".parse(),
        }
        .map_err(|err: std::net::AddrParseError| {
            UpnpError::Malformed(format!("the bind address would not parse: {err}"))
        })?;
        let socket = UdpSocket::bind(bind).map_err(UpnpError::Socket)?;

        for service_type in SERVICE_TYPES {
            let request = m_search_request(service_type);
            socket
                .send_to(request.as_bytes(), self.destination)
                .map_err(UpnpError::Socket)?;
        }

        let deadline = Instant::now() + Duration::from_secs(u64::from(MX_SECONDS) + 1);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            socket
                .set_read_timeout(Some(remaining))
                .map_err(UpnpError::Socket)?;
            let mut buffer = [0_u8; MAX_SSDP_RESPONSE];
            match socket.recv_from(&mut buffer) {
                Ok((read, from)) => {
                    let text = String::from_utf8_lossy(&buffer[..read]);
                    if let Some(found) = parse_ssdp_response(&text, from.ip()) {
                        // A matching answer that fails the address rules ends
                        // the search with the reason, rather than being
                        // skipped: a refusal a caller never sees would leave
                        // a LAN host able to turn a forged answer into a
                        // silent "no gateway here".
                        check_location(&found.location, found.from, self.location_policy())?;
                        return Ok(found);
                    }
                    // Not every datagram on this socket answers this search
                    // (a stray reply to something else on the link); keep
                    // listening until the deadline.
                }
                Err(err)
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    break;
                }
                Err(err) => return Err(UpnpError::Socket(err)),
            }
        }
        Err(UpnpError::Silent {
            searched: SERVICE_TYPES.join(", "),
            mx: MX_SECONDS,
        })
    }
}

/// The `M-SEARCH` request text for one service type (UPnP Device Architecture
/// § 1.2.2).
fn m_search_request(service_type: &str) -> String {
    format!(
        "M-SEARCH * HTTP/1.1\r\n\
         HOST: 239.255.255.250:1900\r\n\
         MAN: \"ssdp:discover\"\r\n\
         MX: {MX_SECONDS}\r\n\
         ST: {service_type}\r\n\r\n"
    )
}

/// Read `LOCATION` and `ST` out of an SSDP response, and refuse anything
/// whose `ST` is not one of [`SERVICE_TYPES`].
///
/// The refusal matters: a gateway's SSDP responder answers a search for
/// every service it hosts, not only the one asked about, and a reply naming
/// `Layer3Forwarding` or `WANCommonInterfaceConfig` must not be read as a
/// WANIPConnection gateway.
pub fn parse_ssdp_response(text: &str, from: IpAddr) -> Option<DiscoveredGateway> {
    let mut location: Option<String> = None;
    let mut service_type: Option<String> = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            // The status line ("HTTP/1.1 200 OK") carries no colon; every
            // header line does, so this is the normal way to skip it rather
            // than a reason to abandon the whole response.
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if key.eq_ignore_ascii_case("LOCATION") {
            location = Some(value.to_string());
        } else if key.eq_ignore_ascii_case("ST") {
            service_type = Some(value.to_string());
        }
    }
    let location = location?;
    let service_type = service_type?;
    if !SERVICE_TYPES.contains(&service_type.as_str()) {
        return None;
    }
    Some(DiscoveredGateway {
        location,
        service_type,
        from,
    })
}

/// Which address classes a description may live in for one search.
///
/// Derived from the search destination by [`Discoverer::location_policy`],
/// never from the answer, so a responder cannot widen the rule it is judged
/// by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocationPolicy {
    /// A search on the link: only a private IPv4 address (RFC 1918) is a
    /// description host this client will fetch from.
    PrivateLan,
    /// A search deliberately aimed at loopback, an operator naming a local
    /// device or a test's fake: loopback is a description host too, and every
    /// other rule still applies.
    Loopback,
}

/// Whether the description at `location`, advertised by a datagram from
/// `from`, is one this client will fetch.
///
/// The rules, in the order they are checked and each its own refusal text:
/// plain `http`, a host written as an address rather than a name (a name
/// resolves wherever its answer says, which hands the decision back to
/// whoever chose it), that address equal to the responder's own, and that
/// address inside `policy`'s class.
///
/// The `from` equality rule is the load-bearing one. A LAN host that answers
/// an `M-SEARCH` first, with `LOCATION` pointed at a loopback service or at a
/// neighbour's admin page, would otherwise have this node fetch that URL, re-
/// fetch it on every mapping renewal, and publish whatever
/// [`UpnpClient::get_external_address`] read out of it to every pinned peer.
pub fn check_location(
    location: &str,
    from: IpAddr,
    policy: LocationPolicy,
) -> Result<(), UpnpError> {
    let refuse = |reason: String| UpnpError::UntrustedLocation {
        location: location.to_string(),
        from,
        reason,
    };
    let url =
        reqwest::Url::parse(location).map_err(|err| refuse(format!("it is not a URL: {err}")))?;
    if url.scheme() != "http" {
        return Err(refuse(format!(
            "the scheme is {:?}, and a device description is fetched over plain http on the link",
            url.scheme()
        )));
    }
    let Some(host) = url.host_str() else {
        return Err(refuse("it names no host at all".to_string()));
    };
    // An IPv6 host arrives bracketed in a URL's serialization.
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let Ok(addr) = host.parse::<IpAddr>() else {
        return Err(refuse(format!(
            "the host {host:?} is a name, not an address, and a name resolves wherever its own answer says"
        )));
    };
    if addr != from {
        return Err(refuse(format!(
            "the host {addr} is not the responder that advertised it"
        )));
    }
    let in_class = match addr {
        IpAddr::V4(v4) => {
            v4.is_private() || (policy == LocationPolicy::Loopback && v4.is_loopback())
        }
        // No IPv6 case: UPnP IGD hands out IPv4 mappings, and an IPv6
        // description host has no private class this rule could name.
        IpAddr::V6(_) => false,
    };
    if !in_class {
        return Err(refuse(format!(
            "{addr} is outside the address class this search accepts ({policy:?})"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The smallest XML reader that survives sibling elements in any order
// ---------------------------------------------------------------------------

/// One element: its tag, its own direct text (entities already decoded), and
/// its child elements in document order.
///
/// Document order is kept because it costs nothing to keep and nothing here
/// depends on it, every read in this file is "the first child with this
/// tag" or "every descendant with this tag", neither of which cares which
/// position a match was found at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmlNode {
    /// The element's tag name, without namespace prefix stripped, a `u:`
    /// prefix on a SOAP action name is kept as part of the tag, since this
    /// reader never needs to resolve a namespace to read a value.
    pub tag: String,
    /// The element's own text, trimmed. Empty for an element that holds only
    /// child elements.
    pub text: String,
    /// Child elements, in document order.
    pub children: Vec<XmlNode>,
}

impl XmlNode {
    /// The first direct child with this tag name.
    pub fn child(&self, tag: &str) -> Option<&XmlNode> {
        self.children.iter().find(|child| child.tag == tag)
    }

    /// Every descendant, this subtree, any depth, with this tag name,
    /// document order. Used for a value that a reader cares about regardless
    /// of how deep the document nested it, such as `<service>` anywhere under
    /// a `<serviceList>` anywhere under a `<device>`.
    pub fn find_all<'a>(&'a self, tag: &str, out: &mut Vec<&'a XmlNode>) {
        if self.tag == tag {
            out.push(self);
        }
        for child in &self.children {
            child.find_all(tag, out);
        }
    }
}

/// How deeply nested an element may be before this parser refuses the
/// document.
///
/// [`Cursor::parse_element`] recurses once per nesting level, so the depth of
/// the document is the depth of the call stack. The body it reads comes from
/// whichever host on the LAN won the SSDP race, under [`MAX_BODY_BYTES`], and
/// 256 KiB of `<a>` is about 85 000 levels: far past what any thread's stack
/// holds, and a stack overflow is an abort, not an error, so the keeper thread
/// takes the whole process down with it. A ceiling turns that into a refused
/// description.
///
/// Thirty-two: a device description is `root > device > deviceList > device >
/// serviceList > service`, about six levels, and every real router this parser
/// was written against sits under ten.
pub const MAX_XML_DEPTH: usize = 32;

/// A cursor over the input text. The whole "parser": skip trivia, read one
/// element, recurse into its children until its own close tag.
struct Cursor<'a> {
    input: &'a str,
    pos: usize,
    /// How many `parse_element` frames are live above this call, so the
    /// recursion can refuse [`MAX_XML_DEPTH`] rather than overflow the stack.
    depth: usize,
}

impl<'a> Cursor<'a> {
    fn rest(&self) -> &'a str {
        &self.input[self.pos..]
    }

    /// Skip whitespace, `<?...?>` processing instructions and `<!--...-->`
    /// comments, in any mixture, until none remain at the cursor.
    fn skip_trivia(&mut self) {
        loop {
            let rest = self.rest();
            let trimmed = rest.trim_start();
            self.pos += rest.len() - trimmed.len();
            let rest = self.rest();
            if let Some(body) = rest.strip_prefix("<?") {
                if let Some(end) = body.find("?>") {
                    self.pos += 2 + end + 2;
                    continue;
                }
            }
            if let Some(body) = rest.strip_prefix("<!--") {
                if let Some(end) = body.find("-->") {
                    self.pos += 4 + end + 3;
                    continue;
                }
            }
            break;
        }
    }

    /// Read one element, its text and its children, up to and including its
    /// own closing tag.
    fn parse_element(&mut self) -> Result<XmlNode, UpnpError> {
        if self.depth >= MAX_XML_DEPTH {
            return Err(UpnpError::Malformed(format!(
                "the description nests deeper than {MAX_XML_DEPTH} elements; refused before \
                 reading further"
            )));
        }
        self.depth += 1;
        let element = self.parse_element_inner();
        self.depth -= 1;
        element
    }

    /// [`Self::parse_element`] without the depth accounting, so that ceiling
    /// is charged once per level on one path in and one path out.
    fn parse_element_inner(&mut self) -> Result<XmlNode, UpnpError> {
        self.skip_trivia();
        let rest = self.rest();
        if !rest.starts_with('<') {
            return Err(UpnpError::Malformed(
                "expected '<' to start an element".into(),
            ));
        }
        let close_angle = rest
            .find('>')
            .ok_or_else(|| UpnpError::Malformed("a tag was never closed with '>'".into()))?;
        let raw = &rest[1..close_angle];
        let self_closing = raw.trim_end().ends_with('/');
        let raw = if self_closing {
            raw.trim_end().strip_suffix('/').unwrap_or(raw).trim_end()
        } else {
            raw
        };
        let tag = raw
            .split(char::is_whitespace)
            .next()
            .unwrap_or_default()
            .to_string();
        if tag.is_empty() {
            return Err(UpnpError::Malformed("an element had no tag name".into()));
        }
        self.pos += close_angle + 1;

        if self_closing {
            return Ok(XmlNode {
                tag,
                text: String::new(),
                children: Vec::new(),
            });
        }

        let mut children = Vec::new();
        let mut text = String::new();
        loop {
            let rest = self.rest();
            if rest.is_empty() {
                return Err(UpnpError::Malformed(format!("<{tag}> was never closed")));
            }
            if let Some(body) = rest.strip_prefix("<!--") {
                let end = body
                    .find("-->")
                    .ok_or_else(|| UpnpError::Malformed("an unterminated comment".into()))?;
                self.pos += 4 + end + 3;
                continue;
            }
            if rest.starts_with("</") {
                let end = rest.find('>').ok_or_else(|| {
                    UpnpError::Malformed("a closing tag was never closed with '>'".into())
                })?;
                self.pos += end + 1;
                break;
            }
            if rest.starts_with('<') {
                children.push(self.parse_element()?);
                continue;
            }
            let next_lt = rest.find('<').unwrap_or(rest.len());
            let chunk = decode_entities(&rest[..next_lt]);
            let trimmed = chunk.trim();
            if !trimmed.is_empty() {
                text.push_str(trimmed);
            }
            self.pos += next_lt;
        }

        Ok(XmlNode {
            tag,
            text,
            children,
        })
    }
}

/// Decode the five predefined XML entities. `&amp;` last, so an entity that
/// decodes to `&lt;` is not itself re-decoded into `<`.
fn decode_entities(input: &str) -> String {
    input
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Parse `input` as one XML document and return its root element.
///
/// Leading whitespace, an `<?xml ...?>` declaration and comments before the
/// root are skipped; nothing after the root's own close tag is read.
///
/// A document nested deeper than [`MAX_XML_DEPTH`] is refused rather than
/// followed: see that constant for the stack this parser would otherwise run
/// off the end of.
pub fn parse_xml(input: &str) -> Result<XmlNode, UpnpError> {
    let mut cursor = Cursor {
        input,
        pos: 0,
        depth: 0,
    };
    cursor.parse_element()
}

/// Escape the five characters SOAP argument text must not carry raw.
fn escape_xml(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// ---------------------------------------------------------------------------
// The device description: finding the WANIPConnection control URL
// ---------------------------------------------------------------------------

/// Walk a parsed device description for a `<service>` whose `serviceType`
/// matches [`SERVICE_TYPES`], and resolve its `controlURL` against the
/// description's own base.
///
/// `find_all` rather than a fixed path: a real device description nests
/// `<service>` under `<serviceList>` under `<device>`, sometimes under a
/// nested `<deviceList>` for a router with multiple logical devices (a guest
/// network, for one), and the nesting depth is not part of the contract this
/// reader owes, only the tag name and its two children are.
pub fn locate_control_url(
    root: &XmlNode,
    description_url: &str,
) -> Result<(String, String), UpnpError> {
    let base = root
        .child("URLBase")
        .map(|node| node.text.as_str())
        .filter(|text| !text.is_empty())
        .unwrap_or(description_url);

    let mut services = Vec::new();
    root.find_all("service", &mut services);

    for wanted in SERVICE_TYPES {
        for service in &services {
            let Some(service_type) = service.child("serviceType").map(|node| node.text.as_str())
            else {
                continue;
            };
            if service_type != wanted {
                continue;
            }
            let Some(control) = service.child("controlURL").map(|node| node.text.as_str()) else {
                continue;
            };
            let resolved = resolve_url(base, control)?;
            // The authority rule. `base` is the description's `URLBase` when
            // it carries one, and both that and an absolute `controlURL`
            // replace the authority wholesale, so the resolved URL is checked
            // against the host the description was actually fetched from and
            // not against whatever the document nominated for itself.
            if !same_authority(&resolved, description_url)? {
                return Err(UpnpError::ControlUrlElsewhere {
                    location: description_url.to_string(),
                    control_url: resolved,
                });
            }
            return Ok((resolved, wanted.to_string()));
        }
    }

    Err(UpnpError::NoWanService {
        location: description_url.to_string(),
    })
}

/// Resolve `relative` against `base`, exactly as a browser resolves a
/// `controlURL` against the page it was found on, `reqwest::Url::join`,
/// which handles both a path already absolute and one that is not.
fn resolve_url(base: &str, relative: &str) -> Result<String, UpnpError> {
    let base_url = reqwest::Url::parse(base).map_err(|err| {
        UpnpError::Malformed(format!(
            "the device description's own URL would not parse: {err}"
        ))
    })?;
    let joined = base_url.join(relative).map_err(|err| {
        UpnpError::Malformed(format!("{relative} does not resolve against {base}: {err}"))
    })?;
    Ok(joined.to_string())
}

/// Whether two URLs share scheme, host and effective port.
///
/// Compared field by field rather than by string prefix: `http://host:80/` and
/// `http://host/` are the same authority, and a prefix test would call
/// `http://192.168.1.1.evil.example/` a match for `http://192.168.1.1/`.
fn same_authority(left: &str, right: &str) -> Result<bool, UpnpError> {
    let parse = |raw: &str| {
        reqwest::Url::parse(raw)
            .map_err(|err| UpnpError::Malformed(format!("{raw} would not parse as a URL: {err}")))
    };
    let left = parse(left)?;
    let right = parse(right)?;
    Ok(left.scheme() == right.scheme()
        && left.host_str() == right.host_str()
        && left.port_or_known_default() == right.port_or_known_default())
}

// ---------------------------------------------------------------------------
// SOAP: the three actions
// ---------------------------------------------------------------------------

/// Build one SOAP envelope for `action` on `service_type`, with `args` as the
/// action's ordered parameters.
fn soap_envelope(service_type: &str, action: &str, args: &[(&str, String)]) -> String {
    let mut body = String::new();
    body.push_str(&format!("<u:{action} xmlns:u=\"{service_type}\">"));
    for (name, value) in args {
        body.push_str(&format!("<{name}>{}</{name}>", escape_xml(value)));
    }
    body.push_str(&format!("</u:{action}>"));
    format!(
        "<?xml version=\"1.0\"?>\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
         <s:Body>{body}</s:Body></s:Envelope>"
    )
}

/// Read a SOAP fault's UPnP error code and description out of a response
/// body, or [`None`] when the body carries neither, the caller falls back
/// to the bare HTTP status in that case.
fn parse_soap_fault(text: &str) -> Option<(u16, String)> {
    let root = parse_xml(text).ok()?;
    let mut codes = Vec::new();
    root.find_all("errorCode", &mut codes);
    let code: u16 = codes.first()?.text.trim().parse().ok()?;
    let mut descriptions = Vec::new();
    root.find_all("errorDescription", &mut descriptions);
    let description = descriptions
        .first()
        .map(|node| node.text.trim().to_string())
        .unwrap_or_default();
    Some((code, description))
}

/// A UPnP IGD client pointed at one gateway's WANIPConnection control URL.
///
/// Built by [`Self::discover`], which is the only way to get one: a control
/// URL with no matching `service_type` is not a client this file can build,
/// so there is no constructor that skips the search and the description.
#[derive(Debug, Clone)]
pub struct UpnpClient {
    control_url: String,
    service_type: String,
}

impl UpnpClient {
    /// Run a search with `discoverer`, fetch the gateway's device
    /// description, and locate its WANIPConnection control URL.
    pub fn discover(discoverer: &Discoverer) -> Result<Self, UpnpError> {
        let found = discoverer.discover()?;
        let answer = fetch(&found.location, reqwest::Method::GET, &[], None)?;
        answer.refuse_redirect(&found.location)?;
        if answer.status >= 400 {
            return Err(UpnpError::UnreadableResponse {
                url: found.location.clone(),
                reason: format!("it answered HTTP {} to a description fetch", answer.status),
            });
        }
        let root = parse_xml(&answer.body)?;
        let (control_url, service_type) = locate_control_url(&root, &found.location)?;
        Ok(Self {
            control_url,
            service_type,
        })
    }

    /// The control URL this client calls every action against.
    pub fn control_url(&self) -> &str {
        &self.control_url
    }

    /// Which service version answered, `SERVICE_TYPES[0]` or `[1]`.
    pub fn service_type(&self) -> &str {
        &self.service_type
    }

    /// `GetExternalIPAddress`: the gateway's view of the internet, the same
    /// question [`crate::peer::reach::NatPmp::external_address`] asks over
    /// NAT-PMP.
    pub fn get_external_address(&self) -> Result<Ipv4Addr, UpnpError> {
        let body = self.call("GetExternalIPAddress", &[])?;
        let root = parse_xml(&body)?;
        let mut nodes = Vec::new();
        root.find_all("NewExternalIPAddress", &mut nodes);
        let text = nodes.first().map(|node| node.text.trim()).ok_or_else(|| {
            UpnpError::UnreadableResponse {
                url: self.control_url.clone(),
                reason: "no NewExternalIPAddress in the response".into(),
            }
        })?;
        text.parse::<Ipv4Addr>()
            .map_err(|err| UpnpError::UnreadableResponse {
                url: self.control_url.clone(),
                reason: format!("{text:?} is not an IPv4 address: {err}"),
            })
    }

    /// `AddPortMapping`: ask for a mapping from `external_port` to
    /// `internal_port` on `internal_client`, held for `lease_duration_secs`
    /// (0 means "until explicitly deleted", per the spec).
    ///
    /// Unlike NAT-PMP, UPnP IGD does not hand back a granted external port:
    /// the action either confirms the port asked for or refuses, most
    /// commonly with error 718 when it is already taken.
    pub fn add_port_mapping(
        &self,
        protocol: MapProtocol,
        external_port: u16,
        internal_port: u16,
        internal_client: Ipv4Addr,
        description: &str,
        lease_duration_secs: u32,
    ) -> Result<(), UpnpError> {
        let args = [
            ("NewRemoteHost", String::new()),
            ("NewExternalPort", external_port.to_string()),
            ("NewProtocol", protocol.label().to_uppercase()),
            ("NewInternalPort", internal_port.to_string()),
            ("NewInternalClient", internal_client.to_string()),
            ("NewEnabled", "1".to_string()),
            ("NewPortMappingDescription", description.to_string()),
            ("NewLeaseDuration", lease_duration_secs.to_string()),
        ];
        self.call("AddPortMapping", &args)?;
        Ok(())
    }

    /// `DeletePortMapping`: drop the mapping for `external_port`.
    pub fn delete_port_mapping(
        &self,
        protocol: MapProtocol,
        external_port: u16,
    ) -> Result<(), UpnpError> {
        let args = [
            ("NewRemoteHost", String::new()),
            ("NewExternalPort", external_port.to_string()),
            ("NewProtocol", protocol.label().to_uppercase()),
        ];
        self.call("DeletePortMapping", &args)?;
        Ok(())
    }

    /// Run one SOAP action against this client's control URL and return its
    /// response body, or a typed [`UpnpError::Refused`] naming the gateway's
    /// own fault code and description.
    fn call(&self, action: &str, args: &[(&str, String)]) -> Result<String, UpnpError> {
        let body = soap_envelope(&self.service_type, action, args);
        let soap_action = format!("\"{}#{action}\"", self.service_type);
        let headers = [
            ("Content-Type", "text/xml; charset=\"utf-8\"".to_string()),
            ("SOAPAction", soap_action),
        ];
        let answer = fetch(
            &self.control_url,
            reqwest::Method::POST,
            &headers,
            Some(body),
        )?;
        answer.refuse_redirect(&self.control_url)?;
        let status = answer.status;
        let text = answer.body;
        if status >= 400 {
            let (code, description) = parse_soap_fault(&text).unwrap_or((
                status,
                "the gateway returned an error with no readable UPnP fault".to_string(),
            ));
            return Err(UpnpError::Refused {
                action: action.to_string(),
                code,
                description,
            });
        }
        Ok(text)
    }
}

/// One HTTP answer, kept whole: a redirect is a status and a `Location`
/// header this client refuses by name, so the header is read out before the
/// body rather than thrown away with the response.
struct Fetched {
    status: u16,
    redirect_to: Option<String>,
    body: String,
}

impl Fetched {
    /// Turn a 3xx into [`UpnpError::Redirected`]. Rule 3 of this module's
    /// doc: the client does not follow redirects, so a redirect must not read
    /// as a short body either.
    fn refuse_redirect(&self, url: &str) -> Result<(), UpnpError> {
        if (300..400).contains(&self.status) {
            return Err(UpnpError::Redirected {
                url: url.to_string(),
                status: self.status,
                to: self
                    .redirect_to
                    .clone()
                    .unwrap_or_else(|| "(none)".to_string()),
            });
        }
        Ok(())
    }
}

/// The most of one answer this client will hold.
///
/// A device description is a few kilobytes of XML and a SOAP response is a few
/// hundred bytes; 256 KiB is far above anything a real router sends and far
/// below anything worth calling memory pressure. The figure is a choice, not a
/// measurement: what it has to be is big enough that no honest device trips it
/// and small enough that a host which wins the SSDP race cannot stream this
/// node out of memory.
pub const MAX_BODY_BYTES: usize = 256 * 1024;

/// Run one blocking HTTP call on a dedicated thread with its own
/// current-thread `tokio` runtime, see this module's doc comment for why.
///
/// The body is read under [`MAX_BODY_BYTES`], twice: a `Content-Length` above
/// it is refused BEFORE a byte of body is read, and the read itself stops at
/// the ceiling, which is the half that matters, since a chunked answer
/// promises no length at all and `Content-Length` is the sender's claim about
/// itself.
fn fetch(
    url: &str,
    method: reqwest::Method,
    headers: &[(&str, String)],
    body: Option<String>,
) -> Result<Fetched, UpnpError> {
    let url_owned = url.to_string();
    let headers_owned: Vec<(String, String)> = headers
        .iter()
        .map(|(key, value)| (key.to_string(), value.clone()))
        .collect();
    let worker = std::thread::spawn(move || -> Result<Fetched, UpnpError> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|err| UpnpError::UnreadableResponse {
                url: url_owned.clone(),
                reason: format!("could not start a runtime for the HTTP call: {err}"),
            })?;
        rt.block_on(async move {
            let client = reqwest::Client::builder()
                // `no_proxy` for the reason `peer::serve::serve_on_own_account`
                // and `peer::lease` give: an ambient `HTTP_PROXY` very commonly
                // points AT tcr, and honouring it here would send a router call
                // through the endpoint it is about.
                .no_proxy()
                // No redirect is ever followed. The URL being fetched already
                // passed `check_location` or the `controlURL` authority rule;
                // following a redirect would let the answer move the call
                // somewhere neither rule ever saw.
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(5))
                .build()
                .map_err(|source| UpnpError::Http {
                    url: url_owned.clone(),
                    source,
                })?;
            let mut request = client.request(method, &url_owned);
            for (key, value) in &headers_owned {
                request = request.header(key.as_str(), value.as_str());
            }
            if let Some(body) = body {
                request = request.body(body);
            }
            let response = request.send().await.map_err(|source| UpnpError::Http {
                url: url_owned.clone(),
                source,
            })?;
            let status = response.status().as_u16();
            let redirect_to = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .map(|value| value.to_string());
            // The sender's own claim first: an answer that says it is larger
            // than the ceiling is refused before a byte of it is read.
            if let Some(promised) = response.content_length() {
                if promised > MAX_BODY_BYTES as u64 {
                    return Err(UpnpError::UnreadableResponse {
                        url: url_owned.clone(),
                        reason: format!(
                            "it promises {promised} bytes and the ceiling here is \
                             {MAX_BODY_BYTES}; refused before reading the body"
                        ),
                    });
                }
            }
            // And then the read, which is the half that holds: a chunked
            // answer promises no length, and a promise is not a limit anyway.
            let mut collected: Vec<u8> = Vec::new();
            let mut response = response;
            while let Some(chunk) = response.chunk().await.map_err(|source| UpnpError::Http {
                url: url_owned.clone(),
                source,
            })? {
                if collected.len() + chunk.len() > MAX_BODY_BYTES {
                    return Err(UpnpError::UnreadableResponse {
                        url: url_owned.clone(),
                        reason: format!(
                            "its body passed the {MAX_BODY_BYTES}-byte ceiling; the read \
                             stopped there rather than holding whatever it kept sending"
                        ),
                    });
                }
                collected.extend_from_slice(&chunk);
            }
            Ok(Fetched {
                status,
                redirect_to,
                body: String::from_utf8_lossy(&collected).into_owned(),
            })
        })
    });
    worker.join().map_err(|_| UpnpError::WorkerPanicked)?
}
