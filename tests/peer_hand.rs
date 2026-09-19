//! The `hand` mode: the owner hands a borrower a short-lived bearer
//! and the borrowed request leaves the BORROWER's machine.
//!
//! # What these gates observe, and the one thing they do not
//!
//! The property row 15 buys is "which machine's client spends the token". Each
//! node here gets its OWN fake origin on its own kernel-chosen port, reached
//! through a per-node `reqwest::ClientBuilder::resolve` override, so the
//! assertion is which origin received the request and which received nothing.
//!
//! **Both nodes run on `127.0.0.1`, so this proves which NODE made the call and
//! not which IP the packet left from.** The brief asked for `127.0.0.2` and
//! `127.0.0.3`; this machine cannot bind them. Measured before substituting,
//! with `127.0.0.1` as the positive control in the same probe:
//!
//! ```text
//! nc -l 127.0.0.2 0  ->  nc: Can't assign requested address
//! nc -l 127.0.0.3 0  ->  nc: Can't assign requested address
//! nc -l 127.0.0.1 0  ->  listening (killed by the timeout)
//! ifconfig lo0       ->  inet 127.0.0.1, inet6 ::1, inet6 fe80::1%lo0
//! ```
//!
//! An `ifconfig lo0 alias` would have made the addresses real and is refused
//! on principle: it mutates a shared machine that is also serving a live proxy.
//! The IP-level property is what row 15's exit lock is for, it belongs to the
//! egress-pin work, and a reader who thinks this file already
//! covers it will not write the test that does.
//!
//! # No real credential anywhere
//!
//! Every bearer here is an obvious fake and the assertions read the string they
//! put in. This repository is public and this is the one test file that
//! handles bearer tokens at all.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tcr_peer_wire::{Control, Lease, LeaseUnit, Window};
use teamclaude_rs::fallback::Ask;
use teamclaude_rs::peer::config::LendMode;
use teamclaude_rs::peer::lease::{
    bearer_fingerprint, handed_tokens, handoff_for, handoff_push_is_due, handoff_renewal_due,
    lease_has_ended, serve_on_handed_bearer, usage_hint, HandedBearer, HandedTokens, Ledger,
    HANDOFF_RENEW_LEAD_MS,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

/// The hostname both clients believe they are talking to. Never resolved: each
/// client carries a `resolve` override pointing it at its own loopback origin.
const ORIGIN_HOST: &str = "api.anthropic.com";

/// An obviously fake bearer. The owner's, and what the borrower must present.
const OWNER_FAKE_BEARER: &str = "fake-owner-bearer-not-a-credential";

/// One fake origin: a loopback listener that records the `authorization` header
/// of every request it receives and answers a canned 200.
struct FakeOrigin {
    port: u16,
    seen: Arc<Mutex<Vec<String>>>,
}

impl FakeOrigin {
    async fn start() -> Self {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .expect("bind a kernel-chosen loopback port");
        let port = listener.local_addr().expect("a bound address").port();
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let recorder = Arc::clone(&recorder);
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut byte = [0_u8; 1];
                    while head.len() < 8192 {
                        match socket.read(&mut byte).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {}
                        }
                        head.push(byte[0]);
                        if head.ends_with(b"\r\n\r\n") {
                            break;
                        }
                    }
                    let text = String::from_utf8_lossy(&head).into_owned();
                    if let Ok(mut recorded) = recorder.lock() {
                        recorded.push(text);
                    }
                    let body = "{\"ok\":true}";
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                         anthropic-ratelimit-unified-7d-utilization: 0.40\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    // Named rather than discarded: a fake origin that failed to
                    // answer would otherwise surface three layers away as a
                    // confusing client-side error, instead of here at the cause.
                    socket
                        .write_all(response.as_bytes())
                        .await
                        .expect("the fake origin answers its canned 200");
                    socket
                        .flush()
                        .await
                        .expect("the fake origin flushes its canned 200");
                });
            }
        });
        Self { port, seen }
    }

    /// Like [`Self::start`], but every answer reports a HIGHER utilization than
    /// the one before it: 0.40, then 0.47, then 0.54.
    ///
    /// A hand-mode borrow can only learn what it cost the owner from the
    /// rate-limit header on its own answers, so a rise needs two answers that
    /// differ. The constant 0.40 of [`Self::start`] is the right fixture for
    /// every other test here and cannot measure a spend at all.
    async fn start_rising() -> Self {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .expect("bind a kernel-chosen loopback port");
        let port = listener.local_addr().expect("a bound address").port();
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        tokio::spawn(async move {
            let answered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let recorder = Arc::clone(&recorder);
                let answered = Arc::clone(&answered);
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut byte = [0_u8; 1];
                    while head.len() < 8192 {
                        match socket.read(&mut byte).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {}
                        }
                        head.push(byte[0]);
                        if head.ends_with(b"\r\n\r\n") {
                            break;
                        }
                    }
                    if let Ok(mut recorded) = recorder.lock() {
                        recorded.push(String::from_utf8_lossy(&head).into_owned());
                    }
                    let nth = answered.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let utilization = 0.40 + 0.07 * nth as f64;
                    let body = "{\"ok\":true}";
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                         anthropic-ratelimit-unified-7d-utilization: {utilization:.2}\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    socket
                        .write_all(response.as_bytes())
                        .await
                        .expect("the fake origin answers its canned 200");
                    socket
                        .flush()
                        .await
                        .expect("the fake origin flushes its canned 200");
                });
            }
        });
        Self { port, seen }
    }

    /// A client that can reach THIS origin and no other: the hostname resolves
    /// to this origin's port and nothing else is routable.
    fn client(&self) -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            // A hang would otherwise turn a red gate into a wait: a loopback
            // origin that has not answered in ten seconds has failed.
            .timeout(std::time::Duration::from_secs(10))
            .resolve(ORIGIN_HOST, SocketAddr::from(([127, 0, 0, 1], self.port)))
            .build()
            .expect("a client")
    }

    fn requests(&self) -> Vec<String> {
        self.seen.lock().expect("the recorder lock").clone()
    }
}

/// One ask, with the headers a real client sends and the credentials a real
/// client also sends.
///
/// `Ask::scrubbed` removes the credentials, so what reaches the hand path is the
/// same map the proxy's own seam would produce: that is what makes the
/// forwarding assertions below a measurement rather than an absence over an
/// empty map.
fn ask<'a>(body: &'a bytes::Bytes) -> Ask<'a> {
    let mut headers = axum::http::HeaderMap::new();
    for (name, value) in [
        ("content-type", "application/json"),
        ("anthropic-version", "2023-06-01"),
        ("authorization", "Bearer not-a-real-client-token"),
    ] {
        headers.insert(
            axum::http::HeaderName::from_static(name),
            axum::http::HeaderValue::from_static(value),
        );
    }
    Ask {
        path: "/v1/messages",
        query: None,
        method: "POST",
        model: None,
        group: None,
        affinity: None,
        tried_local: 1,
        body: body.clone(),
        headers: Ask::scrubbed(&headers),
    }
}

fn lease(lease_id: u128, expires_at_ms: i64, until: Option<u64>) -> Lease {
    Lease {
        lease_id,
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(0.2),
        granted_at_ms: 0,
        expires_at_ms,
        spent: 0.0,
        max_inflight: 2,
        until,
    }
}

/// **The gate for item 2**: a hand-mode borrow is spent by the BORROWER's
/// client and carries the OWNER's bearer, and the owner's own client sends
/// nothing at all.
///
/// The mirror leg is the same lease with no handed bearer, which is what
/// `serve` mode looks like from the borrower's side: the local path declines
/// (`Ok(None)`), the borrower's origin stays untouched, and the request goes
/// out over the owner's Mac through `peer::serve` instead.
///
/// Watched red by returning `Some(build_upstream_headers(req_headers, ""))`
/// unconditionally from `build_handed_bearer_headers` (`src/proxy.rs`), which
/// sends the request with an empty bearer: the body assertion on
/// `OWNER_FAKE_BEARER` fails, naming what arrived.
#[tokio::test]
async fn a_hand_mode_borrow_leaves_from_the_borrower_with_the_owners_bearer() {
    let owner_origin = FakeOrigin::start().await;
    let borrower_origin = FakeOrigin::start().await;
    let lease_id = 0x1111_1111_1111_1111_1111_1111_1111_1111_u128;
    let now_ms = 1_000_000;

    let body = bytes::Bytes::from_static(b"{\"model\":\"fake\"}");
    let base = format!("http://{ORIGIN_HOST}");

    // No handed bearer yet: this is the `serve`-mode shape. Nothing may leave
    // the borrower.
    let declined = serve_on_handed_bearer(
        &base,
        &borrower_origin.client(),
        &ask(&body),
        lease_id,
        Window::SevenDay,
        now_ms,
    )
    .await
    .expect("a missing bearer is not an error");
    assert!(
        declined.is_none(),
        "a lease with no handed bearer must decline the local path, so the request goes over \
         the owner's Mac as every grant written before row 15 meant"
    );
    assert!(
        borrower_origin.requests().is_empty(),
        "nothing may leave the borrower before a bearer was handed over"
    );

    // The owner hands a bearer over. Now the same lease is served from here.
    handed_tokens().lock().expect("the handed-token store").put(
        lease_id,
        OWNER_FAKE_BEARER.to_string(),
        now_ms + 60_000,
    );

    let served = serve_on_handed_bearer(
        &base,
        &borrower_origin.client(),
        &ask(&body),
        lease_id,
        Window::SevenDay,
        now_ms,
    )
    .await
    .expect("the hand-mode request reaches the fake origin")
    .expect("a hand-mode request with a live bearer is served here");
    assert_eq!(
        served.status(),
        200,
        "the borrower answers its client with what its own origin said"
    );

    let borrowed = borrower_origin.requests();
    assert_eq!(
        borrowed.len(),
        1,
        "exactly one request left the borrower, and it is the hand-mode one"
    );
    let lower = borrowed[0].to_lowercase();
    assert!(
        lower.contains(&format!("authorization: bearer {OWNER_FAKE_BEARER}")),
        "the borrower's request must carry the OWNER's handed bearer; it carried:\n{}",
        borrowed[0]
    );
    assert!(
        owner_origin.requests().is_empty(),
        "the owner's client sent nothing: in hand mode the bytes never touch the owner's Mac, \
         which is the whole difference row 15 buys"
    );

    handed_tokens()
        .lock()
        .expect("the handed-token store")
        .forget(lease_id);
}

