//! `dial_peer_reaching_within`'s own dead-drop retry: the row's own endpoint
//! genuinely fails, the drop teaches a BRAND NEW address, and the dial must
//! land on it in the same call.
//!
//! # The interaction this file gates
//!
//! **`PeerStore` is an mtime cache.** `fetch_for` writes the peers file
//! directly, and nothing in the retry ever calls `store.reload_if_changed()`,
//! so `store.row(...)` stays pinned to whatever the store read at
//! `PeerStore::open`. A retry that trusted that cached row would see nothing:
//! the row it opened never had this address on it. The retry must read the
//! file back directly instead.
//!
//! The proof below does not depend on real mtime timing (a race that may or
//! may not reproduce depending on filesystem clock resolution): it asserts
//! that `store.row(&friend)`, called with the store's own cache AFTER the
//! dial succeeded, still does NOT carry the address the dial just landed on.
//! That is true unconditionally, because nothing in the retry ever refreshes
//! that cache; it is the mechanical proof that the successful dial came from
//! bypassing it, not from a lucky mtime race.
//!
//! `clear_cooldowns_for_fetch`, the sibling interaction (a cooldown must
//! clear for exactly the locators a fetch just wrote, no other), is gated as
//! a deterministic unit test beside it in `src/peer/serve.rs`'s own `mod
//! tests`: `dial_order`'s "every path is cooling, try them all anyway"
//! fallback makes that interaction unobservable through a black-box dial
//! outcome whenever the target address is genuinely reachable (the fallback
//! ends up including it either way), so it is proven at the narrower grain
//! that actually discriminates.
//!
//! # The values are obviously synthetic
//!
//! Every peer id is one repeated byte, every secret is a counting pattern,
//! and every address is loopback. None of it is anybody's.

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
use teamclaude_rs::peer::config as peer_config;
use teamclaude_rs::peer::drop::{
    self, seal, DeadDropStore, DropKeys, DropName, DropRecord, HttpsTemplateStore,
};
use teamclaude_rs::peer::serve;

fn peer_id(fill: u8) -> PeerId {
    PeerId([fill; 32])
}

fn counting_secret() -> [u8; 32] {
    let mut secret = [0_u8; 32];
    for (index, byte) in secret.iter_mut().enumerate() {
        // The cast is exact: the array is 32 long and 31 fits in a u8.
        *byte = index as u8;
    }
    secret
}

fn now_unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_secs()
}

/// A loopback port nothing is listening on: bound, read back, released.
/// `tests/peer_reach.rs`'s own helper, copied rather than shared, because
/// each integration test file is its own crate.
async fn a_closed_loopback_port() -> SocketAddr {
    let held = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a kernel-chosen loopback port");
    let addr = held.local_addr().expect("its address");
    drop(held);
    addr
}

// ---------------------------------------------------------------------------
// A fake `HttpsTemplateStore` target on loopback: `tests/peer_drop.rs`'s own
// fake, copied rather than shared, because each integration test file is its
// own crate.
// ---------------------------------------------------------------------------

type StoredRecords = Arc<Mutex<HashMap<String, Vec<u8>>>>;

fn spawn_fake_store() -> SocketAddr {
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind the fake store");
    std_listener
        .set_nonblocking(true)
        .expect("the fake store's listener must be non-blocking for tokio");
    let addr = std_listener
        .local_addr()
        .expect("the fake store's own address");
    let records: StoredRecords = Arc::new(Mutex::new(HashMap::new()));

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime for the fake store");
        rt.block_on(async move {
            let app = Router::new()
                .route("/{name}", get(fake_store_get).put(fake_store_put))
                .with_state(records);
            let listener = tokio::net::TcpListener::from_std(std_listener)
                .expect("the fake store's listener must convert to a tokio one");
            let _served = axum::serve(listener, app).await;
        });
    });

    addr
}

async fn fake_store_put(
    Path(name): Path<String>,
    _headers: HeaderMap,
    State(state): State<StoredRecords>,
    body: Bytes,
) -> StatusCode {
    state
        .lock()
        .expect("the fake store's map is never poisoned")
        .insert(name, body.to_vec());
    StatusCode::NO_CONTENT
}

