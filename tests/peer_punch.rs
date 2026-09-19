//! Two Macs that nobody can dial reaching each other.
//!
//! The instrument is `tests/tools/nat_sim.rs`, a userspace NAT whose contract
//! is written out in its own module doc. Read that first: every claim in this
//! file is a claim about behaviour the simulator implements deliberately, and
//! the one test that proves the simulator is not simply relaying everything
//! ([`an_inbound_first_connect_is_refused`]) is the control the rest stand on.
//!
//! Nothing here reaches a real router, a real peer or the running proxy. The
//! two tests that use real sockets bind a kernel-chosen port on loopback.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use teamclaude_rs::peer::reach::{
    self, KernelPunchNet, PunchArrival, PunchFailure, PunchNet, PunchSlot,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

#[path = "tools/nat_sim.rs"]
mod nat_sim;

use nat_sim::{Internet, Mapping, Refusal};

/// The pair secret both simulated Macs share. A fixed array, so the derived
/// ports below are the same on every run and a failure names a port a reader
/// can re-derive.
const PAIR_SECRET: [u8; 32] = [0x9e; 32];

/// The two public addresses, from RFC 5737 TEST-NET-3.
const PUBLIC_A: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 1);
const PUBLIC_B: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 2);

/// Unix milliseconds, the same clock [`reach::punch`] waits against.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

/// One slot, on the pair's derived port, opening `in_ms` from now.
///
/// Hand built rather than taken from [`reach::punch_plan`] on purpose: the
/// plan's own instants are 30 seconds apart, and what is under test here is
/// the punch and the NAT, not the arithmetic
/// `both_sides_of_a_pair_plan_the_same_slots` (`tests/peer_reach.rs`) already
/// covers. The port is the real derivation, because that is the value the
/// whole mechanism rests on.
fn slot_in(slot: u64, in_ms: i64) -> PunchSlot {
    PunchSlot {
        slot,
        port: reach::derived_port(&PAIR_SECRET, slot),
        opens_at_unix_ms: now_ms() + in_ms,
    }
}

/// Two port-preserving NATs, and a punch that crosses both.
///
/// The two sides enter the slot 600 ms apart, which is the ordinary case and
/// the one that makes the outcome readable: the early side's connect is
/// refused by a box with no mapping yet, and that refusal is what opens the
/// early side's OWN mapping. When the late side then dials the derived port,
/// there is a hole waiting for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_port_preserving_nats_let_the_punch_through() {
    let internet = Internet::new();
    let node_a = internet.attach(PUBLIC_A, Mapping::EndpointIndependent);
    let node_b = internet.attach(PUBLIC_B, Mapping::EndpointIndependent);

    let port = reach::derived_port(&PAIR_SECRET, 900);
    let early = vec![slot_in(900, 200)];
    let late = vec![slot_in(900, 800)];
    let window = Duration::from_millis(2_500);

    let b_side = {
        let node_b = node_b.clone();
        tokio::spawn(
            async move { reach::punch(&node_b, &early, IpAddr::V4(PUBLIC_A), window).await },
        )
    };
    let a_side = {
        let node_a = node_a.clone();
        tokio::spawn(
            async move { reach::punch(&node_a, &late, IpAddr::V4(PUBLIC_B), window).await },
        )
    };

    let mut punched_a = a_side
        .await
        .expect("the late side's task finishes")
        .expect("the late side punches through");
    let mut punched_b = b_side
        .await
        .expect("the early side's task finishes")
        .expect("the early side punches through");

    assert_eq!(
        (punched_a.port, punched_b.port),
        (port, port),
        "both sides met on the port the pair derived for that slot, which is the whole \
         reason neither had to tell the other a number"
    );
    assert_eq!(
        punched_a.arrival,
        PunchArrival::OurConnect,
        "the late side's own connect is what completed: by then the early side's box was \
         holding a mapping"
    );
    assert_eq!(
        punched_b.arrival,
        PunchArrival::TheirConnect,
        "and the early side, whose own dials were refused, got the connection inbound"
    );

    assert_eq!(
        node_a.mappings(),
        vec![(port, port)],
        "the port-preserving claim, read off the box rather than inferred from a punch \
         that worked: the external port IS the port the node bound"
    );
    assert_eq!(node_b.mappings(), vec![(port, port)], "on both boxes");
    assert!(
        node_a.refusals().contains(&Refusal::NoMapping),
        "and the early side's first dials really were refused by a box with no mapping, \
         so this punch crossed a NAT rather than an open door: {:?}",
        node_a.refusals()
    );

    // The stream is a live pair, not two sockets that merely opened.
    punched_a
        .stream
        .write_all(b"punched")
        .await
        .expect("the punched stream takes bytes");
    punched_a.stream.flush().await.expect("and flushes");
    let mut read = [0_u8; 7];
    tokio::time::timeout(
        Duration::from_secs(2),
        punched_b.stream.read_exact(&mut read),
    )
    .await
    .expect("the far side reads inside two seconds")
    .expect("the far side reads the bytes");
    assert_eq!(
        &read, b"punched",
        "and they are the bytes that were written"
    );
}