/// An expired handed bearer is not a bearer. Read-time expiry, so a laptop that
/// slept through the deadline does not wake up using a dead token.
#[test]
fn an_expired_handed_bearer_is_not_returned() {
    let mut store = HandedTokens::new();
    store.put(7, "fake-bearer".to_string(), 500);
    assert_eq!(
        store.bearer(7, 499),
        Some("fake-bearer"),
        "a bearer before its deadline is live"
    );
    assert_eq!(
        store.bearer(7, 500),
        None,
        "the deadline is absolute and inclusive: at it, the bearer is gone"
    );
    assert_eq!(store.len(), 1, "expiry is read, never swept");
    assert!(store.forget(7), "the owner revoked, so the copy goes");
    assert!(store.is_empty());
    assert!(!store.forget(7), "forgetting twice is not a second removal");
}

/// The three refusals that make the exception scoped: a `Serve` grant, a lease
/// that has ended, and a bearer the owner could not produce.
///
/// Watched red by dropping the `mode != LendMode::Hand` guard in `handoff_for`
/// (`src/peer/lease.rs`): the first assertion fails with a `Handoff` built for
/// a serve-mode grant.
#[test]
fn a_handoff_is_built_only_for_a_live_hand_grant() {
    let now_ms = 1_000_000;
    let live = lease(9, now_ms + 60_000, None);
    let bearer = || {
        Some(teamclaude_rs::manager::HandedCredential {
            access_token: OWNER_FAKE_BEARER.to_string(),
            expires_at_ms: now_ms + 60_000,
            utilization: 0.4,
        })
    };

    assert!(
        handoff_for(LendMode::Serve, &live, bearer(), now_ms).is_none(),
        "a serve-mode grant hands over nothing, whatever else is true"
    );
    assert!(
        handoff_for(LendMode::Hand, &live, None, now_ms).is_none(),
        "no bearer is not a reason to send an empty frame"
    );

    let expired = lease(9, now_ms - 1, None);
    assert!(
        handoff_for(LendMode::Hand, &expired, bearer(), now_ms).is_none(),
        "a lease past its own expiry is over"
    );

    // The lease's `until`, in unix SECONDS beside a millisecond clock.
    let ended = lease(9, now_ms + 60_000, Some(999));
    assert!(
        lease_has_ended(&ended, now_ms),
        "until=999s is 999000ms, which this clock passed"
    );
    assert!(
        handoff_for(LendMode::Hand, &ended, bearer(), now_ms).is_none(),
        "stopping at `until` is the revoke, and there is no recall frame"
    );

    let handed = handoff_for(LendMode::Hand, &live, bearer(), now_ms)
        .expect("a live hand grant with a bearer hands it over");
    assert_eq!(
        handed,
        Control::Handoff {
            lease_id: 9,
            access_token: tcr_peer_wire::HandoffToken::new(OWNER_FAKE_BEARER.to_string()),
            expires_at_ms: now_ms + 60_000,
            utilization: Some(0.4),
        },
        "the frame carries the lease, the short-lived bearer, its deadline and the owner's own \
         utilization, and nothing else"
    );
}

/// **The gate for the review's LOW on logging**: formatting a `Handoff` never
/// prints the bearer.
///
/// `listener::serve_control`'s last arm refuses an unexpected frame with
/// `bail!("peer control: {other:?} is not answered by this build")`, and that
/// error is logged by the caller. With a derived `Debug` the owner's plain
/// access token went into the log of any node that was sent a `Handoff` out of
/// turn, which is a credential at rest on a machine that was never granted one.
///
/// The error string is built here the way that arm builds it, so the assertion
/// is about the text that actually reaches the log rather than about a type in
/// isolation.
///
/// Watched red by replacing `HandoffToken`'s manual `Debug` in
/// `crates/tcr-peer-wire/src/lib.rs` with a derived one: the formatted error
/// then reads `access_token: "fake-owner-bearer-not-a-credential"` and both
/// assertions below name it.
#[test]
fn a_formatted_handoff_never_carries_the_bearer() {
    let frame = Control::Handoff {
        lease_id: 9,
        access_token: tcr_peer_wire::HandoffToken::new(OWNER_FAKE_BEARER.to_string()),
        expires_at_ms: 2_000_000,
        utilization: Some(0.4),
    };
    let logged = format!("peer control: {frame:?} is not answered by this build");

    assert!(
        !logged.contains(OWNER_FAKE_BEARER),
        "the bearer's bytes must not reach a log line, and this one reads: {logged}"
    );
    assert!(
        logged.contains("access_token: <redacted>"),
        "the field is still named, so a reader can tell WHICH frame this was: {logged}"
    );
    // The bytes are still reachable on purpose, by one greppable call: a
    // redaction that also broke the borrower would be a different bug.
    let Control::Handoff { access_token, .. } = &frame else {
        panic!("the frame under test is a Handoff");
    };
    assert_eq!(
        access_token.reveal(),
        OWNER_FAKE_BEARER,
        "`reveal` is the one way out, and it must still answer the bearer"
    );
}

/// **The gate for the review's LOW on the session**: the bearer goes out on an
/// `IK` return visit and on no other handshake.
///
/// The frame travels on the one session that proves the
/// dialler holds the private half of a key this Mac has pinned, and until this
/// fix nothing asked: the arm sent it on whatever session had reached it.
///
/// Asserted as a rule rather than through a session, and the reason is in
/// `handoff_is_permitted`'s own doc: `serve_stream` returns a `Pair` session at
/// the six-digit line and an `Enrol` session at `serve_enrolment`, so no other
/// pattern can reach `serve_control` in this build and an enrolment session
/// cannot be driven as far as the lease arm. The value is what carries the rule
/// if either of those early returns ever becomes a fall-through.
///
/// Watched red by answering `true` for `Handshake::Enrol` in
/// `src/peer/listener.rs`: the enrolment assertion below fails by name.
#[test]
fn a_bearer_goes_out_on_a_return_visit_and_on_no_other_handshake() {
    use teamclaude_rs::peer::listener::handoff_is_permitted;
    use teamclaude_rs::peer::noise::Handshake;

    assert!(
        handoff_is_permitted(Handshake::Return),
        "`IK` is the pattern that proves a pinned key, which is the whole of the \
         `only to the grantee`"
    );
    assert!(
        !handoff_is_permitted(Handshake::Enrol),
        "an enrolment proves a one-use invite secret, not an identity this Mac lends to"
    );
    assert!(
        !handoff_is_permitted(Handshake::Pair),
        "a first pairing's static key is not pinned yet: the digits are compared afterwards"
    );
    for knock in [Handshake::Knock, Handshake::KnockPsk] {
        assert!(
            !handoff_is_permitted(knock),
            "a knock carries no static key at all, so there is nobody to hand a bearer to \
             ({knock:?})"
        );
    }
}

/// **The bearer handed over is one the borrower can spend, and it is the
/// account the lease was measured on.**
///
/// The review's finding: `handoff_bearer` answered the FIRST in-scope account
/// in vector order, a throttled one included. Two things were wrong with that.
/// A held account's bearer comes back 429 on every request until the hold
/// expires, and the borrower cannot see the hold, it lives on the owner's Mac.
/// And vector order is not the lease's order: the fraction was cut out of
/// `lendable_fraction`, which is the BEST single account's headroom, so the
/// first account's token is a different account's from the one that funded the
/// lease.
///
/// Both legs are asserted with the wrong answer FIRST in the vector, which is
/// what makes the order the thing under test.
///
/// Watched red by restoring `.find_map(..)` over the same iterator in
/// `Manager::handoff_bearer`: leg one hands over the held account's bearer.
#[test]
fn a_held_account_is_not_handed_over_and_the_roomiest_one_is() {
    let manager_with = |accounts: &str| {
        let config: teamclaude_rs::config::Config = serde_json::from_str(&format!(
            r#"{{
                "proxy": {{ "port": 0 }},
                "upstream": "http://127.0.0.1:1",
                "quotaProbeSeconds": 0,
                "warmupSeconds": 0,
                "accounts": [{accounts}]
            }}"#
        ))
        .expect("the inline config parses");
        teamclaude_rs::manager::Manager::with_live_refresher(config, None)
    };
    let two = r#"{
            "name": "first@example.com",
            "accessToken": "fake-first-bearer-not-a-credential",
            "expiresAt": 1893456000000
        }, {
            "name": "second@example.com",
            "accessToken": "fake-second-bearer-not-a-credential",
            "expiresAt": 1893456000000
        }"#;

    // LEG ONE: the first account is held by a 429, and it is also the one with
    // the MOST room, so the hold is the only thing that can exclude it.
    let held = manager_with(two);
    let mut roomy = reqwest::header::HeaderMap::new();
    roomy.insert(
        "anthropic-ratelimit-unified-5h-utilization",
        "0.05".parse().expect("a header value"),
    );
    let mut busier = reqwest::header::HeaderMap::new();
    busier.insert(
        "anthropic-ratelimit-unified-5h-utilization",
        "0.50".parse().expect("a header value"),
    );
    held.update_quota(0, &roomy);
    held.update_quota(1, &busier);
    held.mark_rate_limited(0, 600);
    assert_eq!(
        held.handoff_bearer(&tcr_peer_wire::LendScope::All, Window::FiveHour)
            .expect("an account that is not held is still lent")
            .access_token,
        "fake-second-bearer-not-a-credential",
        "a held account's bearer is one the borrower cannot spend, and it cannot see the \
         hold either"
    );

    // LEG TWO: neither is held, and the first has almost nothing left.
    let uneven = manager_with(two);
    let mut spent = reqwest::header::HeaderMap::new();
    spent.insert(
        "anthropic-ratelimit-unified-5h-utilization",
        "0.97".parse().expect("a header value"),
    );
    let mut fresh = reqwest::header::HeaderMap::new();
    fresh.insert(
        "anthropic-ratelimit-unified-5h-utilization",
        "0.05".parse().expect("a header value"),
    );
    uneven.update_quota(0, &spent);
    uneven.update_quota(1, &fresh);
    assert_eq!(
        uneven
            .handoff_bearer(&tcr_peer_wire::LendScope::All, Window::FiveHour)
            .expect("both accounts are usable, so one of them is handed over")
            .access_token,
        "fake-second-bearer-not-a-credential",
        "the account handed over is the one with the room the lease was measured against, \
         not whichever one is first in the vector"
    );
}

