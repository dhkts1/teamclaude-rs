//! What this Mac can be reached on from off the LAN.
//!
//! Every test here runs against loopback or against a pure function. Nothing
//! reaches a real router, and nothing touches the running proxy: the NAT-PMP
//! tests stand a fake gateway on `127.0.0.1:0` and point the client at it, which
//! is the whole reason [`teamclaude_rs::peer::reach::NatPmp::at`] takes an
//! address instead of always finding its own.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use teamclaude_rs::peer::noise;
use teamclaude_rs::peer::reach::{
    self, MapProtocol, NatPmp, ReachError, ResultCode, GATEWAY_PORT, TRIES, VERSION,
};

/// A fake NAT-PMP gateway on loopback.
///
/// Answers the three opcodes the client sends and nothing else. `requests`
/// counts what arrived, which is what lets the retry test assert the RFC's
/// three sends rather than assert only that the call eventually failed.
struct FakeGateway {
    addr: SocketAddr,
    requests: Arc<AtomicU32>,
    /// One `(opcode, lifetime)` pair per request, in arrival order.
    ///
    /// The lifetime is what separates a map from a delete on the wire (RFC
    /// 6886 § 3.3 spells a delete as lifetime 0), and the order is the whole
    /// of item 1's gate: a keeper that renewed before it mapped, or never
    /// deleted, leaves a mapping table that looks the same either way.
    asked: Arc<Mutex<Vec<(u8, u32)>>>,
}

/// How the fake answers.
#[derive(Clone, Copy)]
enum Behaviour {
    /// Answer every opcode as a cooperative router would.
    Cooperative {
        /// The external port to hand back, which is deliberately NOT the
        /// internal port asked for: RFC 6886 § 3.3 allows a gateway to pick a
        /// different one, and a client that assumed its own number would
        /// advertise a port nothing listens behind.
        external_port: u16,
    },
    /// Answer every opcode with one result code.
    Refuse(u16),
    /// Read the request and never answer.
    Silent,
}

impl FakeGateway {
    /// Bind on loopback and serve `behaviour` until the process ends.
    fn start(behaviour: Behaviour) -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind the fake gateway on loopback");
        let addr = socket.local_addr().expect("the fake gateway's own address");
        let requests = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&requests);
        let asked = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&asked);

        std::thread::spawn(move || loop {
            let mut buffer = [0_u8; 32];
            let Ok((read, from)) = socket.recv_from(&mut buffer) else {
                return;
            };
            counter.fetch_add(1, Ordering::SeqCst);
            let request = &buffer[..read];
            if let (Some(opcode), Some(lifetime)) =
                (request.get(1).copied(), asked_lifetime(request))
            {
                log.lock().expect("the fake's log").push((opcode, lifetime));
            }
            if let Some(response) = answer(request, behaviour) {
                let _sent = socket.send_to(&response, from);
            }
        });

        Self {
            addr,
            requests,
            asked,
        }
    }

    /// What this gateway was asked, in order.
    fn asked(&self) -> Vec<(u8, u32)> {
        self.asked.lock().expect("the fake's log").clone()
    }
}

/// The lifetime field of a mapping request, or 0 for an opcode that has none.
fn asked_lifetime(request: &[u8]) -> Option<u32> {
    if request.get(1).copied() == Some(0) {
        // Opcode 0 asks for the external address and carries no lifetime.
        return Some(0);
    }
    Some(u32::from_be_bytes([
        *request.get(8)?,
        *request.get(9)?,
        *request.get(10)?,
        *request.get(11)?,
    ]))
}

/// One response for one request, or [`None`] for the silent gateway.
fn answer(request: &[u8], behaviour: Behaviour) -> Option<Vec<u8>> {
    let opcode = *request.get(1)?;
    let (code, external_port) = match behaviour {
        Behaviour::Silent => return None,
        Behaviour::Refuse(code) => (code, 0),
        Behaviour::Cooperative { external_port } => (0, external_port),
    };

    let mut out = vec![VERSION, opcode | 0x80];
    out.extend_from_slice(&code.to_be_bytes());
    // The epoch counter, bytes 4..8 of every response.
    out.extend_from_slice(&7_777_u32.to_be_bytes());
    if opcode == 0 {
        out.extend_from_slice(&Ipv4Addr::new(203, 0, 113, 7).octets());
        return Some(out);
    }

    let internal_port = u16::from_be_bytes([*request.get(4)?, *request.get(5)?]);
    let asked_lifetime = u32::from_be_bytes([
        *request.get(8)?,
        *request.get(9)?,
        *request.get(10)?,
        *request.get(11)?,
    ]);
    // A delete (lifetime 0) is confirmed with zeroes, as RFC 6886 § 3.3
    // requires; anything else gets the port this fake chose.
    let (granted_external, granted_lifetime) = if asked_lifetime == 0 {
        (0, 0)
    } else {
        (external_port, asked_lifetime)
    };
    out.extend_from_slice(&internal_port.to_be_bytes());
    out.extend_from_slice(&granted_external.to_be_bytes());
    out.extend_from_slice(&granted_lifetime.to_be_bytes());
    Some(out)
}

/// **The gate for item 1**: a fake responder answers the three opcodes, and the
/// client reads the GATEWAY's numbers back out of each one.
///
/// The external port the fake grants (`41_234`) is not the internal port asked
/// for (`3_456`), so an implementation that echoed its own request would fail
/// here rather than pass and advertise a dead port.
#[test]
fn the_three_opcodes_round_trip_against_a_fake_gateway() {
    let fake = FakeGateway::start(Behaviour::Cooperative {
        external_port: 41_234,
    });
    let client = NatPmp::at(fake.addr);

    let external = client
        .external_address()
        .expect("opcode 0 against a cooperative fake");
    assert_eq!(
        external.addr,
        Ipv4Addr::new(203, 0, 113, 7),
        "opcode 0 must report the gateway's external address"
    );
    assert_eq!(
        external.epoch_secs, 7_777,
        "bytes 4..8 are the epoch counter"
    );

    for protocol in [MapProtocol::Udp, MapProtocol::Tcp] {
        let mapping = client
            .map(protocol, 3_456, 3_456, 600)
            .unwrap_or_else(|err| panic!("mapping {} failed: {err}", protocol.label()));
        assert_eq!(mapping.protocol, protocol);
        assert_eq!(mapping.internal_port, 3_456);
        assert_eq!(
            mapping.external_port, 41_234,
            "the external port must come from the gateway, not from the request"
        );
        assert_eq!(mapping.lifetime_secs, 600);

        let renewed = client
            .renew(&mapping, 1_200)
            .unwrap_or_else(|err| panic!("renewing {} failed: {err}", protocol.label()));
        assert_eq!(
            renewed.external_port, mapping.external_port,
            "a renewal must not move the port every peer was told about"
        );
        assert_eq!(renewed.lifetime_secs, 1_200);

        let deleted = client
            .delete(protocol, 3_456)
            .unwrap_or_else(|err| panic!("deleting {} failed: {err}", protocol.label()));
        assert_eq!(deleted.external_port, 0);
        assert_eq!(deleted.lifetime_secs, 0);
    }

    assert_eq!(
        fake.requests.load(Ordering::SeqCst),
        7,
        "one request per call and no retries: one opcode 0, then map, renew and \
         delete for each of the two protocols"
    );
}

/// A refusal is a value. **The normal outcome on most routers**, so the one
/// thing it must never be is a panic or an `unwrap` on a zeroed field.
#[test]
fn a_refusal_is_an_error_value_naming_the_code() {
    let fake = FakeGateway::start(Behaviour::Refuse(2));
    let client = NatPmp::at(fake.addr);

    let refusal = client
        .external_address()
        .expect_err("a result code of 2 is not a success");
    assert!(
        matches!(refusal, ReachError::Refused(ResultCode::NotAuthorized)),
        "expected a typed NotAuthorized refusal, got {refusal:?}"
    );
    assert!(
        refusal.to_string().contains("port mapping is off"),
        "the refusal must say what an operator should change: {refusal}"
    );

    let unknown = NatPmp::at(FakeGateway::start(Behaviour::Refuse(9)).addr)
        .map(MapProtocol::Tcp, 1, 1, 60)
        .expect_err("a result code of 9 is not a success");
    assert!(
        matches!(unknown, ReachError::Refused(ResultCode::Unknown(9))),
        "an unregistered code is carried, not flattened: {unknown:?}"
    );
}

/// A silent gateway costs three sends with the RFC's doubling backoff, then
/// yields [`ReachError::Silent`].
///
/// Both halves matter. The count proves the retry loop ran rather than one send
/// happening to time out, and the elapsed floor proves the timeout DOUBLES: a
/// loop that kept 250 ms for all three tries would finish in 0.75 s and pass a
/// count-only assertion.
#[test]
fn a_silent_gateway_costs_three_tries_with_a_doubling_timeout() {
    let fake = FakeGateway::start(Behaviour::Silent);
    let client = NatPmp::at(fake.addr);

    let started = Instant::now();
    let outcome = client
        .external_address()
        .expect_err("a gateway that never answers cannot succeed");
    let elapsed = started.elapsed();

    match outcome {
        ReachError::Silent { gateway, tries } => {
            assert_eq!(gateway, fake.addr);
            assert_eq!(tries, TRIES, "RFC 6886 § 3.1, bounded at three sends here");
        }
        other => panic!("expected a Silent gateway, got {other:?}"),
    }
    assert_eq!(
        fake.requests.load(Ordering::SeqCst),
        TRIES,
        "the gateway must have been asked exactly three times"
    );
    assert!(
        elapsed >= Duration::from_millis(1_750),
        "250 + 500 + 1000 ms is the RFC's backoff; this took {elapsed:?}"
    );
}

/// Every way a reply can be well-formed enough to read and still be the wrong
/// reply. Driven through the pure header check, so each shape is one line.
#[test]
fn a_reply_for_something_else_is_refused_rather_than_read() {
    // A mapping response for the opcode that was actually sent: the control.
    let good = vec![
        VERSION, 0x81, 0, 0, 0, 0, 0, 1, 0x0d, 0x80, 0xa1, 0x12, 0, 0, 2, 0x58,
    ];
    reach::interpret(&good, 1, 16).expect("the positive control must pass");

    let short = good[..12].to_vec();
    assert!(
        matches!(
            reach::interpret(&short, 1, 16),
            Err(ReachError::Malformed(_))
        ),
        "a 12-byte mapping response is short by four"
    );

    let mut wrong_version = good.clone();
    wrong_version[0] = 1;
    assert!(
        matches!(
            reach::interpret(&wrong_version, 1, 16),
            Err(ReachError::Malformed(_))
        ),
        "version 1 does not define this layout"
    );

    // The answer to opcode 2 arriving where opcode 1's answer was expected,
    // which is what a late reply to an earlier request looks like.
    assert!(
        matches!(
            reach::interpret(&good, 2, 16),
            Err(ReachError::Malformed(_))
        ),
        "an answer carrying another opcode must not be read as this one's"
    );
}

