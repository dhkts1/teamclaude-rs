//! The sealed exchange: an ask that names no address, a reply sealed to it
//! that names no address either, and the five ways a reply is refused.
//!
//! # Watched red
//!
//! This file does not compile before `src/peer/ask.rs` exists: every symbol
//! below is that module's public surface, so the red state this gate starts
//! from is a build failure, not a failing assertion.
//!
//! # Everything here runs on this box, against temp files
//!
//! No test in this file connects to a port, starts a process, or touches
//! `127.0.0.1:3456`. Every state file is a fresh temp file this test writes
//! and cleans up itself.

use std::net::SocketAddr;

use teamclaude_rs::peer::ask::{self, Ask};
use teamclaude_rs::peer::noise;
use teamclaude_rs::peer::pair;
use teamclaude_rs::peer::state::{self, PeerState, PendingAsk};

fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tcr-peer-ask-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the scratch dir");
    dir.join("peer-state.json")
}

fn keypair() -> ([u8; noise::KEY_BYTES], [u8; noise::KEY_BYTES]) {
    noise::generate_static().expect("a test keypair")
}

fn some_addrs() -> Vec<SocketAddr> {
    vec![
        "192.0.2.10:7755".parse().expect("a test address"),
        "198.51.100.20:7755".parse().expect("a test address"),
    ]
}

fn row(
    id: u64,
    public: [u8; noise::KEY_BYTES],
    private: [u8; noise::KEY_BYTES],
    until_ms: i64,
) -> PendingAsk {
    PendingAsk {
        id,
        public,
        private,
        until_ms,
    }
}

/// **A reply opens against its own ask and yields the addresses that went
/// in.**
///
/// Watched red: change `noise::seal_to`'s pattern away from `PATTERN_SEAL`
/// and this fails to build a matching responder; change `dialaddrs::encode`
/// or `decode_prefix` and the addresses that come back differ from the ones
/// that went in.
#[test]
fn a_reply_opens_against_its_own_ask_and_yields_the_addresses_that_went_in() {
    let (ask_secret, ask_public) = keypair();
    let (_answer_secret, answer_public) = keypair();
    let ask = Ask { key: ask_public };
    let addrs = some_addrs();

    let reply = ask::seal_addresses(&ask, &addrs, &answer_public).expect("it seals");
    let rendered = reply.to_string_wire();
    assert!(
        rendered.starts_with(ask::REPLY_PREFIX),
        "a reply is minted with its own prefix: {rendered}"
    );

    let now = pair::now_ms();
    let rows = vec![row(1, ask_public, ask_secret, now + 600_000)];
    let (opened, id) =
        ask::open_reply(&rows, &rendered, now).expect("it opens against its own ask");
    assert_eq!(id, 1, "the matching row's id is the one that opened it");
    assert_eq!(
        opened,
        ask::Opened::Addresses(addrs, answer_public),
        "every address that went in comes back out, in order"
    );
}

/// **The same reply against a second Mac's row is refused, with the quiet
/// refusal.**
///
/// "Quiet" means the refusal names no address, no Mac and no key: this test
/// checks both the refusal (`DidNotOpen`) and that its `Display` carries
/// none of the addresses the reply sealed.
#[test]
fn the_same_reply_against_a_second_macs_row_is_refused_with_the_quiet_refusal() {
    let (_ask_secret, ask_public) = keypair();
    let (other_secret, other_public) = keypair();
    let (_answer_secret, answer_public) = keypair();
    let ask = Ask { key: ask_public };
    let addrs = some_addrs();

    let reply = ask::seal_addresses(&ask, &addrs, &answer_public).expect("it seals");
    let rendered = reply.to_string_wire();

    let now = pair::now_ms();
    // The reply was sealed to `ask_public`; this row is a DIFFERENT Mac's
    // outstanding ask, with no relation to it at all.
    let rows = vec![row(2, other_public, other_secret, now + 600_000)];
    let error = ask::open_reply(&rows, &rendered, now)
        .expect_err("a reply sealed to one ask must not open against another's row");
    assert_eq!(error, ask::ReplyRefusal::DidNotOpen);
    let text = format!("{error}");
    for addr in &addrs {
        assert!(
            !text.contains(&addr.to_string()),
            "the refusal must name no address: {text}"
        );
    }
}