/// **The gate for the review's M4**: an account with a strict exit lock is
/// never handed over.
///
/// An exit lock is the operator saying this account's requests leave from one
/// address because the address is load-bearing. A hand-mode grant sends from
/// the BORROWER's machine, so handing a pinned account's bearer over breaks the
/// pin on every request, on a Mac this one cannot see. `handoff_bearer` never
/// consulted `Manager::account_egress` at all.
///
/// Two legs: the pinned account is SKIPPED while an unpinned one beside it is
/// still lent, and a fleet where every account in scope is pinned hands nothing
/// over rather than falling back to the pinned one.
///
/// Watched red by dropping the `pins.get(*index)` filter from
/// `Manager::handoff_bearer` (`src/manager/peer_lend.rs`): the first leg then
/// answers the PINNED account's bearer and names it.
#[test]
fn a_strictly_pinned_account_is_never_handed_over() {
    // An obviously fake peer id, in the wire form `Egress` parses.
    let elsewhere = tcr_peer_wire::PeerId([7_u8; 32]).to_wire();
    let pinned_account = format!(
        r#"{{
            "name": "pinned@example.com",
            "accessToken": "fake-pinned-bearer-not-a-credential",
            "expiresAt": 1893456000000,
            "egress": "via {elsewhere}",
            "egressStrict": true
        }}"#
    );
    let free_account = r#"{
            "name": "free@example.com",
            "accessToken": "fake-free-bearer-not-a-credential",
            "expiresAt": 1893456000000
        }"#;

    let manager_with = |accounts: String| {
        let config: teamclaude_rs::config::Config = serde_json::from_str(&format!(
            r#"{{
                "proxy": {{ "port": 0 }},
                "upstream": "http://127.0.0.1:1",
                "quotaProbeSeconds": 0,
                "warmupSeconds": 0,
                "accounts": [{accounts}]
            }}"#
        ))
        .expect("the inline config parses");
        teamclaude_rs::manager::Manager::with_live_refresher(config, None)
    };

    // The pinned account is FIRST, so a handoff that ignored the pin would
    // answer with it: the order is the thing under test.
    let mixed = manager_with(format!("{pinned_account}, {free_account}"));
    // A measured window on both, because a bearer with no baseline is refused
    // for its own reason and this test is about the PIN.
    for index in 0..2 {
        mixed.update_quota(index, &measured_at("0.10"));
    }
    let handed = mixed
        .handoff_bearer(&tcr_peer_wire::LendScope::All, Window::FiveHour)
        .expect("an unpinned account in scope is still lent");
    assert_eq!(
        handed.access_token, "fake-free-bearer-not-a-credential",
        "the strictly pinned account is skipped and the unpinned one beside it is handed over"
    );

    let all_pinned = manager_with(pinned_account);
    all_pinned.update_quota(0, &measured_at("0.10"));
    assert!(
        all_pinned
            .handoff_bearer(&tcr_peer_wire::LendScope::All, Window::FiveHour)
            .is_none(),
        "with every account in scope pinned there is nothing to hand over, and the answer is \
         no bearer rather than a pin broken quietly"
    );
}

/// **The figure a hand lease is funded by and the token it is served with come
/// from one account, and a scope with no handable account funds nothing.**
///
/// The review's finding: `handoff_bearer` excluded held and strictly pinned
/// accounts and ranked by the least room across three windows, while
/// `lendable_fraction`, which sized the lease, excluded neither and folded
/// `max` over one window. So the handed token was routinely not the account the
/// grant had been measured on, and a scope whose usable accounts were all held
/// or pinned minted a funded lease with no bearer at all: the borrower held a
/// lease it could only spend the serve way, and the owner's ledger showed room
/// promised against an account nobody could use.
///
/// Three legs. The figure agrees with the account picked. The fleet with
/// nothing handable answers `0.0` and no bearer. And `Ledger::grant` refuses a
/// hand ask against that fleet rather than minting.
///
/// Watch it fail by dropping the `LendMode::Hand` clamp from `Ledger::grant`:
/// leg three mints a 0.20 lease with no bearer behind it. Or by folding `max`
/// over the window alone in `Manager::handable_fraction`: leg one reads the
/// held account's room, 0.85, against the free account's token.
#[test]
fn a_hand_lease_is_funded_only_by_an_account_whose_bearer_can_go_out() {
    let manager_with = |accounts: &str| {
        let config: teamclaude_rs::config::Config = serde_json::from_str(&format!(
            r#"{{
                "proxy": {{ "port": 0 }},
                "upstream": "http://127.0.0.1:1",
                "quotaProbeSeconds": 0,
                "warmupSeconds": 0,
                "accounts": [{accounts}]
            }}"#
        ))
        .expect("the inline config parses");
        teamclaude_rs::manager::Manager::with_live_refresher(config, None)
    };
    let two = r#"{
            "name": "held@example.com",
            "accessToken": "fake-held-bearer-not-a-credential",
            "expiresAt": 1893456000000
        }, {
            "name": "free@example.com",
            "accessToken": "fake-free-bearer-not-a-credential",
            "expiresAt": 1893456000000
        }"#;
    let now = time::OffsetDateTime::now_utc();
    let all = tcr_peer_wire::LendScope::All;

    // LEG ONE: the roomiest account is held by a 429, so the answer is the
    // other one's token AND the other one's room.
    let mixed = manager_with(two);
    mixed.update_quota(0, &measured_at("0.05"));
    mixed.update_quota(1, &measured_at("0.40"));
    mixed.mark_rate_limited(0, 600);
    let handed = mixed
        .handoff_bearer(&all, Window::FiveHour)
        .expect("the account that is not held is handed over");
    assert_eq!(
        handed.access_token, "fake-free-bearer-not-a-credential",
        "a held account's bearer is one the borrower cannot spend"
    );
    assert!(
        (handed.utilization - 0.40).abs() < 1e-9,
        "and the baseline on the frame is that account's own window: {}",
        handed.utilization
    );
    let funded = mixed.handable_fraction(&all, Window::FiveHour, now);
    assert!(
        (funded - (0.95 - 0.05 - 0.40)).abs() < 1e-9,
        "the lease is sized on the account it will be served with, which leaves \
         0.95 - 0.05 - 0.40, and it says {funded}"
    );

    // LEG TWO: every account in scope is held, so there is nothing to hand over
    // and nothing to fund with.
    let all_held = manager_with(two);
    all_held.update_quota(0, &measured_at("0.05"));
    all_held.update_quota(1, &measured_at("0.05"));
    all_held.mark_rate_limited(0, 600);
    all_held.mark_rate_limited(1, 600);
    assert!(
        all_held.handoff_bearer(&all, Window::FiveHour).is_none(),
        "no account in scope can have its bearer spent right now"
    );
    assert_eq!(
        all_held.handable_fraction(&all, Window::FiveHour, now),
        0.0,
        "so a hand lease is funded by nothing, however much the fleet has left"
    );
    assert!(
        all_held.lendable_fraction(&all, Window::FiveHour, now) > 0.0,
        "while a SERVE lease still is: a hold is a timer, the request leaves from this Mac, \
         and the advertised figure must not flap with every 429"
    );

    // LEG THREE: the ledger refuses a hand ask against that fleet.
    let home = tempfile::tempdir().expect("a temp home");
    let peers = home.path().join("tcr-peers.json");
    let borrower = tcr_peer_wire::PeerId([7_u8; 32]);
    std::fs::write(
        &peers,
        format!(
            r#"{{
          "peers": [
            {{
              "node": "{node}",
              "label": "borrowing-mac",
              "addedAt": 1,
              "allow": {{ "inspect": true }},
              "lend": {{
                "mode": "hand",
                "window": "5h",
                "fraction": 0.2,
                "ttlS": 300,
                "maxInflight": 2
              }}
            }}
          ]
        }}"#,
            node = borrower.to_wire(),
        ),
    )
    .expect("write the owner's peers file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&peers, std::fs::Permissions::from_mode(0o600))
            .expect("0600 on the peers file");
    }
    let store = teamclaude_rs::peer::config::PeerStore::open(&peers).expect("the peers file reads");
    let ask = tcr_peer_wire::LeaseRequest {
        window: Window::FiveHour,
        unit: LeaseUnit::Fraction(0.20),
        ttl_s: 300,
        max_inflight: 2,
    };

    let mut ledger = Ledger::new();
    ledger.note_owner_headroom(Window::FiveHour, 0.50);
    let refused = ledger.grant(&borrower, &ask, &store, all_held.as_ref());
    assert!(
        refused.answer.lease.is_none(),
        "a hand ask against a fleet with no handable account mints nothing, and it minted \
         {:?}",
        refused.answer.lease
    );
    assert_eq!(
        refused.answer.refusal,
        Some(tcr_peer_wire::LeaseRefusal::OwnerGuard),
        "and it is the owner's own headroom that is missing, not anything the borrower did"
    );

    let mut ledger = Ledger::new();
    ledger.note_owner_headroom(Window::FiveHour, 0.50);
    let granted = ledger.grant(&borrower, &ask, &store, mixed.as_ref());
    assert!(
        granted.answer.lease.is_some(),
        "while the same ask against a fleet with one handable account is funded: {:?}",
        granted.answer.refusal
    );
}