/// The gateway is found from the route table, and the route table has three
/// shapes worth pinning.
///
/// The `route -n get default` case with NO `gateway:` line is the measured one:
/// on a Mac with a VPN up, the kernel's default route is a point-to-point
/// `utun` interface and carries no gateway address, so a client that only read
/// this tool would find nothing on a machine that has a perfectly good NAT
/// router on `en0`.
#[test]
fn the_route_table_parsers_find_the_gateway_and_skip_the_ones_that_are_not_one() {
    let with_gateway = "   route to: default\ndestination: default\n       mask: default\n \
                        gateway: 192.168.1.1\n  interface: en0\n";
    assert_eq!(
        reach::parse_route_get(with_gateway),
        Some(Ipv4Addr::new(192, 168, 1, 1))
    );

    let point_to_point =
        "   route to: default\ndestination: default\n       mask: default\n  interface: utun4\n";
    assert_eq!(
        reach::parse_route_get(point_to_point),
        None,
        "a point-to-point default route has no gateway address to find"
    );

    let netstat = "Routing tables\n\nInternet:\nDestination        Gateway            Flags\n\
                   default            link#24            UCSg\n\
                   default            192.168.1.1        UGScIg\n";
    assert_eq!(
        reach::parse_netstat_inet(netstat),
        Some(Ipv4Addr::new(192, 168, 1, 1)),
        "the link# row is not an address a UDP request can be sent to"
    );

    // Linux. The hex is in the HOST's byte order, so 0101A8C0 is 192.168.1.1;
    // read big-endian it would be 1.1.168.192, an address that looks fine and
    // points nowhere.
    let proc_route = "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\n\
                      eth0\t0000FEA9\t00000000\t0001\t0\t0\t1000\t0000FFFF\n\
                      eth0\t00000000\t0101A8C0\t0003\t0\t0\t100\t00000000\n";
    assert_eq!(
        reach::parse_proc_net_route(proc_route),
        Some(Ipv4Addr::new(192, 168, 1, 1))
    );
    assert_eq!(
        reach::parse_proc_net_route(
            "Iface\tDestination\tGateway\n\
                                     eth0\t0000FEA9\t00000000\t0001\n"
        ),
        None,
        "a table with no default route names no gateway"
    );
}

/// The port number is the RFC's, stated once.
#[test]
fn the_gateway_port_is_the_registered_one() {
    assert_eq!(GATEWAY_PORT, 5351, "RFC 6886 § 3");
    assert_eq!(VERSION, 0, "the only version RFC 6886 defines");
}

/// **The gate for item 2, first half**: which IPv6 addresses count as reachable
/// from the open internet.
///
/// A table rather than a range check, and the rejected rows are the point. The
/// one that matters most is `fd7a:115c:a1e0::/48`: that shape is what a VPN
/// hands this Mac, it looks like a global address, it answers a bind, and no
/// other machine on the internet can reach it. A filter written as "starts with
/// 2 or 3" would pass every row here by accident and still be wrong, which is
/// why the accepted rows include `3fff::`, inside global unicast but outside
/// `2000::/4`.
#[test]
fn only_a_globally_routable_v6_address_counts_as_one() {
    let reachable = [
        "2606:4700:4700::1111",
        "2a00:1450:4001:82f::200e",
        "3fff:1:2::5",
    ];
    for text in reachable {
        let addr: Ipv6Addr = text.parse().expect("a test address must parse");
        assert!(
            reach::is_global_v6(&addr),
            "{text} is global unicast and must be reported"
        );
    }

    let not_reachable = [
        ("::", "unspecified"),
        ("::1", "loopback"),
        ("ff02::1", "multicast"),
        (
            "fe80::1",
            "link-local, and its scope cannot be sent to a peer",
        ),
        (
            "fd7a:115c:a1e0::1",
            "unique-local: the VPN shape, unreachable from off the box",
        ),
        ("fc00::1", "unique-local, the other half of fc00::/7"),
        ("2001:db8::1", "reserved for documentation"),
        ("::ffff:192.0.2.1", "an IPv4 address wearing a v6 shape"),
        ("100::1", "the discard-only prefix"),
    ];
    for (text, why) in not_reachable {
        let addr: Ipv6Addr = text.parse().expect("a test address must parse");
        assert!(
            !reach::is_global_v6(&addr),
            "{text} must be rejected ({why})"
        );
    }
}

/// **The gate for item 2, second half**: the probe answers, and every address it
/// answers with survives the filter above.
///
/// An empty list is a pass, and on this machine it is the real answer: its only
/// non-link-local v6 addresses are two unique-local ones from VPN interfaces.
/// So the assertion is the one that holds either way, nothing unreachable may
/// appear in the list, plus the property that the probe is pure enough to give
/// the same answer twice.
#[test]
fn the_v6_probe_reports_only_addresses_that_pass_the_filter() {
    let found = reach::global_v6_addresses();
    for addr in &found {
        assert!(
            reach::is_global_v6(addr),
            "the probe reported {addr}, which its own filter rejects"
        );
    }
    assert_eq!(
        found,
        reach::global_v6_addresses(),
        "two probes one after the other must agree"
    );
    println!("global v6 addresses on this machine: {found:?}");
}

// ---------------------------------------------------------------------------
// Item 3: the time-derived port
// ---------------------------------------------------------------------------

/// A seeded xorshift64, so a failing row can be re-run.
///
/// "One thousand random secrets" with a real random source is a test that
/// fails once a month on somebody else's machine and cannot be reproduced.
/// This is the same statistical coverage with the seed written down.
struct Seeded(u64);

impl Seeded {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn secret(&mut self) -> [u8; 32] {
        let mut out = [0_u8; 32];
        for chunk in out.chunks_mut(8) {
            chunk.copy_from_slice(&self.next().to_be_bytes());
        }
        out
    }
}

/// **The gate for item 3, first half**: both ends of a real pairing derive the
/// same port for the same slot, with nothing sent about ports.
///
/// The handshake is driven in memory rather than over a socket, three `XX`
/// messages between two `snow` states, because the claim under test is that
/// the two sides' handshake hashes agree, and a socket adds nothing to that. If
/// this ever fails while `the_six_digit_code_binds_the_handshake_not_the_keys`
/// passes, the derivation is at fault and not the handshake.
#[test]
fn both_ends_of_a_pairing_derive_one_port_per_slot() {
    let (initiator_secret, _) = noise::generate_static().expect("a static keypair");
    let (responder_secret, _) = noise::generate_static().expect("a second static keypair");

    let mut dialling =
        noise::initiator_with_secret(&initiator_secret, noise::PATTERN_PAIR, None, None)
            .expect("an XX initiator");
    let mut accepting = noise::responder_with_secret(&responder_secret, noise::PATTERN_PAIR, &[])
        .expect("an XX responder");

    let mut wire = [0_u8; 1024];
    let mut payload = [0_u8; 1024];

    let one = dialling
        .write_message(&[0x11; 8], &mut wire)
        .expect("XX message 1");
    accepting
        .read_message(&wire[..one], &mut payload)
        .expect("the responder reads message 1");
    let two = accepting
        .write_message(&[], &mut wire)
        .expect("XX message 2");
    dialling
        .read_message(&wire[..two], &mut payload)
        .expect("the initiator reads message 2");
    let three = dialling
        .write_message(&[], &mut wire)
        .expect("XX message 3");
    accepting
        .read_message(&wire[..three], &mut payload)
        .expect("the responder reads message 3");

    assert!(
        dialling.is_handshake_finished() && accepting.is_handshake_finished(),
        "both sides must have completed the handshake before a hash is read off it"
    );

    let dialling_secret = reach::port_secret(dialling.get_handshake_hash());
    let accepting_secret = reach::port_secret(accepting.get_handshake_hash());
    assert_eq!(
        dialling_secret, accepting_secret,
        "the two ends of one handshake must derive one port secret; if this differs, no \
         amount of clock agreement will make them meet"
    );

    // Several slots, not one: a derivation that ignored the slot would pass a
    // single-slot check and then never rotate.
    let slots = [0_u64, 1, 57_812_345, u64::from(u32::MAX), u64::MAX - 1];
    for slot in slots {
        assert_eq!(
            reach::derived_port(&dialling_secret, slot),
            reach::derived_port(&accepting_secret, slot),
            "slot {slot} must give both ends the same port"
        );
    }
}

/// **The gate for item 3, second half**: a one-slot skew still meets, and the
/// window is exactly three slots wide.
///
/// The second assertion is the one with teeth. A check that accepted "any
/// port this secret ever derives" would pass the first assertion and have no
/// window at all, so the far slot has to be refused.
#[test]
fn a_one_slot_skew_still_meets_and_a_far_slot_does_not() {
    let mut seeded = Seeded(0x5eed_0000_0000_0003);
    let secret = seeded.secret();
    let slot = reach::current_slot(1_758_200_000);

    for skew in [-1_i64, 0, 1] {
        let theirs = slot.wrapping_add(skew as u64);
        let port = reach::derived_port(&secret, theirs);
        assert!(
            reach::port_is_accepted(&secret, slot, port),
            "a peer {skew} slots off derives {port}, which this end must still accept"
        );
    }

    assert_eq!(
        reach::accepted_ports(&secret, slot),
        [
            reach::derived_port(&secret, slot),
            reach::derived_port(&secret, slot - 1),
            reach::derived_port(&secret, slot + 1),
        ],
        "the window is the slot and its two neighbours, in that order"
    );
    for far in [slot - 2, slot + 2, slot + 100] {
        let port = reach::derived_port(&secret, far);
        assert!(
            !reach::port_is_accepted(&secret, slot, port),
            "slot {far} is outside the window; accepting its port {port} would mean there \
             is no window"
        );
    }
}

/// **The gate for item 3, third half**: one thousand secrets, and what the
/// derivation owes them.
///
/// Three properties in one pass over the same draw. Every port is inside the
/// declared range, because a port outside it is a bind failure or a collision
/// with a service somebody chose. Different pairs get different ports: with
/// 40000 buckets and 1000 draws the birthday estimate is about 12 collisions
/// (1000^2 / (2 * 40000)), so the floor is 950 distinct and a derivation that
/// keyed on anything constant would land far below it. And one secret's port
/// moves with the slot, which is the whole point of a time-derived port.
#[test]
fn a_thousand_secrets_spread_across_the_range_and_move_with_the_slot() {
    let mut seeded = Seeded(0x5eed_0000_0000_0001);
    let mut ports = Vec::with_capacity(1000);
    let slot = reach::current_slot(1_758_200_000);

    for _ in 0..1000 {
        let secret = seeded.secret();
        let port = reach::derived_port(&secret, slot);
        assert!(
            (reach::PORT_FLOOR..reach::PORT_CEILING).contains(&port),
            "{port} is outside {}..{}",
            reach::PORT_FLOOR,
            reach::PORT_CEILING
        );
        ports.push(port);
    }

    let mut distinct = ports.clone();
    distinct.sort_unstable();
    distinct.dedup();
    assert!(
        distinct.len() >= 950,
        "1000 secrets gave only {} distinct ports; the birthday estimate for 40000 buckets \
         is about 12 collisions",
        distinct.len()
    );

    let secret = seeded.secret();
    let mut over_slots: Vec<u16> = (0..1000)
        .map(|step| reach::derived_port(&secret, slot + step))
        .collect();
    over_slots.sort_unstable();
    over_slots.dedup();
    assert!(
        over_slots.len() >= 950,
        "one secret over 1000 consecutive slots gave only {} distinct ports; a derivation \
         that ignored the slot would give 1",
        over_slots.len()
    );
}

