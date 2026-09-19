//! A fake UPnP IGD gateway on loopback, and the pure XML
//! reader that reads it.
//!
//! Nothing here reaches a real router and nothing multicasts on a real
//! interface: [`teamclaude_rs::peer::reach_upnp::Discoverer::at`] takes a
//! destination, so every test here points it at a fake SSDP responder bound
//! to `127.0.0.1:0`, exactly as `tests/peer_reach.rs` points
//! [`teamclaude_rs::peer::reach::NatPmp::at`] at a fake NAT-PMP gateway.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use proptest::prelude::*;

use teamclaude_rs::peer::reach::MapProtocol;
use teamclaude_rs::peer::reach_upnp::{
    self, DiscoveredGateway, Discoverer, LocationPolicy, UpnpClient, UpnpError, MX_SECONDS,
    SERVICE_TYPES,
};

/// The loopback address every fake in this file answers from, and the one a
/// LAN responder must never be allowed to point this client at.
const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

fn cases() -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(64)
}

// ---------------------------------------------------------------------------
// The fake device: an SSDP responder plus an HTTP server, sharing a call log
// ---------------------------------------------------------------------------

/// What the fake device answers `AddPortMapping` with.
#[derive(Clone, Copy)]
enum AddBehaviour {
    /// Confirm the mapping.
    Confirm,
    /// Refuse with UPnP error 718, `ConflictInMappingEntry`, the measured
    /// common case for "this port is already mapped".
    Refuse718,
}

/// What the fake device serves at `/desc.xml`.
#[derive(Clone, Copy, Debug)]
enum DescriptionBehaviour {
    /// The honest description: one WANIPConnection service at a control URL
    /// relative to the description itself.
    Honest,
    /// A 302 to another host, the shape that would move every later SOAP call
    /// off the device that answered the search.
    Redirecting,
    /// An honest-looking description whose `controlURL` is absolute and on
    /// another host entirely.
    ControlUrlElsewhere,
    /// The same move through a `URLBase`, which replaces the authority a
    /// relative `controlURL` resolves against.
    UrlBaseElsewhere,
    /// A description far above the body ceiling, sent with a `Content-Length`
    /// that says so: the refusal this earns happens before a byte of body is
    /// read.
    Oversized,
    /// The same size with no `Content-Length` at all, streamed a chunk at a
    /// time, which is the shape a promise-based check cannot see.
    OversizedChunked,
}

/// Every call the fake saw, in arrival order: `"discover"`, `"add"`,
/// `"external-ip"`, `"delete"`.
type CallLog = Arc<Mutex<Vec<String>>>;

/// The host a hostile description tries to move this client onto. A
/// documentation-range address (RFC 5737): nothing in this suite can reach
/// it, which is the point, the refusal must happen before any call is made.
const ELSEWHERE: &str = "203.0.113.200:8080";

/// A UPnP device standing on loopback: an SSDP UDP responder and an HTTP
/// server serving the device description and the SOAP control URL, sharing
/// one call log and one external IP to hand back.
struct FakeDevice {
    ssdp_addr: SocketAddr,
    calls: CallLog,
}

impl FakeDevice {
    /// Start the fake, with `add_behaviour` deciding how `AddPortMapping`
    /// answers and an honest device description.
    fn start(add_behaviour: AddBehaviour) -> Self {
        Self::start_with(add_behaviour, DescriptionBehaviour::Honest)
    }

    /// Start the fake with both behaviours chosen: the hostile-description
    /// tests pick the description apart from how `AddPortMapping` answers.
    fn start_with(add_behaviour: AddBehaviour, description: DescriptionBehaviour) -> Self {
        let calls: CallLog = Arc::new(Mutex::new(Vec::new()));

        let http_addr = spawn_http_server(Arc::clone(&calls), add_behaviour, description);
        let location = format!("http://{http_addr}/desc.xml");
        let ssdp_addr = spawn_ssdp_responder(Arc::clone(&calls), location);

        Self { ssdp_addr, calls }
    }

    fn log(&self) -> Vec<String> {
        self.calls
            .lock()
            .expect("the call log is never poisoned")
            .clone()
    }
}

/// Bind a loopback UDP socket and answer every `M-SEARCH` datagram it reads
/// with an SSDP response naming `location`. Logs `"discover"` once, the
/// first time any datagram arrives, the client sends two searches (one per
/// service version) but a single gateway is discovered once.
fn spawn_ssdp_responder(calls: CallLog, location: String) -> SocketAddr {
    let socket = UdpSocket::bind("127.0.0.1:0").expect("bind the fake SSDP responder");
    let addr = socket
        .local_addr()
        .expect("the fake responder's own address");
    let logged = Arc::new(AtomicUsize::new(0));

    std::thread::spawn(move || loop {
        let mut buffer = [0_u8; 1024];
        let Ok((_read, from)) = socket.recv_from(&mut buffer) else {
            return;
        };
        if logged.fetch_add(1, Ordering::SeqCst) == 0 {
            calls
                .lock()
                .expect("the call log is never poisoned")
                .push("discover".to_string());
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\n\
             CACHE-CONTROL: max-age=1800\r\n\
             LOCATION: {location}\r\n\
             SERVER: fake-router/1.0 UPnP/1.1\r\n\
             ST: {}\r\n\
             USN: uuid:fake-router::{}\r\n\r\n",
            SERVICE_TYPES[0], SERVICE_TYPES[0]
        );
        let _sent = socket.send_to(response.as_bytes(), from);
    });

    addr
}

