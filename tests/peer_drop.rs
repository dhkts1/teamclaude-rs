//! The per-pair drop name and the sealed record: derivation, framing, and
//! every refusal a reader can answer with. Also the store round trip, against
//! a fake standing in for a real `HttpsTemplateStore` target.
//!
//! # What touches a network in this file
//!
//! Most of this file is pure computation over fixed inputs and reaches
//! nothing outside the test process. The one exception is the fake below: it
//! binds `127.0.0.1:0`, nothing here leaves loopback, and no config
//! directory is read anywhere in this file.
//!
//! # The values are obviously synthetic
//!
//! Every secret is a counting pattern, every peer id is one repeated byte and
//! every address is from the documentation range (RFC 5737 TEST-NET-3). None
//! of it is anybody's.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use tcr_peer_wire::PeerId;
use teamclaude_rs::peer::drop::{
    self, current_slot, seal, DeadDropStore, DropKeys, DropName, DropRecord, HttpsTemplateStore,
    RecordRefusal, DROP_SLOT_SECONDS, MAX_RECORD_AGE,
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

// ---------------------------------------------------------------------------
// A fake `HttpsTemplateStore` target on loopback, and its call log
// ---------------------------------------------------------------------------

/// Every call the fake saw, in arrival order: `"put <name> auth=<header>"`,
/// `"get <name>"`. The header is recorded on the call itself, not inferred
/// from the absence of an error, so a test can prove the token arrived.
type CallLog = Arc<Mutex<Vec<String>>>;

/// What the fake currently holds, name to bytes.
type StoredRecords = Arc<Mutex<HashMap<String, Vec<u8>>>>;

#[derive(Clone)]
struct FakeStoreState {
    calls: CallLog,
    records: StoredRecords,
}

/// Bind a loopback TCP listener and serve `PUT /{name}` and `GET /{name}`
/// over a shared map, on a dedicated thread with its own runtime, the shape
/// `spawn_http_server` in `tests/peer_reach_upnp.rs:161` serves the fake
/// UPnP device in.
fn spawn_fake_store(calls: CallLog) -> SocketAddr {
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind the fake store");
    std_listener
        .set_nonblocking(true)
        .expect("the fake store's listener must be non-blocking for tokio");
    let addr = std_listener
        .local_addr()
        .expect("the fake store's own address");

    let state = FakeStoreState {
        calls,
        records: Arc::new(Mutex::new(HashMap::new())),
    };

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime for the fake store");
        rt.block_on(async move {
            let app = Router::new()
                .route("/{name}", get(fake_store_get).put(fake_store_put))
                .with_state(state);
            let listener = tokio::net::TcpListener::from_std(std_listener)
                .expect("the fake store's listener must convert to a tokio one");
            let _served = axum::serve(listener, app).await;
        });
    });

    addr
}

/// `PUT /{name}`: record the call, the bearer header, and the body, then
/// answer `204`.
async fn fake_store_put(
    Path(name): Path<String>,
    headers: HeaderMap,
    State(state): State<FakeStoreState>,
    body: Bytes,
) -> StatusCode {
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("<none>")
        .to_string();
    state
        .calls
        .lock()
        .expect("the fake store's call log is never poisoned")
        .push(format!("put {name} auth={auth}"));
    state
        .records
        .lock()
        .expect("the fake store's map is never poisoned")
        .insert(name, body.to_vec());
    StatusCode::NO_CONTENT
}

/// `GET /{name}`: record the call, then answer what was last `PUT` there, or
/// `404` if nothing was.
async fn fake_store_get(Path(name): Path<String>, State(state): State<FakeStoreState>) -> Response {
    state
        .calls
        .lock()
        .expect("the fake store's call log is never poisoned")
        .push(format!("get {name}"));
    let found = state
        .records
        .lock()
        .expect("the fake store's map is never poisoned")
        .get(&name)
        .cloned();
    match found {
        Some(bytes) => Response::builder()
            .status(200)
            .body(Body::from(bytes))
            .expect("the fake store's answer builds"),
        None => Response::builder()
            .status(404)
            .body(Body::empty())
            .expect("the fake store's 404 answer builds"),
    }
}