/// The owner pushes a fresh bearer BEFORE the one it handed over stops working.
#[test]
fn a_renewal_is_due_a_lead_time_before_the_handed_bearer_expires() {
    let now_ms = 1_000_000;
    assert!(
        handoff_renewal_due(None, now_ms),
        "nothing handed over yet is the first push"
    );
    assert!(
        !handoff_renewal_due(Some(now_ms + HANDOFF_RENEW_LEAD_MS + 1), now_ms),
        "outside the lead time, the borrower's bearer is still good"
    );
    assert!(
        handoff_renewal_due(Some(now_ms + HANDOFF_RENEW_LEAD_MS), now_ms),
        "at the lead time the next bearer goes out, so the borrower never has a gap"
    );
    assert!(
        handoff_renewal_due(Some(now_ms - 1), now_ms),
        "a bearer that already expired is overdue, not skipped"
    );
}

/// **Exactly one push per NEW bearer, over a simulated last five minutes.**
///
/// # What this measures that `handoff_renewal_due` alone cannot
///
/// The renewal is due for the whole of [`HANDOFF_RENEW_LEAD_MS`] and the
/// control session polls once a second, so an owner whose credential had not
/// been refreshed yet re-sent the SAME bearer on every poll: about three
/// hundred identical frames per lease, each one a Noise message the borrower
/// decodes and writes over an identical copy. The count below is the whole
/// assertion, and it is a count over a SEQUENCE of polls rather than a verdict
/// on one, because a single poll looks correct either way.
///
/// The second half is the half a "send nothing twice" fix could break: when
/// the owner's credential IS refreshed, the very next poll has to send it.
///
/// Watch it fail: drop the fingerprint comparison from
/// `lease::handoff_push_is_due` and the first count is 300 instead of 1.
#[test]
fn exactly_one_push_per_new_bearer_over_the_last_five_minutes() {
    let start_ms = 1_000_000;
    // The bearer the owner holds, expiring inside the lead time, which is what
    // makes every poll in this window "due".
    let first = HandedBearer {
        expires_at_ms: start_ms + HANDOFF_RENEW_LEAD_MS / 2,
        fingerprint: bearer_fingerprint("fake-bearer-one"),
    };

    let mut last: Option<HandedBearer> = None;
    let mut pushes = 0_usize;
    let mut polls = 0_usize;
    // Five minutes of the production poll interval, one second apart.
    for tick in 0..300_i64 {
        polls += 1;
        let now_ms = start_ms + tick * 1_000;
        if handoff_push_is_due(last, first, now_ms) {
            pushes += 1;
            last = Some(first);
        }
    }
    assert_eq!(polls, 300, "the simulated window is the production one");
    assert_eq!(
        pushes,
        1,
        "one push for one bearer: the borrower already holds it after the first, and the \
         other {} polls would have been identical frames on the wire",
        polls - 1
    );

    // The owner's credential is refreshed. The next poll sends it, and then
    // stops again.
    let refreshed = HandedBearer {
        expires_at_ms: start_ms + 600_000,
        fingerprint: bearer_fingerprint("fake-bearer-two"),
    };
    assert_ne!(
        first.fingerprint, refreshed.fingerprint,
        "two different tokens must fingerprint differently, or this measures nothing"
    );
    let now_ms = start_ms + 300_000;
    assert!(
        handoff_push_is_due(last, refreshed, now_ms),
        "a new bearer goes out on the next poll, or the borrower's token dies with no renewal"
    );
    last = Some(refreshed);
    assert!(
        !handoff_push_is_due(last, refreshed, now_ms + 1_000),
        "and then it stops again"
    );

    // Outside the lead time nothing is due at all, whatever the token is.
    let far = HandedBearer {
        expires_at_ms: now_ms + HANDOFF_RENEW_LEAD_MS + 10_000,
        fingerprint: bearer_fingerprint("fake-bearer-three"),
    };
    assert!(
        !handoff_push_is_due(Some(far), far, now_ms),
        "a bearer with more than the lead time left is not renewed early"
    );
}

/// A bearer that has already expired is not handed over.
///
/// The lease's expiry and the credential's are different clocks: a live lease
/// can sit beside a token whose refresh has not happened yet, and
/// `handoff_for` used to send that one, costing the borrower a request to find
/// out it was useless.
///
/// Watch it fail: drop the `expires_at_ms <= now_ms` check from
/// `lease::handoff_for`.
#[test]
fn an_expired_bearer_is_not_handed_over() {
    let now_ms = 1_000_000;
    let live = lease(11, now_ms + 60_000, None);

    assert!(
        handoff_for(
            LendMode::Hand,
            &live,
            Some(teamclaude_rs::manager::HandedCredential {
                access_token: OWNER_FAKE_BEARER.to_string(),
                expires_at_ms: now_ms,
                utilization: 0.4,
            }),
            now_ms
        )
        .is_none(),
        "the deadline is absolute and inclusive, exactly as `HandedTokens::bearer` reads it"
    );
    assert!(
        handoff_for(
            LendMode::Hand,
            &live,
            Some(teamclaude_rs::manager::HandedCredential {
                access_token: OWNER_FAKE_BEARER.to_string(),
                expires_at_ms: now_ms - 1,
                utilization: 0.4,
            }),
            now_ms
        )
        .is_none(),
        "and a bearer already past it is not a frame worth sending"
    );
    assert!(
        handoff_for(
            LendMode::Hand,
            &live,
            Some(teamclaude_rs::manager::HandedCredential {
                access_token: OWNER_FAKE_BEARER.to_string(),
                expires_at_ms: now_ms + 1,
                utilization: 0.4,
            }),
            now_ms
        )
        .is_some(),
        "a bearer with any life left is still handed over"
    );
}

/// **The gate for item 3**: the hint arrives and `spent` rises by the hinted
/// amount; the same request with no hint leaves `spent` unchanged.
///
/// Watched red by making `Ledger::apply_usage_hint` return `0.0` before it adds
/// (`src/peer/lease.rs`): the risen-`spent` assertion fails at 0.0 against 0.03.
#[test]
fn a_usage_hint_raises_spent_and_a_request_without_one_does_not() {
    let now_ms = crate_now_ms();
    let mut ledger = Ledger::new();
    ledger.record(lease(21, now_ms + 600_000, None));
    let before = spent_of(&ledger, 21, now_ms);
    assert_eq!(before, 0.0, "a fresh lease has spent nothing");

    // The request with NO hint: the owner sees a hand-mode response it never
    // received, which is to say it sees nothing, and the lease does not move.
    let no_hint = usage_hint(21, Some(0.40), Some(0.40));
    assert!(
        no_hint.is_none(),
        "a window that did not move reports nothing rather than zero"
    );
    assert!(
        usage_hint(21, Some(0.40), None).is_none(),
        "a response with no rate-limit header says nothing about the window"
    );
    assert!(
        usage_hint(21, Some(0.40), Some(0.10)).is_none(),
        "a window that reset under the request is the owner's windfall, never a credit"
    );
    assert_eq!(
        spent_of(&ledger, 21, now_ms),
        before,
        "with no hint applied, spent is unchanged"
    );

    // The request WITH a hint.
    let hinted = usage_hint(21, Some(0.40), Some(0.43)).expect("a risen window reports a hint");
    let Control::UsageHint { lease_id, spent } = hinted else {
        panic!("usage_hint builds a UsageHint and nothing else, got {hinted:?}");
    };
    assert_eq!(lease_id, 21);
    assert!(
        (spent - 0.03).abs() < 1e-9,
        "the hint carries the rise the borrower observed, got {spent}"
    );

    let charged = ledger.apply_usage_hint(lease_id, spent);
    assert!(
        (charged - spent).abs() < 1e-9,
        "the ledger applies what the hint carried, got {charged}"
    );
    let after = spent_of(&ledger, 21, now_ms);
    assert!(
        (after - (before + spent)).abs() < 1e-9,
        "spent rises by the hinted amount: {before} + {spent} should be {after}"
    );

    // A hint can only ever raise.
    assert_eq!(
        ledger.apply_usage_hint(lease_id, -1.0),
        0.0,
        "the reporter is the party that gains by under-reporting, so a negative is dropped"
    );
    assert_eq!(
        ledger.apply_usage_hint(lease_id, f64::NAN),
        0.0,
        "a hint that is not a number charges nothing"
    );
    assert_eq!(
        ledger.apply_usage_hint(4_242, 0.5),
        0.0,
        "a lease this ledger does not hold is charged nothing"
    );
    assert!(
        (spent_of(&ledger, 21, now_ms) - after).abs() < 1e-9,
        "none of the three refusals moved the figure"
    );
}

fn spent_of(ledger: &Ledger, lease_id: u128, now_ms: i64) -> f64 {
    ledger
        .live(now_ms)
        .into_iter()
        .find(|lease| lease.lease_id == lease_id)
        .map(|lease| lease.spent)
        .expect("the ledger holds the lease under test")
}

/// The same clock the ledger reads, so a lease minted here is live for it.
/// What the owner's seven-day window reads in every fixture here.
///
/// Below the fake origin's first answer (0.40), so the first borrowed answer is
/// a real rise rather than a fall, and named rather than repeated so the
/// arithmetic in the spend gate stays readable.
const OWNER_MEASURED_UTILIZATION: f64 = 0.30;

/// One window measured at `utilization`, in the header form
/// `Manager::update_quota` reads.
///
/// A bearer is handed over only for an account whose window this Mac has
/// measured, because the borrower's spend is reported as a RISE above that
/// figure, so a fixture about anything else still has to measure one.
fn measured_at(utilization: &str) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "anthropic-ratelimit-unified-5h-utilization",
        utilization.parse().expect("a header value"),
    );
    headers
}