/// Bind a loopback TCP listener and serve the device description at
/// `/desc.xml` and the SOAP control URL at `/upnp/control`, on a dedicated
/// thread with its own runtime, the fake needs no part of the outer test's
/// own async context.
fn spawn_http_server(
    calls: CallLog,
    add_behaviour: AddBehaviour,
    description: DescriptionBehaviour,
) -> SocketAddr {
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind the fake device");
    std_listener
        .set_nonblocking(true)
        .expect("the fake device's listener must be non-blocking for tokio");
    let addr = std_listener
        .local_addr()
        .expect("the fake device's own address");

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime for the fake device");
        rt.block_on(async move {
            let app = Router::new()
                .route(
                    "/desc.xml",
                    get(move || async move { device_description(description) }),
                )
                .route(
                    "/upnp/control",
                    post(move |body: String| {
                        let calls = Arc::clone(&calls);
                        async move { soap_control(body, calls, add_behaviour) }
                    }),
                );
            let listener = tokio::net::TcpListener::from_std(std_listener)
                .expect("the fake device's listener must convert to a tokio one");
            let _served = axum::serve(listener, app).await;
        });
    });

    addr
}

/// A device description naming exactly one `WANIPConnection:1` service, at a
/// control URL relative to the description itself, the shape a real router
/// answers in, or one of the three hostile variants.
fn device_description(behaviour: DescriptionBehaviour) -> Response<Body> {
    // Twice the ceiling, so neither leg can pass by rounding.
    let oversized = reach_upnp::MAX_BODY_BYTES * 2;
    if matches!(behaviour, DescriptionBehaviour::Oversized) {
        return Response::builder()
            .status(200)
            .header("content-type", "text/xml")
            .body(Body::from("x".repeat(oversized)))
            .expect("the oversized description builds");
    }
    if matches!(behaviour, DescriptionBehaviour::OversizedChunked) {
        // 8 KiB at a time with no length header: hyper sends this chunked, so
        // the only thing that can stop it is the read itself.
        let chunks = (0..oversized / 8_192)
            .map(|_| Ok::<_, std::io::Error>(bytes::Bytes::from_static(&[b'x'; 8_192])));
        return Response::builder()
            .status(200)
            .header("content-type", "text/xml")
            .body(Body::from_stream(futures::stream::iter(chunks)))
            .expect("the streamed oversized description builds");
    }
    if matches!(behaviour, DescriptionBehaviour::Redirecting) {
        return Response::builder()
            .status(302)
            .header("location", format!("http://{ELSEWHERE}/desc.xml"))
            .body(Body::empty())
            .expect("the canned redirect builds");
    }
    let control_url = match behaviour {
        DescriptionBehaviour::ControlUrlElsewhere => format!("http://{ELSEWHERE}/upnp/control"),
        _ => "/upnp/control".to_string(),
    };
    let url_base = match behaviour {
        DescriptionBehaviour::UrlBaseElsewhere => format!("<URLBase>http://{ELSEWHERE}/</URLBase>"),
        _ => String::new(),
    };
    let body = format!(
        r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  {url_base}
  <device>
    <deviceType>urn:schemas-upnp-org:device:InternetGatewayDevice:1</deviceType>
    <friendlyName>fake-router</friendlyName>
    <serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:Layer3Forwarding:1</serviceType>
        <controlURL>/upnp/l3f</controlURL>
      </service>
      <service>
        <serviceType>{}</serviceType>
        <serviceId>urn:upnp-org:serviceId:WANIPConn1</serviceId>
        <controlURL>{control_url}</controlURL>
        <eventSubURL>/upnp/event</eventSubURL>
        <SCPDURL>/upnp/scpd.xml</SCPDURL>
      </service>
    </serviceList>
  </device>
</root>"#,
        SERVICE_TYPES[0]
    );
    Response::builder()
        .status(200)
        .header("content-type", "text/xml")
        .body(Body::from(body))
        .expect("the canned device description builds")
}

/// Handle one SOAP POST: read the action out of `SOAPAction`-shaped text
/// inside the envelope, log it, and answer per `add_behaviour` for
/// `AddPortMapping`.
fn soap_control(body: String, calls: CallLog, add_behaviour: AddBehaviour) -> Response<Body> {
    let root = reach_upnp::parse_xml(&body).expect("the client's own envelope must parse");
    let mut soap_body = Vec::new();
    root.find_all("s:Body", &mut soap_body);
    let action_node = soap_body
        .first()
        .and_then(|b| b.children.first())
        .expect("a SOAP envelope carries one action element in its body");
    let action = action_node
        .tag
        .strip_prefix("u:")
        .unwrap_or(&action_node.tag)
        .to_string();

    let label = match action.as_str() {
        "AddPortMapping" => "add",
        "GetExternalIPAddress" => "external-ip",
        "DeletePortMapping" => "delete",
        other => panic!("the fake device was asked for an action it does not model: {other}"),
    };
    calls
        .lock()
        .expect("the call log is never poisoned")
        .push(label.to_string());

    match (action.as_str(), add_behaviour) {
        ("AddPortMapping", AddBehaviour::Refuse718) => Response::builder()
            .status(500)
            .header("content-type", "text/xml")
            .body(Body::from(
                r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
<s:Body><s:Fault>
<faultcode>s:Client</faultcode>
<faultstring>UPnPError</faultstring>
<detail><UPnPError xmlns="urn:schemas-upnp-org:control-1-0">
<errorCode>718</errorCode>
<errorDescription>ConflictInMappingEntry</errorDescription>
</UPnPError></detail>
</s:Fault></s:Body></s:Envelope>"#,
            ))
            .expect("the canned fault builds"),
        ("AddPortMapping", AddBehaviour::Confirm) => soap_ok(
            "AddPortMappingResponse",
            "urn:schemas-upnp-org:service:WANIPConnection:1",
            "",
        ),
        ("GetExternalIPAddress", _) => soap_ok(
            "GetExternalIPAddressResponse",
            "urn:schemas-upnp-org:service:WANIPConnection:1",
            "<NewExternalIPAddress>203.0.113.9</NewExternalIPAddress>",
        ),
        ("DeletePortMapping", _) => soap_ok(
            "DeletePortMappingResponse",
            "urn:schemas-upnp-org:service:WANIPConnection:1",
            "",
        ),
        (other, _) => panic!("unhandled action in the fake device: {other}"),
    }
}