/// The three numbers the scheme is written down as. A change to any of them is
/// a change both ends must make on the same day, so it is asserted rather than
/// left to a reader of two files.
#[test]
fn the_port_windows_own_numbers() {
    assert_eq!(reach::SLOT_SECONDS, 30, "one slot, in seconds");
    assert_eq!(reach::PORT_FLOOR, 20_000);
    assert_eq!(
        reach::PORT_CEILING,
        60_000,
        "below the ephemeral range macOS allocates from"
    );
    assert_eq!(reach::current_slot(89), 2, "89 / 30");
    assert_eq!(
        reach::current_slot(90),
        3,
        "the boundary belongs to the later slot"
    );
}

// ---------------------------------------------------------------------------
// Item 4: `tcr peer reach`, through the shipped binary
// ---------------------------------------------------------------------------

/// **`tcr peer reach --json` reports the mapping a serving process holds, and
/// reports none when nothing holds one.**
///
/// The panel draws "reachable from anywhere until 18:04" off this key. It
/// could not, because the mapping lives in the memory of a thread inside a
/// running `tcr` and every other way to ask is a request to the router, which
/// REPLACES the lifetime of the mapping for that internal port (RFC 6886
/// § 3.3). A status read that shortened the thing it was reporting on is the
/// failure this record exists to avoid, and it is why the record is written by
/// the keeper and only read here.
///
/// Three cases, and the third is the one a plausible implementation gets
/// wrong. A record outlives the process that wrote it: a keeper killed with
/// `kill -9` renews nothing, and the record it left behind describes a mapping
/// the router has since dropped. So a passed `expiresAtMs` reads as no mapping
/// rather than as a mapping, and the absent case and the stale case are
/// asserted separately because one implementation passes the first and fails
/// the second.
///
/// Watched red by dropping the `.filter(|record| record.expires_at_ms >
/// now_ms)` from the read in `run_peer_reach`: the stale case then reports the
/// dead mapping. And by removing `"heldMapping"` from the payload: the live
/// case reads null.
#[test]
fn peer_reach_reports_the_mapping_a_serving_process_holds() {
    let dir = scratch("held");
    let peers = peers_file_with_one_peer(&dir, 41235);
    let state = dir.join("peer-state.json");

    let (out, err, ok) = run_reach(&peers, &["--json"]);
    assert!(ok, "tcr peer reach exited non-zero: {err}");
    let parsed: serde_json::Value =
        serde_json::from_str(out.trim()).unwrap_or_else(|why| panic!("invalid JSON: {why}\n{out}"));
    assert!(
        parsed["heldMapping"].is_null(),
        "nothing on this Mac holds a mapping, and the key says so rather than being \
         absent: {out}"
    );

    // Recorded the way a keeper records one, through the library's own writer,
    // so the file gets the 0600 the reader needs and the key the reader reads.
    let live = teamclaude_rs::peer::state::MappingRecord {
        external_address: Some("203.0.113.9:41235".to_string()),
        external_port: 41235,
        internal_port: 41235,
        expires_at_ms: teamclaude_rs::now_ms() + 120_000,
    };
    teamclaude_rs::peer::state::save_mapping(&state, Some(live.clone()))
        .expect("the record writes");

    let (out, err, ok) = run_reach(&peers, &["--json"]);
    assert!(ok, "tcr peer reach exited non-zero: {err}");
    let parsed: serde_json::Value =
        serde_json::from_str(out.trim()).unwrap_or_else(|why| panic!("invalid JSON: {why}\n{out}"));
    let held = &parsed["heldMapping"];
    assert_eq!(
        held["externalAddress"], "203.0.113.9:41235",
        "the socket a peer off this LAN can dial: {out}"
    );
    assert_eq!(held["externalPort"], 41235, "the port on the router: {out}");
    assert_eq!(
        held["internalPort"], 41235,
        "and the port on this Mac behind it: {out}"
    );
    assert_eq!(
        held["expiresAtMs"],
        serde_json::json!(live.expires_at_ms),
        "with the deadline the keeper recorded, unix milliseconds: {out}"
    );

    // The stale case: a keeper that died without deleting its mapping.
    teamclaude_rs::peer::state::save_mapping(
        &state,
        Some(teamclaude_rs::peer::state::MappingRecord {
            expires_at_ms: teamclaude_rs::now_ms() - 1,
            ..live
        }),
    )
    .expect("the stale record writes");

    let (out, err, ok) = run_reach(&peers, &["--json"]);
    assert!(ok, "tcr peer reach exited non-zero: {err}");
    let parsed: serde_json::Value =
        serde_json::from_str(out.trim()).unwrap_or_else(|why| panic!("invalid JSON: {why}\n{out}"));
    assert!(
        parsed["heldMapping"].is_null(),
        "a record whose deadline has passed is not a mapping: nothing renews it and the \
         router has already dropped it: {out}"
    );
}

/// **The address a peer says it sees this Mac at survives a restart: a session
/// in one process, and `tcr peer reach` in a NEW one prints it.**
///
/// A Mac behind a NAT cannot see its own public address and the peer that just
/// greeted it can, so this is the one fact about this machine that only
/// somebody else holds, and it is what a punch aims at. It lived in a
/// process-local register, so every restart went back to "not told" until each
/// pair spoke again, which is exactly the restart the derived port already
/// learned to survive (decision row 17).
///
/// The two processes are the whole point and this test really uses two: the
/// session is stood in for by its one writer, `config::observe_seen_address`,
/// which is what the Hello arms call, and the reader is the BINARY, spawned
/// fresh with an empty register of its own. A test that called
/// `observed_self_for` in this process would pass on a register that was never
/// restored from anything.
///
/// Watched red two ways: drop the `restore_from_peers` call from
/// `run_peer_reach` (the fresh process reads its own empty register and prints
/// "not told"), and drop the write in `observe_seen_address` (there is nothing
/// on the row to restore).
#[test]
fn the_address_a_peer_sees_this_mac_at_survives_a_restart() {
    let dir = scratch("seesus");
    let peers = peers_file_with_one_peer(&dir, 41236);
    let peer = tcr_peer_wire::PeerId([0x22; 32]);
    // The public address a peer would report: documentation space, never a
    // real one, and not a socket this test opens.
    let seen: std::net::SocketAddr = "203.0.113.77:41236".parse().expect("a literal address");

    let (out, err, ok) = run_reach(&peers, &["--json"]);
    assert!(ok, "tcr peer reach exited non-zero: {err}");
    let parsed: serde_json::Value =
        serde_json::from_str(out.trim()).unwrap_or_else(|why| panic!("invalid JSON: {why}\n{out}"));
    assert!(
        parsed["peers"][0]["seesUsAt"].is_null(),
        "nothing has greeted this Mac yet, so the key is null rather than absent: {out}"
    );

    // What a completed session does, through its one writer.
    assert!(
        teamclaude_rs::peer::config::observe_seen_address(&peers, &peer, seen, 1_700_000_000_000)
            .expect("the row takes the address"),
        "the first observation is a write"
    );
    assert!(
        !teamclaude_rs::peer::config::observe_seen_address(&peers, &peer, seen, 1_700_000_001_000)
            .expect("the row is read again"),
        "and the same address twice is not a second write, or every session in a boot \
         rewrites this file"
    );

    let (out, err, ok) = run_reach(&peers, &["--json"]);
    assert!(ok, "tcr peer reach exited non-zero: {err}");
    let parsed: serde_json::Value =
        serde_json::from_str(out.trim()).unwrap_or_else(|why| panic!("invalid JSON: {why}\n{out}"));
    assert_eq!(
        parsed["peers"][0]["seesUsAt"], "203.0.113.77:41236",
        "a process that has run no session at all reads it off the row: {out}"
    );
    assert!(
        parsed["peers"][0]["punchPossible"].is_boolean(),
        "and says whether the two could punch, which is a yes or a no and never an \
         absence: {out}"
    );

    let (text, err, ok) = run_reach(&peers, &[]);
    assert!(ok, "tcr peer reach exited non-zero: {err}");
    assert!(
        text.contains("sees-us-at: 203.0.113.77:41236"),
        "the greppable half carries it too, in the shape the rest of this verb \
         prints: {text}"
    );
}

/// A scratch directory of this test's own, so a sibling test binary running at
/// the same instant cannot read or write this one's peers file.
fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tcr-peer-reach-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the scratch dir");
    dir
}

/// A peers file with one pinned peer and a listener port, written as JSON text
/// rather than through `PeerFile`.
///
/// Text on purpose: this test is about the CLI's output, and going through the
/// config structs would make it fail to COMPILE every time a field is added
/// to those structs elsewhere, which is a different failure from the one this test exists to
/// report.
fn peers_file_with_one_peer(dir: &std::path::Path, listen_port: u16) -> std::path::PathBuf {
    let node = tcr_peer_wire::PeerId([0x22; 32]).to_wire();
    let body = format!(
        r#"{{
  "listen": "127.0.0.1:{listen_port}",
  "peers": [
    {{ "node": "{node}", "label": "studio-mac", "addedAt": 1 }}
  ]
}}"#
    );
    let path = dir.join("tcr-peers.json");
    std::fs::write(&path, body).expect("write the peers file");
    // The store refuses a file other accounts can read.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("0600 on the peers file");
    }
    path
}

/// Run `tcr peer reach` against a temp peers file: `(stdout, stderr, ok)`.
fn run_reach(peers: &std::path::Path, extra: &[&str]) -> (String, String, bool) {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_tcr"))
        .args(["peer", "reach"])
        .args(["--peers", peers.to_str().expect("a utf-8 path")])
        .args(extra)
        .output()
        .expect("spawn tcr peer reach");
    (
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.success(),
    )
}