/// Two symmetric NATs defeat the punch, by name, inside its own slots.
///
/// A box that hands out a fresh external port per destination never maps the
/// port the pair derived, so every dial arrives at a port with no mapping
/// behind it. The punch cannot see the cause from in here, so it reports what
/// it observed, which slots it spent and on which ports, and returns rather
/// than waiting for a fourth.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_symmetric_nats_defeat_the_punch_by_name() {
    let internet = Internet::new();
    let node_a = internet.attach(PUBLIC_A, Mapping::AddressDependent);
    let node_b = internet.attach(PUBLIC_B, Mapping::AddressDependent);

    let plan = vec![slot_in(910, 100), slot_in(911, 500)];
    let ports: Vec<u16> = plan.iter().map(|entry| entry.port).collect();
    let window = Duration::from_millis(300);

    let b_side = {
        let node_b = node_b.clone();
        let plan = plan.clone();
        tokio::spawn(
            async move { reach::punch(&node_b, &plan, IpAddr::V4(PUBLIC_A), window).await },
        )
    };

    let started = Instant::now();
    let outcome = reach::punch(&node_a, &plan, IpAddr::V4(PUBLIC_B), window).await;
    let spent = started.elapsed();

    assert_eq!(
        outcome.err(),
        Some(PunchFailure::NoSlotConnected {
            slots_tried: 2,
            ports: ports.clone(),
        }),
        "both slots opened and neither side got through, and the failure says which ports \
         it was on"
    );
    assert!(
        spent < Duration::from_secs(5),
        "and it ENDED: a punch that cannot work must not hang the dial that asked for it \
         (spent {spent:?})"
    );
    assert!(
        b_side
            .await
            .expect("the other side's task finishes")
            .is_err(),
        "the other side fails too, so this is not one Mac's misconfiguration"
    );

    let refusals = node_b.refusals();
    assert!(
        refusals.contains(&Refusal::NoMapping),
        "and the reason is the one the mode exists to model: nothing was ever mapped on \
         the derived port: {refusals:?}"
    );
    let mapped: Vec<u16> = node_b
        .mappings()
        .into_iter()
        .map(|(external, _)| external)
        .collect();
    for port in &ports {
        assert!(
            !mapped.contains(port),
            "a symmetric box maps port {port} nowhere, which is exactly why the far side's \
             prediction is wrong: {mapped:?}"
        );
    }
}