fn crate_now_ms() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp() * 1_000
}

// ---------------------------------------------------------------------------
// The OWNER's half, over a real session
// ---------------------------------------------------------------------------

/// The owner's listener, its peers file and its manager, as one fixture.
///
/// Built by hand rather than borrowed from `tests/peer_lease.rs`'s `mesh`
/// module, which is private to that binary: what this file needs and that one
/// does not is a manager holding an account with a bearer, since a `Hand`
/// grant is the one path on which the lender reads its own access token.
struct Owner {
    addr: SocketAddr,
    public: [u8; 32],
    ledger: Arc<Mutex<Ledger>>,
    _home: tempfile::TempDir,
}

/// Stand an owner up: a peers file pinning `borrower` with one `hand` grant on
/// the seven-day window, a manager with one obviously fake account, and the
/// shipped accept loop.
async fn owner_serving(borrower: tcr_peer_wire::PeerId, mode: &str) -> Owner {
    owner_lending(
        borrower,
        &format!(
            r#"{{
                "mode": "{mode}",
                "window": "7d",
                "fraction": 0.2,
                "ttlS": 300,
                "maxInflight": 2
              }}"#
        ),
    )
    .await
}

/// The same owner, with the `lend` value written out in full: one grant, or a
/// JSON array of several, which the lease scope lets an operator hold and
/// what the ended-grant gate below needs.
async fn owner_lending(borrower: tcr_peer_wire::PeerId, lend: &str) -> Owner {
    owner_lending_expiring(borrower, lend, 1_893_456_000_000).await
}

/// [`owner_lending`], with the account's bearer expiry chosen.
///
/// The expiry is what decides whether a renewal is DUE
/// (`lease::handoff_renewal_due` against `HANDOFF_RENEW_LEAD_MS`), so a test
/// about renewals has to set it and every other test wants one far away.
async fn owner_lending_expiring(
    borrower: tcr_peer_wire::PeerId,
    lend: &str,
    expires_at_ms: i64,
) -> Owner {
    use teamclaude_rs::peer::config::PeerStore;
    use teamclaude_rs::peer::id::NodeKey;
    use teamclaude_rs::peer::listener::{self, LeaseServing, SessionContext};

    let home = tempfile::tempdir().expect("a temp home");
    let peers = home.path().join("tcr-peers.json");
    std::fs::write(
        &peers,
        format!(
            r#"{{
          "peers": [
            {{
              "node": "{node}",
              "label": "borrowing-mac",
              "addedAt": 1,
              "allow": {{ "inspect": true }},
              "lend": {lend}
            }}
          ]
        }}"#,
            node = borrower.to_wire(),
        ),
    )
    .expect("write the owner's peers file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&peers, std::fs::Permissions::from_mode(0o600))
            .expect("0600 on the peers file");
    }

    let config: teamclaude_rs::config::Config = serde_json::from_str(&format!(
        r#"{{
            "proxy": {{ "port": 0 }},
            "upstream": "http://127.0.0.1:1",
            "quotaProbeSeconds": 0,
            "warmupSeconds": 0,
            "accounts": [
                {{
                    "name": "alice@example.com",
                    "accessToken": "{OWNER_FAKE_BEARER}",
                    "expiresAt": {expires_at_ms},
                    "accountUuid": "11111111-1111-1111-1111-111111111111",
                    "orgUuid": "22222222-2222-2222-2222-222222222222"
                }}
            ]
        }}"#
    ))
    .expect("the inline owner config parses");
    let manager = teamclaude_rs::manager::Manager::with_live_refresher(config, None);
    // THE OWNER'S OWN WINDOW, measured, because a `hand` grant hands a bearer
    // over only for an account whose window this Mac has read: the borrower
    // reports its spend as a rise above that figure, and there is no honest
    // baseline without it. Every lease here is on the seven-day window.
    {
        let mut measured = reqwest::header::HeaderMap::new();
        measured.insert(
            "anthropic-ratelimit-unified-7d-utilization",
            OWNER_MEASURED_UTILIZATION
                .to_string()
                .parse()
                .expect("a header value"),
        );
        manager.update_quota(0, &measured);
    }

    let key = NodeKey::load_or_mint(home.path()).expect("the owner's node key");
    let public = key.id().0;
    let store = PeerStore::open(&peers).expect("the owner's peers file reads");
    let state_path = home.path().join("peer-state.json");
    let ledger = Arc::new(Mutex::new(Ledger::new()));
    {
        let mut held = ledger.lock().expect("ledger lock");
        // Headroom the owner has actually noted, or `grant` has nothing to cut
        // a fraction out of and refuses before mode is ever consulted.
        held.note_owner_headroom(Window::SevenDay, 0.50);
    }
    let context = SessionContext::new(&key, store.path(), &state_path).with_lease_serving(Some(
        LeaseServing {
            ledger: ledger.clone(),
            upstream: "http://127.0.0.1:1".to_string(),
            utilization: Arc::new(teamclaude_rs::peer::serve::NoFleetUtilization),
            manager,
        },
    ));

    let listening = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the owner's peer listener");
    let addr = listening.local_addr().expect("the owner's peer addr");
    tokio::spawn(async move {
        let _ = listener::serve_on_with(listening, context).await;
    });

    Owner {
        addr,
        public,
        ledger,
        _home: home,
    }
}

/// Ask `owner` for one seven-day lease over a fresh `IK` session, and read
/// whatever the owner sends after the grant.
///
/// The second element is `None` when nothing arrived inside a LAN round trip,
/// which is what a serve-mode grant answers: the read waits for the close.
async fn ask_for_a_lease(owner: &Owner, secret: &[u8; 32]) -> (Lease, Option<Control>) {
    let mut stream = tokio::net::TcpStream::connect(owner.addr)
        .await
        .expect("connect to the owner");
    let mut session = teamclaude_rs::peer::noise::dial_handshake(
        &mut stream,
        secret,
        teamclaude_rs::peer::noise::Handshake::Return,
        Some(&owner.public),
        None,
    )
    .await
    .expect("the borrower is pinned, so the return visit completes");

    let header = tcr_peer_wire::StreamHeader {
        kind: tcr_peer_wire::StreamKind::Control,
        target: None,
        via: Vec::new(),
        hops_remaining: 1,
        request_id: teamclaude_rs::peer::lease::random_id().expect("a request id"),
    };
    teamclaude_rs::peer::serve::send_control(&mut stream, &mut session, &header)
        .await
        .expect("the header goes");
    let ask = tcr_peer_wire::LeaseRequest {
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(0.2),
        ttl_s: 300,
        max_inflight: 2,
    };
    teamclaude_rs::peer::serve::send_control(
        &mut stream,
        &mut session,
        &Control::LeaseRequest(ask),
    )
    .await
    .expect("the ask goes");

    let granted: Control = teamclaude_rs::peer::serve::recv_control(&mut stream, &mut session)
        .await
        .expect("the owner answers the ask");
    let Control::LeaseGrant(grant) = granted else {
        panic!("the first answer is the grant, not {granted:?}");
    };
    let minted = grant
        .lease
        .unwrap_or_else(|| panic!("the owner granted a lease, refusal: {:?}", grant.refusal));
    // Whatever comes next, it must not take longer than a LAN round trip: with
    // nothing to hand over, nothing comes at all and this read waits for the
    // close.
    let next = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        teamclaude_rs::peer::serve::recv_control::<_, Control>(&mut stream, &mut session),
    )
    .await;
    let next = match next {
        Err(_elapsed) => None,
        Ok(read) => read.ok(),
    };
    (minted, next)
}

/// **The gate for the review's first HIGH**: the mode is the FUNDING grant's,
/// and a row whose `hand` grant has ended hands nothing over.
///
/// The row here is a failing input: an ended `hand` grant and
/// a live `serve` grant, both on the seven-day window. `PeerRow::grant_for`
/// skips the ended one, so the lease is funded by the serve grant and the
/// owner's bearer must stay on the owner's Mac.
///
/// Watched red by putting the window-only lookup back in `serve_control`'s
/// `Control::LeaseRequest` arm (`store.row(..).lend.iter().find(|g| g.window ==
/// minted.window)`): that lookup answers with the ENDED hand grant, a `Handoff`
/// carrying `OWNER_FAKE_BEARER` arrives, and the assertion below names it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_ended_hand_grant_beside_a_live_serve_grant_hands_nothing_over() {
    let (borrower_secret, borrower_public) =
        teamclaude_rs::peer::noise::generate_static().expect("a borrower keypair");
    let borrower = tcr_peer_wire::PeerId(borrower_public);
    // `until` is unix SECONDS and this one is in 1970, so `LendGrant::has_ended`
    // is true for it on any clock this test can run on.
    let owner = owner_lending(
        borrower,
        r#"[
                {
                  "mode": "hand",
                  "window": "7d",
                  "fraction": 0.2,
                  "ttlS": 300,
                  "maxInflight": 2,
                  "until": 1000
                },
                {
                  "mode": "serve",
                  "window": "7d",
                  "scope": "all",
                  "fraction": 0.2,
                  "ttlS": 300,
                  "maxInflight": 2
                }
              ]"#,
    )
    .await;

    let (minted, next) = ask_for_a_lease(&owner, &borrower_secret).await;
    assert!(
        minted.expires_at_ms > 0,
        "the live serve grant funded a lease, or this gate is asserting about a refusal"
    );
    assert!(
        next.is_none(),
        "the lease was funded by the SERVE grant, so nothing is handed over; this session \
         read {next:?}"
    );
}