/// **The gate for item 4**: the verb runs against a real peers file, reports
/// all three mechanisms, and asks the router for NOTHING unless told to.
///
/// The mapping assertion is the one with teeth. A NAT-PMP map request replaces
/// the lifetime of an existing mapping for the same internal port (RFC 6886
/// § 3.3), so a reachability probe that mapped by default would cut a live
/// mapping down to the probe's two minutes. `--map` is therefore opt-in, and
/// this asserts the default run says so instead of doing it.
///
/// Every router-dependent value is asserted as "present and named", never as a
/// particular address: this test has to pass on a machine with no gateway, a
/// gateway that refuses, and a gateway that answers.
#[test]
fn peer_reach_reports_the_three_mechanisms_and_maps_nothing_by_default() {
    let dir = scratch("cli");
    let peers = peers_file_with_one_peer(&dir, 41234);

    let (out, err, ok) = run_reach(&peers, &["--json"]);
    assert!(ok, "tcr peer reach exited non-zero: {err}");
    let parsed: serde_json::Value =
        serde_json::from_str(out.trim()).unwrap_or_else(|why| panic!("invalid JSON: {why}\n{out}"));

    assert!(
        parsed["ipv6"].is_array(),
        "the v6 list is always a list, empty included: {parsed}"
    );
    assert_eq!(
        parsed["listenPort"], 41234,
        "the port mapped and advertised is the configured listener's"
    );
    assert_eq!(parsed["slotSeconds"], 30);
    let slot = parsed["slot"].as_u64().expect("a slot number");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_secs();
    let here = reach::current_slot(now);
    assert!(
        slot == here || slot + 1 == here,
        "the verb reported slot {slot} and this test computed {here}; more than one slot \
         apart means the verb is not reading the clock"
    );
    assert_eq!(
        parsed["mapping"], "not asked for (pass --map)",
        "a run without --map must write nothing to the router"
    );
    assert!(
        parsed["gateway"].is_string() && parsed["externalAddress"].is_string(),
        "both router readings are reported, refusals included: {parsed}"
    );

    let peer = &parsed["peers"][0];
    assert_eq!(peer["label"], "studio-mac");
    assert!(
        peer["node"]
            .as_str()
            .is_some_and(|id| id.starts_with("tcr-")),
        "the row names the pinned peer by its short id: {peer}"
    );
    assert!(
        peer["derivedPort"].is_null(),
        "no port may be printed while the pair's secret is not stored: {peer}"
    );
    assert!(
        peer["derivedPortUnavailable"]
            .as_str()
            .is_some_and(|why| why.contains("not stored")),
        "and the reason must be in the output, not only in a doc comment: {peer}"
    );

    // The greppable half carries the same facts, since that is the half an
    // operator reads.
    let (text, err, ok) = run_reach(&peers, &[]);
    assert!(ok, "the text run exited non-zero: {err}");
    for needle in [
        "reach: ipv6:",
        "reach: gateway:",
        "reach: external-address:",
        "reach: mapping: not asked for (pass --map)",
        "reach: listen-port: 41234",
        "reach: slot:",
        "reach: peer: studio-mac",
    ] {
        assert!(
            text.contains(needle),
            "the text output is missing {needle:?}:\n{text}"
        );
    }
}

// ---------------------------------------------------------------------------
// Item 1: the mapping this node holds open while `peer.internet` is on
// ---------------------------------------------------------------------------

/// **The gate for item 1**: the fake responder sees map, renew, delete, in
/// that order, and the keeper says it did the same three things.
///
/// The renewal interval is 50 ms here and half an hour in production
/// ([`reach::MAPPING_RENEW_INTERVAL`]); it is a parameter of
/// [`reach::run_mapping`] for exactly this reason, so the ORDER can be
/// measured without waiting out the real one.
///
/// Both halves are asserted because either alone can pass while the feature is
/// broken: the keeper's own step list is what this node believes it did, and
/// the fake's log is what the router was actually asked for.
#[test]
fn the_keeper_maps_renews_and_deletes_in_that_order() {
    let fake = FakeGateway::start(Behaviour::Cooperative {
        external_port: 41_234,
    });
    let stop = Arc::new(AtomicBool::new(false));
    let stopper = Arc::clone(&stop);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(220));
        stopper.store(true, Ordering::SeqCst);
    });

    let steps = reach::run_mapping(
        NatPmp::at(fake.addr),
        3_456,
        600,
        Duration::from_millis(50),
        &stop,
    )
    .expect("the cooperative fake maps the port");

    assert_eq!(
        steps.first(),
        Some(&reach::MappingStep::Mapped),
        "the first thing a keeper does is map: {steps:?}"
    );
    assert_eq!(
        steps.last(),
        Some(&reach::MappingStep::Deleted),
        "and the last thing it does is delete it again: {steps:?}"
    );
    assert!(
        steps
            .iter()
            .filter(|step| **step == reach::MappingStep::Renewed)
            .count()
            >= 1,
        "with at least one renewal in between, over four renewal intervals: {steps:?}"
    );
    assert_eq!(
        steps
            .iter()
            .filter(|step| **step == reach::MappingStep::Mapped)
            .count(),
        1,
        "and exactly one map: a renewal that fell back to mapping would hide a keeper \
         whose first request never ran: {steps:?}"
    );

    // The same three facts, as the router saw them. Opcode 2 is TCP; a
    // lifetime of 0 is RFC 6886's delete.
    let asked: Vec<(u8, u32)> = fake
        .asked()
        .into_iter()
        .filter(|(opcode, _)| *opcode == 2)
        .collect();
    assert!(
        asked.len() >= 3,
        "the router should have seen a map, a renewal and a delete: {asked:?}"
    );
    assert_eq!(asked[0], (2, 600), "the first TCP request is the map");
    assert_eq!(
        asked[1],
        (2, 600),
        "the second is a renewal, which is the same request again (RFC 6886 § 3.3)"
    );
    assert_eq!(
        asked.last(),
        Some(&(2, 0)),
        "and the last is the lifetime-0 delete: {asked:?}"
    );
}

/// **The gate for item 1, second half**: `tcr peer internet off` writes the
/// flag and takes the mapping away.
///
/// The gateway is handed in rather than found, so this asserts against a fake
/// on loopback and never touches the machine's real router.
#[test]
fn internet_off_writes_the_flag_and_leaves_no_mapping() {
    let fake = FakeGateway::start(Behaviour::Cooperative {
        external_port: 41_234,
    });
    let dir = scratch("internet-off");
    let peers = peers_file_with_one_peer(&dir, 41_234);

    let on = reach::set_internet_via(&peers, true, Some(NatPmp::at(fake.addr)))
        .expect("turning the switch on");
    assert_eq!(
        on,
        reach::InternetSwitch::On {
            listen_port: Some(41_234)
        },
        "turning it on names the port that will be mapped and asks the router for nothing"
    );
    assert!(
        fake.asked().is_empty(),
        "turning it ON must not write to the router: the listener asks, at boot: {:?}",
        fake.asked()
    );
    let file = std::fs::read_to_string(&peers).expect("read the peers file back");
    assert!(
        file.contains("\"internet\": true"),
        "the flag is in the file an operator can read: {file}"
    );

    let off = reach::set_internet_via(&peers, false, Some(NatPmp::at(fake.addr)))
        .expect("turning the switch off");
    let reach::InternetSwitch::Off { deleted } = off else {
        panic!("off must report a delete, not {off:?}");
    };
    let deleted = deleted.expect("the cooperative fake confirms the delete");
    assert_eq!(
        deleted.external_port, 0,
        "RFC 6886 § 3.3 requires a delete to be confirmed with external port 0"
    );
    assert_eq!(deleted.lifetime_secs, 0, "and with lifetime 0");
    assert_eq!(
        fake.asked()
            .into_iter()
            .filter(|(opcode, _)| *opcode == 2)
            .collect::<Vec<_>>(),
        vec![(2, 0)],
        "the ONLY thing the router was asked for across both switches is the delete"
    );

    let file = std::fs::read_to_string(&peers).expect("read the peers file back");
    assert!(
        file.contains("\"internet\": false"),
        "and the flag is off again: {file}"
    );
}

// ---------------------------------------------------------------------------
// Item 2: the accept gate `peer.internet` turns on
// ---------------------------------------------------------------------------
//
// # Why three of the four addresses are driven by value
//
// macOS puts only `127.0.0.1`, `::1` and one link-local address on `lo0`
// (`tests/peer_abuse.rs` measured it: binding `127.0.0.2` fails with
// `EADDRNOTAVAIL`), and adding an alias needs root on a machine that is also
// serving a live proxy. So `10.0.0.7` and `2001:db8::1` cannot be dialled from
// here at all. They are driven through the address parameter
// `listener::serve_accepted` already takes, which is the same value
// `serve_connection` reads inside the accept loop and not a copy of it, and
// the wire-bytes half of the invariant is kept honest by one real-socket leg
// from the one address this box has. That is the split
// `tests/peer_pairing.rs:872-884` uses for the same reason.

use std::path::Path;

use teamclaude_rs::peer::config::{self as peer_config, PeerFile};
use teamclaude_rs::peer::id::NodeKey;
use teamclaude_rs::peer::listener::{self, InternetAdmission, SessionContext};
use teamclaude_rs::peer::noise::Handshake;

/// One Mac's files: a peers file, a state file beside it, a node key.
struct GateNode {
    dir: std::path::PathBuf,
    peers: std::path::PathBuf,
    state: std::path::PathBuf,
    key: NodeKey,
}

impl GateNode {
    /// A node whose `peer.internet` is `internet` and which pins nobody.
    fn new(tag: &str, internet: bool) -> Self {
        let dir = scratch(tag);
        let peers = dir.join("tcr-peers.json");
        let state = dir.join("peer-state.json");
        let file = PeerFile {
            internet,
            ..PeerFile::default()
        };
        peer_config::save(&peers, &file).expect("write the peers file");
        let key = NodeKey::load_or_mint(&dir).expect("mint a node key");
        Self {
            dir,
            peers,
            state,
            key,
        }
    }

    fn context(&self) -> SessionContext {
        SessionContext::new(&self.key, &self.peers, &self.state)
    }

    fn config_dir(&self) -> &Path {
        &self.dir
    }
}

/// An `NN` message 1, built exactly as `noise::send_knock` builds its own.
fn knock_message_1() -> Vec<u8> {
    let throwaway = noise::random_secret().expect("a throwaway secret");
    let mut state = noise::initiator_with_secret(&throwaway, noise::PATTERN_KNOCK, None, None)
        .expect("a knock initiator");
    let mut scratch = vec![0_u8; 1024];
    let len = state
        .write_message(&[], &mut scratch)
        .expect("knock message 1");
    scratch[..len].to_vec()
}

/// An `IK` message 1 from a Mac this node has never pinned, aimed at this
/// node's real static key.
///
/// A real handshake and not a 96-byte filler: what this gate has to show is
/// that an `IK` from off the LAN reaches the PIN CHECK, and a frame that could
/// not be decrypted would be refused a step earlier for a reason that has
/// nothing to do with the gate.
fn ik_message_1_at(responder: &NodeKey) -> Vec<u8> {
    let (secret, _public) = noise::generate_static().expect("a stranger's keypair");
    let remote = responder.id().0;
    let mut state =
        noise::initiator_with_secret(&secret, noise::PATTERN_RETURN, Some(&remote), None)
            .expect("an IK initiator");
    let mut scratch = vec![0_u8; 1024];
    let len = state
        .write_message(&[], &mut scratch)
        .expect("IK message 1");
    scratch[..len].to_vec()
}