fn soap_ok(response_tag: &str, service_type: &str, inner: &str) -> Response<Body> {
    let body = format!(
        r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
<s:Body><u:{response_tag} xmlns:u="{service_type}">{inner}</u:{response_tag}></s:Body>
</s:Envelope>"#
    );
    Response::builder()
        .status(200)
        .header("content-type", "text/xml")
        .body(Body::from(body))
        .expect("the canned SOAP response builds")
}

// ---------------------------------------------------------------------------
// The gate: discover, add, external-ip, delete, in that order
// ---------------------------------------------------------------------------

/// **The gate for item 1**: a fake UPnP device answers discover, add,
/// external-ip, delete, and the fake's own call log proves the ORDER, not
/// only that each call happened.
#[test]
fn the_four_calls_run_in_order_against_a_fake_device() {
    let fake = FakeDevice::start(AddBehaviour::Confirm);
    let discoverer = Discoverer::at(fake.ssdp_addr);

    let client = UpnpClient::discover(&discoverer).expect("discovery against a cooperative fake");
    assert_eq!(client.service_type(), SERVICE_TYPES[0]);
    assert!(
        client.control_url().ends_with("/upnp/control"),
        "the control URL must be the one the description named: {}",
        client.control_url()
    );

    client
        .add_port_mapping(
            MapProtocol::Tcp,
            41_234,
            3_456,
            Ipv4Addr::new(192, 168, 1, 50),
            "tcr peer reach (upnp)",
            600,
        )
        .expect("AddPortMapping against a cooperative fake");

    let external = client
        .get_external_address()
        .expect("GetExternalIPAddress against a cooperative fake");
    assert_eq!(external, Ipv4Addr::new(203, 0, 113, 9));

    client
        .delete_port_mapping(MapProtocol::Tcp, 41_234)
        .expect("DeletePortMapping against a cooperative fake");

    assert_eq!(
        fake.log(),
        vec![
            "discover".to_string(),
            "add".to_string(),
            "external-ip".to_string(),
            "delete".to_string(),
        ],
        "the fake's own call log must show the four calls in the order this test made them"
    );
}

/// **The gate for item 2**: a router that answers SSDP but refuses
/// `AddPortMapping` with UPnP error 718 surfaces as a named error variant,
/// never a panic.
#[test]
fn add_port_mapping_refused_with_upnp_error_718_is_a_named_error() {
    let fake = FakeDevice::start(AddBehaviour::Refuse718);
    let discoverer = Discoverer::at(fake.ssdp_addr);
    let client = UpnpClient::discover(&discoverer).expect("discovery against the fake");

    let outcome = client.add_port_mapping(
        MapProtocol::Tcp,
        41_234,
        3_456,
        Ipv4Addr::new(192, 168, 1, 50),
        "tcr peer reach (upnp)",
        600,
    );

    match outcome {
        Err(UpnpError::Refused {
            action,
            code,
            description,
        }) => {
            assert_eq!(action, "AddPortMapping");
            assert_eq!(code, 718);
            assert_eq!(description, "ConflictInMappingEntry");
        }
        other => panic!("expected a named Refused(718) error, got {other:?}"),
    }
}

/// A gateway that never answers SSDP costs [`MX_SECONDS`] and yields a named
/// [`UpnpError::Silent`], the same contract NAT-PMP's `ReachError::Silent`
/// gives for a router with the protocol off.
#[test]
fn a_silent_gateway_yields_a_named_silent_error() {
    // Bound but never read: exactly the SSDP-side equivalent of
    // `tests/peer_reach.rs`'s `Behaviour::Silent`.
    let socket = UdpSocket::bind("127.0.0.1:0").expect("bind a socket nobody answers from");
    let addr = socket.local_addr().expect("its own address");
    // Keep the socket alive for the duration of the search so the OS does not
    // recycle the port out from under the test.
    let _keep_alive = socket;

    let discoverer = Discoverer::at(addr);
    let started = Instant::now();
    let outcome = discoverer
        .discover()
        .expect_err("nobody answers this socket");
    let elapsed = started.elapsed();

    assert!(
        matches!(
            outcome,
            UpnpError::Silent { mx, .. } if mx == MX_SECONDS
        ),
        "expected a Silent error naming MX_SECONDS, got {outcome:?}"
    );
    assert!(
        elapsed >= Duration::from_secs(u64::from(MX_SECONDS)),
        "a silent search must wait at least MX seconds before giving up: took {elapsed:?}"
    );
}

// ---------------------------------------------------------------------------
// Pure-function coverage: the SSDP response reader
// ---------------------------------------------------------------------------

/// [`reach_upnp::parse_ssdp_response`] reads `LOCATION` and `ST`, and refuses
/// a service type this client does not search for, a gateway's SSDP
/// responder answers every service it hosts, and a `Layer3Forwarding` row
/// must not be read as a WANIPConnection gateway.
#[test]
fn parse_ssdp_response_reads_the_wan_service_and_rejects_others() {
    let text = format!(
        "HTTP/1.1 200 OK\r\nLOCATION: http://192.168.1.1:5000/desc.xml\r\nST: {}\r\n\r\n",
        SERVICE_TYPES[1]
    );
    let from = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1));
    let found =
        reach_upnp::parse_ssdp_response(&text, from).expect("a well-formed response parses");
    assert_eq!(
        found,
        DiscoveredGateway {
            location: "http://192.168.1.1:5000/desc.xml".to_string(),
            service_type: SERVICE_TYPES[1].to_string(),
            from,
        }
    );

    let other_service = "HTTP/1.1 200 OK\r\nLOCATION: http://192.168.1.1:5000/desc.xml\r\n\
                          ST: urn:schemas-upnp-org:service:Layer3Forwarding:1\r\n\r\n";
    assert_eq!(
        reach_upnp::parse_ssdp_response(other_service, from),
        None,
        "a reply naming a service this client did not search for must not be read as one"
    );

    let no_location = format!("HTTP/1.1 200 OK\r\nST: {}\r\n\r\n", SERVICE_TYPES[0]);
    assert_eq!(reach_upnp::parse_ssdp_response(&no_location, from), None);
}