/// One symmetric side is NOT enough to defeat a punch, and the gate says so.
///
/// The brief's gate expected a single address-dependent box to beat the punch.
/// It does not, and the reason is worth having in a test rather than in
/// somebody's memory: a connection needs ONE predictable port, not two. The
/// symmetric side's dial is refused, but that refusal opens the symmetric
/// side's own mapping towards the other public address, and the
/// port-preserving side is still sitting on a port the symmetric side can
/// predict. So the punch lands, in the direction that was always going to
/// work.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_symmetric_side_still_gets_through() {
    let internet = Internet::new();
    let node_a = internet.attach(PUBLIC_A, Mapping::EndpointIndependent);
    let node_b = internet.attach(PUBLIC_B, Mapping::AddressDependent);

    let port = reach::derived_port(&PAIR_SECRET, 920);
    let window = Duration::from_millis(2_500);
    let a_plan = vec![slot_in(920, 200)];
    let b_plan = vec![slot_in(920, 800)];

    let a_side = {
        let node_a = node_a.clone();
        tokio::spawn(
            async move { reach::punch(&node_a, &a_plan, IpAddr::V4(PUBLIC_B), window).await },
        )
    };
    let punched_b = reach::punch(&node_b, &b_plan, IpAddr::V4(PUBLIC_A), window)
        .await
        .expect("the symmetric side reaches the predictable one");
    let punched_a = a_side
        .await
        .expect("the predictable side's task finishes")
        .expect("and it is reached");

    assert_eq!(
        (punched_a.port, punched_b.port),
        (port, port),
        "on the derived port, in both readings"
    );
    assert_eq!(
        punched_b.arrival,
        PunchArrival::OurConnect,
        "the symmetric side's own connect is the one that completed"
    );
    assert_eq!(
        punched_a.arrival,
        PunchArrival::TheirConnect,
        "and the port-preserving side took it inbound"
    );
    assert!(
        node_b
            .mappings()
            .iter()
            .all(|(external, _)| *external != port),
        "while the symmetric box never mapped the derived port at all, so nothing the \
         other side predicted about IT was right: {:?}",
        node_b.mappings()
    );
}