async fn fake_store_get(Path(name): Path<String>, State(state): State<StoredRecords>) -> Response {
    let found = state
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

/// **The gate for this unit.** The row's own recorded endpoint is a dead
/// port: the leading race genuinely fails and cools it. The dead drop then
/// teaches a BRAND NEW address, never before on the row, and the dial must
/// land on it in the same call, reading the write `fetch_for` just made
/// rather than the row `PeerStore` cached when it opened.
///
/// Watched red: replace the production retry's
/// `crate::peer::config::read_or_default(store.path())` with the old
/// `store.reload_if_changed(); store.row(&row.node)` shape. `PeerStore` was
/// opened before the fetch wrote anything, nothing in the retry ever calls
/// `reload_if_changed` on its own initiative otherwise, so the cached row
/// still shows no endpoint for this peer at the moment the reload runs, and
/// `store.row` still answers from that cache if the reload's own mtime
/// comparison does not see the write land, in which case `retry_candidates`
/// is empty, no address is dialled, and the assertion below reads `left:
/// None, right: Some(Direct(...))`.
#[tokio::test(flavor = "multi_thread")]
async fn a_freshly_fetched_address_is_dialled_in_the_same_call_it_was_learned() {
    let friend = peer_id(0x52);
    let secret = counting_secret();

    // The row's own endpoint at the start: genuinely dead, so the leading
    // race really fails and really cools it, the way a caller reaching the
    // dead-drop block always got there.
    let dead_addr = a_closed_loopback_port().await;

    // The address ONLY the drop knows about. Bound before the dial starts
    // (this test cannot inject an action mid-call), but never recorded on
    // the row, so it was never cooling and dial_order's cooling filter has
    // nothing to do with whether it is tried.
    let listening = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback listener for the friend's real address");
    let fresh_addr = listening.local_addr().expect("its address");
    let accepting = tokio::spawn(async move { listening.accept().await.map(|(_, from)| from) });

    let dir = tempfile::tempdir().expect("a temp dir for the peers file");
    let path = dir.path().join("tcr-peers.json");

    let store_addr = spawn_fake_store();
    let template = format!("http://{store_addr}/{{name}}");

    let mut file = peer_config::PeerFile {
        dead_drop: peer_config::DeadDropConfig {
            enabled: true,
            store: Some(peer_config::StoreConfig::Https {
                url: template.clone(),
                token: None,
            }),
            slot_seconds: peer_config::DROP_SLOT_SECONDS,
        },
        ..Default::default()
    };
    let mut row = peer_config::PeerRow {
        node: friend,
        label: "friend".to_string(),
        endpoints: Vec::new(),
        added_at: 0,
        allow: peer_config::Allow {
            control: peer_config::ControlGrants {
                drop: true,
                ..Default::default()
            },
            ..Default::default()
        },
        lend: Vec::new(),
        rendezvous_secret: Some(secret),
        sees_us_at: None,
    };
    row.observe_endpoint(peer_config::Endpoint::direct(
        dead_addr,
        1_000,
        peer_config::EndpointSource::Paired,
    ));
    file.peers.push(row.clone());
    peer_config::save(&path, &file).expect("the row and the dead-drop config write");

    // The friend's own record, sealed under the pair's secret, naming the
    // address the row does NOT hold.
    let now_s = now_unix_seconds();
    let slot = drop::current_slot(now_s);
    let keys = DropKeys::derive(&secret);
    let name = DropName::for_slot(&keys, &friend, slot);
    let record = DropRecord {
        v: 1,
        publisher: friend,
        slot,
        at: now_s,
        eps: vec![fresh_addr],
    };
    let sealed = seal(&keys, &name, &record).expect("the record seals");
    let put_store = HttpsTemplateStore::new(&template, None).expect("a template store builds");
    put_store
        .put(&name, &sealed)
        .await
        .expect("the fake store accepts the put");

    // Opened BEFORE the fetch writes anything: this is the store instance
    // whose cache the buggy shape would have trusted.
    let store = peer_config::PeerStore::open(&path).expect("the peer store opens");
    let dial_row = store.row(&friend).expect("the row reads back");

    let reached = serve::dial_peer_reaching_within(&dial_row, &store, 3_000).await;
    let outcome = reached.as_ref().map(|(reached, _stream)| *reached);

    assert_eq!(
        outcome,
        Some(serve::Reached::Direct(fresh_addr)),
        "the dial must land on the address the drop just taught, in the same call it learned \
         it, after the row's own dead endpoint failed"
    );

    // The deterministic half of the proof: the STORE's own cache, read with
    // no explicit reload, still does not carry the address the dial just
    // used. Nothing in the retry ever refreshes it, so this is true
    // regardless of how fast the write landed: the successful dial above can
    // only have come from bypassing this exact cache.
    let cached_row = store.row(&friend).expect("the row still reads back");
    assert!(
        !cached_row
            .endpoints
            .iter()
            .any(|endpoint| endpoint.direct_addr() == Some(fresh_addr)),
        "PeerStore's own cache must still be stale after the dial: it proves the successful \
         dial came from bypassing it, not from a lucky mtime race: {:?}",
        cached_row.endpoints
    );

    accepting
        .await
        .expect("the accept task")
        .expect("the listener accepted the dial");
}