// ---------------------------------------------------------------------------
// An SSDP answer is an unauthenticated datagram: the four refusals
// ---------------------------------------------------------------------------

/// **Refusal one, end to end**: a responder that answers the search but
/// advertises a description on a DIFFERENT host is refused by name, and the
/// refusal happens before any HTTP call is attempted.
///
/// This is a failing input with the two address classes swapped
/// so it can run on loopback: there a LAN host pointed the description at
/// loopback, here a loopback responder points it at a LAN address. The rule
/// that refuses both is the same one, "the description lives on the host that
/// answered", and the direction described is covered by
/// [`the_briefed_ssrf_answer_is_refused_by_its_own_rule`] just below.
#[test]
fn a_description_on_another_host_than_the_responder_is_refused_by_name() {
    let calls: CallLog = Arc::new(Mutex::new(Vec::new()));
    // Nothing listens there, and nothing may try to: the refusal must land
    // before a socket is opened to it.
    let lying = spawn_ssdp_responder(
        Arc::clone(&calls),
        "http://192.168.7.7:5000/desc.xml".to_string(),
    );

    let outcome = Discoverer::at(lying).discover();

    match outcome {
        Err(UpnpError::UntrustedLocation {
            location,
            from,
            reason,
        }) => {
            assert_eq!(location, "http://192.168.7.7:5000/desc.xml");
            assert_eq!(from, LOOPBACK);
            assert!(
                reason.contains("is not the responder"),
                "the refusal must name the rule it broke, got {reason:?}"
            );
        }
        other => panic!("expected a named UntrustedLocation refusal, got {other:?}"),
    }

    let client = UpnpClient::discover(&Discoverer::at(lying));
    assert!(
        matches!(client, Err(UpnpError::UntrustedLocation { .. })),
        "building a client must be refused the same way, got {client:?}"
    );
}

/// The same input in its own direction: a host on the LAN answers the
/// search with a `LOCATION` on loopback. Pure, because this suite cannot make
/// a datagram arrive from a LAN address, and the check is the same function
/// the socket path calls.
///
/// The port is deliberately NOT the one the live proxy serves on: a fixture
/// in this repo never names it, even in a string nothing connects to.
#[test]
fn the_briefed_ssrf_answer_is_refused_by_its_own_rule() {
    let from = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50));
    let outcome = reach_upnp::check_location(
        "http://127.0.0.1:9910/_tcr/description.xml",
        from,
        LocationPolicy::PrivateLan,
    );
    match outcome {
        Err(UpnpError::UntrustedLocation { reason, .. }) => {
            assert!(reason.contains("is not the responder"), "got {reason:?}")
        }
        other => panic!("expected a named UntrustedLocation refusal, got {other:?}"),
    }
}

/// **Refusal two**: loopback is a description host only for a search that was
/// deliberately aimed at loopback, and the policy is read off the search
/// destination, never off the answer.
#[test]
fn a_loopback_description_is_refused_for_a_search_on_the_link() {
    assert_eq!(
        Discoverer::multicast().location_policy(),
        LocationPolicy::PrivateLan,
        "the production search accepts only a private IPv4 description host"
    );
    assert_eq!(
        Discoverer::at(SocketAddr::new(LOOPBACK, 1900)).location_policy(),
        LocationPolicy::Loopback,
        "a search aimed at loopback is the one case loopback is in class"
    );

    let outcome = reach_upnp::check_location(
        "http://127.0.0.1:5000/desc.xml",
        LOOPBACK,
        LocationPolicy::PrivateLan,
    );
    match outcome {
        Err(UpnpError::UntrustedLocation { reason, .. }) => assert!(
            reason.contains("outside the address class"),
            "the refusal must name the class rule, got {reason:?}"
        ),
        other => panic!("expected a named UntrustedLocation refusal, got {other:?}"),
    }

    reach_upnp::check_location(
        "http://127.0.0.1:5000/desc.xml",
        LOOPBACK,
        LocationPolicy::Loopback,
    )
    .expect("the same answer is in class for a search aimed at loopback");
    reach_upnp::check_location(
        "http://192.168.1.1:5000/desc.xml",
        IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)),
        LocationPolicy::PrivateLan,
    )
    .expect("a real gateway on the link is exactly what this rule lets through");
}

/// A host written as a NAME, and a scheme that is not plain `http`, are both
/// refused: a name resolves wherever its own answer says, which would hand the
/// address decision back to whoever chose the name.
#[test]
fn check_location_refuses_a_name_host_and_a_foreign_scheme() {
    let name = reach_upnp::check_location(
        "http://router.example/desc.xml",
        LOOPBACK,
        LocationPolicy::Loopback,
    );
    match name {
        Err(UpnpError::UntrustedLocation { reason, .. }) => {
            assert!(
                reason.contains("is a name, not an address"),
                "got {reason:?}"
            )
        }
        other => panic!("expected a named refusal for a name host, got {other:?}"),
    }

    let scheme =
        reach_upnp::check_location("file:///etc/hosts", LOOPBACK, LocationPolicy::Loopback);
    match scheme {
        Err(UpnpError::UntrustedLocation { reason, .. }) => {
            assert!(reason.contains("the scheme is"), "got {reason:?}")
        }
        other => panic!("expected a named refusal for a foreign scheme, got {other:?}"),
    }
}

/// **Refusal three**: a description fetch that answers with a redirect is a
/// named error, never followed and never read as a short body.
#[test]
fn a_description_that_redirects_is_refused_by_name() {
    let fake = FakeDevice::start_with(AddBehaviour::Confirm, DescriptionBehaviour::Redirecting);
    let outcome = UpnpClient::discover(&Discoverer::at(fake.ssdp_addr));

    match outcome {
        Err(UpnpError::Redirected { status, to, .. }) => {
            assert_eq!(status, 302);
            assert_eq!(to, format!("http://{ELSEWHERE}/desc.xml"));
        }
        other => panic!("expected a named Redirected refusal, got {other:?}"),
    }
    assert_eq!(
        fake.log(),
        vec!["discover".to_string()],
        "nothing beyond the search may have reached the device"
    );
}