/// A real `HttpsTemplateStore`, pointed at the loopback fake above, seals a
/// record, `put`s it, `get`s it back, and opens it: the round trip the whole
/// backend exists for.
///
/// The absence of an error is not evidence the fake was reached: a store
/// whose `put` silently dropped the call and whose `get` silently returned
/// `Ok(None)` would also pass a test that only checked the round trip's
/// result. The call log is the second, independent check that the fake
/// actually served a `PUT` and a `GET` at the same name, in that order, and
/// that the bearer token this store was built with actually arrived.
#[tokio::test]
async fn a_fake_store_on_loopback_round_trips_a_record() {
    let calls: CallLog = Arc::new(Mutex::new(Vec::new()));
    let addr = spawn_fake_store(Arc::clone(&calls));

    let publisher = peer_id(0x11);
    let at = 1_758_240_000;
    let slot = current_slot(at);
    let keys = DropKeys::derive(&counting_secret());
    let name = DropName::for_slot(&keys, &publisher, slot);
    let sealed = seal(&keys, &name, &record_for(&publisher, slot, at)).expect("the record seals");

    let template = format!("http://{addr}/{{name}}");
    let store = HttpsTemplateStore::new(&template, Some("testtoken123"))
        .expect("a template with a {name} placeholder must build");

    store
        .put(&name, &sealed)
        .await
        .expect("the fake store must accept the put");

    let fetched = store
        .get(&name)
        .await
        .expect("the fake store must answer the get")
        .expect("the fake store must have what was just put");
    assert_eq!(fetched, sealed, "the bytes must round-trip unchanged");

    let opened = drop::open(&keys, &name, slot, &publisher, &fetched, at + 10)
        .expect("the round-tripped record must still open");
    assert_eq!(opened, record_for(&publisher, slot, at));

    let name_wire = name.to_wire();
    let log = calls
        .lock()
        .expect("the fake store's call log is never poisoned")
        .clone();
    assert_eq!(
        log,
        vec![
            format!("put {name_wire} auth=Bearer testtoken123"),
            format!("get {name_wire}"),
        ],
        "the fake must have been served a put and then a get at the same \
         name, with the bearer token attached, not merely have answered \
         without an error: {log:?}"
    );
}

/// A friend this node no longer pins is refused before the store is ever
/// asked, not merely refused to write.
///
/// The absence of an error is not the assertion here: a `fetch_for` that
/// called the store unconditionally and then quietly dropped the answer for
/// a row it could not find would also return `Ok(0)`. The call log is the
/// independent check that the store was never reached at all.
#[tokio::test]
async fn a_record_from_a_revoked_peer_is_refused() {
    let calls: CallLog = Arc::new(Mutex::new(Vec::new()));
    let addr = spawn_fake_store(Arc::clone(&calls));
    let template = format!("http://{addr}/{{name}}");
    let store = HttpsTemplateStore::new(&template, None).expect("a template store builds");

    let friend = peer_id(0x44);
    let dir = tempfile::tempdir().expect("a temp dir for the peers file");
    let path = dir.path().join("tcr-peers.json");

    // Pinned, switched on, and holding a rendezvous secret: everything a live
    // fetch needs, so the row being gone is the only reason the fetch below
    // finds nothing.
    let mut file = teamclaude_rs::peer::config::PeerFile::default();
    file.peers.push(teamclaude_rs::peer::config::PeerRow {
        node: friend,
        label: "revoked".to_string(),
        endpoints: Vec::new(),
        added_at: 0,
        allow: teamclaude_rs::peer::config::Allow {
            control: teamclaude_rs::peer::config::ControlGrants {
                drop: true,
                ..Default::default()
            },
            ..Default::default()
        },
        lend: Vec::new(),
        rendezvous_secret: Some(counting_secret()),
        sees_us_at: None,
    });
    teamclaude_rs::peer::config::save(&path, &file).expect("the pinned row writes");

    // `tcr peer forget`'s own effect: the row is gone.
    let mut revoked = file.clone();
    revoked.peers.clear();
    teamclaude_rs::peer::config::save(&path, &revoked).expect("the revocation writes");

    let fetched = drop::fetch_for(&store, &path, &friend, 1_758_240_000)
        .await
        .expect("a revoked peer is not an error, it is nothing to fetch");
    assert!(fetched.is_empty(), "a revoked peer's drop writes nothing");

    let log = calls
        .lock()
        .expect("the fake store's call log is never poisoned")
        .clone();
    assert!(
        log.is_empty(),
        "fetch_for must not call the store at all for a peer this node no longer pins, \
         so the absence of an error here is not the assertion, the call count is: {log:?}"
    );
}
