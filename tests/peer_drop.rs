//! The per-pair drop name and the sealed record: derivation, framing, and
//! every refusal a reader can answer with.
//!
//! # The only network here is a fake this file starts
//!
//! There is no temp file and no real surface. The crypto tests are pure
//! computation over fixed inputs; the store tests run against a fake bound to
//! `127.0.0.1:0`, a port the kernel picks and only this test holds. Nothing
//! reaches the proxy on `127.0.0.1:3456`, the operator's config directory, or
//! anything else outside the test process, and the two templates that name a
//! host on the internet are refused before a connection is attempted.
//!
//! # The values are obviously synthetic
//!
//! Every secret is a counting pattern, every peer id is one repeated byte and
//! every address is from the documentation range (RFC 5737 TEST-NET-3). None
//! of it is anybody's.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::put;
use axum::Router;
use tcr_peer_wire::PeerId;
use teamclaude_rs::peer::drop::{
    self, current_slot, seal, DeadDropStore, DropKeys, DropName, DropRecord, HttpsTemplateStore,
    RecordRefusal, StoreRefusal, DROP_SLOT_SECONDS, MAX_RECORD_AGE, MAX_STORED_RECORD_BYTES,
};

/// A peer id that is plainly not a real one: 32 copies of one byte.
fn peer_id(fill: u8) -> PeerId {
    PeerId([fill; 32])
}

/// The pair secret the ladder tests run on: bytes 0 through 31.
fn counting_secret() -> [u8; 32] {
    let mut secret = [0_u8; 32];
    for (index, byte) in secret.iter_mut().enumerate() {
        // The cast is exact: the array is 32 long and 31 fits in a u8.
        *byte = index as u8;
    }
    secret
}

/// One address, from the documentation range.
fn sample_endpoint() -> SocketAddr {
    "203.0.113.4:7755"
        .parse()
        .expect("a literal socket address from the documentation range")
}

fn record_for(publisher: &PeerId, slot: u64, at: u64) -> DropRecord {
    DropRecord {
        v: 1,
        publisher: *publisher,
        slot,
        at,
        eps: vec![sample_endpoint()],
    }
}

/// Two asserts, both of which must fire: a reader holding the wrong pair
/// secret looks in the wrong place, and cannot read the right place either.
///
/// The first is the one that matters in practice, because it means a wrong
/// reader never even makes a fetch that could hit. The second is the one that
/// matters cryptographically: handed the right name by any means, the wrong
/// seal key still gets nothing.
#[test]
fn a_reader_with_the_wrong_pair_secret_gets_nothing() {
    let publisher = peer_id(0x11);
    let slot = current_slot(1_758_240_000);

    let right = DropKeys::derive(&counting_secret());
    let wrong = DropKeys::derive(&[0xee; 32]);

    let right_name = DropName::for_slot(&right, &publisher, slot);
    let wrong_name = DropName::for_slot(&wrong, &publisher, slot);
    assert_ne!(
        right_name, wrong_name,
        "a different pair secret must name a different location, or a wrong \
         reader fetches the right one's record"
    );

    let sealed = seal(
        &right,
        &right_name,
        &record_for(&publisher, slot, 1_758_240_000),
    )
    .expect("the record seals under the right keys");

    let refusal = drop::open(
        &wrong,
        &right_name,
        slot,
        &publisher,
        &sealed,
        1_758_240_010,
    )
    .expect_err("the wrong seal key must not open the record");
    assert!(
        matches!(refusal, RecordRefusal::DidNotOpen),
        "handed the right name, the wrong key must still get nothing: {refusal}"
    );
}