/// **Refusal four**: a `controlURL` that resolves onto another authority is
/// refused, whether it is written absolute or moved there by a `URLBase`.
#[test]
fn a_control_url_on_another_host_is_refused_by_name() {
    for description in [
        DescriptionBehaviour::ControlUrlElsewhere,
        DescriptionBehaviour::UrlBaseElsewhere,
    ] {
        let fake = FakeDevice::start_with(AddBehaviour::Confirm, description);
        let outcome = UpnpClient::discover(&Discoverer::at(fake.ssdp_addr));
        match outcome {
            Err(UpnpError::ControlUrlElsewhere { control_url, .. }) => assert_eq!(
                control_url,
                format!("http://{ELSEWHERE}/upnp/control"),
                "the refusal must name the URL it would have called"
            ),
            other => panic!("expected a named ControlUrlElsewhere refusal, got {other:?}"),
        }
        assert_eq!(
            fake.log(),
            vec!["discover".to_string()],
            "no SOAP call may be made to the host the description nominated"
        );
    }
}

/// The same rule as a pure read of a description, including the case that
/// makes a string prefix test wrong: `http://192.168.1.1.evil.example/` is a
/// different authority from `http://192.168.1.1/` even though one starts with
/// the other.
#[test]
fn locate_control_url_refuses_a_lookalike_authority() {
    let doc = format!(
        r#"<root><device><serviceList>
      <service><serviceType>{}</serviceType>
        <controlURL>http://192.168.1.1.evil.example/upnp/control</controlURL></service>
    </serviceList></device></root>"#,
        SERVICE_TYPES[0]
    );
    let root = reach_upnp::parse_xml(&doc).expect("a well-formed description must parse");
    let outcome = reach_upnp::locate_control_url(&root, "http://192.168.1.1:5000/desc.xml");
    assert!(
        matches!(outcome, Err(UpnpError::ControlUrlElsewhere { .. })),
        "expected a named ControlUrlElsewhere refusal, got {outcome:?}"
    );

    // The same host on an explicit default port is the SAME authority, and
    // must still be accepted: this rule refuses another host, not another
    // spelling.
    let same = format!(
        r#"<root><device><serviceList>
      <service><serviceType>{}</serviceType>
        <controlURL>http://192.168.1.1:80/upnp/control</controlURL></service>
    </serviceList></device></root>"#,
        SERVICE_TYPES[0]
    );
    let root = reach_upnp::parse_xml(&same).expect("a well-formed description must parse");
    let (control_url, _service_type) =
        reach_upnp::locate_control_url(&root, "http://192.168.1.1/desc.xml")
            .expect("the same authority written with its default port must be accepted");
    assert_eq!(control_url, "http://192.168.1.1/upnp/control");
}

// ---------------------------------------------------------------------------
// Pure-function coverage: the XML reader
// ---------------------------------------------------------------------------

/// Comments, a processing instruction, entities and a self-closing tag, all
/// in one document, every shape [`reach_upnp::parse_xml`] must not choke on.
#[test]
fn parse_xml_reads_comments_entities_and_self_closing_tags() {
    let doc = r#"<?xml version="1.0"?>
<!-- a router's own comment -->
<root>
  <name>fake &amp; friendly &lt;router&gt;</name>
  <empty/>
  <nested><a>1</a><b>2</b></nested>
</root>"#;
    let root = reach_upnp::parse_xml(doc).expect("a well-formed document must parse");
    assert_eq!(root.tag, "root");
    assert_eq!(
        root.child("name").map(|n| n.text.as_str()),
        Some("fake & friendly <router>")
    );
    assert_eq!(root.child("empty").map(|n| n.text.as_str()), Some(""));
    let nested = root
        .child("nested")
        .expect("the nested element must be found");
    assert_eq!(nested.child("a").map(|n| n.text.as_str()), Some("1"));
    assert_eq!(nested.child("b").map(|n| n.text.as_str()), Some("2"));
}

/// [`reach_upnp::locate_control_url`] finds the WANIPConnection service among
/// several siblings and resolves its relative `controlURL` against the
/// description's own URL.
#[test]
fn locate_control_url_finds_the_wan_service_and_resolves_a_relative_url() {
    let doc = format!(
        r#"<root>
  <device>
    <serviceList>
      <service><serviceType>urn:schemas-upnp-org:service:Layer3Forwarding:1</serviceType>
        <controlURL>/upnp/l3f</controlURL></service>
      <service><serviceType>{}</serviceType>
        <controlURL>/upnp/control</controlURL></service>
    </serviceList>
  </device>
</root>"#,
        SERVICE_TYPES[0]
    );
    let root = reach_upnp::parse_xml(&doc).expect("a well-formed description must parse");
    let (control_url, service_type) =
        reach_upnp::locate_control_url(&root, "http://192.168.1.1:5000/desc.xml")
            .expect("the WANIPConnection service must be found");
    assert_eq!(control_url, "http://192.168.1.1:5000/upnp/control");
    assert_eq!(service_type, SERVICE_TYPES[0]);
}

/// A description naming no WANIPConnection service in either version is a
/// named error, not a panic and not a silent empty answer.
#[test]
fn locate_control_url_names_the_error_when_no_wan_service_is_present() {
    let doc = r#"<root><device><serviceList>
      <service><serviceType>urn:schemas-upnp-org:service:Layer3Forwarding:1</serviceType>
        <controlURL>/upnp/l3f</controlURL></service>
    </serviceList></device></root>"#;
    let root = reach_upnp::parse_xml(doc).expect("a well-formed description must parse");
    let outcome = reach_upnp::locate_control_url(&root, "http://192.168.1.1:5000/desc.xml");
    assert!(
        matches!(outcome, Err(UpnpError::NoWanService { .. })),
        "expected a named NoWanService error, got {outcome:?}"
    );
}