/// Hand `message_1` to the shipped connection handler as if it had been
/// accepted from `from` on a listener bound to `bind`, and report `(what the
/// handler said, bytes written back)`.
///
/// An in-memory duplex rather than a socket, because `from` is the whole point
/// and this box has one address. Everything under the call is the shipped
/// code: `listener::serve_accepted` is `serve_connection`, which is what the
/// accept loop runs. `bind` is likewise named rather than really bound, the
/// same reason: this box cannot bind `0.0.0.0` in a test that also wants a
/// scratch port free of every sibling test binary's own listeners.
async fn offer_from(
    context: SessionContext,
    from: SocketAddr,
    bind: SocketAddr,
    message_1: &[u8],
) -> (Result<(), String>, usize) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let (theirs, ours) = tokio::io::duplex(8192);
    let served =
        tokio::spawn(async move { listener::serve_accepted(ours, &context, &from, bind).await });

    let mut theirs = theirs;
    let mut framed = Vec::with_capacity(2 + message_1.len());
    framed.extend_from_slice(
        &u16::try_from(message_1.len())
            .expect("a short frame")
            .to_be_bytes(),
    );
    framed.extend_from_slice(message_1);
    theirs.write_all(&framed).await.expect("write message 1");
    theirs.flush().await.expect("flush message 1");

    let mut back = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), theirs.read_to_end(&mut back)).await;
    let outcome = match served.await.expect("the handler task") {
        Ok(()) => Ok(()),
        Err(err) => Err(format!("{err:#}")),
    };
    (outcome, back.len())
}

/// **The gate for item 2, the allow list itself.**
///
/// Every prefix decision row 14 names is asserted as LAN, and the addresses
/// this repository uses for documentation are asserted as not-LAN. The second
/// half is the one with teeth: a gate written as a negation would call
/// `2001:db8::1` LAN the day a new prefix is assigned, and this is what says
/// it does not.
#[test]
fn lan_scope_is_the_allow_list_row_14_names() {
    for lan in [
        "127.0.0.1",
        "127.9.9.9",
        "::1",
        "10.0.0.7",
        "172.16.0.1",
        "172.31.255.254",
        "192.168.1.4",
        "fe80::1",
        "fd00::1",
        "fc00::1",
        // A dual-stack listener reports a v4 peer as v4-mapped, and that is
        // the same LAN it would be reported as on a v4 socket.
        "::ffff:10.0.0.7",
    ] {
        let addr: IpAddr = lan.parse().expect("an address");
        assert!(
            listener::is_lan_scope(addr),
            "{lan} is on decision row 14's list and must read as LAN scope"
        );
    }

    for internet in [
        "203.0.113.7",
        "198.51.100.1",
        "192.0.2.1",
        "8.8.8.8",
        // Just outside each range whose edge is easy to get wrong.
        "172.15.255.255",
        "172.32.0.1",
        "100.63.255.255",
        "100.128.0.1",
        "169.253.0.1",
        // RFC 6598 carrier-grade NAT, which is what a tailnet is numbered out
        // of, and RFC 3927 link-local. Both used to read as LAN, which
        // admitted every tailnet node on earth to the knock path whatever the
        // internet switch said.
        "100.64.0.1",
        "100.127.255.254",
        "169.254.1.1",
        "169.254.169.254",
        "2001:db8::1",
        "2606:4700::1111",
    ] {
        let addr: IpAddr = internet.parse().expect("an address");
        assert!(
            !listener::is_lan_scope(addr),
            "{internet} is not on decision row 14's list and must read as off-LAN"
        );
    }
}

/// **The gate for the decision**: the old assertion here was `internet_admission`'s own
/// bug written down as a test: "with peer.internet off, anything from
/// anywhere is answered" is exactly `listen: 0.0.0.0:7755` becoming a
/// world-reachable socket on the loosest policy).
///
/// What decides row 14 now is the BIND, not the switch: a listener bound to a
/// LAN or loopback address answers everything, whatever the source, because
/// nothing off the LAN can reach it without a mapping this node did not make.
/// A listener bound to a globally routable address (`0.0.0.0` included) is
/// answerable from off the LAN by construction, so from there on only a
/// return visit or an enrolment is answered, independent of `peer.internet`,
/// which [`internet_admission`] reads for one narrowing case only, measured
/// on its own in
/// `a_neighbour_in_this_macs_own_v6_prefix_is_answered_with_the_switch_off`.
/// Every call here passes no prefixes and the switch off, which is the Mac
/// with no global IPv6 at all: the common case, and the one where the bind
/// class decides alone.
#[test]
fn only_ik_is_answered_from_off_the_lan_when_the_bind_is_globally_routable() {
    let off_lan: IpAddr = "2001:db8::1".parse().expect("a documentation address");
    let on_lan: IpAddr = "10.0.0.7".parse().expect("an RFC 1918 address");
    let loopback: IpAddr = "127.0.0.1".parse().expect("loopback");
    let lan_bind: IpAddr = "10.0.0.5".parse().expect("a private bind address");
    let global_bind: IpAddr = "0.0.0.0".parse().expect("an unspecified bind address");

    for pattern in [
        Handshake::Knock,
        Handshake::KnockPsk,
        Handshake::Pair,
        Handshake::Return,
        Handshake::Enrol,
    ] {
        assert_eq!(
            listener::internet_admission(lan_bind, off_lan, pattern, false, &[]),
            InternetAdmission::Answer,
            "bound to a LAN address, {pattern:?} from anywhere is answered: off the LAN \
             cannot reach this socket without a mapping this node did not make"
        );
        for lan in [on_lan, loopback] {
            assert_eq!(
                listener::internet_admission(global_bind, lan, pattern, false, &[]),
                InternetAdmission::Answer,
                "and on the LAN, {pattern:?} from {lan} is answered even on a globally \
                 routable bind"
            );
        }
    }

    for refused in [Handshake::Knock, Handshake::KnockPsk, Handshake::Pair] {
        assert_eq!(
            listener::internet_admission(global_bind, off_lan, refused, false, &[]),
            InternetAdmission::Refuse,
            "{refused:?} from off the LAN, on a globally routable bind, proves nothing this \
             node issued, so it gets nothing"
        );
    }
    for answered in [Handshake::Return, Handshake::Enrol] {
        assert_eq!(
            listener::internet_admission(global_bind, off_lan, answered, false, &[]),
            InternetAdmission::Answer,
            "{answered:?} carries something this node issued, so it reaches the pin check"
        );
    }
}

/// **The gate for item 2, on the wire.**
///
/// Four connections through the shipped handler, with the switch ON: a knock
/// from `127.0.0.1` and from `10.0.0.7` are answered, a knock from
/// `2001:db8::1` gets zero bytes, and an `IK` from `2001:db8::1` gets past the
/// gate and is refused by the pin check instead.
///
/// The last one is why the refusals are told apart by their REASON and not
/// only by their byte count: an unpinned `IK` writes nothing back either, so
/// "zero bytes" alone cannot tell "refused at the door" from "refused at the
/// pin check", and it is the second that proves the gate let `IK` through.
#[tokio::test(flavor = "multi_thread")]
async fn a_knock_from_off_the_lan_gets_zero_bytes_and_an_ik_reaches_the_pin_check() {
    // `peer.internet` is OFF, and that must no longer matter to what gets
    // answered; the bind below does.
    let node = GateNode::new("gate-on", false);
    let off_lan: SocketAddr = "[2001:db8::1]:44444"
        .parse()
        .expect("a documentation socket");
    let on_lan: SocketAddr = "10.0.0.7:44444".parse().expect("an RFC 1918 socket");
    let loopback: SocketAddr = "127.0.0.1:44444".parse().expect("a loopback socket");
    let global_bind: SocketAddr = "0.0.0.0:7755".parse().expect("an unspecified bind address");

    for lan in [loopback, on_lan] {
        let (_outcome, written) =
            offer_from(node.context(), lan, global_bind, &knock_message_1()).await;
        assert!(
            written > 0,
            "a knock from {lan} is on the LAN and must be answered, whatever the bind"
        );
    }

    let (outcome, written) =
        offer_from(node.context(), off_lan, global_bind, &knock_message_1()).await;
    assert_eq!(
        written, 0,
        "a knock from off the LAN, on a globally routable bind, must get zero bytes back, not \
         a refusal it can read"
    );
    let refusal = outcome.expect_err("a refused knock is an error, not a silent success");
    assert!(
        refusal.contains("globally routable"),
        "and the refusal must name the gate that made it: {refusal}"
    );

    let (outcome, written) = offer_from(
        node.context(),
        off_lan,
        global_bind,
        &ik_message_1_at(&NodeKey::load_or_mint(node.config_dir()).expect("the node's own key")),
    )
    .await;
    assert_eq!(
        written, 0,
        "an unpinned IK writes nothing back either, which is why the reason matters below"
    );
    let refusal = outcome.expect_err("an unpinned IK is refused");
    assert!(
        !refusal.contains("globally routable"),
        "an IK from off the LAN must reach the pin check and be refused THERE, not at the \
         internet gate: {refusal}"
    );
}

/// **The control**: the bug the fix above closes was
/// exactly the assertion this test used to make: "peer.internet is off, so
/// nothing about the source address may change the answer" was true of the
/// SWITCH and false of the SOCKET: a bind that cannot be reached from off the
/// LAN (loopback here) answers a knock from `2001:db8::1` regardless of the
/// switch, because nothing about row 14 needs to apply to a socket a stranger
/// cannot reach in the first place. Watch this fail by reverting
/// `internet_admission` to read `bind` as `is_lan_scope(bind) == false`
/// unconditionally: the knock below would then be refused even on loopback.
#[tokio::test(flavor = "multi_thread")]
async fn a_lan_scoped_bind_answers_the_same_knock_from_off_the_lan_regardless_of_the_switch() {
    let node = GateNode::new("gate-off", false);
    let off_lan: SocketAddr = "[2001:db8::1]:44444"
        .parse()
        .expect("a documentation socket");
    let loopback_bind: SocketAddr = "127.0.0.1:7755".parse().expect("a loopback bind address");

    let (_outcome, written) =
        offer_from(node.context(), off_lan, loopback_bind, &knock_message_1()).await;
    assert!(
        written > 0,
        "a bind that off the LAN cannot reach answers everything, switch or no switch"
    );
}