/// **A truncated paste is refused as cut short, not as a forgery.**
///
/// The length is checked before the AEAD ever runs: a chat client that
/// truncated the paste and an attacker who forged bytes must read as two
/// different refusals, and this asserts which one a short paste gets.
#[test]
fn a_truncated_paste_is_cut_short_not_a_forgery() {
    let (ask_secret, ask_public) = keypair();
    let (_answer_secret, answer_public) = keypair();
    let ask = Ask { key: ask_public };
    let reply = ask::seal_addresses(&ask, &some_addrs(), &answer_public).expect("it seals");
    let rendered = reply.to_string_wire();

    // Cut the paste in half: still starts with the prefix, still decodes as
    // base32, but far too short to hold a frame, a nonce and a tag.
    let cut = &rendered[..rendered.len() / 2];

    let now = pair::now_ms();
    let rows = vec![row(3, ask_public, ask_secret, now + 600_000)];
    let error = ask::open_reply(&rows, cut, now).expect_err("a cut-short paste must be refused");
    assert_eq!(
        error,
        ask::ReplyRefusal::CutShort,
        "a truncated paste is refused for being short, not called a forgery: {error}"
    );
}

/// **A second open of the same reply is refused.**
///
/// This is `PeerState::spend_ask` and `state::open_and_spend_ask`'s claim, not
/// `open_reply`'s alone: `open_reply` is pure and would happily open the same
/// bytes twice against an unmodified row list, which is why the locked
/// read-modify-write in `state::open_and_spend_ask` is what this test drives.
#[test]
fn a_second_open_of_the_same_reply_is_refused() {
    let (ask_secret, ask_public) = keypair();
    let (_answer_secret, answer_public) = keypair();
    let ask = Ask { key: ask_public };
    let reply = ask::seal_addresses(&ask, &some_addrs(), &answer_public).expect("it seals");
    let rendered = reply.to_string_wire();

    let path = scratch("second-open");
    let now = pair::now_ms();
    state::add_ask(&path, row(4, ask_public, ask_secret, now + 600_000))
        .expect("the ask is written");

    let first = state::open_and_spend_ask(&path, &rendered, now);
    assert!(first.is_ok(), "the first open succeeds: {first:?}");

    let second = state::open_and_spend_ask(&path, &rendered, now);
    assert!(
        second.is_err(),
        "the row was spent on the first open, so the second finds nothing to open against"
    );
}

/// **A reply one millisecond past the ask's deadline is refused, and one
/// millisecond before it still opens.**
///
/// The shape `tests/peer_abuse.rs`'s
/// `an_accepted_window_is_shut_at_the_instant_it_expires_and_before_it_opens`
/// already uses: only the clock differs between the two calls, so a refusal
/// caused by the wrong ask (already ruled out above) cannot be the answer
/// here.
#[test]
fn a_reply_past_the_asks_deadline_is_refused_and_just_before_it_still_opens() {
    let (ask_secret, ask_public) = keypair();
    let (_answer_secret, answer_public) = keypair();
    let ask = Ask { key: ask_public };
    let addrs = some_addrs();
    let reply = ask::seal_addresses(&ask, &addrs, &answer_public).expect("it seals");
    let rendered = reply.to_string_wire();

    let until_ms = pair::now_ms() + 600_000;
    let rows = vec![row(5, ask_public, ask_secret, until_ms)];

    let (opened, _id) = ask::open_reply(&rows, &rendered, until_ms - 1)
        .expect("one millisecond before the deadline, the row is still live");
    assert_eq!(opened, ask::Opened::Addresses(addrs, answer_public));

    let error = ask::open_reply(&rows, &rendered, until_ms)
        .expect_err("at the deadline itself, the row must no longer be live");
    assert_eq!(error, ask::ReplyRefusal::DidNotOpen);
}

/// An ask round-trips through its own wire form and carries no address of any
/// kind: it is minted before this Mac knows which Mac will answer it.
#[test]
fn an_ask_round_trips_and_names_nothing() {
    let (_secret, public) = keypair();
    let ask = Ask { key: public };
    let rendered = ask.to_string_wire();
    assert!(rendered.starts_with(ask::ASK_PREFIX));
    assert_eq!(Ask::parse(&rendered).expect("it parses"), ask);
}

/// `PendingAsk`'s `Debug` never prints the private key: the same rule
/// `MovedKeys` follows, checked here rather than assumed, since a `{:?}` in a
/// log line is how a secret reaches a file that keeps scrollback.
#[test]
fn a_pending_asks_debug_output_never_carries_its_private_key() {
    let (secret, public) = keypair();
    let ask = row(6, public, secret, pair::now_ms() + 600_000);
    let text = format!("{ask:?}");
    assert!(
        !text.contains(&hex_lower(&secret)),
        "the private key must never appear in Debug output: {text}"
    );
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A sanity check that [`PeerState::default`] carries no asks, so the tests
/// above that build a fresh state file are not accidentally inheriting rows
/// from a default that changed under them.
#[test]
fn a_fresh_state_carries_no_asks() {
    assert!(PeerState::default().asks.is_empty());
}