// ---------------------------------------------------------------------------
// Property tests: sibling element order must never change what is read
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    /// **The proptest the brief asks for**: shuffle a `<service>` element's
    /// four children into every order a router's XML serializer might choose
    /// and confirm every one is still found by tag. `seed` is four bytes used
    /// as sort keys, so each proptest case produces a different permutation
    /// without needing a dedicated shuffle strategy.
    #[test]
    fn xml_child_lookup_is_order_independent_under_shuffle(seed in prop::collection::vec(any::<u8>(), 4)) {
        let fields = [
            ("serviceType", SERVICE_TYPES[0]),
            ("controlURL", "/upnp/control"),
            ("eventSubURL", "/upnp/event"),
            ("SCPDURL", "/upnp/scpd.xml"),
        ];
        let mut indexed: Vec<(u8, (&str, &str))> = seed.into_iter().zip(fields).collect();
        indexed.sort_by_key(|(key, _)| *key);

        let mut xml = String::from("<service>");
        for (_, (tag, value)) in &indexed {
            xml.push_str(&format!("<{tag}>{value}</{tag}>"));
        }
        xml.push_str("</service>");

        let node = reach_upnp::parse_xml(&xml).expect("a shuffled but well-formed document must parse");
        for (tag, value) in fields {
            prop_assert_eq!(
                node.child(tag).map(|c| c.text.as_str()),
                Some(value),
                "field {} must be found regardless of sibling order (xml: {})",
                tag,
                xml
            );
        }
    }

    /// The same property one level up: shuffle THREE `<service>` siblings
    /// (the two WANIPConnection versions plus a decoy) and confirm
    /// [`reach_upnp::locate_control_url`] still finds `SERVICE_TYPES[0]`
    /// first, regardless of which position it shuffled into.
    #[test]
    fn locate_control_url_is_order_independent_under_shuffle(seed in prop::collection::vec(any::<u8>(), 3)) {
        let services = [
            ("urn:schemas-upnp-org:service:Layer3Forwarding:1", "/upnp/l3f"),
            (SERVICE_TYPES[0], "/upnp/control"),
            (SERVICE_TYPES[1], "/upnp/control2"),
        ];
        let mut indexed: Vec<(u8, (&str, &str))> = seed.into_iter().zip(services).collect();
        indexed.sort_by_key(|(key, _)| *key);

        let mut xml = String::from("<root><device><serviceList>");
        for (_, (service_type, control_url)) in &indexed {
            xml.push_str(&format!(
                "<service><serviceType>{service_type}</serviceType><controlURL>{control_url}</controlURL></service>"
            ));
        }
        xml.push_str("</serviceList></device></root>");

        let root = reach_upnp::parse_xml(&xml).expect("a shuffled but well-formed description must parse");
        let (control_url, service_type) = reach_upnp::locate_control_url(&root, "http://192.168.1.1/desc.xml")
            .expect("the WANIPConnection:1 service must be found regardless of sibling order");
        prop_assert_eq!(control_url, "http://192.168.1.1/upnp/control");
        prop_assert_eq!(service_type, SERVICE_TYPES[0]);
    }
}

// ---------------------------------------------------------------------------
// The fallback: what the mapping keeper does when NAT-PMP says nothing
// ---------------------------------------------------------------------------

/// Serialize the tests that read or write the process-wide register
/// [`teamclaude_rs::peer::reach::external_socket`] reports from.
///
/// That register is one value per PROCESS, on purpose (its own doc says why:
/// the keeper thread and `Hello.addrs` are three call sites apart with no
/// channel between them), and `cargo test` runs the tests in this file as
/// threads of one process. So any two tests that grant a mapping publish to
/// the same cell: measured here as a test asserting on port 41236 reading the
/// sibling test's 41234. A lock rather than a per-test register, because the
/// production shape is the single register and a test-only second one would
/// stop measuring the thing that ships.
fn register_lock() -> &'static Mutex<()> {
    static LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Take [`register_lock`], ignoring a poisoning left by an unrelated failed
/// test: the value behind it is `()`, so there is nothing to be corrupted.
fn hold_register() -> std::sync::MutexGuard<'static, ()> {
    register_lock()
        .lock()
        .unwrap_or_else(|held| held.into_inner())
}

/// A NAT-PMP gateway address nobody answers from, held open so the OS cannot
/// recycle the port mid-test.
fn silent_natpmp() -> (SocketAddr, UdpSocket) {
    let socket = UdpSocket::bind("127.0.0.1:0").expect("bind a silent NAT-PMP gateway");
    let addr = socket.local_addr().expect("its own address");
    (addr, socket)
}

/// **The gate for the fallback**: NAT-PMP silent and UPnP answering, and the
/// keeper comes back with a mapping.
///
/// The UPnP client had no production caller at all before this: `rg reach_upnp
/// src` outside its own file hit `pub mod` and nothing else, so a router that
/// speaks only UPnP left `tcr peer internet on` with no mapping and no reason
/// an operator could read.
///
/// The step list is what the assertion rests on rather than the mapping alone,
/// because which protocol answered is the fact that separates this from a
/// NAT-PMP success, and a `Mapped` here would mean the fallback never ran.
///
/// Watched red by removing the `Err(ReachError::Silent { .. })` arm from
/// `MappingKeeper::map`: the keeper returns the silent error and never asks
/// UPnP anything.
#[test]
fn natpmp_silence_falls_through_to_upnp_and_maps() {
    use teamclaude_rs::peer::reach::{MappingKeeper, MappingStep, NatPmp};

    // `map_over_upnp` publishes to the process-wide register, which a sibling
    // test asserts on.
    let _register = hold_register();

    let fake = FakeDevice::start(AddBehaviour::Confirm);
    let (silent, _keep_alive) = silent_natpmp();

    let mut keeper = MappingKeeper::new(NatPmp::at(silent), 41_234, 600)
        .with_upnp(Discoverer::at(fake.ssdp_addr));
    let mapping = keeper
        .map()
        .expect("NAT-PMP says nothing and the UPnP fake answers, so something must map");

    assert_eq!(
        keeper.steps(),
        &[MappingStep::MappedOverUpnp],
        "the step list has to name WHICH protocol mapped, or one router looks like another"
    );
    assert_eq!(
        (mapping.internal_port, mapping.external_port),
        (41_234, 41_234),
        "UPnP IGD confirms the port asked for or refuses, so there is no other port it \
         could have granted"
    );
    assert_eq!(
        fake.log(),
        vec!["discover", "add", "external-ip"],
        "the fake has to have been discovered, asked, and asked for the address to \
         advertise, in that order, or this mapping came from somewhere else"
    );
}