/// **The real-socket leg**: the accept loop this node actually runs, on the one
/// address this box can dial, with the switch on.
///
/// Everything above hands the source address in by value. This one does not:
/// it binds `127.0.0.1:0`, runs the shipped `serve_on_with`, and dials it, so
/// the gate is proved to be reached through a real accept at least once.
#[tokio::test(flavor = "multi_thread")]
async fn a_real_loopback_knock_is_answered_through_the_accept_loop() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let node = GateNode::new("gate-socket", true);
    let bound = listener::bind("127.0.0.1:0".parse().expect("a loopback address"))
        .await
        .expect("bind a kernel-chosen port on loopback");
    let addr = bound.local_addr().expect("the bound address");
    let context = node.context();
    tokio::spawn(async move {
        let _ = listener::serve_on_with(bound, context).await;
    });

    let message_1 = knock_message_1();
    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .expect("dial the listener");
    let mut framed = Vec::with_capacity(2 + message_1.len());
    framed.extend_from_slice(
        &u16::try_from(message_1.len())
            .expect("a short frame")
            .to_be_bytes(),
    );
    framed.extend_from_slice(&message_1);
    stream.write_all(&framed).await.expect("write message 1");
    stream.flush().await.expect("flush message 1");

    let mut back = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut back)).await;
    assert!(
        !back.is_empty(),
        "a knock from 127.0.0.1 is on the LAN and must be answered through the real accept \
         loop with peer.internet on"
    );
}

// ---------------------------------------------------------------------------
// Item 3: what a dialler tries when the recorded port stops answering
// ---------------------------------------------------------------------------

/// A pinned row with one recorded endpoint and nothing else.
fn row_at(node: tcr_peer_wire::PeerId, recorded: SocketAddr) -> peer_config::PeerRow {
    let mut row = peer_config::PeerRow {
        node,
        label: "studio-mac".to_string(),
        endpoints: Vec::new(),
        added_at: 1,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: peer_config::Allow::default(),
        lend: Vec::new(),
    };
    row.observe_endpoint(peer_config::Endpoint::direct(
        recorded,
        1_000,
        peer_config::EndpointSource::Paired,
    ));
    row
}

/// A loopback port nothing is listening on: bound, read back, released.
///
/// Not a hard-coded number, because a hard-coded "surely nothing is here"
/// port is the kind of thing that passes for a year and then collides with
/// whatever another test started.
async fn a_closed_loopback_port() -> SocketAddr {
    let held = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a kernel-chosen loopback port");
    let addr = held.local_addr().expect("its address");
    drop(held);
    addr
}

/// Bind a loopback listener on the FIRST of `ports` the kernel will give.
///
/// The derived ports are real numbers in 20000..60000 and this machine runs
/// other things, so a test that insisted on one exact port would be a flake
/// with a security-shaped failure message. Which of the three it got is
/// returned, because that is what the assertion is about.
async fn bind_one_of(ports: &[u16]) -> (tokio::net::TcpListener, u16) {
    for port in ports {
        if let Ok(bound) = tokio::net::TcpListener::bind(("127.0.0.1", *port)).await {
            return (bound, *port);
        }
    }
    panic!("none of {ports:?} could be bound on loopback");
}

/// **The gate for item 3**: the recorded port is dead, and the dial lands on
/// the pair's rendezvous port for the current slot.
///
/// The row's only endpoint points at a closed port, so a dialler that tried
/// the recorded endpoints and stopped returns nothing here. What makes the
/// second attempt possible is the pair's port secret, which both ends compute
/// from a handshake they both ran and neither one sends.
#[tokio::test(flavor = "multi_thread")]
async fn a_dead_recorded_port_falls_through_to_the_pairs_rendezvous_port() {
    let node = tcr_peer_wire::PeerId([0x31; 32]);
    let secret = [0x5a_u8; 32];
    reach::remember_port_secret(node, secret);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_secs();
    let ports = reach::accepted_ports(&secret, reach::current_slot(now));
    // The CURRENT slot's port, which is `accepted_ports`'s first entry. The
    // two neighbours are the skew case and get their own test below.
    let (listening, port) = bind_one_of(&ports[..1]).await;
    let accepting = tokio::spawn(async move { listening.accept().await.map(|(_, from)| from) });

    let dead = a_closed_loopback_port().await;
    let row = row_at(node, dead);
    let reached = teamclaude_rs::peer::serve::dial_peer_within(&row, 3_000)
        .await
        .map(|(addr, _stream)| addr);

    assert_eq!(
        reached,
        Some(SocketAddr::new(dead.ip(), port)),
        "the recorded port {} is closed, so the dial must land on this pair's rendezvous \
         port {port}",
        dead.port()
    );
    accepting
        .await
        .expect("the accept task")
        .expect("the listener accepted the dial");
}

/// **The gate for item 3, the skew half**: a listener one slot behind is still
/// reached.
///
/// `accepted_ports` is current, previous, next, so a peer whose clock or whose
/// bind landed in the slot before this one is on the list. Without the two
/// neighbours a dial that crossed a slot boundary between deriving the port
/// and connecting would miss by one, every thirty seconds, for no reason the
/// operator could see.
#[tokio::test(flavor = "multi_thread")]
async fn one_slot_of_skew_still_meets() {
    let node = tcr_peer_wire::PeerId([0x32; 32]);
    let secret = [0x77_u8; 32];
    reach::remember_port_secret(node, secret);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_secs();
    let slot = reach::current_slot(now);
    let previous = reach::derived_port(&secret, slot - 1);
    let (listening, port) = bind_one_of(&[previous]).await;
    let accepting = tokio::spawn(async move { listening.accept().await.map(|(_, from)| from) });

    let dead = a_closed_loopback_port().await;
    let row = row_at(node, dead);
    let reached = teamclaude_rs::peer::serve::dial_peer_within(&row, 3_000)
        .await
        .map(|(addr, _stream)| addr);

    assert_eq!(
        reached,
        Some(SocketAddr::new(dead.ip(), port)),
        "a peer listening on the PREVIOUS slot's port is one slot of skew away and must \
         still be reached"
    );
    accepting
        .await
        .expect("the accept task")
        .expect("the listener accepted the dial");
}

/// The control: with no port secret for this pair, the dial tries the recorded
/// endpoints and stops.
///
/// Without this, a dialler that scanned ports would pass both tests above and
/// nobody would know. It also pins the floor `reach::port_secrets` states: a
/// process holding no secret for a pair, from a session or from the peers
/// file, has exactly the reach it had before derived ports existed.
#[tokio::test(flavor = "multi_thread")]
async fn with_no_secret_for_the_pair_nothing_but_the_recorded_endpoint_is_tried() {
    let node = tcr_peer_wire::PeerId([0x33; 32]);
    let secret = [0x11_u8; 32];
    // Deliberately NOT registered against `node`.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_secs();
    let ports = reach::accepted_ports(&secret, reach::current_slot(now));
    let (listening, _port) = bind_one_of(&ports[..1]).await;
    let _held = listening;

    let dead = a_closed_loopback_port().await;
    let row = row_at(node, dead);
    let reached = teamclaude_rs::peer::serve::dial_peer_within(&row, 900)
        .await
        .map(|(addr, _stream)| addr);

    assert_eq!(
        reached,
        None,
        "this process holds no secret for {}, so there are no rendezvous ports to try and \
         the dial must give up after the recorded endpoint",
        node.display()
    );
    assert!(
        reach::rendezvous_ports(&node, now).is_empty(),
        "and the port list itself must be empty, not merely unreachable"
    );
}

/// Run `tcr peer internet on|off` against a temp peers file: `(stdout, stderr, ok)`.
fn run_internet(peers: &std::path::Path, state: &str) -> (String, String, bool) {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_tcr"))
        .args(["peer", "internet", state])
        .args(["--peers", peers.to_str().expect("a utf-8 path")])
        .output()
        .expect("spawn tcr peer internet");
    (
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.success(),
    )
}

/// **The gate for the `tcr peer internet on|off` arm**: the switch is the one
/// thing between a Mac that answers only its own LAN and one a pinned key can
/// reach from anywhere, so the verb has to move the flag in the FILE, which is
/// what both the mapping keeper and the accept gate read.
///
/// Asserted on the file's own bytes rather than through `PeerFile`, for the
/// reason `peers_file_with_one_peer` gives: a field added to that struct
/// elsewhere must not turn this into a compile error.
///
/// The `off` leg asserts the flag and the wording, never a deleted mapping: a
/// machine running this suite may have no gateway at all, and the verb says so
/// either way.
#[test]
fn peer_internet_on_then_off_moves_the_flag_in_the_peers_file() {
    let dir = scratch("cli-internet");
    let peers = peers_file_with_one_peer(&dir, 41567);

    let raw = std::fs::read_to_string(&peers).expect("the fixture reads");
    assert!(
        !raw.contains("\"internet\""),
        "the fixture starts with no internet key at all, so a default of false is what \
         the first run has to change: {raw}"
    );

    let (out, err, ok) = run_internet(&peers, "on");
    assert!(ok, "tcr peer internet on exited non-zero: {err}");
    assert!(
        out.contains("peer.internet: on") && out.contains("41567"),
        "the on run names the port it will map, so an operator can see WHICH socket \
         goes through the router: {out}"
    );
    let raw = std::fs::read_to_string(&peers).expect("the peers file reads after on");
    assert!(
        raw.contains("\"internet\": true"),
        "on has to reach the file the keeper and the accept gate read: {raw}"
    );

    let (out, err, ok) = run_internet(&peers, "off");
    assert!(ok, "tcr peer internet off exited non-zero: {err}");
    assert!(
        out.contains("peer.internet: off"),
        "the off run says so plainly, with or without a gateway to unmap at: {out}"
    );
    let raw = std::fs::read_to_string(&peers).expect("the peers file reads after off");
    assert!(
        !raw.contains("\"internet\": true"),
        "off has to put the flag back, or a Mac stays reachable off its LAN after \
         the operator said stop: {raw}"
    );
}