/// Three asserts, one per defence against an old record served again.
///
/// The name binds the slot, the sealed bytes bind the name, and the freshness
/// ceiling bounds how long a record that IS at its own name stays usable.
#[test]
fn a_replayed_record_from_an_older_slot_is_refused() {
    let publisher = peer_id(0x11);
    let at = 1_758_240_000;
    let slot = current_slot(at);
    let keys = DropKeys::derive(&counting_secret());

    let name = DropName::for_slot(&keys, &publisher, slot);
    let sealed = seal(&keys, &name, &record_for(&publisher, slot, at)).expect("the record seals");

    // Served at the next slot's name: the associated data binds the name, so
    // the bytes do not even authenticate there.
    let next_name = DropName::for_slot(&keys, &publisher, slot + 1);
    let moved = drop::open(&keys, &next_name, slot + 1, &publisher, &sealed, at + 10)
        .expect_err("a record must not open at another slot's name");
    assert!(
        matches!(moved, RecordRefusal::DidNotOpen),
        "moving a record to another name must fail the seal: {moved}"
    );

    // Served at its own name, long after it was written.
    let late = at + MAX_RECORD_AGE.as_secs() + 1;
    let stale = drop::open(&keys, &name, slot, &publisher, &sealed, late)
        .expect_err("a record past the age ceiling must be refused");
    match stale {
        RecordRefusal::Stale { age_s, max_s } => {
            assert_eq!(max_s, MAX_RECORD_AGE.as_secs(), "the ceiling it names");
            assert_eq!(age_s, MAX_RECORD_AGE.as_secs() + 1, "the age it names");
        }
        other => panic!("expected a staleness refusal, got {other}"),
    }

    // Written with an inner slot that is not the one its name belongs to.
    let backdated = seal(&keys, &name, &record_for(&publisher, slot - 5, at))
        .expect("the backdated record seals");
    let wrong_slot = drop::open(&keys, &name, slot, &publisher, &backdated, at + 10)
        .expect_err("an inner slot that disagrees with the name must be refused");
    match wrong_slot {
        RecordRefusal::WrongSlot { found, expected } => {
            assert_eq!(found, slot - 5, "the slot the record claims");
            assert_eq!(expected, slot, "the slot the name belongs to");
        }
        other => panic!("expected a slot refusal, got {other}"),
    }
}

/// The positive control for every other refusal in this file.
///
/// A surface answering with an error document, or with anything that is not a
/// record at all, is told apart from a forgery before any key is used. Green
/// refusals elsewhere are only meaningful because this one proves the refusal
/// path is reached at all rather than the code never running.
#[test]
fn a_store_serving_random_bytes_is_refused_by_name() {
    let publisher = peer_id(0x11);
    let slot = current_slot(1_758_240_000);
    let keys = DropKeys::derive(&counting_secret());
    let name = DropName::for_slot(&keys, &publisher, slot);

    let not_a_record = b"<!doctype html><title>404</title>";
    let refusal = drop::open(&keys, &name, slot, &publisher, not_a_record, 1_758_240_010)
        .expect_err("an error page must not be read as a record");
    match refusal {
        RecordRefusal::NotARecord { found } => {
            assert_eq!(&found, b"<!do", "the refusal names the bytes it found");
        }
        other => panic!("expected a not-a-record refusal, got {other}"),
    }
}

/// Both Macs compute one name for one direction, and the two directions are
/// two different names.
///
/// The second half is the one a bug would hide behind: without the publisher
/// in the derivation both ends would write to one location and overwrite each
/// other every slot, and the first assert alone would still pass.
#[test]
fn both_ends_derive_the_same_name_for_one_direction() {
    let secret = counting_secret();
    let here = DropKeys::derive(&secret);
    let there = DropKeys::derive(&secret);

    let alice = peer_id(0x11);
    let bob = peer_id(0x22);
    let slot = current_slot(1_758_240_000);

    let read_by_alice = DropName::for_slot(&here, &bob, slot);
    let written_by_bob = DropName::for_slot(&there, &bob, slot);
    assert_eq!(
        read_by_alice, written_by_bob,
        "the reader and the writer must land on one name, or neither ever \
         finds the other"
    );

    let alices_own = DropName::for_slot(&here, &alice, slot);
    assert_ne!(
        alices_own, written_by_bob,
        "the two directions must be two names, or each Mac overwrites the \
         other every slot"
    );

    // And a name is 32 lower-case hex characters, which is the only form.
    let wire = written_by_bob.to_wire();
    assert_eq!(wire.len(), 32, "the wire form's width");
    assert!(
        wire.chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
        "the wire form must be lower-case hex: {wire}"
    );
}