/// **The gate for the review's M2**: a poisoned lease ledger hands nothing
/// over.
///
/// The arm used to read the lease's scope back through a SECOND lock on the
/// same ledger and turn a poisoned one into `LendScope::All`, which resolves
/// the owner's bearer against every account on the Mac rather than the ones
/// the operator lent. The scope now leaves `Ledger::grant` in the same value as
/// the answer, so there is no second lock and a poisoned ledger refuses the
/// whole request at the first one: no grant, no `Handoff`, and a session that
/// closes.
///
/// Watched red by putting both halves of the defect back at once, which is the
/// state the review describes: `serving.ledger.lock()` in the
/// `Control::LeaseRequest` arm recovering with
/// `unwrap_or_else(|poisoned| poisoned.into_inner())`, and the scope read back
/// with `serving.ledger.lock().map(..).unwrap_or_default()`. The owner then
/// answers a poisoned ledger with a lease AND a `Handoff` carrying
/// `OWNER_FAKE_BEARER`, and the assertion below names the frame.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_poisoned_lease_ledger_hands_nothing_over() {
    let (borrower_secret, borrower_public) =
        teamclaude_rs::peer::noise::generate_static().expect("a borrower keypair");
    let borrower = tcr_peer_wire::PeerId(borrower_public);
    let owner = owner_serving(borrower, "hand").await;

    // Poison it the only way a `std::sync::Mutex` is poisoned: a panic while
    // the guard is held. The join handle's `Err` IS the panic, so it is read
    // rather than dropped.
    let ledger = Arc::clone(&owner.ledger);
    let panicked = std::thread::spawn(move || {
        let _held = ledger.lock().expect("the ledger is not poisoned yet");
        panic!("poisoning the owner's ledger on purpose");
    })
    .join();
    assert!(
        panicked.is_err(),
        "the helper thread has to panic for the ledger to be poisoned"
    );
    assert!(
        owner.ledger.lock().is_err(),
        "the ledger is poisoned, or this gate is asserting about a healthy one"
    );

    let mut stream = tokio::net::TcpStream::connect(owner.addr)
        .await
        .expect("connect to the owner");
    let mut session = teamclaude_rs::peer::noise::dial_handshake(
        &mut stream,
        &borrower_secret,
        teamclaude_rs::peer::noise::Handshake::Return,
        Some(&owner.public),
        None,
    )
    .await
    .expect("the borrower is pinned, so the return visit completes");
    let header = tcr_peer_wire::StreamHeader {
        kind: tcr_peer_wire::StreamKind::Control,
        target: None,
        via: Vec::new(),
        hops_remaining: 1,
        request_id: teamclaude_rs::peer::lease::random_id().expect("a request id"),
    };
    teamclaude_rs::peer::serve::send_control(&mut stream, &mut session, &header)
        .await
        .expect("the header goes");
    let ask = tcr_peer_wire::LeaseRequest {
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(0.2),
        ttl_s: 300,
        max_inflight: 2,
    };
    teamclaude_rs::peer::serve::send_control(
        &mut stream,
        &mut session,
        &Control::LeaseRequest(ask),
    )
    .await
    .expect("the ask goes");

    // Every frame this session can still read, and there must be none: the
    // refusal closes the stream. Two reads, because the defect answered with a
    // grant FIRST and the bearer after it.
    let mut read = Vec::new();
    for _ in 0..2 {
        let next = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            teamclaude_rs::peer::serve::recv_control::<_, Control>(&mut stream, &mut session),
        )
        .await;
        match next {
            Ok(Ok(frame)) => read.push(frame),
            // A closed stream and a read that never answered are the same
            // fact here: nothing more is coming.
            Ok(Err(_)) | Err(_) => break,
        }
    }
    assert!(
        !read
            .iter()
            .any(|frame| matches!(frame, Control::Handoff { .. })),
        "a poisoned ledger hands no bearer over, and this session read {read:?}"
    );
    assert!(
        read.is_empty(),
        "a poisoned ledger answers nothing at all, and this session read {read:?}"
    );
}

/// **The gate for the owner half**: after granting a `hand` lease, the owner's
/// listener sends the bearer on the SAME session, unasked.
///
/// `lease::handoff_for` and `Ledger::apply_usage_hint` have had tests since
/// they were written, and both of those call the function directly. Nothing
/// called either from a session, so an owner could grant a hand-mode lease all
/// day and never hand anything over, which from the borrower's side is
/// indistinguishable from serve mode.
///
/// Driven frame by frame rather than through `lease::request_lease`, and it
/// stays that way now that the borrower half IS wired: this gate is about what
/// the OWNER puts on the session, and reading it frame by frame is what lets
/// the serve leg below assert that nothing arrives. The borrower's end of the
/// same exchange is
/// `a_borrowed_hand_lease_leaves_the_owners_bearer_in_this_process`.
///
/// The `serve`-mode leg in the same test is the positive control, and it is the
/// assertion with teeth: without it, "a Handoff arrived" would also pass
/// against a listener that hands its bearer to every borrower.
///
/// Watched red by dropping the `handoff_for` block from `serve_control`'s
/// `Control::LeaseRequest` arm: the hand-mode leg reads nothing after the
/// grant and the first assertion names it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hand_grant_is_followed_by_the_owners_bearer_on_the_same_session() {
    for (mode, expect_handoff) in [("hand", true), ("serve", false)] {
        let (borrower_secret, borrower_public) =
            teamclaude_rs::peer::noise::generate_static().expect("a borrower keypair");
        let borrower = tcr_peer_wire::PeerId(borrower_public);
        // The same fixture with the one word changed, which is the only
        // difference the two legs rest on.
        let owner = owner_serving(borrower, mode).await;

        let mut stream = tokio::net::TcpStream::connect(owner.addr)
            .await
            .expect("connect to the owner");
        let mut session = teamclaude_rs::peer::noise::dial_handshake(
            &mut stream,
            &borrower_secret,
            teamclaude_rs::peer::noise::Handshake::Return,
            Some(&owner.public),
            None,
        )
        .await
        .expect("the borrower is pinned, so the return visit completes");

        let header = tcr_peer_wire::StreamHeader {
            kind: tcr_peer_wire::StreamKind::Control,
            target: None,
            via: Vec::new(),
            hops_remaining: 1,
            request_id: teamclaude_rs::peer::lease::random_id().expect("a request id"),
        };
        teamclaude_rs::peer::serve::send_control(&mut stream, &mut session, &header)
            .await
            .expect("the header goes");
        let ask = tcr_peer_wire::LeaseRequest {
            window: Window::SevenDay,
            unit: LeaseUnit::Fraction(0.2),
            ttl_s: 300,
            max_inflight: 2,
        };
        teamclaude_rs::peer::serve::send_control(
            &mut stream,
            &mut session,
            &Control::LeaseRequest(ask),
        )
        .await
        .expect("the ask goes");

        let granted: Control = teamclaude_rs::peer::serve::recv_control(&mut stream, &mut session)
            .await
            .expect("the owner answers the ask");
        let Control::LeaseGrant(grant) = granted else {
            panic!("the first answer is the grant, not {granted:?}");
        };
        let minted = grant
            .lease
            .unwrap_or_else(|| panic!("the owner granted a lease, refusal: {:?}", grant.refusal));

        // Whatever comes next, it must not take longer than a LAN round trip:
        // in serve mode nothing comes at all and the read waits for the close.
        let next = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            teamclaude_rs::peer::serve::recv_control::<_, Control>(&mut stream, &mut session),
        )
        .await;

        if expect_handoff {
            let frame = next
                .expect("a hand-mode grant is followed by a frame, and this one timed out")
                .expect("and that frame reads as a Control");
            let Control::Handoff {
                lease_id,
                access_token,
                expires_at_ms,
                utilization,
            } = frame
            else {
                panic!("a hand grant is followed by the bearer, not by {frame:?}");
            };
            assert_eq!(
                lease_id, minted.lease_id,
                "the bearer is handed for the lease this session just minted"
            );
            assert_eq!(
                access_token.reveal(),
                OWNER_FAKE_BEARER,
                "and it is the owner's own bearer, read through the manager"
            );
            assert!(
                expires_at_ms > 0,
                "with an absolute expiry, so the borrower can stop without waiting for a 401"
            );
            assert_eq!(
                utilization,
                Some(OWNER_MEASURED_UTILIZATION),
                "and with the owner's own utilization on the lease's window, which is the \
                 baseline every spend this borrower reports is a rise above"
            );
            assert_eq!(
                owner
                    .ledger
                    .lock()
                    .expect("ledger lock")
                    .grantee_of(minted.lease_id),
                Some(borrower),
                "and the ledger still names the borrower as the only party that may spend it"
            );
        } else {
            let handed = match next {
                Err(_elapsed) => None,
                Ok(read) => read.ok(),
            };
            assert!(
                handed.is_none(),
                "the control: a serve-mode grant hands nothing over, and this session \
                 read {handed:?}"
            );
        }
    }
}