/// **The other half of the gate**: both silent, and the verb says so.
///
/// `tcr peer reach` reports NAT-PMP's own words for every outcome, and with
/// the fallback in place "no mapping" has two causes an operator has to tell
/// apart: nobody answered at all, or a device answered and THIS NODE refused
/// it. This pins the first. The second keeps its own name by construction:
/// `map_over_upnp` carries `UpnpError`'s `Display` through, so
/// `UntrustedLocation`, `ControlUrlElsewhere` and `Redirected` reach the same
/// line as themselves.
///
/// Driven at the keeper rather than through the binary, because the verb's
/// NAT-PMP client is `on_default_gateway()` and the machine running this suite
/// has a real router: a test that shelled out would measure the operator's LAN.
///
/// Watched red by having `map_over_upnp` answer `Ok` on a discovery failure:
/// the keeper reports a mapping that nothing granted.
#[test]
fn both_protocols_silent_is_a_named_refusal_and_not_a_mapping() {
    use teamclaude_rs::peer::reach::{MappingKeeper, NatPmp};

    let (silent_pmp, _keep_pmp) = silent_natpmp();
    let (silent_upnp, _keep_upnp) = silent_natpmp();

    let mut keeper = MappingKeeper::new(NatPmp::at(silent_pmp), 41_235, 600)
        .with_upnp(Discoverer::at(silent_upnp));
    let refusal = keeper
        .map()
        .expect_err("nothing answered either protocol, so nothing can have been mapped");

    assert!(
        keeper.steps().is_empty(),
        "and no step was recorded, because a step list is what did happen: {:?}",
        keeper.steps()
    );
    assert!(
        matches!(
            refusal,
            teamclaude_rs::peer::reach::ReachError::Silent { .. }
        ),
        "both protocols silent is a SILENCE and not an unreadable answer: it is the one \
         outcome an operator fixes by looking at the router rather than at this Mac, and \
         `tcr peer reach` reads this variant to print \"router did not answer\": \
         {refusal:?}"
    );
    assert!(
        refusal.to_string().contains("did not answer"),
        "and it says so in its own words: {refusal}"
    );
}

/// **The gate for the keeper's protocol routing**: a mapping granted over UPnP
/// is advertised from the UPnP gateway's answer, renewed over UPnP, and
/// deleted over UPnP at shutdown.
///
/// # The three bugs this is the control for
///
/// `run_mapping_with_upnp` used to publish `external_socket_for(&client, ..)`
/// straight after `keeper.map()`, with `client` being the NAT-PMP client that
/// had just been SILENT: the fallback published the router's real address from
/// inside `map_over_upnp` and this line immediately wrote `None` over it, so a
/// node whose router speaks only UPnP mapped a port and advertised nothing.
///
/// `renew` and `delete` then spoke NAT-PMP unconditionally. The renewal asked
/// the silent gateway, failed, was logged as a warning and the loop carried on,
/// so the mapping lapsed after its lifetime with peers still holding the
/// address; the delete asked the same silent gateway, so shutdown left a
/// forward standing on the router pointing at a listener that had stopped.
///
/// Driven through `run_mapping_with_upnp` rather than through `MappingKeeper`
/// directly, because two of the three faults were in the loop and not in the
/// keeper, and a test at the keeper would have passed with the loop still
/// writing `None`.
#[test]
fn a_upnp_mapping_is_published_renewed_and_deleted_over_upnp() {
    use std::sync::atomic::AtomicBool;
    use teamclaude_rs::peer::reach::{self, MappingStep, NatPmp};

    // The published address is asserted below, so no sibling test may be
    // publishing to the same register while this one runs.
    let _register = hold_register();

    let fake = FakeDevice::start(AddBehaviour::Confirm);
    let (silent, _keep_alive) = silent_natpmp();
    let discoverer = Discoverer::at(fake.ssdp_addr);
    let stop = Arc::new(AtomicBool::new(false));

    let running = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            reach::run_mapping_with_upnp(
                NatPmp::at(silent),
                Some(discoverer),
                41_236,
                600,
                Duration::from_millis(200),
                &stop,
            )
        })
    };

    // The register is SAMPLED rather than read once, and that is the whole
    // instrument for the overwrite: the old loop published the UPnP address
    // from inside `map_over_upnp` and then wrote `None` over it a moment later
    // from the silent NAT-PMP client, and the next renewal put the address
    // back. A single read a second in would have seen an address either way
    // and measured nothing. So: sample from before the mapping exists until a
    // renewal has run, and assert the register never went back to `None` once
    // it held an address.
    //
    // `203.0.113.9` is what the fake device answers `GetExternalIPAddress`
    // with; the port is the one asked for, since IGD confirms that port or
    // refuses.
    //
    // Keyed on THIS test's port, because the register is process-wide and
    // nothing clears it: a sibling test that mapped 41234 earlier leaves that
    // address sitting there, and a sample taken before this keeper has mapped
    // anything would otherwise read the sibling's answer as this one's.
    const MAPPED_PORT: u16 = 41_236;
    let mut published: Option<SocketAddr> = None;
    let mut withdrawn_after_publishing = false;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match (reach::external_socket(), published) {
            (Some(addr), None) if addr.port() == MAPPED_PORT => published = Some(addr),
            (Some(addr), Some(_)) if addr.port() != MAPPED_PORT => {
                withdrawn_after_publishing = true
            }
            (None, Some(_)) => withdrawn_after_publishing = true,
            _ => {}
        }
        // A second `add` is a renewal: IGD extends a lease by re-issuing
        // AddPortMapping with the same remote host, external port and protocol.
        let adds = fake.log().iter().filter(|call| *call == "add").count();
        if published.is_some() && adds >= 2 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "30 seconds and still no UPnP grant in the register and no renewal at the \
             gateway: published={published:?}, calls={:?}",
            fake.log()
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    assert_eq!(
        published.map(|addr| addr.to_string()),
        Some("203.0.113.9:41236".to_string()),
        "the advertised address must be the one the UPnP gateway named"
    );
    assert!(
        !withdrawn_after_publishing,
        "the register went back to None after the UPnP grant reached it: something \
         published the silent NAT-PMP client's absence of an answer over a live mapping"
    );

    stop.store(true, Ordering::SeqCst);
    let steps = running
        .join()
        .expect("the keeper thread")
        .expect("the UPnP fake granted the mapping, so the run must not have errored");

    assert_eq!(
        steps.first(),
        Some(&MappingStep::MappedOverUpnp),
        "the run has to have started from the UPnP fallback: {steps:?}"
    );
    assert!(
        steps.contains(&MappingStep::Renewed),
        "a renewal that reached the gateway has to be in the step list: {steps:?}"
    );
    assert_eq!(
        steps.last(),
        Some(&MappingStep::Deleted),
        "and the last thing a keeper does is take the mapping away: {steps:?}"
    );
    assert!(
        fake.log().contains(&"delete".to_string()),
        "the delete has to reach the gateway that granted the mapping; over NAT-PMP it \
         leaves a forward on the router pointing at a stopped listener: {:?}",
        fake.log()
    );
    assert_eq!(
        reach::external_socket(),
        None,
        "and nothing is advertised once the mapping is gone"
    );
}