/// The control the rest of this file stands on: with no mapping, an inbound
/// connect is refused.
///
/// Without it, every test above could pass against a simulator that simply
/// relayed everything to everyone, which would prove nothing about a punch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_inbound_first_connect_is_refused() {
    let internet = Internet::new();
    let node_a = internet.attach(PUBLIC_A, Mapping::EndpointIndependent);
    let node_b = internet.attach(PUBLIC_B, Mapping::EndpointIndependent);

    let port = reach::derived_port(&PAIR_SECRET, 930);
    // B is listening, and that is not enough: nothing it sent out opened a
    // hole, so the box turns the connection away.
    let listening = {
        let node_b = node_b.clone();
        tokio::spawn(async move {
            node_b
                .accept_on(port, Duration::from_millis(600))
                .await
                .map(|_| ())
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;

    let refused = node_a
        .connect_from(
            port,
            SocketAddr::new(IpAddr::V4(PUBLIC_B), port),
            Duration::from_millis(200),
        )
        .await
        .err()
        .expect("an inbound-first connect is refused");
    assert_eq!(
        refused.kind(),
        std::io::ErrorKind::ConnectionRefused,
        "and refused as a connection refusal, not as a timeout: {refused}"
    );
    assert!(
        node_b.refusals().contains(&Refusal::NoMapping),
        "for the named reason: {:?}",
        node_b.refusals()
    );
    assert!(
        listening
            .await
            .expect("the listener's task finishes")
            .is_err(),
        "and the listener, which never sent anything out, got nothing"
    );
}

/// The shipped socket layer: a punch connect leaves from the port it bound,
/// while a listener holds the same port.
///
/// The one thing the simulator cannot check, because it IS the thing the
/// simulator replaces. Both halves of a punch bind one port at the same
/// instant, which the kernel refuses without `SO_REUSEADDR` and
/// `SO_REUSEPORT`, and the far side's prediction is worth nothing unless the
/// outbound connect really leaves from that port.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kernel_punch_connect_leaves_from_the_port_it_bound() {
    let far = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a kernel-chosen port on loopback");
    let far_addr = far.local_addr().expect("the far side's own address");

    // A free port for the punch, learned by binding and releasing one.
    let punch_port = {
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a second kernel-chosen port");
        probe.local_addr().expect("its address").port()
    };

    let net = KernelPunchNet;
    // The listening half of the punch, holding the same port the connect below
    // leaves from.
    let listening = tokio::spawn(async move {
        KernelPunchNet
            .accept_on(punch_port, Duration::from_millis(800))
            .await
            .map(|_| ())
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let connected = net
        .connect_from(punch_port, far_addr, Duration::from_secs(2))
        .await;
    let (_stream, from) = far.accept().await.expect("the far side accepts");
    assert!(
        connected.is_ok(),
        "the punch's own connect half opened, with the listening half holding the same \
         port: {:?}",
        connected.err()
    );
    assert_eq!(
        from.port(),
        punch_port,
        "and it left from the bound port, or nothing the far side predicted about this \
         connection would be true"
    );
    // The listener gets nothing here: the connect above went somewhere else.
    assert!(
        listening
            .await
            .expect("the listening task finishes")
            .is_err(),
        "the listening half timed out, which is the ordinary end of a slot"
    );
}

/// The frame that asks for a punch carries an address and a slot, and no port.
///
/// The port is the pair's derivation, so a frame that carried one would be a
/// second answer to "which port", and the first reader to trust the wrong one
/// would punch at a port the other side never binds.
#[test]
fn a_punch_at_frame_carries_a_slot_and_an_address_and_no_port() {
    let frame = tcr_peer_wire::Control::PunchAt {
        slot: 4_242,
        public_addr: "203.0.113.5:41000".to_string(),
    };
    let value = serde_json::to_value(&frame).expect("a PunchAt serializes");
    let object = value.as_object().expect("it is an object");
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec!["publicAddr", "slot", "type"],
        "three fields, camelCase on the wire like every other control message"
    );
    assert_eq!(
        object.get("type").and_then(|kind| kind.as_str()),
        Some("punch_at"),
        "under the tag the enum is already read by"
    );

    let back: tcr_peer_wire::Control =
        serde_json::from_value(value).expect("and it parses back to the same frame");
    assert_eq!(back, frame);
}

/// A build that does not know the frame reads it as `Unknown` rather than
/// failing the parse, which is what keeps one upgraded Mac from closing
/// sessions with the rest of the fleet.
#[test]
fn an_older_build_reads_a_punch_at_as_unknown() {
    let older: tcr_peer_wire::Control = serde_json::from_str(
        r#"{"type":"some_frame_from_a_newer_build","slot":1,"publicAddr":"203.0.113.5:1"}"#,
    )
    .expect("an unknown control frame still parses");
    assert_eq!(older, tcr_peer_wire::Control::Unknown);
}

/// A punch request records the address it was given, and plans from the slot
/// it was TOLD rather than from this Mac's own clock.
#[test]
fn a_punch_request_plans_from_the_slot_it_was_told() {
    let peer = tcr_peer_wire::PeerId([0x81; 32]);
    let told: SocketAddr = "203.0.113.30:41000".parse().expect("a test address parses");

    // No secret yet: the request is refused by name, and the address is still
    // recorded, because an address a peer just told us is the freshest one
    // there is.
    assert_eq!(
        reach::punch_request(peer, 7_000, "203.0.113.30:41000"),
        Err(PunchFailure::NoRendezvousSecret)
    );
    assert_eq!(
        reach::observed_peer_for(&peer),
        Some(told),
        "the address was recorded even though the punch could not start"
    );

    let secret = [0x82_u8; 32];
    reach::remember_port_secret(peer, secret);
    let (ip, plan) = reach::punch_request(peer, 7_000, "203.0.113.30:41000")
        .expect("with a secret held, the request is answerable");
    assert_eq!(ip, told.ip(), "aimed at the address the peer gave");
    assert_eq!(
        plan.iter().map(|entry| entry.slot).collect::<Vec<_>>(),
        vec![7_000, 7_001, 7_002],
        "and planned from the slot on the wire, which is the number the OTHER side chose: \
         re-deriving one here is how two Macs end up in different slots"
    );
    assert_eq!(
        plan[0].port,
        reach::derived_port(&secret, 7_000),
        "on the port that slot derives for this pair"
    );
}

/// An address that is not an address is named, not dropped.
#[test]
fn a_punch_request_with_an_unreadable_address_is_named() {
    let peer = tcr_peer_wire::PeerId([0x83; 32]);
    assert_eq!(
        reach::punch_request(peer, 1, "over there"),
        Err(PunchFailure::AddressNotUnderstood {
            told: "over there".to_string()
        }),
        "a pinned Mac that disagrees about a format is worth a line that says what it sent"
    );
}

/// A dial with a short budget takes the slots that fit inside it, and no
/// others.
///
/// The half of the punch that keeps it out of a borrow's way: a slot boundary
/// is up to thirty seconds out, and a punch that waited for one would spend a
/// ten second borrow timeout proving nothing. What it must NOT do is hang, so
/// the filter is a function with its own test rather than a comparison inside
/// the dial.
#[test]
fn a_punch_takes_only_the_slots_that_fit_the_dials_budget() {
    let secret = [0x84_u8; 32];
    let slot_ms = i64::try_from(reach::SLOT_SECONDS).expect("the slot width fits") * 1_000;
    // Two milliseconds into a slot, so the first boundary is a whole slot away.
    let now = 2_000_000 * slot_ms + 2;
    let plan = reach::punch_plan(&secret, now, reach::PUNCH_SLOTS);

    assert!(
        reach::punch_slots_within(&plan, now, Duration::from_secs(1)).is_empty(),
        "a one second budget reaches no boundary at all, and the dial must learn that at \
         once rather than by waiting"
    );
    assert_eq!(
        reach::punch_slots_within(&plan, now, Duration::from_secs(reach::SLOT_SECONDS + 1)).len(),
        1,
        "a budget one slot wide reaches the first boundary"
    );
    assert_eq!(
        reach::punch_slots_within(&plan, now, Duration::from_secs(reach::SLOT_SECONDS * 4)).len(),
        reach::PUNCH_SLOTS as usize,
        "and a patient caller gets all three"
    );
}

/// The line `tcr peer reach` prints for one pinned Mac.
///
/// The verb itself lives in `src/main.rs`, which this file does not touch, so
/// the reading is a function here, and the two call sites need their own review.
#[test]
fn the_reach_line_says_what_a_mac_sees_us_as_and_whether_a_punch_is_possible() {
    let peer = tcr_peer_wire::PeerId([0x85; 32]);
    let unknown = reach::reach_punch_line("laptop-2", &peer);
    assert!(
        unknown.contains("sees-us-at: not told") && unknown.contains("punch-possible: no:"),
        "a pair that has never met off the LAN says so, and says why it cannot punch: \
         {unknown}"
    );

    reach::remember_observed_self(
        peer,
        "203.0.113.40:41000".parse().expect("a test address parses"),
    );
    reach::remember_observed_peer(
        peer,
        "203.0.113.41:41001".parse().expect("a test address parses"),
    );
    reach::remember_port_secret(peer, [0x86; 32]);
    let known = reach::reach_punch_line("laptop-2", &peer);
    assert!(
        known.contains("sees-us-at: 203.0.113.40:41000") && known.contains("punch-possible: yes"),
        "and once both halves are held it prints the address and says yes: {known}"
    );
    assert!(
        known.starts_with("reach: peer: laptop-2 "),
        "in the shape the rest of that verb prints: {known}"
    );
}