/// **The gate for the borrower half**: a hand-mode borrow driven through
/// `lease::request_lease` leaves the owner's bearer in THIS process's store.
///
/// Both halves existed and nothing connected them. The owner
/// sent the frame (the gate above) and `request_lease` read exactly one answer
/// and returned, so the session was dropped with the bearer still in flight and
/// `HandedTokens::put` had no production caller anywhere. From the borrower's
/// side that is indistinguishable from a lender running in serve mode.
///
/// Polled rather than read once: the reader lives in a task the ask spawns, so
/// the lease comes back first and the bearer lands a round trip later. The poll
/// has a ceiling and the failure below names what was never stored.
///
/// The serve-mode leg is the positive control and it is the assertion with
/// teeth: without it, "a bearer is in the store" would also pass against a
/// borrower that stored anything it was sent.
///
/// Watched red by deleting the `tokio::spawn(read_handed_bearers(..))` line
/// from `lease::request_lease` (`src/peer/lease.rs`): the hand leg polls out
/// with an empty store and the first assertion names the lease id.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_borrowed_hand_lease_leaves_the_owners_bearer_in_this_process() {
    for (mode, expect_bearer) in [("hand", true), ("serve", false)] {
        let borrower_home = tempfile::tempdir().expect("the borrower's temp home");
        let borrower_peers = borrower_home.path().join("tcr-peers.json");
        // `request_lease` mints the borrower's key beside its peers file, so
        // the id the owner has to pin is read from there and not invented.
        let borrower = teamclaude_rs::peer::id::NodeKey::load_or_mint(borrower_home.path())
            .expect("the borrower's node key")
            .id();
        let owner = owner_serving(borrower, mode).await;

        // The borrower's own row for the owner: `disclose` on, because the
        // borrower's half of the two opt-ins is checked here before it asks.
        std::fs::write(
            &borrower_peers,
            format!(
                r#"{{
          "peers": [
            {{
              "node": "{node}",
              "label": "lending-mac",
              "addedAt": 1,
              "allow": {{ "allowDisclose": true }},
              "endpoints": [
                {{
                  "kind": "direct",
                  "addr": "{addr}",
                  "source": "paired",
                  "observedAtMs": 1
                }}
              ]
            }}
          ]
        }}"#,
                node = tcr_peer_wire::PeerId(owner.public).to_wire(),
                addr = owner.addr,
            ),
        )
        .expect("write the borrower's peers file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&borrower_peers, std::fs::Permissions::from_mode(0o600))
                .expect("0600 on the peers file");
        }
        let store = teamclaude_rs::peer::config::PeerStore::open(&borrower_peers)
            .expect("the borrower's peers file reads");

        let ask = tcr_peer_wire::LeaseRequest {
            window: Window::SevenDay,
            unit: LeaseUnit::Fraction(0.2),
            ttl_s: 300,
            max_inflight: 2,
        };
        let lease = teamclaude_rs::peer::lease::request_lease(
            &store,
            &tcr_peer_wire::PeerId(owner.public),
            &ask,
        )
        .await
        .expect("the ask reaches the owner")
        .expect("the owner grants a lease in both modes");

        let bearer_of = |lease_id: u128| {
            handed_tokens()
                .lock()
                .expect("the handed-token store's lock")
                .bearer(lease_id, crate_now_ms())
                .map(str::to_string)
        };
        let mut held = None;
        for _ in 0..100 {
            held = bearer_of(lease.lease_id);
            if held.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        if expect_bearer {
            assert_eq!(
                held.as_deref(),
                Some(OWNER_FAKE_BEARER),
                "a hand grant leaves the owner's bearer in this process, under the lease it \
                 was granted for ({:032x})",
                lease.lease_id
            );
        } else {
            assert!(
                held.is_none(),
                "the control: a serve-mode grant hands nothing over, and this process holds \
                 {held:?} for the lease it just took"
            );
        }
    }
}

/// **A hand-mode borrow reports what it spent, and the owner's ledger moves.**
///
/// The review's finding: nothing in production built a `Control::UsageHint`.
/// `usage_hint` existed, the owner's arm that applies one existed, and no
/// caller joined them, so a hand-mode lease was debited by nobody: the request
/// leaves the borrower's Mac, the owner never sees it, and `spent` stayed at
/// 0.0 for the life of the lease. The ceiling `may_relay` enforces was
/// therefore never reached and the owner's own `tcr peer ls` said a lease that
/// had spent the account's window was untouched.
///
/// The whole loop is driven here: the borrower asks for a lease, the owner
/// grants in `hand` mode and pushes its bearer on that session, the borrower
/// serves two requests from its own Mac against an origin whose utilization
/// rises between them, and the OWNER's own ledger is what is asserted.
///
/// **The first answer is charged too**, which is the review's first HIGH here.
/// A rise needs two readings and the second reading is the answer's own header,
/// so the FIRST one has to come from somewhere else: the owner puts its own
/// utilization on the `Handoff` frame and the borrower's meter starts there. It
/// used to start at `None`, so every lease's first answer was free and a
/// borrower that asked for a fresh lease per request was never debited at all.
///
/// Watch it fail by deleting the `report_handed_spend` call from
/// `serve_on_handed_bearer`: no hint is ever sent and `spent` stays at 0.0. Or
/// by seeding `HandedMeter.last` with `None` in `register_handed_meter` and
/// dropping the `note_handed_baseline` call: the ledger then carries 0.07, the
/// second answer's rise alone, and the first answer's 0.10 is never charged.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hand_mode_borrow_reports_its_spend_to_the_owner() {
    let borrower_home = tempfile::tempdir().expect("the borrower's temp home");
    let borrower_peers = borrower_home.path().join("tcr-peers.json");
    let borrower = teamclaude_rs::peer::id::NodeKey::load_or_mint(borrower_home.path())
        .expect("the borrower's node key")
        .id();
    let owner = owner_serving(borrower, "hand").await;

    std::fs::write(
        &borrower_peers,
        format!(
            r#"{{
      "peers": [
        {{
          "node": "{node}",
          "label": "lending-mac",
          "addedAt": 1,
          "allow": {{ "allowDisclose": true }},
          "endpoints": [
            {{
              "kind": "direct",
              "addr": "{addr}",
              "source": "paired",
              "observedAtMs": 1
            }}
          ]
        }}
      ]
    }}"#,
            node = tcr_peer_wire::PeerId(owner.public).to_wire(),
            addr = owner.addr,
        ),
    )
    .expect("write the borrower's peers file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&borrower_peers, std::fs::Permissions::from_mode(0o600))
            .expect("0600 on the peers file");
    }
    let store = teamclaude_rs::peer::config::PeerStore::open(&borrower_peers)
        .expect("the borrower's peers file reads");

    let lease = teamclaude_rs::peer::lease::request_lease(
        &store,
        &tcr_peer_wire::PeerId(owner.public),
        &tcr_peer_wire::LeaseRequest {
            window: Window::SevenDay,
            unit: LeaseUnit::Fraction(0.2),
            ttl_s: 300,
            max_inflight: 2,
        },
    )
    .await
    .expect("the ask reaches the owner")
    .expect("the owner grants a hand-mode lease");

    // The bearer arrives on the same session, unasked: the borrow cannot leave
    // from here until it has.
    for _ in 0..200 {
        let held = handed_tokens()
            .lock()
            .expect("the handed-token store's lock")
            .bearer(lease.lease_id, crate_now_ms())
            .is_some();
        if held {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let origin = FakeOrigin::start_rising().await;
    let base = format!("http://{ORIGIN_HOST}");
    let body = bytes::Bytes::from_static(b"{\"model\":\"fake\"}");
    for request in 1..=2 {
        let served = serve_on_handed_bearer(
            &base,
            &origin.client(),
            &ask(&body),
            lease.lease_id,
            Window::SevenDay,
            crate_now_ms(),
        )
        .await
        .unwrap_or_else(|err| panic!("hand-mode request {request} reaches the origin: {err}"))
        .unwrap_or_else(|| panic!("hand-mode request {request} is served from this Mac"));
        assert_eq!(served.status(), 200);
    }

    // The owner's OWN ledger, which is the only place a debit counts.
    //
    // BOTH answers are charged, and the first one is the point. The meter's
    // baseline used to be `None` until an answer had already been served, so
    // the first answer on every lease was free and a borrower that re-asked per
    // request paid for nothing at all; this waits for the full figure rather
    // than for "something above zero", which the first charge alone would
    // satisfy.
    let expected = (0.40 - OWNER_MEASURED_UTILIZATION) + 0.07;
    let mut spent = 0.0;
    for _ in 0..200 {
        spent = spent_of(
            &owner.ledger.lock().expect("the owner's ledger lock"),
            lease.lease_id,
            crate_now_ms(),
        );
        if (spent - expected).abs() < 1e-6 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        (spent - expected).abs() < 1e-6,
        "the owner's ledger must carry the rise above the baseline its own handoff carried \
         (0.40 - {OWNER_MEASURED_UTILIZATION}) plus the rise between the two answers \
         (0.47 - 0.40), which is {expected}, and it carries {spent}"
    );
}

/// **The owner hands one bearer over, does not re-send it, and stops when the
/// lease is revoked.**
///
/// The bearer travels on the SAME session, unasked, which is the whole shape:
/// the borrower has nothing to ask with, since the token is the only thing it
/// holds and a request for a new one carries no proof the old one is
/// legitimate.
///
/// The bearer here expires inside `HANDOFF_RENEW_LEAD_MS`, so every poll finds
/// a renewal DUE and the only thing stopping a frame is that the token has not
/// changed. That is the case this fixture can reach: the owner's credential is
/// a constant in it, and a refreshed one is measured where it can be, over a
/// sequence of polls, in
/// `exactly_one_push_per_new_bearer_over_the_last_five_minutes`.
///
/// The revoke leg is "revocation IS stop renewing". There is no recall frame,
/// so what the test can observe is the ABSENCE of a later frame, and the
/// absence is bounded by a wait several poll intervals long so that "not yet"
/// cannot pass for "never".
///
/// Watched red by dropping the fingerprint comparison from
/// `lease::handoff_push_is_due`: a second, identical bearer arrives within a
/// second and the count is 2.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_owner_hands_a_bearer_once_and_stops_at_revoke() {
    let (borrower_secret, borrower_public) =
        teamclaude_rs::peer::noise::generate_static().expect("a borrower keypair");
    let borrower = tcr_peer_wire::PeerId(borrower_public);
    // Inside the renewal lead, so every poll finds one due.
    let expires_at_ms = crate_now_ms() + HANDOFF_RENEW_LEAD_MS / 2;
    let owner = owner_lending_expiring(
        borrower,
        r#"{
            "mode": "hand",
            "window": "7d",
            "scope": "all",
            "fraction": 0.2,
            "ttlS": 300,
            "maxInflight": 2
          }"#,
        expires_at_ms,
    )
    .await;

    let mut stream = tokio::net::TcpStream::connect(owner.addr)
        .await
        .expect("connect to the owner");
    let mut session = teamclaude_rs::peer::noise::dial_handshake(
        &mut stream,
        &borrower_secret,
        teamclaude_rs::peer::noise::Handshake::Return,
        Some(&owner.public),
        None,
    )
    .await
    .expect("the borrower is pinned, so the return visit completes");

    let header = tcr_peer_wire::StreamHeader {
        kind: tcr_peer_wire::StreamKind::Control,
        target: None,
        via: Vec::new(),
        hops_remaining: 1,
        request_id: teamclaude_rs::peer::lease::random_id().expect("a request id"),
    };
    teamclaude_rs::peer::serve::send_control(&mut stream, &mut session, &header)
        .await
        .expect("the header goes");
    teamclaude_rs::peer::serve::send_control(
        &mut stream,
        &mut session,
        &Control::LeaseRequest(tcr_peer_wire::LeaseRequest {
            window: Window::SevenDay,
            unit: LeaseUnit::Fraction(0.2),
            ttl_s: 300,
            max_inflight: 2,
        }),
    )
    .await
    .expect("the ask goes");

    let granted: Control = teamclaude_rs::peer::serve::recv_control(&mut stream, &mut session)
        .await
        .expect("the owner answers the ask");
    let Control::LeaseGrant(grant) = granted else {
        panic!("the first answer is the grant, not {granted:?}");
    };
    let minted = grant
        .lease
        .unwrap_or_else(|| panic!("the owner granted a lease, refusal: {:?}", grant.refusal));

    // The bearer that rides the grant, and then nothing while it is unchanged.
    //
    // This assertion used to read "at least three", and that was the review's
    // third finding written down as a test: the owner's credential does not
    // change in this fixture, so those extra frames were the SAME token going
    // out once a second for the whole five-minute lead. The renewal mechanism
    // is measured over a sequence of polls by
    // `exactly_one_push_per_new_bearer_over_the_last_five_minutes`; what is
    // measured here, on the wire, is that the first one arrives and no
    // duplicate of it follows.
    let mut handoffs = 0_usize;
    for _ in 0..2 {
        let read = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            teamclaude_rs::peer::serve::recv_control::<_, Control>(&mut stream, &mut session),
        )
        .await;
        let Ok(Ok(Control::Handoff {
            lease_id,
            access_token,
            ..
        })) = read
        else {
            break;
        };
        assert_eq!(lease_id, minted.lease_id, "each one is for this lease");
        assert_eq!(
            access_token.reveal(),
            OWNER_FAKE_BEARER,
            "and carries the real bearer"
        );
        handoffs += 1;
    }
    assert_eq!(
        handoffs, 1,
        "the grant's own bearer arrives unasked, and the unchanged token is not re-sent on \
         every poll for the rest of its life"
    );

    // Revoke, as an operator does it: the grant leaves the peers file. There is
    // no ledger removal and no recall frame, so this IS the mechanism.
    let peers = owner._home.path().join("tcr-peers.json");
    let raw = std::fs::read_to_string(&peers).expect("the peers file reads");
    let stripped = raw.replace(r#""mode": "hand""#, r#""mode": "serve""#);
    assert_ne!(raw, stripped, "the fixture has a hand grant to revoke");
    std::fs::write(&peers, stripped).expect("rewrite the peers file");
    let after = tokio::time::timeout(
        std::time::Duration::from_secs(4),
        teamclaude_rs::peer::serve::recv_control::<_, Control>(&mut stream, &mut session),
    )
    .await;
    assert!(
        after.is_err(),
        "revocation IS stop renewing: nothing may arrive after it, and this session read \
         {after:?}"
    );
}