/// **The gate for the rendezvous secret on disk**: a process that has
/// completed no session reaches a peer on the pair's derived port, because a
/// previous boot wrote the secret to the peers file.
///
/// Three properties in one test, because they are one mechanism.
///
/// 1. What is written is the DERIVED secret and never the handshake hash it
///    came from (decision row 17). The hash's own hex is asserted absent from
///    the file bytes, with the secret's hex present in the same read as the
///    positive control: an absence assertion against a file that was never
///    written would pass on its own.
/// 2. `restore_from_peers` puts it in the register, which is what a boot does.
/// 3. The dial then lands on the pair's port for this slot, with the recorded
///    endpoint dead, in a process where no handshake has run.
///
/// Watched red by making `observe_rendezvous_secret` return `Ok(false)`
/// without writing: the restore counts 0 and the dial reaches `None`.
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_reaches_a_peer_on_the_derived_port_from_the_peers_file() {
    use teamclaude_rs::peer::config;

    let node = tcr_peer_wire::PeerId([0x44; 32]);
    // Stands in for a completed handshake: the value never leaves this test,
    // and what matters is that the FILE holds something else.
    let handshake_hash = [0x7e_u8; 48];
    let secret = reach::port_secret(&handshake_hash);
    let hash_hex: String = handshake_hash.iter().map(|b| format!("{b:02x}")).collect();
    let secret_hex: String = secret.iter().map(|b| format!("{b:02x}")).collect();

    assert!(
        reach::port_secret_for(&node).is_none(),
        "the control: this process has completed no session with that key, which is the \
         state a restart leaves behind"
    );

    let dir = scratch("restart-rendezvous");
    let peers = dir.join("tcr-peers.json");
    std::fs::write(
        &peers,
        format!(
            r#"{{
  "peers": [
    {{ "node": "{node_wire}", "label": "studio-mac", "addedAt": 1 }}
  ]
}}"#,
            node_wire = node.to_wire()
        ),
    )
    .expect("write the peers file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&peers, std::fs::Permissions::from_mode(0o600))
            .expect("0600 on the peers file");
    }

    assert!(
        config::observe_rendezvous_secret(&peers, &node, secret).expect("the write runs"),
        "a pinned row takes the secret"
    );

    let raw = std::fs::read_to_string(&peers).expect("the peers file reads");
    assert!(
        raw.contains(&secret_hex),
        "the positive control: the derived secret IS in the file, so the absence below \
         is about what was written and not about an empty file"
    );
    assert!(
        !raw.contains(&hash_hex),
        "and the handshake hash is NOT, because it is what the six-digit pairing compare \
         is built from and it stays in memory: {raw}"
    );

    let file = config::read_or_default(&peers).expect("the peers file parses back");
    assert_eq!(
        reach::restore_from_peers(&file),
        1,
        "a boot restores the one row that carries a secret"
    );
    assert_eq!(
        reach::port_secret_for(&node),
        Some(secret),
        "and the register holds exactly what the file did, byte for byte"
    );

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_secs();
    let ports = reach::accepted_ports(&secret, reach::current_slot(now));
    let (listening, port) = bind_one_of(&ports[..1]).await;
    let accepting = tokio::spawn(async move { listening.accept().await.map(|(_, from)| from) });

    let dead = a_closed_loopback_port().await;
    let row = row_at(node, dead);
    let reached = teamclaude_rs::peer::serve::dial_peer_within(&row, 3_000)
        .await
        .map(|(addr, _stream)| addr);

    assert_eq!(
        reached,
        Some(SocketAddr::new(dead.ip(), port)),
        "the recorded port {} is dead and no session has run in this process, so the only \
         thing that can reach this peer is the secret the last boot wrote down",
        dead.port()
    );
    accepting
        .await
        .expect("the accept task")
        .expect("the listener accepted the dial");
}

/// **Forgetting a Mac takes its rendezvous secret away too.**
///
/// `tcr peer forget` drops the row, and until this fix the process kept that
/// pair's port secret: the peers file no longer named the Mac and a dialler in
/// the same process would still have computed its three accepted ports for as
/// long as `tcr` ran. Forgetting has to mean every way of reaching it, not the
/// ways written down.
///
/// The row's own copy goes with the row, because the row is what held it. This
/// asserts both halves: the register is empty afterwards and the saved file
/// holds no `rendezvousSecret` for that key.
///
/// The `is_some` before the forget is the positive control: without it, an
/// assertion that the register is empty afterwards would pass against a
/// register that was never filled.
///
/// Watched red by dropping the `forget_port_secret` call from `pair::forget`:
/// the first assertion still reads the secret after the peer is forgotten.
#[test]
fn forgetting_a_peer_clears_its_rendezvous_secret() {
    use teamclaude_rs::peer::config;

    let node = tcr_peer_wire::PeerId([0x66; 32]);
    let secret = reach::port_secret(&[0x1d_u8; 48]);

    let dir = scratch("forget-rendezvous");
    let peers = dir.join("tcr-peers.json");
    std::fs::write(
        &peers,
        format!(
            r#"{{
  "peers": [
    {{ "node": "{node_wire}", "label": "studio-mac", "addedAt": 1 }}
  ]
}}"#,
            node_wire = node.to_wire()
        ),
    )
    .expect("write the peers file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&peers, std::fs::Permissions::from_mode(0o600))
            .expect("0600 on the peers file");
    }
    config::observe_rendezvous_secret(&peers, &node, secret).expect("the write runs");
    reach::remember_port_secret(node, secret);

    assert_eq!(
        reach::port_secret_for(&node),
        Some(secret),
        "the control: this process holds the pair's secret before the forget, so what \
         follows is the forget and not an empty register"
    );

    let store = config::PeerStore::open(&peers).expect("open the peers file");
    assert!(
        teamclaude_rs::peer::pair::forget(&store, &node).expect("the forget runs"),
        "the row was there to remove"
    );

    assert_eq!(
        reach::port_secret_for(&node),
        None,
        "a forgotten Mac leaves no way to compute its rendezvous ports, or `forget` \
         means only the ways that were written down"
    );
    let raw = std::fs::read_to_string(&peers).expect("the peers file reads");
    assert!(
        !raw.contains("rendezvousSecret"),
        "and the row's own copy went with the row: {raw}"
    );
}

// ---------------------------------------------------------------------------
// The address the far side sees this Mac at
// ---------------------------------------------------------------------------

/// A Hello built for a peer this node saw arrive from a public address carries
/// that address back to it.
///
/// The gate for the learning half of the punch: without this field a Mac
/// behind a NAT has no way at all to know what the rest of the world reaches
/// it at, and the whole punch is two sides dialling addresses neither of them
/// knows.
#[test]
fn a_hello_carries_the_address_this_node_saw_the_peer_from() {
    use teamclaude_rs::peer::listener::{hello_for_peer, NodeFacts};

    let node = tcr_peer_wire::PeerId([0x71; 32]);
    let seen: SocketAddr = "203.0.113.7:51820".parse().expect("a test address parses");

    let mut facts = NodeFacts::minimal(node);
    facts.observed_peer_at = Some(seen);
    let hello = hello_for_peer(&facts, &Default::default());

    assert_eq!(
        hello.observed_you_at.as_deref(),
        Some("203.0.113.7:51820"),
        "a bare pin is told its own public address, or a Mac behind a NAT never learns one"
    );
}

/// And a node that saw nothing sends no field, rather than an empty string a
/// reader has to decide the meaning of.
#[test]
fn a_hello_from_a_node_that_saw_no_address_carries_no_field() {
    use teamclaude_rs::peer::listener::{hello_for_peer, NodeFacts};

    let hello = hello_for_peer(
        &NodeFacts::minimal(tcr_peer_wire::PeerId([0x72; 32])),
        &Default::default(),
    );
    assert_eq!(hello.observed_you_at, None, "nothing seen, nothing told");
    let json = serde_json::to_string(&hello).expect("the Hello serializes");
    assert!(
        !json.contains("observedYouAt"),
        "and the key is absent from the wire rather than null: {json}"
    );
}

/// A Hello written by a build that predates the field still parses, and the
/// field reads as "not told".
///
/// The additive claim, checked against bytes rather than against a struct this
/// build could change on both sides at once.
#[test]
fn an_older_hello_without_the_field_still_parses() {
    let node = tcr_peer_wire::PeerId([0x74; 32]).to_wire();
    let older = format!(
        r#"{{"proto":1,"node":"{node}","label":"old","seq":3,
        "caps":{{"egress":false,"forward":false,"lends":false}},"addrs":[],"ttlS":60}}"#
    );
    let hello: tcr_peer_wire::Hello =
        serde_json::from_str(&older).expect("a Hello from an older build still parses");
    assert_eq!(hello.observed_you_at, None, "the field reads as not told");

    let newer = format!(
        r#"{{"proto":1,"node":"{node}","label":"new","seq":3,
        "caps":{{"egress":false,"forward":false,"lends":false}},"addrs":[],"ttlS":60,
        "observedYouAt":"203.0.113.9:4000"}}"#
    );
    let hello: tcr_peer_wire::Hello =
        serde_json::from_str(&newer).expect("and the positive control parses too");
    assert_eq!(
        hello.observed_you_at.as_deref(),
        Some("203.0.113.9:4000"),
        "the control: the same parse DOES read the field when it is there, so the \
         assertion above is about the absent key and not about a parse that drops it"
    );
}

/// Forgetting a Mac takes away the address it saw this one at, along with the
/// pair's rendezvous secret.
///
/// Two ways to meet, one act of forgetting: a register nobody cleared would
/// still hand a punch that Mac's public address for as long as the process
/// lived.
#[test]
fn forgetting_a_peer_takes_away_the_address_it_saw_us_at() {
    let node = tcr_peer_wire::PeerId([0x73; 32]);
    let seen: SocketAddr = "203.0.113.11:7000".parse().expect("a test address parses");
    reach::remember_observed_self(node, seen);
    reach::remember_observed_peer(node, seen);

    assert_eq!(
        reach::observed_self_for(&node),
        Some(seen),
        "the control: the register holds it before the forget"
    );

    assert!(
        reach::forget_port_secret(&node),
        "the forget answers that something was held"
    );
    assert_eq!(
        reach::observed_self_for(&node),
        None,
        "and a forgotten Mac's view of this one goes with it"
    );
    assert_eq!(
        reach::observed_peer_for(&node),
        None,
        "as does this Mac's view of it"
    );
}

// ---------------------------------------------------------------------------
// The schedule two Macs punch on
// ---------------------------------------------------------------------------

/// Two Macs a fraction of a second apart compute the SAME first slot, the same
/// port and the same instant.
///
/// The whole reason a punch needs no port on the wire. If the plan started in
/// the slot already open, the side that computed it late would bind, dial and
/// give up before the other side had started, and both would report a NAT
/// problem they do not have.
#[test]
fn both_sides_of_a_pair_plan_the_same_slots() {
    let secret = [0x5a_u8; 32];
    let slot_ms = i64::try_from(reach::SLOT_SECONDS).expect("the slot width fits") * 1_000;
    // One instant just after a boundary and one just before the next: the
    // widest disagreement two clocks inside one slot can have.
    let early = 1_000_000 * slot_ms + 1;
    let late = 1_000_000 * slot_ms + slot_ms - 1;

    let first = reach::punch_plan(&secret, early, reach::PUNCH_SLOTS);
    let second = reach::punch_plan(&secret, late, reach::PUNCH_SLOTS);

    assert_eq!(
        first, second,
        "two clocks inside one slot plan the same punch, or the port is a number the two \
         sides would have to tell each other"
    );
    assert_eq!(
        first.len(),
        reach::PUNCH_SLOTS as usize,
        "three slots, the number the dial order names"
    );
    assert_eq!(
        first[0].slot, 1_000_001,
        "and the plan starts at the NEXT boundary, never in the slot already part spent"
    );
    assert_eq!(
        first[0].opens_at_unix_ms,
        1_000_001 * slot_ms,
        "the instant is the boundary itself, absolute rather than a duration from now"
    );
    for (step, entry) in first.iter().enumerate() {
        assert_eq!(
            entry.port,
            reach::derived_port(&secret, entry.slot),
            "slot {step} punches on the port the pair already derives for it"
        );
        assert!(
            (reach::PORT_FLOOR..reach::PORT_CEILING).contains(&entry.port),
            "and it is inside the window the derivation promises: {entry:?}"
        );
    }
}