/// The key ladder against values computed outside this program.
///
/// A hand-rolled MAC that is only ever compared to its own output is
/// self-consistent and agrees with nobody, the rule `config::hmac_sha256`'s
/// own doc states, and a ladder built on one inherits the problem. Both halves
/// below therefore come from a separate implementation.
///
/// The name proves the root key and the name key; the sealed record proves the
/// root key, the seal key, the associated data and the framing, because a
/// reference implementation sealed it and this build has to open it.
///
/// Re-derive both with (the second needs the `cryptography` package, the first
/// is the standard library alone):
///
/// ```text
/// python3 - <<'PY'
/// import hmac, hashlib, json
/// from cryptography.hazmat.primitives.ciphers.aead import ChaCha20Poly1305
/// ALPH = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"
/// def crockford(b):
///     n = int.from_bytes(b, "big") << 4
///     return "".join(ALPH[(n >> (5 * (51 - i))) & 31] for i in range(52))
/// secret = bytes(range(32))
/// root = hmac.new(secret, b"tcr peer dead-drop root v1" + b"\x01", hashlib.sha256).digest()
/// name_key = hmac.new(root, b"tcr peer dead-drop name v1" + b"\x01", hashlib.sha256).digest()
/// seal_key = hmac.new(root, b"tcr peer dead-drop seal v1" + b"\x01", hashlib.sha256).digest()
/// pub = crockford(bytes([0x11] * 32))
/// slot, at = 488400, 1758240000
/// name = hmac.new(name_key, b"slot" + slot.to_bytes(8, "big") + pub.encode(), hashlib.sha256).digest()[:16]
/// plaintext = json.dumps({"v": 1, "pub": pub, "slot": slot, "at": at,
///                         "eps": ["203.0.113.4:7755"]}, separators=(",", ":")).encode()
/// nonce = bytes(range(12))
/// body = ChaCha20Poly1305(seal_key).encrypt(nonce, plaintext, name + pub.encode())
/// print(pub); print(name.hex()); print((b"TCRD" + bytes([1]) + nonce + body).hex())
/// PY
/// ```
#[test]
fn hmac_ladder_matches_a_hand_computed_vector() {
    /// `PeerId([0x11; 32]).to_wire()`, from the reference encoder.
    const PUBLISHER_WIRE: &str = "248H248H248H248H248H248H248H248H248H248H248H248H248G";
    /// The name for that publisher in slot 488400 under the counting secret.
    const NAME_WIRE: &str = "16dcf087ea3fc7107505dc8248d44c9d";
    /// A record sealed by the reference implementation at that name.
    const SEALED_HEX: &str = "5443524401000102030405060708090a0b44bd14a1e8f03aba62656794edcd\
9675a0cb5995dae9295f868398ed59b2111b7e83d118d21e982b7df994154f6c2a25550ee22521722c9db3808a8779\
92dcea8bdcdf84473ccb8037cccdd782992386b0abc2eef363dee9c42e150f894a65664bdf733f26257c2121f2d196\
f143048c0eeff64513008e13dc39b82d3c2b94e943ca254179cb85d42428a4ac1a";

    let publisher = peer_id(0x11);
    assert_eq!(
        publisher.to_wire(),
        PUBLISHER_WIRE,
        "the derivation eats the publisher's wire form, so the two encoders \
         must already agree"
    );

    let slot = 488_400;
    assert_eq!(
        current_slot(1_758_240_000),
        slot,
        "the slot the vector was computed for, over a {DROP_SLOT_SECONDS}-second slot"
    );

    let keys = DropKeys::derive(&counting_secret());
    assert_eq!(
        DropName::for_slot(&keys, &publisher, slot).to_wire(),
        NAME_WIRE,
        "the name key's output must match the reference ladder"
    );

    let sealed = decode_hex(SEALED_HEX);
    let name = DropName::for_slot(&keys, &publisher, slot);
    let opened = drop::open(&keys, &name, slot, &publisher, &sealed, 1_758_240_010)
        .expect("a record sealed by the reference implementation must open here");
    assert_eq!(opened, record_for(&publisher, slot, 1_758_240_000));

    // And `Debug` on the keys prints no key material.
    assert_eq!(format!("{keys:?}"), "DropKeys(set)");
}

// ---------------------------------------------------------------------------
// A fake surface on loopback, and the calls it saw
// ---------------------------------------------------------------------------

/// An obviously synthetic bearer token. It is a literal in this file and a
/// credential to nothing.
const TEST_TOKEN: &str = "not-a-real-token-0000";

/// How the fake answers.
#[derive(Clone, Copy, Debug)]
enum StoreBehaviour {
    /// Hold what was put and serve it back, 404 for a name nothing was put at.
    /// This is a valid production surface, so a test against it exercises the
    /// shipping client and not a stub.
    Honest,
    /// Answer 500 to everything, the shape of a surface whose backend broke.
    Failing,
    /// Answer a `GET` with one byte more than the ceiling.
    Oversized,
    /// Accept the connection and then say nothing, which is the shape a
    /// connection-level failure cannot stand in for: the socket is open and
    /// the request arrived.
    Silent,
}