/// **The gate for the body ceiling**: a device that answers with more than
/// [`reach_upnp::MAX_BODY_BYTES`] is refused by name, whether it declares the
/// size or streams it.
///
/// # What the four rules do not cover
///
/// The location rule, the control-URL authority rule, the redirect refusal and
/// `no_proxy` all bound WHERE this client can be sent. None of them decides
/// WHO answers: the first `M-SEARCH` reply on the link wins, and any host on
/// the LAN can be that. So the host that wins the race gets to serve the bytes,
/// and before this ceiling existed it could serve as many as it liked into a
/// `String` this node held.
///
/// Two legs, because they catch different lies. The first answer declares its
/// length, which is refused before any body is read. The second declares
/// nothing and is sent chunked, which the declaration check cannot see at all:
/// only the read stops it.
///
/// Watch it fail: delete the chunk-loop ceiling in `reach_upnp::fetch` and the
/// streamed leg reads 512 KiB into memory and fails on the XML instead, with a
/// message about a parse rather than a size.
#[test]
fn a_description_above_the_body_ceiling_is_refused_by_name() {
    // The phrase each check writes, so the two are told apart: a declared
    // length is refused before the body is read at all, and a streamed answer
    // can only be stopped by the read. Without this, the read ceiling alone
    // passes both legs and the declaration check goes untested.
    for (behaviour, says) in [
        (
            DescriptionBehaviour::Oversized,
            "refused before reading the body",
        ),
        (
            DescriptionBehaviour::OversizedChunked,
            "the read stopped there",
        ),
    ] {
        let fake = FakeDevice::start_with(AddBehaviour::Confirm, behaviour);
        let outcome = UpnpClient::discover(&Discoverer::at(fake.ssdp_addr));
        let refusal = outcome.expect_err("a body above the ceiling cannot be accepted");
        assert!(
            matches!(refusal, UpnpError::UnreadableResponse { .. }),
            "{behaviour:?}: the refusal has to be this node's own, naming the ceiling, not a \
             parse error three layers later: {refusal:?}"
        );
        assert!(
            refusal
                .to_string()
                .contains(&reach_upnp::MAX_BODY_BYTES.to_string()),
            "{behaviour:?}: and it has to say what the ceiling is: {refusal}"
        );
        assert!(
            refusal.to_string().contains(says),
            "{behaviour:?}: and WHICH check stopped it, or one check covers for the other \
             and goes untested: expected {says:?} in {refusal}"
        );
    }
}

// ---------------------------------------------------------------------------
// A description that nests deeper than any router's
// ---------------------------------------------------------------------------

/// **A 10 000-deep description is refused, and this process is still alive to
/// say so.**
///
/// The parser recurses once per nesting level and the body it reads is
/// whatever host on the LAN won the SSDP race, up to
/// [`reach_upnp::MAX_BODY_BYTES`]. 256 KiB of `<a>` is tens of thousands of
/// levels, and a stack overflow is an abort: the mapping keeper runs on a
/// thread with the default stack, so the whole proxy dies, with no error
/// anywhere for the operator to read.
///
/// Watch it fail: remove the [`reach_upnp::MAX_XML_DEPTH`] check from
/// `Cursor::parse_element` and this test does not fail with an assertion, it
/// kills the test process with a stack overflow, which is exactly the
/// behaviour it exists to rule out.
#[test]
fn a_description_nested_deeper_than_any_router_is_refused_rather_than_followed() {
    let depth = 10_000;
    let mut doc = String::with_capacity(depth * 8);
    for _ in 0..depth {
        doc.push_str("<a>");
    }
    for _ in 0..depth {
        doc.push_str("</a>");
    }

    let refusal = reach_upnp::parse_xml(&doc).expect_err(
        "a description nested past the ceiling must be an error, not a followed recursion",
    );
    assert!(
        matches!(refusal, UpnpError::Malformed(_)),
        "the refusal is this parser's own: {refusal:?}"
    );
    assert!(
        refusal
            .to_string()
            .contains(&reach_upnp::MAX_XML_DEPTH.to_string()),
        "and it names the ceiling it hit: {refusal}"
    );

    // The ceiling is not so low that a real description trips it: a router's
    // own document is about six levels deep.
    let honest = "<root><device><deviceList><device><serviceList><service>\
                  <serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType>\
                  </service></serviceList></device></deviceList></device></root>";
    reach_upnp::parse_xml(honest).expect("a real description is nowhere near the ceiling");
}