/// The side that is TOLD a slot plans from that number and not from its own
/// clock.
#[test]
fn the_told_side_plans_from_the_slot_it_was_given() {
    let secret = [0x5b_u8; 32];
    let plan = reach::punch_plan_from_slot(&secret, 4_242, reach::PUNCH_SLOTS);
    assert_eq!(
        plan.iter().map(|entry| entry.slot).collect::<Vec<_>>(),
        vec![4_242, 4_243, 4_244],
        "the number on the wire is the one that was chosen, not one re-derived here"
    );
    assert_eq!(
        plan[0].port,
        reach::derived_port(&secret, 4_242),
        "on the port that slot derives"
    );
}

/// A punch that cannot start says which of the two things is missing.
///
/// Both arms, and in order: a pair with no observed address is the ordinary
/// state of two Macs that have only met on one LAN, and a pair with an address
/// but no secret is what a restart leaves. They need different answers from
/// an operator, so they are different names.
#[test]
fn a_punch_that_cannot_start_names_what_is_missing() {
    let node = tcr_peer_wire::PeerId([0x75; 32]);
    assert_eq!(
        reach::punch_target(&node),
        Err(reach::PunchFailure::PeerAddressUnknown),
        "nothing has ever seen this peer, so there is no address to aim at"
    );
    assert!(!reach::punch_is_possible(&node));

    reach::remember_observed_peer(
        node,
        "203.0.113.20:41000".parse().expect("a test address parses"),
    );
    assert_eq!(
        reach::punch_target(&node),
        Err(reach::PunchFailure::NoRendezvousSecret),
        "an address without a secret is a port the two cannot compute"
    );

    reach::remember_port_secret(node, [0x5c; 32]);
    let (addr, secret) = reach::punch_target(&node).expect("both halves are held now");
    assert_eq!(
        addr,
        "203.0.113.20".parse::<std::net::IpAddr>().expect("parses"),
        "the punch aims at the address, and the port comes from the derivation"
    );
    assert_eq!(secret, [0x5c; 32]);
    assert!(reach::punch_is_possible(&node));

    let named = reach::PunchFailure::NoSlotConnected {
        slots_tried: 3,
        ports: vec![20_001, 20_002, 20_003],
    }
    .to_string();
    assert!(
        named.contains("3 slots") && named.contains("20001"),
        "and the failure a symmetric NAT produces prints the count and the ports: {named}"
    );
}

// ---------------------------------------------------------------------------
// The home-LAN IPv6 case row 14 got wrong
// ---------------------------------------------------------------------------

/// **The gate for the home-LAN IPv6 regression**: a source in one of this
/// Mac's own `/64` prefixes is answered with the switch OFF, and
/// `2001:db8::1`, outside it, is refused.
///
/// # The case the bind class alone got wrong
///
/// Two Macs on one home network whose ISP delegates a `/64` both hold
/// `2001:...` addresses and both bind `[::]`. Neither address is LAN scope,
/// correctly so: they are globally routable. Row 14 then refused the first
/// pairing and the knock between two Macs on one desk, which is a regression
/// against the same two Macs on IPv4 and is not what row 14 is for.
///
/// Sharing a `/64` with one of this host's own addresses is not authentication
/// and is not treated as any: it reopens the pattern's OWN admission, the
/// operator's pairing window and the knock bucket, exactly as a LAN source
/// does. What it rules out is a stranger anywhere else on the internet.
///
/// The switch ON is the second half and it is the important one: the operator
/// has asked to be reachable from the internet, so row 14 applies in full and
/// the same neighbour is refused.
///
/// Watch it fail: drop the `!internet &&` from `internet_admission`'s refusal
/// arm and the switch-on leg answers; drop the whole
/// `shares_a_global_v6_prefix` arm and the neighbour is refused.
#[test]
fn a_neighbour_in_this_macs_own_v6_prefix_is_answered_with_the_switch_off() {
    let global_bind: IpAddr = "::"
        .parse()
        .expect("the unspecified dual-stack bind address");
    // This Mac's own delegated address, and the Mac on the next desk in the
    // same prefix with a different host part.
    let own: std::net::Ipv6Addr = "2001:db8:1234:5678::1"
        .parse()
        .expect("a documentation address");
    let neighbour: IpAddr = "2001:db8:1234:5678:aaaa:bbbb:cccc:dddd"
        .parse()
        .expect("a second address in the same /64");
    let stranger: IpAddr = "2001:db8::1".parse().expect("an address outside that /64");

    for pattern in [Handshake::Knock, Handshake::KnockPsk, Handshake::Pair] {
        assert_eq!(
            listener::internet_admission(global_bind, neighbour, pattern, false, &[own]),
            InternetAdmission::Answer,
            "{pattern:?} from a Mac in this Mac's own /64, with the switch off, is the LAN \
             case wearing a global address: refusing it is the regression this gate exists \
             for"
        );
        assert_eq!(
            listener::internet_admission(global_bind, stranger, pattern, false, &[own]),
            InternetAdmission::Refuse,
            "{pattern:?} from outside every prefix this Mac holds gets nothing, which is the \
             whole of row 14"
        );
        assert_eq!(
            listener::internet_admission(global_bind, neighbour, pattern, true, &[own]),
            InternetAdmission::Refuse,
            "and with `peer.internet` ON the operator asked to be reachable from the \
             internet, so {pattern:?} is refused from the same neighbour"
        );
        assert_eq!(
            listener::internet_admission(global_bind, neighbour, pattern, false, &[]),
            InternetAdmission::Refuse,
            "a Mac with no global IPv6 of its own shares a prefix with nobody, so \
             {pattern:?} is refused exactly as before"
        );
    }

    // The prefix is the first 64 bits and nothing else: the host part differs
    // on every Mac on the link and changes by the hour under privacy
    // extensions.
    assert!(
        listener::shares_a_global_v6_prefix(neighbour, &[own]),
        "the same /64 with a different host part is a neighbour"
    );
    assert!(
        !listener::shares_a_global_v6_prefix(stranger, &[own]),
        "a different /64 is not"
    );
    assert!(
        !listener::shares_a_global_v6_prefix(
            "10.0.0.7".parse().expect("an RFC 1918 address"),
            &[own]
        ),
        "an IPv4 source is the IPv4 question, answered before this one"
    );
    assert!(
        !listener::shares_a_global_v6_prefix(
            "::ffff:10.0.0.7".parse().expect("a v4-mapped address"),
            &[own]
        ),
        "and an IPv4 address wearing a v6 shape is still the IPv4 question, never a /64 \
         comparison against 0:0:0:ffff"
    );
}

// ---------------------------------------------------------------------------
// `tcr peer internet off` reaches the keeper inside a running server
// ---------------------------------------------------------------------------

/// **A keeper whose `peer.internet` flag goes off deletes its mapping and
/// stops**, without anybody setting its stop flag.
///
/// The two processes are the whole of the problem. `tcr peer internet off`
/// runs in the CLI, writes the flag and deletes over NAT-PMP; the keeper lives
/// in the serving process, read `peer.internet` once at boot and kept renewing.
/// So the renewal re-created the mapping the CLI had just deleted, up to half
/// an hour later, and the operator's Mac went on being reachable from the
/// internet until it was restarted. A UPnP-granted mapping was worse: the
/// CLI's NAT-PMP delete never took that one away at all, and ending the loop
/// is what routes the delete through `MappingKeeper::delete`, which asks the
/// protocol that granted it.
///
/// Watch it fail: make the `keep_going()` check in `run_mapping_while` a
/// `false` and this test runs until its watchdog fires, on a stop flag it is
/// asserting nothing ever had to set.
#[test]
fn a_keeper_whose_internet_flag_goes_off_deletes_its_mapping_and_stops() {
    let fake = FakeGateway::start(Behaviour::Cooperative {
        external_port: 41_234,
    });
    let dir = tempfile::tempdir().expect("a temp profile directory");
    let peers_path = dir.path().join("tcr-peers.json");
    let on = teamclaude_rs::peer::config::PeerFile {
        internet: true,
        ..teamclaude_rs::peer::config::PeerFile::default()
    };
    teamclaude_rs::peer::config::save(&peers_path, &on).expect("the peers file writes");

    // The switch goes off in another thread, exactly as the CLI does it: a
    // write to the file, and nothing that can reach this loop directly.
    let flipping = peers_path.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        let off = teamclaude_rs::peer::config::PeerFile {
            internet: false,
            ..teamclaude_rs::peer::config::PeerFile::default()
        };
        teamclaude_rs::peer::config::save(&flipping, &off).expect("the peers file writes");
    });

    // The watchdog, so a keeper that ignores the flag ends the test instead of
    // running forever. Five seconds is far past the flag poll.
    let stop = Arc::new(AtomicBool::new(false));
    let watchdog = Arc::clone(&stop);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(5));
        watchdog.store(true, Ordering::SeqCst);
    });

    let started = Instant::now();
    let steps = reach::run_mapping_while(
        NatPmp::at(fake.addr),
        None,
        3_456,
        600,
        Duration::from_millis(50),
        &stop,
        &|| reach::internet_is_on(&peers_path),
    )
    .expect("the cooperative fake maps the port");
    let took = started.elapsed();

    assert!(
        took < Duration::from_secs(4),
        "the keeper has to notice the switch on its own; it only stopped after {took:?}, \
         which is the watchdog and not the flag"
    );
    assert!(
        !stop.load(Ordering::SeqCst),
        "and it stopped without anybody setting its stop flag"
    );
    assert_eq!(
        steps.last(),
        Some(&reach::MappingStep::Deleted),
        "a keeper that stops takes its mapping with it: {steps:?}"
    );

    // As the router saw it: the last TCP request is RFC 6886's lifetime-0
    // delete.
    let asked: Vec<(u8, u32)> = fake
        .asked()
        .into_iter()
        .filter(|(opcode, _)| *opcode == 2)
        .collect();
    assert_eq!(
        asked.last(),
        Some(&(2, 0)),
        "and the router was asked to take it away: {asked:?}"
    );
}

/// **The mapping decision a serving process takes, on all three inputs.**
///
/// Pure, so both the `internet on` arm and the loopback arm are asserted
/// without a router: `mapping_boot` is what `start_peer_mapping` routes on,
/// and `tests/peer_boot.rs` asserts the booted server reaches it.
#[test]
fn the_mapping_decision_reads_the_switch_and_the_bound_address() {
    let routable: SocketAddr = "192.168.1.4:9600"
        .parse()
        .expect("a literal address parses");
    let loopback: SocketAddr = "127.0.0.1:9600".parse().expect("a literal address parses");

    assert_eq!(
        reach::mapping_boot(true, routable),
        reach::MappingBoot::Wanted {
            internal_port: 9_600
        },
        "the switch on and a listener something can reach is the case a mapping is for"
    );
    assert_eq!(
        reach::mapping_boot(false, routable),
        reach::MappingBoot::SwitchOff,
        "the switch off asks the router for nothing"
    );
    assert_eq!(
        reach::mapping_boot(true, loopback),
        reach::MappingBoot::LoopbackOnly,
        "and a loopback listener is a mapping that would forward a public port at a socket \
         nothing off this Mac can reach"
    );
}