/// Every call the fake served, in arrival order.
type CallLog = Arc<Mutex<Vec<String>>>;

/// What the fake is holding, by name.
type Held = Arc<Mutex<HashMap<String, Vec<u8>>>>;

/// A surface standing on loopback: `PUT /drop/{name}` and `GET /drop/{name}`,
/// over one call log, in `tests/peer_reach_upnp.rs`'s shape.
struct FakeStore {
    /// The template a store is built from, hole and all.
    template: String,
    calls: CallLog,
}

impl FakeStore {
    async fn start(behaviour: StoreBehaviour) -> Self {
        let calls: CallLog = Arc::new(Mutex::new(Vec::new()));
        let held: Held = Arc::new(Mutex::new(HashMap::new()));

        let put_calls = Arc::clone(&calls);
        let put_held = Arc::clone(&held);
        let get_calls = Arc::clone(&calls);
        let get_held = Arc::clone(&held);

        let app = Router::new().route(
            "/drop/{name}",
            put(
                move |Path(name): Path<String>, headers: HeaderMap, body: Bytes| {
                    let calls = Arc::clone(&put_calls);
                    let held = Arc::clone(&put_held);
                    async move {
                        log_call(&calls, "put", &name, &headers);
                        match behaviour {
                            StoreBehaviour::Silent => {
                                tokio::time::sleep(Duration::from_secs(60)).await;
                                StatusCode::NO_CONTENT
                            }
                            StoreBehaviour::Failing => StatusCode::INTERNAL_SERVER_ERROR,
                            StoreBehaviour::Honest | StoreBehaviour::Oversized => {
                                held.lock()
                                    .expect("the fake's store is never poisoned")
                                    .insert(name, body.to_vec());
                                StatusCode::NO_CONTENT
                            }
                        }
                    }
                },
            )
            .get(move |Path(name): Path<String>, headers: HeaderMap| {
                let calls = Arc::clone(&get_calls);
                let held = Arc::clone(&get_held);
                async move {
                    log_call(&calls, "get", &name, &headers);
                    match behaviour {
                        StoreBehaviour::Silent => {
                            tokio::time::sleep(Duration::from_secs(60)).await;
                            StatusCode::NO_CONTENT.into_response()
                        }
                        StoreBehaviour::Failing => {
                            StatusCode::INTERNAL_SERVER_ERROR.into_response()
                        }
                        StoreBehaviour::Oversized => {
                            // One byte past the ceiling, so the refusal cannot
                            // be an off-by-one on either side of it.
                            (StatusCode::OK, vec![0_u8; MAX_STORED_RECORD_BYTES + 1])
                                .into_response()
                        }
                        StoreBehaviour::Honest => {
                            let found = held
                                .lock()
                                .expect("the fake's store is never poisoned")
                                .get(&name)
                                .cloned();
                            match found {
                                Some(bytes) => (StatusCode::OK, bytes).into_response(),
                                None => StatusCode::NOT_FOUND.into_response(),
                            }
                        }
                    }
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the fake surface on a loopback port the kernel picks");
        let addr = listener
            .local_addr()
            .expect("the fake surface's own address");
        tokio::spawn(async move {
            let _served = axum::serve(listener, app).await;
        });

        Self {
            // Doubled braces so the hole survives this format string.
            template: format!("http://{addr}/drop/{{name}}"),
            calls,
        }
    }

    fn log(&self) -> Vec<String> {
        self.calls
            .lock()
            .expect("the call log is never poisoned")
            .clone()
    }
}

/// Record one call, and whether it arrived with a bearer token, so a test can
/// assert the fake SERVED it rather than assert an error did not happen.
fn log_call(calls: &CallLog, verb: &str, name: &str, headers: &HeaderMap) {
    let authorized = if headers.contains_key(axum::http::header::AUTHORIZATION) {
        " +auth"
    } else {
        ""
    };
    calls
        .lock()
        .expect("the call log is never poisoned")
        .push(format!("{verb} {name}{authorized}"));
}

// ---------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------

/// A record goes out to a real `HttpsTemplateStore`, comes back from the fake
/// and opens, and the fake's own log says it served every call.
///
/// The log is the point. A round trip asserted as "no error was returned"
/// would pass against a client that never made a request at all, and against a
/// fake that answered 200 to everything without storing anything.
#[tokio::test]
async fn a_fake_store_on_loopback_round_trips_a_record() {
    let fake = FakeStore::start(StoreBehaviour::Honest).await;
    let store =
        HttpsTemplateStore::with_timeout(&fake.template, Some(TEST_TOKEN), Duration::from_secs(5))
            .expect("a loopback template with a hole is a store");

    let publisher = peer_id(0x11);
    let at = 1_758_240_000;
    let slot = current_slot(at);
    let keys = DropKeys::derive(&counting_secret());
    let name = DropName::for_slot(&keys, &publisher, slot);
    let sealed = seal(&keys, &name, &record_for(&publisher, slot, at)).expect("the record seals");

    store
        .put(&name, &sealed)
        .await
        .expect("the fake accepts the put");

    let fetched = store
        .get(&name)
        .await
        .expect("the fake answers the get")
        .expect("the record is at its own name");
    assert_eq!(
        fetched, sealed,
        "the bytes fetched must be the bytes published, unchanged"
    );

    let opened = drop::open(&keys, &name, slot, &publisher, &fetched, at + 10)
        .expect("a record that survived the round trip must open");
    assert_eq!(
        opened.eps,
        vec![sample_endpoint()],
        "the address the publisher left is the address the reader gets"
    );

    // A name nothing was ever written to: absence is an ordinary outcome.
    let empty = DropName::for_slot(&keys, &publisher, slot + 1);
    let missing = store
        .get(&empty)
        .await
        .expect("a 404 is an outcome, not an error");
    assert!(
        missing.is_none(),
        "nothing at a name must read as nothing rather than as bytes"
    );

    assert_eq!(
        fake.log(),
        vec![
            format!("put {} +auth", name.to_wire()),
            format!("get {} +auth", name.to_wire()),
            format!("get {} +auth", empty.to_wire()),
        ],
        "the fake must have served all three calls, at the names the pair \
         derived, each carrying the bearer token"
    );

    // And the token reaches none of the renderings a human or a log line gets.
    let rendered = format!("{store:?}");
    assert!(
        !rendered.contains(TEST_TOKEN),
        "the bearer token must not reach Debug: {rendered}"
    );
}

/// A surface whose backend broke answers 500, and both legs name the status
/// and the drop name rather than collapsing to one word.
#[tokio::test]
async fn a_surface_answering_500_is_refused_with_its_status() {
    let fake = FakeStore::start(StoreBehaviour::Failing).await;
    let store = HttpsTemplateStore::with_timeout(&fake.template, None, Duration::from_secs(5))
        .expect("a loopback template with a hole is a store");

    let publisher = peer_id(0x11);
    let slot = current_slot(1_758_240_000);
    let keys = DropKeys::derive(&counting_secret());
    let name = DropName::for_slot(&keys, &publisher, slot);

    let refused_put = store
        .put(&name, b"sealed bytes")
        .await
        .expect_err("a 500 must not read as a successful publish");
    match refused_put {
        StoreRefusal::Status {
            status,
            name: named,
        } => {
            assert_eq!(status, 500, "the status the surface answered");
            assert_eq!(named, name.to_wire(), "the name the call was for");
        }
        other => panic!("expected a status refusal on the put, got {other}"),
    }

    let refused_get = store
        .get(&name)
        .await
        .expect_err("a 500 must not read as nothing at that name");
    assert!(
        matches!(refused_get, StoreRefusal::Status { status: 500, .. }),
        "a 500 must be a status refusal and never an Ok(None): {refused_get}"
    );

    assert_eq!(
        fake.log().len(),
        2,
        "both calls must have reached the fake, or the refusals prove nothing"
    );
}

/// A surface that answers with more than the ceiling is stopped while the body
/// is read.
///
/// The assert that matters is the refusal's own ceiling figure: it proves the
/// limit that fired is this module's, not a timeout or a connection error that
/// happens to look like a refusal.
#[tokio::test]
async fn an_answer_over_the_ceiling_is_refused_while_it_is_read() {
    let fake = FakeStore::start(StoreBehaviour::Oversized).await;
    let store = HttpsTemplateStore::with_timeout(&fake.template, None, Duration::from_secs(5))
        .expect("a loopback template with a hole is a store");

    let publisher = peer_id(0x11);
    let slot = current_slot(1_758_240_000);
    let keys = DropKeys::derive(&counting_secret());
    let name = DropName::for_slot(&keys, &publisher, slot);

    let refusal = store
        .get(&name)
        .await
        .expect_err("a body past the ceiling must not be held and returned");
    match refusal {
        StoreRefusal::TooLarge {
            name: named,
            ceiling,
        } => {
            assert_eq!(named, name.to_wire(), "the name the call was for");
            assert_eq!(
                ceiling, MAX_STORED_RECORD_BYTES,
                "the refusal names the ceiling it enforced"
            );
        }
        other => panic!("expected a size refusal, got {other}"),
    }

    assert_eq!(
        fake.log(),
        vec![format!("get {}", name.to_wire())],
        "the fake must have served the oversized answer"
    );
}

/// A surface that accepts the connection and then says nothing is abandoned at
/// the configured timeout, not waited on.
///
/// The socket is open and the request arrived, which the fake's own log
/// proves: this is the failure a connection error cannot stand in for.
#[tokio::test]
async fn a_surface_that_never_answers_times_out() {
    let fake = FakeStore::start(StoreBehaviour::Silent).await;
    let within = Duration::from_millis(300);
    let store = HttpsTemplateStore::with_timeout(&fake.template, None, within)
        .expect("a loopback template with a hole is a store");

    let publisher = peer_id(0x11);
    let slot = current_slot(1_758_240_000);
    let keys = DropKeys::derive(&counting_secret());
    let name = DropName::for_slot(&keys, &publisher, slot);

    let started = Instant::now();
    let refusal = store
        .get(&name)
        .await
        .expect_err("a surface that never answers must not be waited on forever");
    let waited = started.elapsed();

    match refusal {
        StoreRefusal::TimedOut {
            name: named,
            within_ms,
        } => {
            assert_eq!(named, name.to_wire(), "the name the call was for");
            assert_eq!(
                within_ms,
                within.as_millis(),
                "the refusal names the timeout it gave up at"
            );
        }
        other => panic!("expected a timeout refusal, got {other}"),
    }
    assert!(
        waited < Duration::from_secs(5),
        "the call must give up at its own timeout, and it waited {waited:?}"
    );
    assert_eq!(
        fake.log(),
        vec![format!("get {}", name.to_wire())],
        "the request must have reached the fake, or this is a connection \
         failure wearing a timeout's name"
    );
}

/// A template with no hole is refused when it is read, not at the first
/// publish an hour later.
#[test]
fn a_template_with_no_name_hole_is_refused_at_construction() {
    let refusal = HttpsTemplateStore::new("https://drop.example.com/records", None)
        .expect_err("a template with no hole must not build a store");
    assert!(
        refusal.to_string().contains("{name}"),
        "the refusal must name the hole that is missing: {refusal}"
    );

    // The positive control: the same URL with the hole is a store, so the
    // refusal above is about the hole and not about the URL.
    HttpsTemplateStore::new("https://drop.example.com/records/{name}", None)
        .expect("the same template with a hole is a store");
}

/// Plain `http` is accepted for loopback and for nothing else.
///
/// Both halves are asserted. A rule that only refuses is indistinguishable
/// from a rule that refuses everything, and the fake in this file is a plain
/// `http` surface that has to keep working.
#[test]
fn plain_http_is_accepted_only_for_a_loopback_host() {
    for accepted in [
        "http://127.0.0.1:8080/drop/{name}",
        "http://[::1]:8080/drop/{name}",
        "http://localhost:8080/drop/{name}",
        "https://drop.example.com/records/{name}",
    ] {
        HttpsTemplateStore::new(accepted, None)
            .unwrap_or_else(|err| panic!("{accepted} must be a store: {err}"));
    }

    let refusal = HttpsTemplateStore::new("http://drop.example.com/records/{name}", None)
        .expect_err("plain http to a host on the internet must be refused");
    assert!(
        refusal.to_string().contains("https"),
        "the refusal must say what is required instead: {refusal}"
    );
}

/// Lower-case hex into bytes, for the vector above. Panics on anything that is
/// not a pair of hex digits, which is what a test wants of a constant it wrote
/// itself.
fn decode_hex(hex: &str) -> Vec<u8> {
    assert!(hex.len().is_multiple_of(2), "hex comes in pairs");
    (0..hex.len() / 2)
        .map(|index| {
            let pair = hex
                .get(index * 2..index * 2 + 2)
                .expect("the slice is inside a string whose length was checked");
            u8::from_str_radix(pair, 16).expect("the vector is lower-case hex")
        })
        .collect()
}