// ---------------------------------------------------------------------------
// One answer to "can this account's bearer be handed over", at the CLI
// ---------------------------------------------------------------------------

/// Run `tcr peer <args>` against a temp peers file and a temp config, through
/// the binary this build produced.
///
/// `--peers` and `--config` point the whole surface at temp files, so nothing
/// here reads the real config directory and nothing touches the proxy.
fn run_tcr_peer(
    peers: &std::path::Path,
    config: &std::path::Path,
    args: &[&str],
) -> (String, bool) {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_tcr"))
        .arg("peer")
        .args(args)
        .args(["--peers", peers.to_str().expect("a utf-8 path")])
        .args(["--config", config.to_str().expect("a utf-8 path")])
        .output()
        .unwrap_or_else(|err| panic!("spawn tcr peer {args:?}: {err}"));
    let mut said = String::from_utf8_lossy(&output.stdout).to_string();
    said.push_str(&String::from_utf8_lossy(&output.stderr));
    (said, output.status.success())
}

/// **`--exits-from local --must` then `lend --mode hand` is refused at lend
/// time.**
///
/// # The three predicates, and the one that disagreed
///
/// "This account's bearer cannot be handed over" was written out three times:
/// `Manager::handoff_bearer` and `peer ls`'s handed-key countdown both read
/// `strict`, and the lend verb read `strict && !egress.is_local()`. So an
/// account pinned to THIS Mac with `--must` took a hand grant at the CLI that
/// the serving path then refused to fund on every single request: granted,
/// worth nothing, and silent about it.
///
/// Driven through the shipped binary, both steps, because the disagreement was
/// between a CLI check and a runtime one and a unit test on either side alone
/// is what let it happen.
///
/// Watch it fail: put `&& !account.egress_pin().egress.is_local()` back into
/// the `covered.iter().all(..)` predicate in `run_peer`'s lend arm.
#[test]
fn a_strict_local_pin_refuses_a_hand_grant_at_lend_time() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let peers = dir.path().join("tcr-peers.json");
    let config = dir.path().join("teamclaude.json");
    let node = tcr_peer_wire::PeerId([31_u8; 32]);

    std::fs::write(
        &peers,
        format!(
            r#"{{ "peers": [ {{ "node": "{node}", "label": "attic-nuc", "addedAt": 1 }} ] }}"#,
            node = node.to_wire(),
        ),
    )
    .expect("write the peers file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&peers, std::fs::Permissions::from_mode(0o600))
            .expect("0600 on the peers file, which the CLI refuses to read without");
    }
    std::fs::write(
        &config,
        r#"{
            "proxy": { "port": 0 },
            "accounts": [
                {
                    "name": "alice@example.com",
                    "accessToken": "fake-owner-bearer-not-a-credential",
                    "expiresAt": 1893456000000
                }
            ]
        }"#,
    )
    .expect("write the config");

    // The operator pins the account to THIS Mac and makes it strict.
    let (said, ok) = run_tcr_peer(
        &peers,
        &config,
        &[
            "account",
            "alice@example.com",
            "--exits-from",
            "local",
            "--must",
        ],
    );
    assert!(ok, "the pin has to be written: {said}");
    assert!(
        said.contains("refused rather than sent from this Mac"),
        "and the line has to say the pin is strict: {said}"
    );

    let (said, ok) = run_tcr_peer(
        &peers,
        &config,
        &[
            "lend",
            &node.to_wire(),
            "--window",
            "7d",
            "--fraction",
            "0.2",
            "--mode",
            "hand",
        ],
    );
    assert!(
        !ok,
        "a hand grant over an account pinned strictly to this Mac has to be refused at lend \
         time, or it is granted and then refused on every request: {said}"
    );
    assert!(
        said.contains("egressStrict"),
        "and the refusal has to name the exit lock the operator set: {said}"
    );

    // Nothing was written: a refusal changes nothing on disk.
    let written = std::fs::read_to_string(&peers).expect("the peers file still reads");
    assert!(
        !written.contains("hand"),
        "a refused lend must leave no grant behind: {written}"
    );

    // The mirror leg: the same account without `--must` takes the grant, which
    // is what keeps this a check on strictness and not on pinning.
    let (said, ok) = run_tcr_peer(
        &peers,
        &config,
        &["account", "alice@example.com", "--no-must"],
    );
    assert!(ok, "the pin is relaxed: {said}");
    let (said, ok) = run_tcr_peer(
        &peers,
        &config,
        &[
            "lend",
            &node.to_wire(),
            "--window",
            "7d",
            "--fraction",
            "0.2",
            "--mode",
            "hand",
        ],
    );
    assert!(
        ok,
        "a non-strict pin falls back to local, which is exactly what a hand-mode borrower \
         does, so the grant stands: {said}"
    );
}

/// **An account index the exit locks do not cover is refused, not lent.**
///
/// # The window, and why an absent pin is not an absent lock
///
/// `Manager::handoff_bearer` reads the exit locks off the config and the
/// accounts off the runtime vector, pairing them by index, and takes the two
/// locks one at a time so it can be no half of a deadlock.
/// `Manager::add_account` pushes the runtime row first and the config row
/// second, under the same discipline, so between its two pushes the runtime
/// vector is one longer than the pins. `pins.get(index)` is `None` there, and
/// the old filter read that as "no pin, so not strict": the bearer of an
/// account whose exit lock this code had not seen yet went to a borrower.
///
/// Asserted on the resolver rather than by racing two threads, because a race
/// that has to be LOST to fail is a gate that passes whether the bug is there
/// or not. The window is real and short; the decision is what can be pinned.
///
/// Watch it fail: put `!pins.get(index).is_some_and(..)` back in
/// `EgressPin::may_be_handed` and the unresolvable index reads as lendable.
#[test]
fn an_account_index_the_exit_locks_do_not_cover_is_refused() {
    use teamclaude_rs::config::{Egress, EgressPin};

    let free = EgressPin {
        egress: Egress::Local,
        strict: false,
    };
    let locked = EgressPin {
        egress: Egress::Local,
        strict: true,
    };
    let pins = [free, locked];

    assert!(
        EgressPin::may_be_handed(&pins, 0),
        "a pin that resolves and is not strict is lent like any other account"
    );
    assert!(
        !EgressPin::may_be_handed(&pins, 1),
        "a strict pin is never handed over, which is the rule this shares with every other \
         reader of it"
    );
    assert!(
        !EgressPin::may_be_handed(&pins, 2),
        "and an index past the end is NOT KNOWN rather than NOT PINNED: mid-append the \
         runtime vector is one longer than the pins, and the two answers are the same shape \
         and opposite decisions"
    );
    assert!(
        !EgressPin::may_be_handed(&[], 0),
        "with no pins read at all nothing is lendable"
    );
}
