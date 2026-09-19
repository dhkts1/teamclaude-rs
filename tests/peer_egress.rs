//! The blind carry, end to end and at its edges.
//!
//! # What is real here and what is a stand-in
//!
//! Real: the Noise handshake and session, the stream header, the production
//! authorization gate (`listener::peer_stream_gate_rows`), the production
//! gateway handler (`tunnel::handle_tunnel_on`), the production requester side
//! (`egress::accept_once_splice`), a real rustls client, a real TLS handshake
//! and a real HTTP response carried through the splice.
//!
//! Stand-ins, both named at their call site: the gateway's accept loop, for the
//! reason `gateway_on` gives, and
//! `the_real_listener_hands_a_tunnel_to_the_production_handler` drives it, but
//! it carries with `OriginRoute::Resolve` and a completed carry on this box
//! must not resolve a real name; and the origin's address, because the
//! allow-list is by
//! NAME (`api.anthropic.com`) and a test cannot rewrite the machine's resolver,
//! so the gateway is handed `OriginRoute::Fixed` while production always
//! resolves.
//!
//! # House rules this file is built to
//!
//! Every socket binds `127.0.0.1:0` (kernel-chosen), every file is under a
//! process-and-thread-unique scratch directory, no account is real
//! (`alice@example.com`, `1111…`), and nothing reads the operator's config
//! directory or cache directory: `egress::install_peers_path` points the seam
//! at the scratch copy before any request runs. Nothing here touches the proxy on
//! `127.0.0.1:3456`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tcr_peer_wire::{PeerId, StreamHeader, StreamKind, TunnelTarget};
use teamclaude_rs::config::{Account, Config, PacingConfig, ProxyConfig, ThrottleConfig};
use teamclaude_rs::manager::Manager;
use teamclaude_rs::oauth::{OAuthError, RefreshFuture, TokenRefresher};
use teamclaude_rs::peer::config::{Allow, Endpoint, EndpointSource, PeerFile, PeerRow};
use teamclaude_rs::peer::egress::{self, EgressState, GatewayCandidate, ViaSetting};
use teamclaude_rs::peer::listener;
use teamclaude_rs::peer::noise::{self, Handshake};
use teamclaude_rs::peer::tunnel::{self, Admission, Carry, OriginRoute, TunnelBudget};
use teamclaude_rs::probe::{ProbeError, ProbeFuture, UsageProber};
use teamclaude_rs::warmer::{AccountWarmer, WarmError, WarmFuture};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tower::ServiceExt as _;

/// The one origin this mesh carries, and the name every certificate, SNI check
/// and allow-list decision in this file is about.
const ORIGIN: &str = "api.anthropic.com";

/// What the fake origin answers, so a carried body is recognisable at the far
/// end rather than merely non-empty.
const CANNED_BODY: &str =
    r#"{"id":"msg_carried","type":"message","content":[{"type":"text","text":"carried"}]}"#;

/// A scratch directory named after this process and thread, so five lanes
/// running at once never collide on one path.
fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tcr-peer-egress-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the scratch dir");
    dir
}

// ---------------------------------------------------------------------------
// Two Macs
// ---------------------------------------------------------------------------

/// One machine's key material, as a test holds it.
struct Node {
    secret: [u8; 32],
    id: PeerId,
}

fn node() -> Node {
    let (secret, public) = noise::generate_static().expect("mint a static keypair");
    Node {
        secret,
        id: PeerId(public),
    }
}

/// The fixture spelling of "these are the sockets this row was paired over".
///
/// A fixture address that is not a socket address is a defect in the fixture,
/// so it panics here rather than shrinking the endpoint list quietly and
/// leaving a candidate-set assertion to fail three screens away.
fn paired_endpoints(addrs: &[String]) -> Vec<Endpoint> {
    addrs
        .iter()
        .map(|addr| {
            Endpoint::direct(
                addr.parse().expect("a fixture address is a socket address"),
                0,
                EndpointSource::Paired,
            )
        })
        .collect()
}

/// A pinned row for `peer`, with `gateway` granted or not.
fn row(peer: &PeerId, label: &str, addrs: Vec<String>, gateway: bool) -> PeerRow {
    PeerRow {
        node: *peer,
        label: label.to_string(),
        endpoints: paired_endpoints(&addrs),
        added_at: 1_767_225_600_000,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: Allow {
            gateway,
            ..Allow::default()
        },
        lend: Vec::new(),
    }
}

/// The gateway's half, as `src/peer/listener.rs`'s `StreamKind::Tunnel` arm
/// runs it: accept the session, read the header, run the PRODUCTION gate, then
/// hand the stream to the PRODUCTION handler.
///
/// Test-only only in its accept loop. The three calls inside it are the three
/// the listener arm makes, in the same order, which is why this proves
/// something about the shipped path rather than about itself.
///
/// **It is no longer standing in for a missing arm.** The
/// real dispatch is landed, and
/// `the_real_listener_hands_a_tunnel_to_the_production_handler` measures it
/// end to end. What this keeps is the one thing that arm cannot give a test on
/// this box: an origin reached by ADDRESS. The arm carries with
/// `OriginRoute::Resolve`, so any test of a completed carry through it would
/// resolve `api.anthropic.com` for real.
fn gateway_on(
    listener: TcpListener,
    secret: [u8; 32],
    rows: Vec<PeerRow>,
    route: OriginRoute,
    cap_bytes: u64,
) -> tokio::task::JoinHandle<anyhow::Result<(u64, u64)>> {
    gateway_on_target(listener, secret, rows, route, cap_bytes, None)
}

/// [`gateway_on`], with the handler's target taken from the caller instead of
/// from the header.
///
/// It was built for a case that no longer exists: `TunnelTarget::Peer` could
/// not be serialized at all, so a relay header could not be written by
/// anybody and the handler's own relay refusal had to be driven directly, and
/// it is kept now for a narrower one: driving the handler with a target the
/// header did NOT carry is how a caller proves the refusal below is the
/// HANDLER's and not the gate's, which is the distinction
/// `a_relay_target_is_refused_by_a_gateway` measures.
fn gateway_on_target(
    listener: TcpListener,
    secret: [u8; 32],
    rows: Vec<PeerRow>,
    route: OriginRoute,
    cap_bytes: u64,
    target_override: Option<TunnelTarget>,
) -> tokio::task::JoinHandle<anyhow::Result<(u64, u64)>> {
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        serve_one_carry(stream, &secret, &rows, route, cap_bytes, target_override).await
    })
}

/// One accepted socket, carried the way the production listener carries it.
///
/// Split out of [`gateway_on_target`] when the exit-lock tests needed a gateway
/// that serves SEVERAL carries in a row ([`gateway_serving`]): one body, two
/// accept loops, rather than a second copy of the three production calls that
/// could drift from this one.
async fn serve_one_carry(
    mut stream: TcpStream,
    secret: &[u8; 32],
    rows: &[PeerRow],
    route: OriginRoute,
    cap_bytes: u64,
    target_override: Option<TunnelTarget>,
) -> anyhow::Result<(u64, u64)> {
    // A pinned requester on an IK return: the same authorization the
    // production responder applies, between message 1 and message 2.
    let pin_rows = rows.to_vec();
    let mut session = noise::accept_handshake(
        &mut stream,
        secret,
        noise::Handshake::Return,
        &[],
        move |remote| noise::pin_check_rows(remote, &pin_rows),
    )
    .await?;
    let frame = noise::recv_encrypted(&mut stream, &mut session.transport).await?;
    let header: StreamHeader = serde_json::from_slice(&frame)?;
    let row = rows.iter().find(|row| row.node == session.peer);
    listener::peer_stream_gate_rows(&header, row).map_err(anyhow::Error::new)?;
    // The peer the gateway charges is the one the HANDSHAKE proved, read
    // off the session before it is handed over, never anything the header
    // said about itself.
    let peer = session.peer;
    let target = target_override
        .or_else(|| header.target.clone())
        .ok_or_else(|| anyhow::anyhow!("a TUNNEL header with no target"))?;
    let budget = Mutex::new(TunnelBudget::new());
    tunnel::handle_tunnel_on(
        stream,
        session,
        Carry {
            route,
            peer,
            target: &target,
            hosts: egress::PEER_EGRESS_HOSTS,
            cap_bytes,
            budget: &budget,
            now_ms: 1_767_225_600_000,
        },
    )
    .await
}

/// [`gateway_on`], serving `times` carries one after another.
///
/// A pinned account is supposed to leave through the same Mac on EVERY
/// request, so the gate for it needs a Mac that is still there for the second
/// and third: a one-shot gateway would pass the same test by accident on
/// request one and prove nothing about the rest.
///
/// It returns one line per carry, the error text when there was one, so a
/// failing assertion downstream can say what the gateway saw instead of
/// leaving the reader with a bare count.
fn gateway_serving(
    listener: TcpListener,
    secret: [u8; 32],
    rows: Vec<PeerRow>,
    origin: SocketAddr,
    cap_bytes: u64,
    times: usize,
) -> tokio::task::JoinHandle<Vec<String>> {
    tokio::spawn(async move {
        let mut outcomes = Vec::new();
        for _ in 0..times {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let outcome = serve_one_carry(
                        stream,
                        &secret,
                        &rows,
                        OriginRoute::Fixed(origin),
                        cap_bytes,
                        None,
                    )
                    .await;
                    outcomes.push(match outcome {
                        Ok((up, down)) => format!("carried up={up} down={down}"),
                        Err(err) => err.to_string(),
                    });
                }
                Err(err) => outcomes.push(format!("accept failed: {err}")),
            }
        }
        outcomes
    })
}

// ---------------------------------------------------------------------------
// A fake origin that really speaks TLS
// ---------------------------------------------------------------------------

/// A CA and a leaf for [`ORIGIN`], so a real rustls client can complete a real
/// handshake through the carry.
///
/// A CA-signed leaf rather than a self-signed one: a self-signed end-entity
/// handed to a client as a trust anchor is a path webpki treats differently
/// from the production one, and the point of this fixture is that the client
/// does exactly what it does in production: validate a chain against a root it
/// trusts.
struct Origin {
    ca_pem: String,
    leaf_pem: String,
    key_pem: String,
}

fn mint_origin() -> Origin {
    use rcgen::{
        BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer,
        KeyPair, KeyUsagePurpose,
    };

    let ca_key = KeyPair::generate().expect("a CA keypair");
    let mut ca_params = CertificateParams::new(Vec::new()).expect("CA params");
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "tcr peer egress test CA");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca_cert = ca_params.self_signed(&ca_key).expect("self-sign the CA");
    let ca_pem = ca_cert.pem();

    let issuer = Issuer::from_params(&ca_params, ca_key);
    let leaf_key = KeyPair::generate().expect("a leaf keypair");
    let mut leaf_params = CertificateParams::new(vec![ORIGIN.to_string()]).expect("leaf params");
    leaf_params
        .distinguished_name
        .push(DnType::CommonName, ORIGIN);
    leaf_params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let leaf = leaf_params
        .signed_by(&leaf_key, &issuer)
        .expect("sign the leaf");

    Origin {
        ca_pem,
        leaf_pem: leaf.pem(),
        key_pem: leaf_key.serialize_pem(),
    }
}

/// Serve exactly one TLS connection with the canned `/v1/messages` answer, and
/// report the request line the client sent through the carry.
fn fake_origin_on(
    listener: TcpListener,
    origin: &Origin,
) -> tokio::task::JoinHandle<anyhow::Result<String>> {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};

    let certs: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut origin.leaf_pem.as_bytes())
            .collect::<Result<_, _>>()
            .expect("the leaf parses");
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut origin.key_pem.as_bytes())
        .expect("the key parses")
        .expect("the key is present");
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .expect("a server config");
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));

    tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut tls = acceptor.accept(stream).await?;
        // Read the request head and nothing past it.
        let mut head = Vec::new();
        let mut byte = [0_u8; 1];
        while head.len() < 8192 {
            let read = tls.read(&mut byte).await?;
            if read == 0 {
                break;
            }
            head.push(byte[0]);
            if head.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
             connection: close\r\n\r\n{CANNED_BODY}",
            CANNED_BODY.len()
        );
        tls.write_all(response.as_bytes()).await?;
        tls.flush().await?;
        Ok(String::from_utf8_lossy(&head).into_owned())
    })
}

/// A client that trusts the fake origin's CA and resolves [`ORIGIN`] to the
/// loopback splice. Everything else is what `src/peer/egress.rs` builds.
fn carried_client(origin: &Origin, splice_port: u16) -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        // A carry that is broken rather than refused would otherwise hang this
        // suite: a mutation that dropped the first record turned a red test
        // into a four-minute wait. A request that cannot complete in ten
        // seconds against a loopback origin has failed.
        .timeout(std::time::Duration::from_secs(10))
        .add_root_certificate(
            reqwest::Certificate::from_pem(origin.ca_pem.as_bytes()).expect("the CA parses"),
        )
        .resolve(ORIGIN, SocketAddr::from(([127, 0, 0, 1], splice_port)))
        .build()
        .expect("a client")
}

async fn loopback() -> TcpListener {
    TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind a kernel-chosen loopback port")
}

/// What a pooled account's answer carries that the local client must never see,
/// and what it carries that must survive.
///
/// The stripped set is `src/proxy.rs`'s own: the whole `anthropic-ratelimit-`
/// family (matched by prefix, so a member Anthropic has not invented yet is
/// covered) plus `anthropic-organization-id`. `request-id` and
/// `anthropic-organization` are the near-misses that prove the filter is not a
/// substring match.
const CARRIED_RESPONSE_HEAD: &str = concat!(
    "HTTP/1.1 200 OK\r\n",
    "content-type: application/json\r\n",
    "content-length: 2\r\n",
    "request-id: req_carried\r\n",
    "anthropic-organization: not-the-id\r\n",
    "anthropic-ratelimit-unified-5h-utilization: 1.0\r\n",
    "anthropic-ratelimit-requests-remaining: 0\r\n",
    "anthropic-ratelimit-something-new: 1\r\n",
    "anthropic-organization-id: 11111111-1111-1111-1111-111111111111\r\n",
    "\r\n",
    "{}"
);

/// **A carried response is filtered by the proxy's own rules, not by a local
/// list.**
///
/// The review's finding at `egress.rs:906`: `axum_response_from` kept a
/// private eight-name hop-by-hop list: two names short of
/// `proxy::is_response_skip`, and with no equivalent of
/// `proxy::is_account_scoped` at all: so a carried response handed the local
/// client the pooled account's quota window and its org id, the two things the
/// direct path strips because rotation means they describe a different account
/// on every request. Claude Code renders its usage banner off exactly those
/// headers.
///
/// Driven against a plain loopback origin rather than through
/// `retry_through_peer`, because a carry that completes end to end resolves
/// `api.anthropic.com` for real (the allow-list), which this suite may not do.
/// The response handed to the function is a real `reqwest::Response` with real
/// headers off a real socket.
///
/// Watched red by restoring the old `is_hop_by_hop` filter in
/// `axum_response_from`: `anthropic-ratelimit-unified-5h-utilization` reaches
/// the client and the assertion below names it.
#[tokio::test]
async fn a_carried_response_never_hands_the_client_the_serving_accounts_headers() {
    let listener = loopback().await;
    let addr = listener.local_addr().expect("the origin address");
    let origin = tokio::spawn(async move {
        let (mut stream, _peer) = listener.accept().await.expect("one connection");
        let mut buf = [0_u8; 2048];
        let head = stream.read(&mut buf).await.expect("the request head");
        assert!(head > 0, "the client must send a request before an answer");
        stream
            .write_all(CARRIED_RESPONSE_HEAD.as_bytes())
            .await
            .expect("the canned answer");
        stream.flush().await.expect("flush");
    });

    let response = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("a client")
        .get(format!("http://{addr}/v1/messages"))
        .send()
        .await
        .expect("the loopback origin answers");
    let carried = egress::axum_response_from(response);
    origin.await.expect("the origin task joins");

    assert_eq!(carried.status(), StatusCode::OK);
    for name in [
        "anthropic-ratelimit-unified-5h-utilization",
        "anthropic-ratelimit-requests-remaining",
        "anthropic-ratelimit-something-new",
        "anthropic-organization-id",
    ] {
        assert!(
            carried.headers().get(name).is_none(),
            "{name} describes the pooled account that carried this, not the caller"
        );
    }
    assert!(
        carried.headers().get("content-length").is_none(),
        "the framing headers are the carried connection's, and the body is re-streamed"
    );
    assert_eq!(
        carried
            .headers()
            .get("request-id")
            .map(|value| value.to_str().expect("ascii")),
        Some("req_carried"),
        "a request id identifies a REQUEST and is what makes one failed call debuggable"
    );
    assert_eq!(
        carried
            .headers()
            .get("anthropic-organization")
            .map(|value| value.to_str().expect("ascii")),
        Some("not-the-id"),
        "the family is a prefix match, not a substring one"
    );
    assert_eq!(
        carried
            .headers()
            .get("content-type")
            .map(|value| value.to_str().expect("ascii")),
        Some("application/json")
    );
}

// ---------------------------------------------------------------------------
// Item 1: the tunnel carries a real TLS session, blind
// ---------------------------------------------------------------------------

/// The whole carry: a real client, a real TLS handshake, a real HTTP answer,
/// through a gateway that never held a key for any of it.
///
/// The value assertion is the canned body, not merely a 200: a splice that
/// crossed its two directions, or dropped the last frame, still answers with
/// SOMETHING. It also asserts the gateway's own byte counters are non-zero in
/// both directions, because a carry that only ever went one way would be a
/// half-working splice with a passing status line.
#[tokio::test]
async fn a_carry_delivers_the_canned_messages_body_through_a_gateway() {
    let (collector, _guard) = capture();
    let requester = node();
    let gateway = node();
    let origin = mint_origin();

    let origin_listener = loopback().await;
    let origin_addr = origin_listener.local_addr().expect("the origin address");
    let origin_task = fake_origin_on(origin_listener, &origin);

    let gateway_listener = loopback().await;
    let gateway_addr = gateway_listener.local_addr().expect("the gateway address");
    let gateway_task = gateway_on(
        gateway_listener,
        gateway.secret,
        vec![row(&requester.id, "requester", Vec::new(), true)],
        OriginRoute::Fixed(origin_addr),
        1024 * 1024,
    );

    let splice = egress::accept_once_splice(
        &row(
            &gateway.id,
            "gateway",
            vec![gateway_addr.to_string()],
            false,
        ),
        &requester.secret,
        ORIGIN,
        443,
    )
    .await
    .expect("the gateway takes the carry");

    let response = carried_client(&origin, splice.port)
        .post(format!("https://{ORIGIN}/v1/messages"))
        .header("content-type", "application/json")
        .body(r#"{"model":"claude-opus-4-6","messages":[]}"#)
        .send()
        .await
        .expect("the carried request completes");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body = response.text().await.expect("the carried body");
    assert_eq!(
        body, CANNED_BODY,
        "the body the origin sent must be the body the client read, byte for byte"
    );

    let head = origin_task
        .await
        .expect("the origin task joins")
        .expect("the origin served");
    assert!(
        head.starts_with("POST /v1/messages "),
        "the origin must see the request the client made, not a rewritten one: {head:?}"
    );

    let (up, down) = gateway_task
        .await
        .expect("the gateway task joins")
        .expect("the gateway carried the stream");
    assert!(up > 0, "the gateway must have carried bytes outbound");
    assert!(down > 0, "the gateway must have carried bytes inbound");

    // **The gateway's log line, field for field.** A gateway is the one place
    // where a lazy log line would leak a peer's whole request stream, so the
    // assertion is on the exact field SET and not on a few fields being
    // present: anything added later: a path, a header, a body length, a
    // request id: fails here.
    let carried = collector.matching("peer tunnel: carried");
    assert_eq!(carried.len(), 1, "one line per carried stream: {carried:?}");
    let mut keys: Vec<&str> = carried[0].fields.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec!["bytes_down", "bytes_up", "host", "ms", "peer", "port"],
        "the gateway may log the peer, the host, the port, two byte counts and a duration, \
         and nothing else"
    );
    assert_eq!(
        carried[0].fields.get("host").map(String::as_str),
        Some(ORIGIN)
    );
    // The peer is the SHORT display form, never the full key: a log file is
    // not where a pinned static key belongs.
    assert_eq!(
        carried[0].fields.get("peer").map(String::as_str),
        Some(requester.id.display().as_str())
    );
    assert!(!carried[0]
        .fields
        .values()
        .any(|value| value.contains(&requester.id.to_wire())));
}

/// A gateway refuses a host that is not on the allow-list, and refuses it
/// BEFORE anything is dialled.
///
/// Deny by default is the inverse of the local proxy's blind-tunnel default,
/// and this is the test the module docs promise.
#[tokio::test]
async fn a_host_off_the_allow_list_is_refused() {
    let requester = node();
    let gateway = node();
    let gateway_listener = loopback().await;
    let gateway_addr = gateway_listener.local_addr().expect("the gateway address");
    let rows = vec![row(&requester.id, "requester", Vec::new(), true)];
    let gateway_task = gateway_on(
        gateway_listener,
        gateway.secret,
        rows,
        // A fixed route that would succeed if the allow-list did not run
        // first: the refusal must not be an accident of an unreachable host.
        OriginRoute::Fixed(SocketAddr::from(([127, 0, 0, 1], 9))),
        1024 * 1024,
    );

    let mut stream = TcpStream::connect(gateway_addr)
        .await
        .expect("dial the gateway");
    let mut session = noise::dial_handshake(
        &mut stream,
        &requester.secret,
        Handshake::Return,
        Some(&gateway.id.0),
        None,
    )
    .await
    .expect("the handshake completes");
    let header = StreamHeader {
        kind: StreamKind::Tunnel,
        target: Some(TunnelTarget::Origin {
            host: "example.com".to_string(),
            port: 443,
        }),
        via: Vec::new(),
        hops_remaining: 1,
        request_id: 7,
    };
    noise::send_encrypted(
        &mut stream,
        &mut session.transport,
        &serde_json::to_vec(&header).expect("the header serializes"),
    )
    .await
    .expect("the header is written");

    let refusal = gateway_task
        .await
        .expect("the gateway task joins")
        .expect_err("a host off the allow-list must be refused");
    let text = format!("{refusal:#}");
    assert!(
        text.contains("example.com") && text.contains("allow-list"),
        "the refusal must name the host and the reason: {text}"
    );
}

/// A gateway grant is not a relay grant, at both checks that exist.
///
/// The production gate refuses `TunnelTarget::Peer` for want of `allow.relay`,
/// and the GATEWAY handler refuses it again even when a grant admits it ,
/// because `handle_tunnel_on` is handed no peers file and so can check neither
/// the relay grant nor the target's pin. Two checks, measured separately,
/// because a reader who saw only one would not know whether an `allow.relay`
/// grant opens an unbounded relay through the gateway path.
///
/// Forwarding itself exists now and is `tunnel::handle_forward_on`, gated by
/// `tests/peer_forward.rs`. What this test keeps measuring is that the two
/// entry points stayed apart: a carry never becomes a forward by a grant.
#[tokio::test]
async fn a_relay_target_is_refused_by_a_gateway() {
    let requester = node();
    let gateway = node();
    let onward = node();
    let header = StreamHeader {
        kind: StreamKind::Tunnel,
        target: Some(TunnelTarget::Peer { node: onward.id }),
        via: Vec::new(),
        hops_remaining: 1,
        request_id: 8,
    };

    // Check one: the gate, under a gateway-only grant.
    let gateway_only = row(&requester.id, "requester", Vec::new(), true);
    let refusal = listener::peer_stream_gate_rows(&header, Some(&gateway_only))
        .expect_err("a relay target under a gateway grant must be refused");
    assert!(
        format!("{refusal}").contains("relay") || format!("{refusal:?}").contains("NotGranted"),
        "the gate's refusal must be about the missing grant: {refusal:?}"
    );
    // And the same gate admits the origin form, so the refusal above is about
    // the TARGET and not about the kind.
    let origin_header = StreamHeader {
        target: Some(TunnelTarget::Origin {
            host: ORIGIN.to_string(),
            port: 443,
        }),
        ..header.clone()
    };
    assert!(listener::peer_stream_gate_rows(&origin_header, Some(&gateway_only)).is_ok());

    // Check two: the handler, given a relay target with the gate out of the
    // way. `relay` is granted on the row here so the gate cannot be what
    // refuses, and the header on the wire is the origin form because the wire
    // cannot carry the other one (see `gateway_on_target`).
    let mut relaying = row(&requester.id, "requester", Vec::new(), true);
    relaying.allow.relay = true;
    let gateway_listener = loopback().await;
    let gateway_addr = gateway_listener.local_addr().expect("the gateway address");
    let gateway_task = gateway_on_target(
        gateway_listener,
        gateway.secret,
        vec![relaying],
        OriginRoute::Resolve,
        1024 * 1024,
        Some(TunnelTarget::Peer { node: onward.id }),
    );

    let mut stream = TcpStream::connect(gateway_addr)
        .await
        .expect("dial the gateway");
    let mut session = noise::dial_handshake(
        &mut stream,
        &requester.secret,
        Handshake::Return,
        Some(&gateway.id.0),
        None,
    )
    .await
    .expect("the handshake completes");
    noise::send_encrypted(
        &mut stream,
        &mut session.transport,
        &serde_json::to_vec(&origin_header).expect("the origin header serializes"),
    )
    .await
    .expect("the header is written");

    let refusal = gateway_task
        .await
        .expect("the gateway task joins")
        .expect_err("the handler must refuse a relay target too");
    let text = format!("{refusal:#}");
    assert!(
        text.contains("relay"),
        "the handler's refusal must say it does not relay: {text}"
    );
}

/// A relay target round-trips on the wire, which is what forward-dial
/// had to land before a client could ask for one at all.
///
/// # This test is the INVERSE of the one it replaces
///
/// It was `a_relay_target_cannot_be_serialized_yet`, and it asserted that
/// `serde_json::to_vec` REFUSED this header: `TunnelTarget` is internally
/// tagged (`tag = "target"`), `PeerId` serializes as a string, and serde
/// refuses an internally-tagged newtype variant whose content is not a map. So
/// `TUNNEL{Peer}` could not be written by any node, and the forwarder every
/// gate in `tests/peer_forward.rs` measures was reachable by nobody. That
/// test's own failure message said what to do when the patch landed, and this
/// is it: the variant is now `Peer { node }` and the assertion is that it
/// travels, and travels back as the same id.
#[test]
fn a_relay_target_travels_on_the_wire() {
    let header = StreamHeader {
        kind: StreamKind::Tunnel,
        target: Some(TunnelTarget::Peer {
            node: PeerId([6_u8; 32]),
        }),
        via: Vec::new(),
        hops_remaining: 1,
        request_id: 9,
    };
    let bytes = serde_json::to_vec(&header).expect("a relay target serializes now");
    let read: StreamHeader = serde_json::from_slice(&bytes).expect("and reads back");
    assert_eq!(
        read.target,
        Some(TunnelTarget::Peer {
            node: PeerId([6_u8; 32])
        }),
        "the forwarder learns WHICH pinned Mac to carry to from this field and nothing \
         else, so a round trip that lost it would be an unroutable forward"
    );
}

/// A ClientHello that names another host closes the carry, and the gateway
/// never dials the origin.
#[tokio::test]
async fn a_mismatched_sni_closes_the_carry() {
    let requester = node();
    let gateway = node();
    let gateway_listener = loopback().await;
    let gateway_addr = gateway_listener.local_addr().expect("the gateway address");
    let gateway_task = gateway_on(
        gateway_listener,
        gateway.secret,
        vec![row(&requester.id, "requester", Vec::new(), true)],
        OriginRoute::Fixed(SocketAddr::from(([127, 0, 0, 1], 9))),
        1024 * 1024,
    );

    let splice = egress::accept_once_splice(
        &row(
            &gateway.id,
            "gateway",
            vec![gateway_addr.to_string()],
            false,
        ),
        &requester.secret,
        ORIGIN,
        443,
    )
    .await
    .expect("the gateway takes the carry");

    // A real ClientHello for the WRONG name, produced by a real client: the
    // point is a handshake this gateway was not asked to carry, not a
    // hand-written byte string only this parser would accept.
    let wrong = reqwest::Client::builder()
        .no_proxy()
        .resolve(
            "platform.claude.com",
            SocketAddr::from(([127, 0, 0, 1], splice.port)),
        )
        .build()
        .expect("a client");
    let attempt = wrong.get("https://platform.claude.com/").send().await;
    assert!(
        attempt.is_err(),
        "a domain-fronted carry must fail at the client too, not merely be logged"
    );

    let refusal = gateway_task
        .await
        .expect("the gateway task joins")
        .expect_err("a mismatched SNI must be refused");
    let text = format!("{refusal:#}");
    assert!(
        text.contains("platform.claude.com") && text.contains(ORIGIN),
        "the refusal must name both the host asked for and the host named: {text}"
    );
}

/// A peer that opens a carry and then says nothing is closed on, rather than
/// holding a task on somebody else's Mac for as long as it likes.
///
/// Found by a mutation: with the origin allow-list disabled, the refusal test
/// stopped refusing and hung instead, which said the deadline was missing
/// rather than that the mutation was wrong.
#[tokio::test]
async fn a_carry_that_sends_no_client_hello_is_closed_on() {
    let requester = node();
    let gateway = node();
    let gateway_listener = loopback().await;
    let gateway_addr = gateway_listener.local_addr().expect("the gateway address");
    let gateway_task = gateway_on(
        gateway_listener,
        gateway.secret,
        vec![row(&requester.id, "requester", Vec::new(), true)],
        OriginRoute::Fixed(SocketAddr::from(([127, 0, 0, 1], 9))),
        1024 * 1024,
    );

    let mut stream = TcpStream::connect(gateway_addr)
        .await
        .expect("dial the gateway");
    let mut session = noise::dial_handshake(
        &mut stream,
        &requester.secret,
        Handshake::Return,
        Some(&gateway.id.0),
        None,
    )
    .await
    .expect("the handshake completes");
    let header = StreamHeader {
        kind: StreamKind::Tunnel,
        target: Some(TunnelTarget::Origin {
            host: ORIGIN.to_string(),
            port: 443,
        }),
        via: Vec::new(),
        hops_remaining: 1,
        request_id: 11,
    };
    noise::send_encrypted(
        &mut stream,
        &mut session.transport,
        &serde_json::to_vec(&header).expect("the header serializes"),
    )
    .await
    .expect("the header is written");
    // And then nothing at all.

    let started = std::time::Instant::now();
    let refusal = tokio::time::timeout(tunnel::FIRST_RECORD_TIMEOUT * 4, gateway_task)
        .await
        .expect("the gateway must not wait forever")
        .expect("the gateway task joins")
        .expect_err("a silent carry must be closed on");
    assert!(
        format!("{refusal:#}").contains("no complete ClientHello"),
        "the refusal must name what was missing: {refusal:#}"
    );
    assert!(
        started.elapsed() < tunnel::FIRST_RECORD_TIMEOUT * 3,
        "the deadline must be the five-second one, not a socket timeout"
    );
}

/// The parser reads the SNI out of a REAL rustls ClientHello.
///
/// The control that makes every other SNI assertion here worth anything: a
/// hand-built fixture would agree with this parser and with nothing else.
#[tokio::test]
async fn the_sni_parser_reads_a_real_client_hello() {
    let listener = loopback().await;
    let port = listener.local_addr().expect("the address").port();
    let client = reqwest::Client::builder()
        .no_proxy()
        .resolve(ORIGIN, SocketAddr::from(([127, 0, 0, 1], port)))
        .build()
        .expect("a client");
    // Nothing answers, so this fails: after writing a real ClientHello.
    let attempt =
        tokio::spawn(async move { client.get(format!("https://{ORIGIN}/")).send().await });

    let (mut stream, _) = listener.accept().await.expect("accept the handshake");
    let mut first = vec![0_u8; 4096];
    let read = stream.read(&mut first).await.expect("the ClientHello");
    first.truncate(read);
    drop(stream);
    let _ = attempt.await;

    assert_eq!(
        tunnel::client_hello_sni(&first)
            .expect("a ClientHello parses")
            .as_deref(),
        Some(ORIGIN),
        "the parser must read the name a real client sent"
    );
    assert!(tunnel::assert_sni_matches_target(&first, ORIGIN).is_ok());
    let mismatch = tunnel::assert_sni_matches_target(&first, "platform.claude.com")
        .expect_err("a mismatch must be refused");
    assert!(format!("{mismatch:#}").contains("closing"));
    // Case is not a mismatch: a DNS name is case-insensitive.
    assert!(tunnel::assert_sni_matches_target(&first, "API.Anthropic.CoM").is_ok());
    // Bytes that are not a handshake record at all are a different fact from a
    // handshake with no name, and get a different message.
    let refusal = tunnel::client_hello_sni(b"GET / HTTP/1.1\r\n\r\n")
        .expect_err("plain HTTP is not a ClientHello");
    assert!(format!("{refusal:#}").contains("not a TLS handshake"));
}

// ---------------------------------------------------------------------------
// Item 3: the byte cap
// ---------------------------------------------------------------------------

/// The ledger rolls its hour, and a spent hour refuses the next carry.
///
/// **The allowance figures moved and the old ones are gone on
/// purpose.** They used to be "whatever is left of the hour", which is what let
/// concurrent carries spend the hour several times over
/// (`five_concurrent_carries_at_a_four_carry_budget_refuse_the_fifth`); one
/// carry now holds one `cap / MAX_OPEN_CARRIES_PER_PEER` slice. The facts this
/// test exists for: the hour is per peer, a spent hour refuses, and the window
/// rolls, are unchanged.
#[test]
fn a_spent_hour_refuses_the_next_carry() {
    let peer = PeerId([3_u8; 32]);
    let other = PeerId([4_u8; 32]);
    let mut budget = TunnelBudget::new();
    let now = 1_767_225_600_000_i64;
    // One slice of a 1000-byte hour, with four open carries allowed.
    let slice = 1000 / tunnel::MAX_OPEN_CARRIES_PER_PEER;
    let open = match budget.admit(&peer, 1000, now) {
        Admission::Carry { allowance, open } => {
            assert_eq!(allowance, slice);
            open
        }
        refused => panic!("an untouched hour must admit a carry, got {refused:?}"),
    };
    // Closed having spent 600, which is more than its slice only because a
    // test may say so; the ledger records what it is told.
    budget.close(open, &peer, 600, now);
    let open = match budget.admit(&peer, 1000, now) {
        Admission::Carry { allowance, open } => {
            assert_eq!(
                allowance,
                400.min(slice),
                "what is left, capped at one slice"
            );
            open
        }
        refused => panic!("400 bytes left must admit a carry, got {refused:?}"),
    };
    budget.close(open, &peer, 400, now);
    assert_eq!(
        budget.admit(&peer, 1000, now),
        Admission::OverBudget {
            spent: 1000,
            cap: 1000
        }
    );
    // The cap is per peer, not per gateway: one peer spending its hour must not
    // refuse another peer's carry.
    assert!(matches!(
        budget.admit(&other, 1000, now),
        Admission::Carry { .. }
    ));
    // And the hour rolls.
    let later = now + tunnel::BUDGET_WINDOW_MS + 1;
    assert!(matches!(
        budget.admit(&peer, 1000, later),
        Admission::Carry { .. }
    ));
    assert_eq!(budget.spent_by(&peer, later), 0);
}

/// A peer whose hour is spent is refused by the gateway itself, with a byte cap
/// small enough that the refusal is the only possible outcome.
#[tokio::test]
async fn a_gateway_refuses_a_carry_over_the_byte_cap() {
    let requester = node();
    let gateway = node();
    let gateway_listener = loopback().await;
    let gateway_addr = gateway_listener.local_addr().expect("the gateway address");
    // Zero bytes for the hour: the refusal happens at admission, before a
    // ClientHello is read and before the origin is dialled.
    let gateway_task = gateway_on(
        gateway_listener,
        gateway.secret,
        vec![row(&requester.id, "requester", Vec::new(), true)],
        OriginRoute::Fixed(SocketAddr::from(([127, 0, 0, 1], 9))),
        0,
    );

    let splice = egress::accept_once_splice(
        &row(
            &gateway.id,
            "gateway",
            vec![gateway_addr.to_string()],
            false,
        ),
        &requester.secret,
        ORIGIN,
        443,
    )
    .await
    .expect("the gateway takes the stream header");
    // Nothing needs to dial the splice: the gateway refuses on the header.
    splice.abort();

    let refusal = gateway_task
        .await
        .expect("the gateway task joins")
        .expect_err("a spent hour must refuse the carry");
    let text = format!("{refusal:#}");
    assert!(
        text.contains("carried bytes this hour"),
        "the refusal must name the cap: {text}"
    );
}

/// The cap is enforced DURING a carry too, not only at admission.
///
/// A cap that only admitted would be a cap on stream count: one stream that
/// never ends could spend any number of bytes. The allowance here is smaller
/// than the canned answer, so the carry starts and then stops.
#[tokio::test]
async fn a_carry_stops_when_its_allowance_runs_out() {
    let requester = node();
    let gateway = node();
    let origin = mint_origin();

    let origin_listener = loopback().await;
    let origin_addr = origin_listener.local_addr().expect("the origin address");
    let _origin_task = fake_origin_on(origin_listener, &origin);

    let gateway_listener = loopback().await;
    let gateway_addr = gateway_listener.local_addr().expect("the gateway address");
    // Enough to be admitted, far too little for a TLS handshake.
    let gateway_task = gateway_on(
        gateway_listener,
        gateway.secret,
        vec![row(&requester.id, "requester", Vec::new(), true)],
        OriginRoute::Fixed(origin_addr),
        64,
    );

    let splice = egress::accept_once_splice(
        &row(
            &gateway.id,
            "gateway",
            vec![gateway_addr.to_string()],
            false,
        ),
        &requester.secret,
        ORIGIN,
        443,
    )
    .await
    .expect("the gateway takes the carry");

    let attempt = carried_client(&origin, splice.port)
        .post(format!("https://{ORIGIN}/v1/messages"))
        .body("{}")
        .send()
        .await;
    assert!(
        attempt.is_err(),
        "a carry cut off at its allowance must fail the request, not answer it"
    );
    let outcome = gateway_task.await.expect("the gateway task joins");
    assert!(
        outcome.is_err(),
        "the gateway must report the carry it cut off: {outcome:?}"
    );
}

// ---------------------------------------------------------------------------
// Item 3: `tcr peer via`
// ---------------------------------------------------------------------------

/// `auto`, `off`, and a peer id, and nothing else.
#[test]
fn via_parses_the_three_words_it_accepts() {
    assert_eq!(ViaSetting::parse("auto").expect("auto"), ViaSetting::auto());
    assert_eq!(ViaSetting::parse(" off ").expect("off"), ViaSetting::off());
    let peer = PeerId([9_u8; 32]);
    assert_eq!(
        ViaSetting::parse(&peer.to_wire()).expect("a peer id"),
        ViaSetting::pinned(peer)
    );
    let refusal = ViaSetting::parse("maybe").expect_err("a word that is neither");
    assert!(format!("{refusal:#}").contains("neither `auto`, `off`, nor a peer id"));
    assert_eq!(ViaSetting::default(), ViaSetting::auto());
}

/// `off` asks nobody, a pinned Mac never falls back to another one, and `auto`
/// asks the freshest first.
///
/// The substitution case is the one that matters: a silent fallback here would
/// be a request leaving by a route the operator refused.
#[test]
fn resolve_via_never_substitutes_a_mac_the_operator_did_not_choose() {
    let fresh = PeerId([1_u8; 32]);
    let stale = PeerId([2_u8; 32]);
    let unseen = PeerId([3_u8; 32]);
    let addressless = PeerId([4_u8; 32]);
    let candidates = vec![
        GatewayCandidate {
            row: row(&stale, "stale", vec!["127.0.0.1:1".to_string()], false),
            last_seen_ms: Some(1_000),
        },
        GatewayCandidate {
            row: row(&fresh, "fresh", vec!["127.0.0.1:2".to_string()], false),
            last_seen_ms: Some(9_000),
        },
        GatewayCandidate {
            row: row(&unseen, "unseen", vec!["127.0.0.1:3".to_string()], false),
            last_seen_ms: None,
        },
        GatewayCandidate {
            row: row(&addressless, "addressless", Vec::new(), false),
            last_seen_ms: Some(9_999),
        },
    ];

    assert!(egress::resolve_via(&ViaSetting::off(), &candidates).is_empty());

    let auto = egress::resolve_via(&ViaSetting::auto(), &candidates);
    assert_eq!(
        auto.iter().map(|c| c.row.node).collect::<Vec<_>>(),
        vec![fresh, stale, unseen],
        "freshest first, never-seen last, and a Mac with no address is not a candidate"
    );

    let pinned = egress::resolve_via(&ViaSetting::pinned(stale), &candidates);
    assert_eq!(
        pinned.iter().map(|c| c.row.node).collect::<Vec<_>>(),
        vec![stale],
        "a pinned choice is one Mac or none"
    );
    assert!(
        egress::resolve_via(&ViaSetting::pinned(PeerId([7_u8; 32])), &candidates).is_empty(),
        "a pinned Mac that is not a candidate must not fall back to another one"
    );
    assert!(
        egress::resolve_via(&ViaSetting::pinned(addressless), &candidates).is_empty(),
        "a pinned Mac with no address is not reachable and is not substituted"
    );
}

/// The cached belief about the direct path, and what it is allowed to decide.
#[test]
fn the_direct_path_goes_cold_and_comes_back() {
    let now = 1_767_225_600_000_i64;
    assert!(EgressState::Healthy.direct_is_worth_trying(now));
    let cold = EgressState::Cold {
        until_ms: now + egress::COLD_MS,
    };
    assert!(!cold.direct_is_worth_trying(now));
    assert!(cold.direct_is_worth_trying(now + egress::COLD_MS));
    // The skeleton's `pick_egress` has no candidate set, so the only honest
    // answer it can give is "no peer", and it must not invent one. This
    // report carries the signature note.
    assert_eq!(egress::pick_egress(EgressState::Healthy, now), None);
    assert_eq!(egress::pick_egress(cold, now), None);
}

/// Deny by default, on the requester's side too: a host the mesh does not carry
/// is refused before a gateway is dialled.
#[tokio::test]
async fn the_requester_refuses_a_host_the_mesh_does_not_carry() {
    let gateway = node();
    let requester = node();
    for (host, port) in [
        ("example.com", 443_u16),
        (ORIGIN, 8443),
        ("platform.claude.com", 80),
    ] {
        let refusal = egress::accept_once_splice(
            &row(
                &gateway.id,
                "gateway",
                vec!["127.0.0.1:1".to_string()],
                false,
            ),
            &requester.secret,
            host,
            port,
        )
        .await
        .expect_err("a host or port off the list must be refused");
        assert!(
            format!("{refusal:#}").contains("not an origin this mesh carries"),
            "{host}:{port} must be refused by the allow-list"
        );
    }
    assert!(egress::host_allowed(ORIGIN, 443));
    assert!(egress::host_allowed("platform.claude.com", 443));
    assert!(!egress::host_allowed("api.anthropic.com.evil.test", 443));
}

// ---------------------------------------------------------------------------
// Item 2: the seam, and item 4: the metric line
// ---------------------------------------------------------------------------

/// One captured tracing event: the message, plus the fields as strings.
#[derive(Debug, Clone)]
struct Event {
    message: String,
    fields: HashMap<String, String>,
}

/// Collect every event on this thread while the guard lives.
#[derive(Clone, Default)]
struct Collector {
    events: Arc<Mutex<Vec<Event>>>,
}

impl Collector {
    fn events(&self) -> Vec<Event> {
        self.events.lock().expect("the collector lock").clone()
    }

    fn matching(&self, needle: &str) -> Vec<Event> {
        self.events()
            .into_iter()
            .filter(|event| event.message.contains(needle))
            .collect()
    }

    /// Where `needle` first appears in this thread's event ORDER.
    ///
    /// Order is the assertion for the carry offer: a count alone cannot tell
    /// "offered after the fleet was walked" from "offered on the first
    /// account's connect failure", and the second one is the bug.
    fn first_index(&self, needle: &str) -> Option<usize> {
        self.events()
            .iter()
            .position(|event| event.message.contains(needle))
    }

    /// Every message, in order, for an assertion's failure output.
    fn messages(&self) -> Vec<String> {
        self.events()
            .into_iter()
            .map(|event| event.message)
            .collect()
    }
}

impl<S> tracing_subscriber::Layer<S> for Collector
where
    S: tracing::Subscriber,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        struct Visitor {
            message: String,
            fields: HashMap<String, String>,
        }
        impl tracing::field::Visit for Visitor {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                let rendered = format!("{value:?}");
                if field.name() == "message" {
                    self.message = rendered.trim_matches('"').to_string();
                } else {
                    self.fields.insert(field.name().to_string(), rendered);
                }
            }
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                if field.name() == "message" {
                    self.message = value.to_string();
                } else {
                    self.fields
                        .insert(field.name().to_string(), value.to_string());
                }
            }
        }
        let mut visitor = Visitor {
            message: String::new(),
            fields: HashMap::new(),
        };
        event.record(&mut visitor);
        self.events.lock().expect("the collector lock").push(Event {
            message: visitor.message,
            fields: visitor.fields,
        });
    }
}

/// Capture this thread's tracing events for as long as the guard lives.
///
/// `set_default`, not `set_global_default`: every `#[tokio::test]` here runs on
/// a current-thread runtime, so the tasks it spawns are polled on this same
/// thread and their events land in this collector: while another test in this
/// binary, on another thread, cannot pollute the count. A global subscriber is
/// what forced `tests/peer_refusal_log.rs` into its own binary.
fn capture() -> (Collector, tracing::subscriber::DefaultGuard) {
    use tracing_subscriber::layer::SubscriberExt as _;
    let collector = Collector::default();
    let subscriber = tracing_subscriber::registry().with(collector.clone());
    let guard = tracing::subscriber::set_default(subscriber);
    (collector, guard)
}

struct NeverRefreshes;
impl TokenRefresher for NeverRefreshes {
    fn refresh(&self, _refresh_token: String) -> RefreshFuture {
        Box::pin(async { Err(OAuthError::Transient("no refresher in egress tests".into())) })
    }
}

struct NeverProbes;
impl UsageProber for NeverProbes {
    fn probe(&self, _access_token: String) -> ProbeFuture {
        Box::pin(async {
            Err(ProbeError {
                status: None,
                message: "no prober in egress tests".into(),
                retry_after_secs: None,
            })
        })
    }
}

struct NeverWarms;
impl AccountWarmer for NeverWarms {
    fn warm(&self, _access_token: String, _upstream: String) -> WarmFuture {
        Box::pin(async {
            Err(WarmError {
                status: None,
                message: "no warmer in egress tests".into(),
            })
        })
    }
}

/// One fake account. No real email, no real uuid: this repository is public.
fn fake_account() -> Account {
    Account {
        name: "alice@example.com".to_string(),
        account_type: "oauth".to_string(),
        account_uuid: Some("11111111-1111-1111-1111-111111111111".to_string()),
        org_uuid: None,
        org_name: None,
        access_token: "at-alice".to_string(),
        refresh_token: Some("rt-alice".to_string()),
        expires_at: Some(4_102_444_800_000),
        priority: Some(0),
        switch_threshold: None,
        disabled: None,
        groups: None,
        organization_type: None,
        rate_limit_tier: None,
        seat_tier: None,
        egress: teamclaude_rs::config::Egress::Local,
        egress_strict: false,
        extra: serde_json::Map::new(),
    }
}

/// A one-account fleet pointed at `upstream`.
fn fleet(upstream: &str) -> Arc<Manager> {
    fleet_of(upstream, &["alice@example.com"])
}

/// A fleet of one account per name, pointed at `upstream`.
///
/// Distinct names, because the carry guard is about ROTATION: a fleet of one is
/// out of moves on its first failure and can never show the difference between
/// "offered after the ladder was walked" and "offered on the first failure".
fn fleet_of(upstream: &str, names: &[&str]) -> Arc<Manager> {
    let accounts = names
        .iter()
        .enumerate()
        .map(|(n, name)| Account {
            name: (*name).to_string(),
            account_uuid: Some(format!("1111111{n}-1111-1111-1111-111111111111")),
            access_token: format!("at-{n}"),
            refresh_token: Some(format!("rt-{n}")),
            ..fake_account()
        })
        .collect();
    fleet_with(upstream, accounts)
}

/// A fleet pointed at `upstream`, carrying exactly `accounts`.
fn fleet_with(upstream: &str, accounts: Vec<Account>) -> Arc<Manager> {
    let config = Config {
        quarantined_accounts: Vec::new(),
        migrated_legacy_throttle: false,
        renamed_accounts: Vec::new(),
        rename_write_error: None,
        proxy: ProxyConfig::default(),
        upstream: upstream.to_string(),
        switch_threshold: 0.95,
        fable_weekly_threshold: None,
        pacing: PacingConfig {
            max_in_flight_per_account: None,
            min_spacing_ms: None,
        },
        account_throttle: ThrottleConfig::default(),
        fleet_throttle: ThrottleConfig::default(),
        lock_account: None,
        control_account: None,
        control_reserve: 0.05,
        control_pooled: false,
        reset_urgency_tier_hours: 24,
        http1_only: false,
        accounts,
        group_settings: std::collections::HashMap::new(),
        pricing: Default::default(),
        usage_retention_days: 90,
        extra: serde_json::Map::new(),
    };
    Manager::new(
        config,
        Arc::new(NeverRefreshes),
        Arc::new(NeverProbes),
        Arc::new(NeverWarms),
        None,
    )
}

/// One `/v1/messages` POST through the real router.
async fn post_messages(manager: Arc<Manager>) -> axum::response::Response {
    teamclaude_rs::proxy::app(manager)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"claude-opus-4-6","messages":[]}"#.to_string(),
                ))
                .expect("a request"),
        )
        .await
        .expect("the router answers")
}

/// **The no-regression half of the seam gate.** A blackholed upstream with no
/// mesh at all answers exactly what it answered before this test file
/// existed: the connect-phase 503, with its `retry-after` and its
/// `x-should-retry: true`.
///
/// `127.0.0.1:1` is refused immediately (RST), so every attempt dies at
/// CONNECT, `unknown_outcome_transport_failure` stays false, and the seam is
/// consulted and answers "nobody to ask". The captured line is the proof the
/// seam RAN rather than merely compiled: a passing 503 alone would be
/// identical whether the hunk executes or not.
#[tokio::test]
async fn a_blackholed_upstream_with_no_mesh_answers_todays_503() {
    let peers = scratch("no-mesh").join("tcr-peers.json");
    // An empty peers file: pinned nobody, so there is no Mac to ask. Written
    // rather than absent so this exercises the read path, and never the
    // operator's config directory.
    teamclaude_rs::peer::config::save(&peers, &PeerFile::default()).expect("write the peers file");
    assert!(
        egress::install_peers_path(peers),
        "nothing else in this binary installs a peers path, and a lost race here would \
         point the seam at the operator's config directory"
    );

    let (collector, _guard) = capture();
    let response = post_messages(fleet("http://127.0.0.1:1")).await;

    assert_eq!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "a connect-phase failure with no mesh must still be today's 503"
    );
    assert_eq!(
        response
            .headers()
            .get("x-should-retry")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    assert!(response.headers().contains_key("retry-after"));

    let line = collector.matching("returning 503 to client");
    assert_eq!(
        line.len(),
        1,
        "exactly one terminal line, and it is the 503 one: {:?}",
        collector
            .events()
            .iter()
            .map(|e| e.message.clone())
            .collect::<Vec<_>>()
    );
    assert!(
        line[0].fields.contains_key("transport_failures"),
        "the count line the daily baseline reads must carry its figure: {:?}",
        line[0]
    );
    // The seam ran and declined, which is the assertion: `127.0.0.1:1` is not
    // an origin this mesh carries, so no Mac is asked. **A 503 alone would be
    // identical whether the hunk executes or not**, which is why this line is
    // the gate. The branch where a Mac IS asked cannot be reached through
    // `proxy::handle` in a test: its client validates the real certificate for
    // `api.anthropic.com` and a test cannot hand it a root without putting a
    // root-injection seam in the production path: so that branch is driven
    // directly in `a_gateway_that_does_not_answer_leaves_todays_503` and end to
    // end in `a_carry_delivers_the_canned_messages_body_through_a_gateway`.
    assert!(
        !collector
            .matching("not an origin the mesh carries")
            .is_empty(),
        "the seam must have been consulted: {:?}",
        collector
            .events()
            .iter()
            .map(|e| e.message.clone())
            .collect::<Vec<_>>()
    );
}

/// A pinned Mac that does not answer still leaves the request with today's
/// answer, and says which Mac was asked.
#[tokio::test]
async fn a_gateway_that_does_not_answer_leaves_todays_503() {
    let dir = scratch("dead-gateway");
    let peers = dir.join("tcr-peers.json");
    let mut file = PeerFile::default();
    // Port 1 on loopback: refused instantly, so the test does not wait out a
    // connect timeout.
    //
    // **The carry grant is now `true`**, where it was `false`:
    // a Mac outside the grant is no longer a candidate at all
    // (`only_a_mac_inside_the_carry_grant_is_a_candidate`), so the ungranted
    // row this test used to carry would now be filtered out before the dial
    // and there would be no Mac to name. The fact under test: a Mac that does
    // not answer leaves the request with today's answer, and says which Mac
    // was asked: is unchanged.
    file.peers.push(carry_row(
        &PeerId([5_u8; 32]),
        "asleep",
        vec!["127.0.0.1:1".to_string()],
        true,
    ));
    teamclaude_rs::peer::config::save(&peers, &file).expect("write the peers file");

    let (collector, _guard) = capture();
    // This process may already have had a peers path installed by another test
    // in this binary, so the seam is exercised directly here rather than
    // through the installed cell: the proxy hunk's own call is covered by the
    // no-mesh test above.
    let carried = egress::retry_through_peer(egress::CarriedRequest {
        url: &format!("https://{ORIGIN}/v1/messages"),
        method: &axum::http::Method::POST,
        headers: axum::http::HeaderMap::new(),
        body: Some(bytes::Bytes::from_static(b"{}")),
        peers_path: &peers,
    })
    .await;
    assert!(
        matches!(carried, egress::ViaCarry::NotTaken),
        "a Mac that does not answer took nothing, so the request keeps today's answer"
    );
    assert!(
        !collector.matching("did not take the carry").is_empty(),
        "the refusal must name the Mac that was asked: {:?}",
        collector
            .events()
            .iter()
            .map(|e| e.message.clone())
            .collect::<Vec<_>>()
    );
}

/// **Item 4.** The offline path still emits the count line the daily baseline
/// reads, with the fields it reads it by.
///
/// A name that cannot resolve takes the resolver arm, which is the arm the
/// metric is about. The assertion is on the message AND on the two fields,
/// because a cron that greps the message and reads `dns_failures` breaks on
/// either one going missing.
#[tokio::test]
async fn the_offline_path_still_logs_the_503_count_line() {
    let (collector, _guard) = capture();
    let response = post_messages(fleet("https://tcr-egress-test.invalid")).await;
    assert_eq!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "a dead resolver is the recoverable 503, not a 502"
    );

    let line = collector.matching("returning 503 to client");
    assert_eq!(
        line.len(),
        1,
        "one terminal line: {:?}",
        collector
            .events()
            .iter()
            .map(|e| e.message.clone())
            .collect::<Vec<_>>()
    );
    assert!(
        line[0].message.contains("name resolution is failing"),
        "the offline arm's own line, not the connect-phase one: {:?}",
        line[0]
    );
    assert!(
        line[0].fields.contains_key("dns_failures"),
        "the field the baseline counts by: {:?}",
        line[0]
    );
    assert!(
        line[0].fields.contains_key("retry_after"),
        "the field a client is steered by: {:?}",
        line[0]
    );
}

// ---------------------------------------------------------------------------
// The carry guard, which is about the REQUEST and not
// about one attempt
// ---------------------------------------------------------------------------

/// **The CRITICAL, red first.** A carry is offered only after the ladder has
/// rotated through the fleet, not on the first account's connect failure.
///
/// Two accounts, an upstream that refuses every connection (`127.0.0.1:1`
/// answers with an RST), and the order of the captured lines is the assertion:
/// the offer must come AFTER a rotation. With the guard as it shipped: one
/// conjunct, `err.is_connect()`, the offer fired on the first account, before
/// the second had been tried at all, and a fleet whose second account WOULD
/// have answered had its POST carried anyway.
///
/// Watched red by deleting `&& rotation_is_spent` from the guard at
/// `src/proxy.rs:2757`: the offer line then precedes every rotation line and
/// this fails on the index comparison.
#[tokio::test]
async fn the_carry_is_offered_only_after_the_fleet_has_been_walked() {
    let (collector, _guard) = capture();
    let response = post_messages(fleet_of(
        "http://127.0.0.1:1",
        &["alice@example.com", "bob@example.com"],
    ))
    .await;
    assert_eq!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "the answer is unchanged: a connect-phase failure is today's 503"
    );

    // `127.0.0.1` is not an origin the mesh carries, so this is the seam's own
    // first line and it fires once per offer: which makes it both the "was it
    // offered" and the "how many times" probe, without any peers file.
    let offers = collector.matching("not an origin the mesh carries");
    assert_eq!(
        offers.len(),
        1,
        "one offer per request, never one per attempt: {:?}",
        collector.messages()
    );
    let offered_at = collector
        .first_index("not an origin the mesh carries")
        .expect("the offer line");
    let rotated_at = collector
        .first_index("rotating to another account")
        .expect("a two-account fleet must rotate before it gives up");
    assert!(
        offered_at > rotated_at,
        "the carry must be offered after the rotation, not on the first account's failure: {:?}",
        collector.messages()
    );
}

/// **The double-send, red first.** An upstream answer anywhere in the request
/// means the request left this box, so there is no carry: even when the
/// attempt that fails last failed at connect.
///
/// The fake upstream answers ONE request and then stops listening, so attempt
/// one gets a real HTTP status and attempt two gets a connect refusal from the
/// same address. That is exactly the shape the shipped guard admitted: its only
/// conjunct described the current attempt, so a request an upstream may already
/// have acted on was carried a second time through a peer.
///
/// Watched red by deleting the `every_attempt_transport_failed(..)` conjunct
/// from the guard: the offer line appears and this fails.
#[tokio::test]
async fn an_upstream_answer_anywhere_in_the_request_stops_the_carry() {
    let listener = loopback().await;
    let upstream = format!(
        "http://{}",
        listener.local_addr().expect("the upstream address")
    );
    // One answer, then the listener is gone: the SECOND attempt's connect is
    // refused by the kernel. The listener is dropped BEFORE the response is
    // written, so the ordering is not a race.
    let fake_upstream = tokio::spawn(async move {
        let (mut conn, _) = listener.accept().await.expect("the first attempt connects");
        let mut seen = vec![0_u8; 4096];
        let _read = conn.read(&mut seen).await.expect("the request arrives");
        drop(listener);
        conn.write_all(
            b"HTTP/1.1 403 Forbidden\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
        )
        .await
        .expect("the answer is written");
        conn.flush().await.expect("the answer is flushed");
    });

    let (collector, _guard) = capture();
    let response = post_messages(fleet_of(
        &upstream,
        &["alice@example.com", "bob@example.com"],
    ))
    .await;
    fake_upstream.await.expect("the fake upstream finishes");

    assert_ne!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "an upstream answered, so this is not the no-route 503"
    );
    assert!(
        collector
            .matching("not an origin the mesh carries")
            .is_empty(),
        "nothing may be carried once an upstream has answered this request: {:?}",
        collector.messages()
    );
}

/// **The other half of the double-send, red first.** An attempt that died PAST
/// the connect phase means the request may already be on the wire, so no later
/// connect failure may carry it.
///
/// The fake upstream accepts, reads the request, stops listening and then
/// closes WITHOUT answering: attempt one fails past connect
/// (`unknown_outcome_transport_failure`), and the retry's connect is refused.
/// Every conjunct but `!unknown_outcome_transport_failure` is satisfied here :
/// nothing reached an upstream, the last attempt died at connect, the ladder is
/// out of moves: so this test isolates that one.
///
/// Watched red by deleting `!unknown_outcome_transport_failure` from the guard:
/// the offer line appears and this fails.
#[tokio::test]
async fn an_attempt_that_died_past_connect_stops_the_carry() {
    let listener = loopback().await;
    let upstream = format!(
        "http://{}",
        listener.local_addr().expect("the upstream address")
    );
    let fake_upstream = tokio::spawn(async move {
        let (mut conn, _) = listener.accept().await.expect("the first attempt connects");
        let mut seen = vec![0_u8; 4096];
        // The request is READ (it left this box), and then the connection
        // dies with no answer, which is the "unknown outcome" the guard is
        // about.
        let _read = conn.read(&mut seen).await.expect("the request arrives");
        drop(listener);
        drop(conn);
    });

    let (collector, _guard) = capture();
    let response = post_messages(fleet_of(&upstream, &["alice@example.com"])).await;
    fake_upstream.await.expect("the fake upstream finishes");

    assert!(
        collector
            .matching("not an origin the mesh carries")
            .is_empty(),
        "a request that may already be on the wire must never be carried again: {:?}",
        collector.messages()
    );
    assert_eq!(
        response.status(),
        StatusCode::BAD_GATEWAY,
        "and the answer is the unknown-outcome 502 it was before"
    );
}

// ---------------------------------------------------------------------------
// The stored setting, and the operator's knob
// ---------------------------------------------------------------------------

/// The setting is stored in the peers file and read back out of it, as the bare
/// word when there is nothing to tune and as an object when there is.
#[test]
fn via_round_trips_through_the_peers_file() {
    let peers = scratch("via-round-trip").join("tcr-peers.json");
    let mut file = PeerFile::default();

    // A file with no `via` key at all is `auto`: the default a fresh install
    // has, and the one every existing file on disk has.
    teamclaude_rs::peer::config::save(&peers, &file).expect("write the peers file");
    let read = teamclaude_rs::peer::config::read_or_default(&peers).expect("read it back");
    assert_eq!(egress::via_setting(&read), ViaSetting::auto());

    file.via = ViaSetting::off();
    teamclaude_rs::peer::config::save(&peers, &file).expect("write `off`");
    let read = teamclaude_rs::peer::config::read_or_default(&peers).expect("read `off` back");
    assert!(
        egress::via_setting(&read).is_off(),
        "the stored choice is what the request path reads"
    );
    let raw = std::fs::read_to_string(&peers).expect("the file text");
    assert!(
        raw.contains("\"via\": \"off\""),
        "with nothing tuned the key is the bare word, readable by eye: {raw}"
    );

    let pinned = PeerId([11_u8; 32]);
    file.via = ViaSetting::pinned(pinned)
        .with_setup_timeout_ms(1_500)
        .expect("1500ms is below the built-in bound");
    teamclaude_rs::peer::config::save(&peers, &file).expect("write a tuned pin");
    let raw = std::fs::read_to_string(&peers).expect("the file text");
    assert!(
        raw.contains("\"setupTimeoutMs\": 1500"),
        "a tuned setting stores the number beside the word: {raw}"
    );
    let read = teamclaude_rs::peer::config::read_or_default(&peers).expect("read the pin back");
    let setting = egress::via_setting(&read);
    assert_eq!(setting, file.via);
    assert_eq!(setting.to_spec(), pinned.to_wire());
    assert_eq!(
        setting.setup_timeout(),
        std::time::Duration::from_millis(1_500),
        "the request path waits the operator's number, not the constant"
    );
    assert_eq!(
        ViaSetting::auto().setup_timeout(),
        egress::CARRY_SETUP_TIMEOUT,
        "and an untuned setting is the constant"
    );

    // The knob only lowers. A carry runs after the direct path has already
    // failed, so a longer wait is worse for the caller than the answer it has.
    let refusal = ViaSetting::auto()
        .with_setup_timeout_ms(9_000)
        .expect_err("above the built-in bound");
    assert!(format!("{refusal:#}").contains("only lowers it"));
    let refusal = ViaSetting::auto()
        .with_setup_timeout_ms(0)
        .expect_err("zero is `off` spelled as a race");
    assert!(format!("{refusal:#}").contains("use `off` to mean that"));
}

/// A pinned row for `peer`, with `carry` granted or not: the field
/// [`egress::candidates_from`] actually reads, which is a different grant from
/// [`row`]'s `gateway` (that one is the gateway-side ACL, `Allow::gateway`,
/// checked by `peer_stream_gate_rows` and unrelated to which Macs THIS node
/// may ask to carry).
fn carry_row(peer: &PeerId, label: &str, addrs: Vec<String>, carry: bool) -> PeerRow {
    PeerRow {
        node: *peer,
        label: label.to_string(),
        endpoints: paired_endpoints(&addrs),
        added_at: 1_767_225_600_000,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: Allow {
            carry,
            ..Allow::default()
        },
        lend: Vec::new(),
    }
}

/// Only a Mac inside the carry grant is a candidate, and a Mac without one is
/// not asked even when it is the only Mac pinned.
#[test]
fn only_a_mac_inside_the_carry_grant_is_a_candidate() {
    let granted = PeerId([21_u8; 32]);
    let ungranted = PeerId([22_u8; 32]);
    let mut file = PeerFile::default();
    file.peers.push(carry_row(
        &granted,
        "granted",
        vec!["127.0.0.1:1".to_string()],
        true,
    ));
    file.peers.push(carry_row(
        &ungranted,
        "ungranted",
        vec!["127.0.0.1:2".to_string()],
        false,
    ));

    let candidates = egress::candidates_from(&file, &[(granted, 1_000), (ungranted, 9_000)]);
    assert_eq!(
        candidates.iter().map(|c| c.row.node).collect::<Vec<_>>(),
        vec![granted],
        "a pinned Mac outside the carry grant is not a candidate, however fresh it is"
    );
    assert!(
        egress::resolve_via(&ViaSetting::pinned(ungranted), &candidates).is_empty(),
        "and pinning it does not put it back"
    );
}

/// A `gateway` grant is the OTHER direction (this peer may ask US to carry)
/// and does not make a row a via candidate on its own: a row that only has
/// `gateway: true` (`carry` left at its default `false`), is never offered,
/// even under `auto`.
///
/// Watch it fail by reverting [`egress::candidates_from`]'s filter to
/// `row.allow.gateway`: this row is then a candidate and the assertion below
/// goes red.
#[test]
fn a_gateway_grant_alone_does_not_make_a_row_a_carry_candidate() {
    let peer = PeerId([23_u8; 32]);
    let mut file = PeerFile::default();
    file.peers.push(PeerRow {
        node: peer,
        label: "gateway-only".to_string(),
        endpoints: paired_endpoints(&["127.0.0.1:1".to_string()]),
        added_at: 1_767_225_600_000,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: Allow {
            gateway: true,
            carry: false,
            ..Allow::default()
        },
        lend: Vec::new(),
    });

    let candidates = egress::candidates_from(&file, &[(peer, 1_000)]);
    assert!(
        candidates.is_empty(),
        "a `gateway` grant with no `carry` grant is not a via candidate: {candidates:?}"
    );
    assert!(
        egress::resolve_via(&ViaSetting::auto(), &candidates).is_empty(),
        "and `auto` finds nothing to resolve, since the candidate list was already empty"
    );
}

/// `tcr peer via` writes the choice, refuses a Mac this one never pinned, and
/// refuses a timeout above the built-in bound.
///
/// Runs the binary this build produced, with `--peers` pointed at a scratch
/// file, so it reads no real config and touches no running proxy.
#[test]
fn via_writes_the_choice_and_refuses_a_mac_this_one_never_pinned() {
    let peers = scratch("via-cli").join("tcr-peers.json");
    let pinned = PeerId([31_u8; 32]);
    let stranger = PeerId([32_u8; 32]);
    let mut file = PeerFile::default();
    file.peers.push(row(
        &pinned,
        "studio-mac",
        vec!["127.0.0.1:1".to_string()],
        true,
    ));
    teamclaude_rs::peer::config::save(&peers, &file).expect("write the peers file");

    let stored = |peers: &std::path::Path| {
        egress::via_setting(
            &teamclaude_rs::peer::config::read_or_default(peers).expect("read the peers file"),
        )
    };

    let (out, err, ok) = run_tcr_peer(&peers, &["via", "off"]);
    assert!(ok, "`via off` must be accepted: {out}{err}");
    assert!(stored(&peers).is_off(), "and stored: {out}");

    let (out, err, ok) = run_tcr_peer(&peers, &["via", "auto"]);
    assert!(ok, "`via auto` must be accepted: {out}{err}");
    assert_eq!(stored(&peers), ViaSetting::auto());

    let (out, err, ok) = run_tcr_peer(&peers, &["via", &pinned.to_wire()]);
    assert!(ok, "a pinned Mac must be accepted: {out}{err}");
    assert_eq!(stored(&peers), ViaSetting::pinned(pinned));

    // A grant written by an unrelated command must not take the choice with
    // it: `via` is a field on the same struct every `tcr peer` write
    // round-trips, which is the whole reason it is stored there.
    let (out, err, ok) = run_tcr_peer(&peers, &["allow", &pinned.to_wire(), "gateway", "on"]);
    assert!(ok, "the unrelated write must succeed: {out}{err}");
    assert_eq!(
        stored(&peers),
        ViaSetting::pinned(pinned),
        "an unrelated `tcr peer` write must not drop the setting"
    );

    let (out, err, ok) = run_tcr_peer(&peers, &["via", &stranger.to_wire()]);
    assert!(!ok, "a Mac this one never pinned must be refused: {out}");
    assert!(
        err.contains("is not a Mac this one has pinned"),
        "and the refusal must say why: {err}"
    );
    assert_eq!(
        stored(&peers),
        ViaSetting::pinned(pinned),
        "a refused write changes nothing"
    );

    let (out, err, ok) = run_tcr_peer(&peers, &["via", "auto", "--setup-timeout-ms", "9000"]);
    assert!(
        !ok,
        "a timeout above the built-in bound must be refused: {out}"
    );
    assert!(err.contains("only lowers it"), "{err}");

    let (out, err, ok) = run_tcr_peer(&peers, &["via", "auto", "--setup-timeout-ms", "1200"]);
    assert!(ok, "lowering it must be accepted: {out}{err}");
    assert_eq!(
        stored(&peers).setup_timeout(),
        std::time::Duration::from_millis(1_200)
    );
}

/// **`tcr peer account --exits-from` writes the exit lock a panel switch
/// asks for, by peer id and by the pinned Mac's NAME, and `--must` changes the
/// strictness of a pin it was not given again.**
///
/// The panel builds an argv and reads a `--json` sibling; there was no argv
/// for this one, so an exit lock could only be set by hand-editing the config
/// this repository tells everybody never to hand-edit.
///
/// Four assertions, and the last two are the ones that catch a plausible
/// wrong implementation. A name has to resolve to the same pin the id writes,
/// or an operator reading `tcr peer ls` and typing what they see gets an
/// error or, worse, a different Mac. And `--must` on its own must leave the
/// pin alone: re-sending a pin the operator did not name is this verb
/// inventing one, and `local` is the value it would invent.
///
/// The config is a temp file. The live one at the operator's own config path
/// holds working credentials for real accounts and is never what a test
/// touches; `--config` is why this verb takes the flag at all.
///
/// Watched red three ways: return `Ok` from `set_account_egress` without
/// writing (both pins read `local`), drop the name lookup arm (the name is
/// refused as not a peer id), and apply `Some(Egress::Local)` when
/// `--exits-from` is absent (the `--must` step clears the pin).
#[test]
fn peer_account_writes_the_exit_lock_by_id_by_name_and_leaves_a_pin_alone() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let peers = dir.path().join("tcr-peers.json");
    let config = dir.path().join("teamclaude.json");
    let peer = node().id;
    let file = teamclaude_rs::peer::config::PeerFile {
        peers: vec![row(&peer, "attic-nuc", Vec::new(), true)],
        ..Default::default()
    };
    // Through the library's own writer, which is what gives the file the 0600
    // the binary refuses to read a peers file without.
    teamclaude_rs::peer::config::save(&peers, &file).expect("the peers fixture writes");
    // An obviously fake account: no real email is needed for a query that
    // matches on the name, and this file is written by the binary under test.
    std::fs::write(
        &config,
        r#"{"accounts":[{"name":"work-fake","accessToken":"not-a-real-token"}]}"#,
    )
    .expect("the config fixture writes");

    let wire = peer.to_wire();
    let read_pin = |label: &str| -> teamclaude_rs::config::EgressPin {
        let loaded = teamclaude_rs::config::load(&config)
            .unwrap_or_else(|err| panic!("the config reads back after {label}: {err}"));
        loaded.accounts[0].egress_pin()
    };

    let (out, err, ok) = run_tcr_peer(
        &peers,
        &[
            "account",
            "work-fake",
            "--config",
            config.to_str().expect("a utf-8 path"),
            "--exits-from",
            &wire,
        ],
    );
    assert!(ok, "the pin by id: {out}{err}");
    assert_eq!(
        read_pin("the pin by id").egress,
        teamclaude_rs::config::Egress::Via(peer),
        "the account leaves through the Mac the id names: {out}"
    );
    assert!(
        !read_pin("the pin by id").strict,
        "and a pin nobody called must falls back rather than refusing: {out}"
    );

    let (out, err, ok) = run_tcr_peer(
        &peers,
        &[
            "account",
            "work-fake",
            "--config",
            config.to_str().expect("a utf-8 path"),
            "--exits-from",
            "local",
        ],
    );
    assert!(ok, "back to local: {out}{err}");
    assert_eq!(
        read_pin("back to local").egress,
        teamclaude_rs::config::Egress::Local,
        "`local` is the word for this Mac's own socket: {out}"
    );

    let (out, err, ok) = run_tcr_peer(
        &peers,
        &[
            "account",
            "work-fake",
            "--config",
            config.to_str().expect("a utf-8 path"),
            "--exits-from",
            "attic-nuc",
        ],
    );
    assert!(
        ok,
        "the NAME a pinned Mac shows under resolves to its id: {out}{err}"
    );
    assert_eq!(
        read_pin("the pin by name").egress,
        teamclaude_rs::config::Egress::Via(peer),
        "and lands the same pin the id did: {out}"
    );

    let (out, err, ok) = run_tcr_peer(
        &peers,
        &[
            "account",
            "work-fake",
            "--config",
            config.to_str().expect("a utf-8 path"),
            "--must",
        ],
    );
    assert!(ok, "--must on its own: {out}{err}");
    let after = read_pin("--must on its own");
    assert_eq!(
        after.egress,
        teamclaude_rs::config::Egress::Via(peer),
        "--must changes what an unreachable exit COSTS and never where the exit is: {out}"
    );
    assert!(
        after.strict,
        "and it does change that: a request this exit cannot carry is now refused: {out}"
    );

    let (out, err, ok) = run_tcr_peer(
        &peers,
        &[
            "account",
            "work-fake",
            "--config",
            config.to_str().expect("a utf-8 path"),
            "--exits-from",
            "no-such-mac",
        ],
    );
    assert!(
        !ok,
        "a name no pinned Mac answers to is refused rather than written: {out}{err}"
    );
    assert_eq!(
        read_pin("the refused name").egress,
        teamclaude_rs::config::Egress::Via(peer),
        "and the refusal leaves the pin that was there: {out}{err}"
    );
}

/// `tcr peer <args>` out of this build, never the installed binary.
fn run_tcr_peer(peers: &std::path::Path, args: &[&str]) -> (String, String, bool) {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_tcr"))
        .arg("peer")
        .args(args)
        .arg("--peers")
        .arg(peers)
        .output()
        .expect("spawn the tcr built by this build");
    (
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.success(),
    )
}

/// **Item 6.** `via off` dials nothing at all: no gateway sees a connection,
/// and the request is not made to wait out a setup timeout.
#[tokio::test]
async fn via_off_costs_no_dial_and_no_wait() {
    let dir = scratch("via-off");
    let peers = dir.join("tcr-peers.json");
    // A real listener, so a dial would be accepted and counted. Nothing may
    // reach it.
    let gateway = loopback().await;
    let gateway_addr = gateway.local_addr().expect("the gateway address");
    let dialled = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = Arc::clone(&dialled);
    let accepting = tokio::spawn(async move {
        while let Ok((conn, _)) = gateway.accept().await {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            drop(conn);
        }
    });

    let mut file = PeerFile::default();
    file.peers.push(row(
        &PeerId([41_u8; 32]),
        "studio-mac",
        vec![gateway_addr.to_string()],
        true,
    ));
    file.via = ViaSetting::off();
    teamclaude_rs::peer::config::save(&peers, &file).expect("write the peers file");

    let (collector, _guard) = capture();
    let started = std::time::Instant::now();
    let carried = egress::retry_through_peer(egress::CarriedRequest {
        url: &format!("https://{ORIGIN}/v1/messages"),
        method: &axum::http::Method::POST,
        headers: axum::http::HeaderMap::new(),
        body: Some(bytes::Bytes::from_static(b"{}")),
        peers_path: &peers,
    })
    .await;
    let elapsed = started.elapsed();
    accepting.abort();

    assert!(
        matches!(carried, egress::ViaCarry::NotTaken),
        "`off` means no carry"
    );
    assert_eq!(
        dialled.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "`off` must not dial the Mac it is pinned to, or any other"
    );
    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "`off` must cost a file read and nothing else, not a setup timeout: {elapsed:?}"
    );
    assert!(
        !collector
            .matching("never routes out through a peer")
            .is_empty(),
        "and it says so once, at debug: {:?}",
        collector.messages()
    );
    // The discriminator for the FAST PATH rather than for `off` itself:
    // `resolve_via` would also answer "no candidates" for `off`, three reads
    // later. Nothing may read the runtime state file: which logs when it is
    // missing: before the setting is honoured.
    assert!(
        collector.matching("peer state:").is_empty()
            && collector.matching("no runtime state to order").is_empty(),
        "`off` must be honoured before the runtime state is even read: {:?}",
        collector.messages()
    );
}

/// **Item 6, the lever end to end.** A gateway that accepts the connection and
/// then says nothing is abandoned at the operator's own timeout, not at the
/// five-second constant.
///
/// Watched red by making `retry_through_peer` use `CARRY_SETUP_TIMEOUT` instead
/// of `setting.setup_timeout()`: the call then takes five seconds and the
/// elapsed-time assertion fails.
#[tokio::test]
async fn the_operators_timeout_is_what_a_silent_gateway_is_given() {
    let dir = scratch("via-timeout");
    let peers = dir.join("tcr-peers.json");
    // Accepts, then holds the socket open and never speaks: the handshake
    // waits for a message that never comes, which is what the setup timeout
    // exists to bound.
    let gateway = loopback().await;
    let gateway_addr = gateway.local_addr().expect("the gateway address");
    let held = tokio::spawn(async move {
        let mut open = Vec::new();
        while let Ok((conn, _)) = gateway.accept().await {
            open.push(conn);
        }
    });

    let mut file = PeerFile::default();
    file.peers.push(carry_row(
        &PeerId([42_u8; 32]),
        "silent-mac",
        vec![gateway_addr.to_string()],
        true,
    ));
    file.via = ViaSetting::auto()
        .with_setup_timeout_ms(300)
        .expect("300ms is below the built-in bound");
    teamclaude_rs::peer::config::save(&peers, &file).expect("write the peers file");

    let (collector, _guard) = capture();
    let started = std::time::Instant::now();
    let carried = egress::retry_through_peer(egress::CarriedRequest {
        url: &format!("https://{ORIGIN}/v1/messages"),
        method: &axum::http::Method::POST,
        headers: axum::http::HeaderMap::new(),
        body: Some(bytes::Bytes::from_static(b"{}")),
        peers_path: &peers,
    })
    .await;
    let elapsed = started.elapsed();
    held.abort();

    assert!(
        matches!(carried, egress::ViaCarry::NotTaken),
        "a silent Mac carries nothing"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "the operator's 300ms must bound this, not the 5s constant: {elapsed:?}"
    );
    let gave_up = collector.matching("did not answer inside the setup timeout");
    assert_eq!(
        gave_up.len(),
        1,
        "one line, naming the Mac and the bound it was given: {:?}",
        collector.messages()
    );
    assert_eq!(
        gave_up[0].fields.get("timeout_ms").map(String::as_str),
        Some("300"),
        "the line must carry the bound that was actually used: {:?}",
        gave_up[0]
    );
}

/// **A lender that TAKES the carry and never answers releases the request
/// anyway.**
///
/// The setup timeout bounded the dial and the handshake only,
/// so a Mac that completed the handshake, accepted the TUNNEL and then never
/// produced a reply held the client's request open with no bound at all:
/// `send_through` waits on a splice whose far end nobody is driving. The
/// deadline now covers the whole carry: dial, handshake and the origin's reply
///: with the operator's own value.
///
/// The far end here is the production gateway handler with a FIXED origin that
/// accepts TCP and then says nothing, which is exactly the shape that hung: the
/// carry is real and taken, the requester's ClientHello goes out, and no
/// ServerHello ever comes back. Nothing leaves this box.
///
/// Watched red by moving `send_through(...)` back out from under the deadline.
/// It was given a 600-second one instead, purely so the mutation run would end,
/// and it used every second of it: the assertion below failed with
/// `elapsed = 600.007306334s`. That is the measured answer to "what else bounds
/// this path": nothing does. The client `send_through` builds sets no timeout
/// of its own, so before this deadline a lender that took a carry and then
/// stayed mute held the caller's request open for as long as it cared to.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lender_that_takes_the_carry_and_never_answers_still_releases_the_request() {
    let dir = scratch("carry-deadline");
    let peers = dir.join("tcr-peers.json");
    // The requester's key has to exist BEFORE the carry runs, because the
    // gateway pins it: `retry_through_peer` mints it in this same directory.
    let requester = teamclaude_rs::peer::id::NodeKey::load_or_mint(&dir).expect("mint this node");
    let gateway = node();

    // An origin that accepts and then says nothing at all. The TLS handshake
    // through the carry can therefore never complete.
    let silent_origin = loopback().await;
    let origin_addr = silent_origin.local_addr().expect("the origin address");
    let origin_task = tokio::spawn(async move {
        let mut open = Vec::new();
        while let Ok((conn, _peer)) = silent_origin.accept().await {
            open.push(conn);
        }
    });

    let gateway_listener = loopback().await;
    let gateway_addr = gateway_listener.local_addr().expect("the gateway address");
    let gateway_task = gateway_on(
        gateway_listener,
        gateway.secret,
        vec![row(&requester.id(), "requester", Vec::new(), true)],
        OriginRoute::Fixed(origin_addr),
        1024 * 1024,
    );

    let mut file = PeerFile::default();
    file.peers.push(carry_row(
        &gateway.id,
        "mute-lender",
        vec![gateway_addr.to_string()],
        true,
    ));
    file.via = ViaSetting::auto()
        .with_setup_timeout_ms(700)
        .expect("700ms is below the built-in bound");
    teamclaude_rs::peer::config::save(&peers, &file).expect("write the peers file");

    let (collector, _guard) = capture();
    let started = std::time::Instant::now();
    let carried = egress::retry_through_peer(egress::CarriedRequest {
        url: &format!("https://{ORIGIN}/v1/messages"),
        method: &axum::http::Method::POST,
        headers: axum::http::HeaderMap::new(),
        body: Some(bytes::Bytes::from_static(b"{}")),
        peers_path: &peers,
    })
    .await;
    let elapsed = started.elapsed();
    origin_task.abort();
    gateway_task.abort();

    // **THE ITEM-1 GATE.** This assertion used to read `carried.is_none()`,
    // which is the same word this path answers for `via off` and for a Mac with
    // no address: the caller read it as "no carry happened", slept, and sent
    // the request again on the same account. The gateway TOOK this one, and
    // what failed afterwards is a splice that may already have put it on the
    // wire.
    let egress::ViaCarry::TakenAndFailed(gap) = &carried else {
        panic!(
            "a carry the gateway took and then dropped is not the same fact as a Mac that              never took it: this request may already have been served"
        );
    };
    assert!(
        gap.contains("took this request"),
        "the gap names what happened, and it reaches the client's 502: {gap}"
    );
    let answer = egress::delivered_unknown_response(gap);
    assert_eq!(
        answer.status().as_u16(),
        502,
        "a request that may already have run is answered, never retried"
    );
    assert_eq!(
        answer
            .headers()
            .get("x-should-retry")
            .and_then(|value| value.to_str().ok()),
        Some("false"),
        "a client that retries this pays for the request twice"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "the client's request must be released on the carry deadline, not held \
         until something else gives up: {elapsed:?}"
    );
    let gave_up = collector.matching("never answered inside the carry deadline");
    assert_eq!(
        gave_up.len(),
        1,
        "one line, naming the Mac that took the carry: {:?}",
        collector.messages()
    );
    assert_eq!(
        gave_up[0].fields.get("timeout_ms").map(String::as_str),
        Some("700"),
        "the line must carry the bound that was actually used: {gave_up:?}"
    );
}

// ---------------------------------------------------------------------------
// The cap counts the carries that are still open
// ---------------------------------------------------------------------------

/// **Item 3, red first.** Five carries opened at once against a four-carry
/// budget: the fifth is refused, and closing one lets the next in.
///
/// Before this change the ledger only charged a carry when it CLOSED, so all
/// five were admitted and each was handed the whole remaining hour as its
/// allowance: a per-hour cap that five concurrent streams could spend five
/// times over. Watched red by deleting `self.reserved_by(peer)` from
/// `TunnelBudget::admit`.
#[test]
fn five_concurrent_carries_at_a_four_carry_budget_refuse_the_fifth() {
    let peer = PeerId([51_u8; 32]);
    let other = PeerId([52_u8; 32]);
    let now = 1_767_225_600_000_i64;
    // A cap of exactly four slices, so the arithmetic is the assertion rather
    // than a coincidence of the constant.
    let cap = 4 * 1_000_u64;
    let mut budget = TunnelBudget::new();

    let mut open = Vec::new();
    for n in 0..4 {
        match budget.admit(&peer, cap, now) {
            Admission::Carry {
                allowance,
                open: id,
            } => {
                assert_eq!(allowance, 1_000, "carry {n} holds one slice of the hour");
                open.push(id);
            }
            refused => panic!("carry {n} of four must be admitted, got {refused:?}"),
        }
    }
    assert_eq!(
        budget.reserved_by(&peer),
        cap,
        "four open carries hold the whole hour"
    );
    assert_eq!(
        budget.admit(&peer, cap, now),
        Admission::OverBudget { spent: cap, cap },
        "the fifth is refused while the other four are still open"
    );
    // Another peer's hour is untouched: the cap is per peer.
    assert!(matches!(
        budget.admit(&other, cap, now),
        Admission::Carry { .. }
    ));

    // One closes having spent almost nothing, and its slice comes back.
    let closed = open.pop().expect("four were opened");
    budget.close(closed, &peer, 10, now);
    match budget.admit(&peer, cap, now) {
        Admission::Carry { allowance, .. } => assert_eq!(
            allowance, 990,
            "the slice that came back, less what the closed carry actually spent"
        ),
        refused => panic!("a released slice must admit the next carry, got {refused:?}"),
    }
    // Closing the same reservation twice cannot free a second slice.
    let before = budget.reserved_by(&peer);
    budget.close(closed, &peer, 0, now);
    assert_eq!(
        budget.reserved_by(&peer),
        before,
        "a double close charges, and frees nothing extra"
    );
}

// ---------------------------------------------------------------------------
// A carried request is a served request
// ---------------------------------------------------------------------------

/// **Item 4.** A carried request lands in the ledger the TUI and
/// `tcr status --json` read, against the account whose credential it used.
///
/// Driven through `record_carried` rather than through `proxy::handle`, because
/// a carry that gets as far as a response needs the upstream to BE
/// `api.anthropic.com` (the origin allow-list), and this suite reaches no
/// network beyond loopback. The proxy's own call site passes exactly these
/// values; `handle`'s terminal outcome passes the same ones to the same three
/// methods.
///
/// Watched red by deleting the `push_log` call from `record_carried`.
#[tokio::test]
async fn a_carried_request_lands_in_the_ledger() {
    let manager = fleet_of("http://127.0.0.1:1", &["alice@example.com"]);
    let before = manager
        .snapshot(time::OffsetDateTime::now_utc())
        .recent
        .len();

    egress::record_carried(
        &manager,
        egress::CarriedRecord {
            account_idx: 0,
            account: Some("alice@example.com".to_string()),
            session_key: Some(7),
            session_kind: teamclaude_rs::stats::SessionKind::Stable,
            wire_session_id: Some("11111111-1111-1111-1111-111111111111"),
            model: Some("claude-opus-4-6".to_string()),
            method: "POST".to_string(),
            path: "/v1/messages".to_string(),
            status: 200,
            tool_uses: &[],
            tool_results: &[],
            upstream_headers: &axum::http::HeaderMap::new(),
        },
    )
    .await;

    let snapshot = manager.snapshot(time::OffsetDateTime::now_utc());
    assert_eq!(
        snapshot.recent.len(),
        before + 1,
        "a carried request is one line in the ring buffer, like any other"
    );
    let line = snapshot.recent.first().expect("the line just pushed");
    assert_eq!(line.path, "/v1/messages");
    assert_eq!(line.method, "POST");
    assert_eq!(line.status, 200);
    assert_eq!(
        line.account, "alice@example.com",
        "the account is OURS: a carry borrows a route, never an account"
    );
    assert!(
        snapshot.sessions.iter().any(|session| session.requests > 0),
        "and the served counter moved: {:?}",
        snapshot.sessions
    );
}

/// **A carried request's spend is visible to the rotation model.**
///
/// The review's finding at `egress.rs:840`: a carried response carries the
/// pooled account's `anthropic-ratelimit-unified-*` headers exactly as a direct
/// one does, and until `record_carried` folded them in, that spend was invisible
/// to `lendable_fraction`: the figure this whole feature lends on. So a Mac
/// that had spent its window through a peer went on advertising it as lendable.
///
/// The figures asserted are the ones the fake upstream "sent": utilization
/// `0.42` on a 5-hour window that resets in two hours, read back through the
/// public accessor `lendable_fraction` itself reads
/// (`Manager::window_utilizations`).
///
/// Watched red by deleting the `manager.update_quota` call from
/// `record_carried` (`src/peer/egress.rs`): the window stays `None`, i.e. "we
/// have never seen this account's quota", and the assertion below names it.
#[tokio::test]
async fn a_carried_requests_spend_reaches_the_quota_window() {
    let manager = fleet_of("http://127.0.0.1:1", &["alice@example.com"]);
    let now = time::OffsetDateTime::now_utc();
    assert_eq!(
        manager.window_utilizations(tcr_peer_wire::Window::FiveHour, now),
        vec![None],
        "before the carry this fleet has never seen this account's window"
    );

    let reset = (now + time::Duration::hours(2)).unix_timestamp();
    let mut upstream_headers = axum::http::HeaderMap::new();
    upstream_headers.insert(
        "anthropic-ratelimit-unified-5h-utilization",
        "0.42".parse().expect("a header value"),
    );
    upstream_headers.insert(
        "anthropic-ratelimit-unified-5h-reset",
        reset.to_string().parse().expect("a header value"),
    );

    egress::record_carried(
        &manager,
        egress::CarriedRecord {
            account_idx: 0,
            account: Some("alice@example.com".to_string()),
            session_key: Some(9),
            session_kind: teamclaude_rs::stats::SessionKind::Stable,
            wire_session_id: Some("11111111-1111-1111-1111-111111111111"),
            model: Some("claude-opus-4-6".to_string()),
            method: "POST".to_string(),
            path: "/v1/messages".to_string(),
            status: 200,
            tool_uses: &[],
            tool_results: &[],
            upstream_headers: &upstream_headers,
        },
    )
    .await;

    assert_eq!(
        manager.window_utilizations(tcr_peer_wire::Window::FiveHour, now),
        vec![Some(0.42)],
        "the carried response's own headers are what the window must now read"
    );
    assert!(
        manager.lendable_fraction(
            &tcr_peer_wire::LendScope::All,
            tcr_peer_wire::Window::FiveHour,
            now
        ) < 1.0,
        "and the figure this feature lends on is the one that moved"
    );
}

/// **A carried rejection holds the account, exactly as a direct one does.**
///
/// The review's finding: `PinnedEgress::Answered` returns the carried response
/// from ABOVE the rotation loop, so the 401/403/429/529 ladder never saw it. A
/// pinned account that upstream had just rejected went on being selected, and
/// the next request took the same rejection: the pin turned off pacing,
/// refreshing and holding for the one account whose traffic an operator most
/// wanted shaped.
///
/// The half of the ladder that is a FACT ABOUT THE ACCOUNT is recorded in
/// `record_carried` now. The other half, rotating this request away, cannot
/// apply: the response is already on its way to the client, and a pinned
/// account has nowhere to rotate to anyway.
///
/// Watched red by deleting the `record_the_hold_the_ladder_would` call from
/// `record_carried`: the account reads `Active` and serves the next request
/// straight into the same rejection.
#[tokio::test]
async fn a_carried_rejection_holds_the_account_the_way_a_direct_one_does() {
    let manager = fleet_of("http://127.0.0.1:1", &["alice@example.com"]);
    assert_ne!(
        manager.account_status(0),
        Some(teamclaude_rs::manager::AccountStatus::Throttled),
        "before the carry this account is servable: the control for the assertion below"
    );

    let mut upstream_headers = axum::http::HeaderMap::new();
    upstream_headers.insert(
        "anthropic-ratelimit-unified-status",
        "rejected".parse().expect("a header value"),
    );
    upstream_headers.insert(
        "anthropic-ratelimit-unified-5h-status",
        "rejected".parse().expect("a header value"),
    );
    upstream_headers.insert("retry-after", "600".parse().expect("a header value"));

    egress::record_carried(
        &manager,
        egress::CarriedRecord {
            account_idx: 0,
            account: Some("alice@example.com".to_string()),
            session_key: None,
            session_kind: teamclaude_rs::stats::SessionKind::Stable,
            wire_session_id: None,
            model: None,
            method: "POST".to_string(),
            path: "/v1/messages".to_string(),
            status: 429,
            tool_uses: &[],
            tool_results: &[],
            upstream_headers: &upstream_headers,
        },
    )
    .await;

    assert_eq!(
        manager.account_status(0),
        Some(teamclaude_rs::manager::AccountStatus::Throttled),
        "a rejection that arrived through a peer holds the account, or the next request \
         is sent straight into it again"
    );
    assert!(
        !manager.hold_expired(0, teamclaude_rs::now_ms()),
        "and the hold is still running: it was armed for what the origin asked"
    );
}

/// **A carried response runs the same recovery the direct path runs.**
///
/// The review's finding: `record_the_hold_the_ladder_would` recorded the 429
/// hold and nothing else, and no carried status ever ran `clear_rate_limited`.
/// The direct path runs it on every non-429 (`src/proxy.rs`, "any non-429 is
/// live proof a rate-limit hold no longer binds"), and a pinned account's
/// requests all leave this way, so an account Throttled by ONE carried 429
/// never came back to Active: it dropped out of selection, out of keep-warm and
/// out of `handoff_bearer` for good, while the very next carried 200 proved it
/// was serving.
///
/// Watch it fail by deleting the `clear_rate_limited` call from
/// `record_the_hold_the_ladder_would`: the account is still Throttled after a
/// carried 200.
#[tokio::test]
async fn a_carried_success_clears_the_hold_a_carried_rejection_armed() {
    let manager = fleet_of("http://127.0.0.1:1", &["alice@example.com"]);
    manager.mark_rate_limited(0, 300);
    assert_eq!(
        manager.account_status(0),
        Some(teamclaude_rs::manager::AccountStatus::Throttled),
        "the fixture starts held, or the assertion below is about nothing"
    );

    egress::record_carried(
        &manager,
        egress::CarriedRecord {
            account_idx: 0,
            account: Some("alice@example.com".to_string()),
            session_key: None,
            session_kind: teamclaude_rs::stats::SessionKind::Stable,
            wire_session_id: None,
            model: None,
            method: "POST".to_string(),
            path: "/v1/messages".to_string(),
            status: 200,
            tool_uses: &[],
            tool_results: &[],
            upstream_headers: &axum::http::HeaderMap::new(),
        },
    )
    .await;

    assert_ne!(
        manager.account_status(0),
        Some(teamclaude_rs::manager::AccountStatus::Throttled),
        "a carried 200 is live proof the hold no longer binds, exactly as a direct one is"
    );
}

/// **A carried 401 on a token-only account condemns it, the way the direct path
/// does.**
///
/// The direct path's first 401 arm: an account with no refresh token has a
/// credential nothing but a re-login can revive, so it is marked `Error` rather
/// than left Active to be selected, 401'd and rotated away from on every
/// request while `tcr status` reports it healthy. A carried 401 recorded none
/// of that.
///
/// Watch it fail by deleting the `status == 401` arm from
/// `record_the_hold_the_ladder_would`: the account stays Active.
#[tokio::test]
async fn a_carried_401_on_a_token_only_account_marks_it_error() {
    let manager = fleet_with(
        "http://127.0.0.1:1",
        vec![Account {
            name: "alice@example.com".to_string(),
            account_uuid: Some("11111111-1111-1111-1111-111111111111".to_string()),
            access_token: "at-0".to_string(),
            // A `tcr login --token` account: inference only, and no refresh
            // that could ever revive a rejected credential.
            refresh_token: None,
            ..fake_account()
        }],
    );

    egress::record_carried(
        &manager,
        egress::CarriedRecord {
            account_idx: 0,
            account: Some("alice@example.com".to_string()),
            session_key: None,
            session_kind: teamclaude_rs::stats::SessionKind::Stable,
            wire_session_id: None,
            model: None,
            method: "POST".to_string(),
            path: "/v1/messages".to_string(),
            status: 401,
            tool_uses: &[],
            tool_results: &[],
            upstream_headers: &axum::http::HeaderMap::new(),
        },
    )
    .await;

    assert_eq!(
        manager.account_status(0),
        Some(teamclaude_rs::manager::AccountStatus::Error),
        "a rejected credential with no refresh is dead whichever route the request left by"
    );
}

/// **A carried 429 with no guidance parks for what the direct path parks, not a
/// minute.**
///
/// `retry_after.unwrap_or(60)` fabricated a sixty-second hold for a 429 that
/// named no `retry-after`, which is the exact number
/// `classify_transient_429` exists to have stopped using: the direct path parks
/// `NO_GUIDANCE_HOLD_SECS` plus up to five seconds of jitter, 15 to 20 seconds,
/// so the fleet un-parks staggered instead of going dark for a minute. A pinned
/// account's traffic all leaves this way, so the old number was the one an
/// operator actually felt.
///
/// The instrument is the hold's own deadline, probed 25 seconds out: past every
/// value the direct path can choose and well inside the fabricated 60.
///
/// Watch it fail by restoring `manager.mark_rate_limited(idx,
/// retry_after.clamp(1, 300))`: the hold is 60 seconds and is still running.
#[tokio::test]
async fn a_carried_transient_429_parks_for_what_the_direct_path_parks() {
    let manager = fleet_of("http://127.0.0.1:1", &["alice@example.com"]);

    // A 429 with NO unified rejection and NO `retry-after`: the transient shape.
    egress::record_carried(
        &manager,
        egress::CarriedRecord {
            account_idx: 0,
            account: Some("alice@example.com".to_string()),
            session_key: None,
            session_kind: teamclaude_rs::stats::SessionKind::Stable,
            wire_session_id: None,
            model: None,
            method: "POST".to_string(),
            path: "/v1/messages".to_string(),
            status: 429,
            tool_uses: &[],
            tool_results: &[],
            upstream_headers: &axum::http::HeaderMap::new(),
        },
    )
    .await;

    assert_eq!(
        manager.account_status(0),
        Some(teamclaude_rs::manager::AccountStatus::Throttled),
        "a 429 still holds the account: this is about how long, not whether"
    );
    assert!(
        !manager.hold_expired(0, teamclaude_rs::now_ms() + 14_000),
        "and the hold is a real one: still running 14 seconds out"
    );
    assert!(
        manager.hold_expired(0, teamclaude_rs::now_ms() + 25_000),
        "a transient 429 with no guidance parks 15 to 20 seconds, the way the direct \
         path parks it, and not the fabricated minute"
    );
}

// ---------------------------------------------------------------------------
// The REAL listener carries a TUNNEL
// ---------------------------------------------------------------------------

/// **The shipped dispatch arm hands a TUNNEL to the production handler.**
///
/// Every other carry test in this file drives `gateway_on`, a test-only accept
/// loop, because `listener::serve_stream`'s `StreamKind::Tunnel` arm was a
/// `bail!("phase 6 lands TUNNEL")`: so the three production calls were
/// measured in the right order by a copy of the arm rather than by the arm.
/// This one drives `listener::serve_on_with`: the shipped accept loop, the
/// shipped handshake, the shipped stream gate and the shipped dispatch.
///
/// **What it measures, and why it stays off the network.** The arm carries with
/// `OriginRoute::Resolve`, which in production resolves `api.anthropic.com`
/// for real, so a completed carry cannot be a test on this box. It does not
/// need to be: the handler answers the allow-list and the byte cap and then
/// waits for the requester's ClientHello for `FIRST_RECORD_TIMEOUT` BEFORE it
/// dials anything (`tunnel::handle_tunnel_on`'s documented order). A requester
/// that sends the header and then nothing at all therefore proves the whole
/// arm: the stream reached the handler, which owned it and closed it on its
/// own five-second deadline, without one packet leaving this Mac.
///
/// Watched red: restore the arm's `bail!` and the connection is dropped
/// immediately after the header, so `elapsed` is milliseconds rather than the
/// five seconds asserted below.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_real_listener_hands_a_tunnel_to_the_production_handler() {
    let dir = scratch("real-listener-tunnel");
    let requester = node();
    let gateway_key =
        teamclaude_rs::peer::id::NodeKey::load_or_mint(&dir).expect("the gateway's node key");

    // A pinned requester holding the one carry grant, in a peers file written
    // by the one writer (0600, atomic).
    let peers = dir.join("tcr-peers.json");
    let mut file = PeerFile::default();
    file.peers
        .push(row(&requester.id, "requester", Vec::new(), true));
    teamclaude_rs::peer::config::save(&peers, &file).expect("write the peers file");

    let listening = loopback().await;
    let gateway_addr = listening.local_addr().expect("the gateway address");
    let store = teamclaude_rs::peer::config::PeerStore::open(&peers).expect("open the peers file");
    // The state file is inside the scratch dir and does not exist, so this test
    // reads nothing under the operator's cache directory and cannot be
    // perturbed by an operator with a pairing window open while the suite runs.
    let context =
        listener::SessionContext::new(&gateway_key, store.path(), &dir.join("peer-state.json"));
    tokio::spawn(async move {
        let _ = listener::serve_on_with(listening, context).await;
    });

    let mut stream = TcpStream::connect(gateway_addr)
        .await
        .expect("dial the real listener");
    let mut session = noise::dial_handshake(
        &mut stream,
        &requester.secret,
        Handshake::Return,
        Some(&gateway_key.id().0),
        None,
    )
    .await
    .expect("a pinned peer completes the handshake with the real listener");
    let header = StreamHeader {
        kind: StreamKind::Tunnel,
        target: Some(TunnelTarget::Origin {
            host: ORIGIN.to_string(),
            port: 443,
        }),
        via: Vec::new(),
        hops_remaining: 1,
        request_id: 81,
    };
    noise::send_encrypted(
        &mut stream,
        &mut session.transport,
        &serde_json::to_vec(&header).expect("the header serializes"),
    )
    .await
    .expect("the header is written");

    // And then nothing at all: the handler's own deadline is what closes this.
    let started = std::time::Instant::now();
    let mut carried = Vec::new();
    let closed = tokio::time::timeout(
        tunnel::FIRST_RECORD_TIMEOUT * 3,
        stream.read_to_end(&mut carried),
    )
    .await;
    let elapsed = started.elapsed();

    assert!(
        closed.is_ok(),
        "the production handler must close a silent carry on its own deadline, not hold the \
         socket open: still open after {elapsed:?}"
    );
    assert!(
        elapsed >= tunnel::FIRST_RECORD_TIMEOUT,
        "the stream reached `tunnel::handle_tunnel_on` and was closed by its \
         FIRST_RECORD_TIMEOUT, so the close cannot be quicker than that: {elapsed:?}. A close \
         in milliseconds is the dispatch arm refusing the kind outright."
    );
    assert!(
        elapsed < tunnel::FIRST_RECORD_TIMEOUT * 2,
        "and it is that deadline rather than some socket timeout: {elapsed:?}"
    );
}

/// **A TUNNEL whose header names no target is refused by the shipped arm.**
///
/// The gate above it (`peer_stream_gate_rows`) already refuses
/// `(Tunnel, None)`, so this is the arm's own belt-and-braces: the pattern
/// match that reads the target cannot be written as an `unwrap`, and the
/// refusal has to be the arm's rather than a panic in a connection task.
///
/// Watched with the gate's `(StreamKind::Tunnel, None) => false` row flipped to
/// `true`: the arm still refuses and the listener still answers other
/// connections, which is what this measures.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tunnel_with_no_target_is_refused_by_the_real_listener() {
    let dir = scratch("real-listener-tunnel-no-target");
    let requester = node();
    let gateway_key =
        teamclaude_rs::peer::id::NodeKey::load_or_mint(&dir).expect("the gateway's node key");

    let peers = dir.join("tcr-peers.json");
    let mut file = PeerFile::default();
    file.peers
        .push(row(&requester.id, "requester", Vec::new(), true));
    teamclaude_rs::peer::config::save(&peers, &file).expect("write the peers file");

    let listening = loopback().await;
    let gateway_addr = listening.local_addr().expect("the gateway address");
    let store = teamclaude_rs::peer::config::PeerStore::open(&peers).expect("open the peers file");
    let context =
        listener::SessionContext::new(&gateway_key, store.path(), &dir.join("peer-state.json"));
    tokio::spawn(async move {
        let _ = listener::serve_on_with(listening, context).await;
    });

    let mut stream = TcpStream::connect(gateway_addr)
        .await
        .expect("dial the real listener");
    let mut session = noise::dial_handshake(
        &mut stream,
        &requester.secret,
        Handshake::Return,
        Some(&gateway_key.id().0),
        None,
    )
    .await
    .expect("a pinned peer completes the handshake");
    let header = StreamHeader {
        kind: StreamKind::Tunnel,
        target: None,
        via: Vec::new(),
        hops_remaining: 1,
        request_id: 82,
    };
    noise::send_encrypted(
        &mut stream,
        &mut session.transport,
        &serde_json::to_vec(&header).expect("the header serializes"),
    )
    .await
    .expect("the header is written");

    let mut carried = Vec::new();
    let read = tokio::time::timeout(
        tunnel::FIRST_RECORD_TIMEOUT,
        stream.read_to_end(&mut carried),
    )
    .await
    .expect("a targetless TUNNEL is refused at once, not waited on")
    .expect("the socket reads to its end");
    assert_eq!(
        read, 0,
        "the refusal writes nothing back: a stranger's malformed header earns no answer"
    );
}

// ---------------------------------------------------------------------------
// The exit lock: one account, one way out, every request (decisions row 15)
// ---------------------------------------------------------------------------

/// [`fake_account`], locked to leave from `peer`.
///
/// The two keys are written the way an operator writes them, as JSON on the
/// account row, so this fixture exercises the parse and not a Rust value that
/// skipped it.
fn pinned_account(peer: &PeerId, strict: bool) -> Account {
    Account {
        egress: teamclaude_rs::config::Egress::Via(*peer),
        egress_strict: strict,
        ..fake_account()
    }
}

/// This Mac's own socket, as a test can see it: a plain HTTP server that counts
/// every connection it accepts and answers the canned body.
///
/// The counter is the whole point. "The pin was honoured" is not a status code,
/// it is the absence of a connection from this box to the upstream, and a 503
/// alone would look the same whether the direct path ran and failed or never
/// ran at all.
fn local_exit(listener: TcpListener) -> Arc<std::sync::atomic::AtomicUsize> {
    let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&seen);
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut head = Vec::new();
            let mut byte = [0_u8; 1];
            while head.len() < 8192 {
                match stream.read(&mut byte).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => head.push(byte[0]),
                }
                if head.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{CANNED_BODY}",
                CANNED_BODY.len()
            );
            if stream.write_all(response.as_bytes()).await.is_err() {
                return;
            }
        }
    });
    seen
}

/// The far end of the carry, counting connections and reading nothing.
///
/// It does not speak TLS, and it does not need to: what this fixture measures
/// is WHICH process opened the connection to the origin, which is the whole of
/// the exit lock. A completed 200 through the production carry cannot be
/// observed in this process at all, and the reason is structural rather than an
/// omission: `send_through` builds its client with this crate's `rustls-tls`
/// feature, which carries the bundled webpki roots, so there is no root store a
/// test can add a fixture CA to and no `SSL_CERT_FILE` that would be read. The
/// body half of a carry is already measured, against a real TLS origin, by
/// `a_carry_delivers_the_canned_messages_body_through_a_gateway`.
fn origin_exit(listener: TcpListener) -> Arc<std::sync::atomic::AtomicUsize> {
    let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&seen);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            drop(stream);
        }
    });
    seen
}

fn count(seen: &Arc<std::sync::atomic::AtomicUsize>) -> usize {
    seen.load(std::sync::atomic::Ordering::SeqCst)
}

/// One `pinned_egress` call with the pieces the proxy hunk hands it.
fn attempt<'a>(
    manager: &'a Arc<Manager>,
    method: &'a axum::http::Method,
    peers: &'a std::path::Path,
) -> egress::PinnedAttempt<'a> {
    egress::PinnedAttempt {
        manager,
        account_idx: 0,
        method,
        path_and_query: "/v1/messages",
        headers: axum::http::HeaderMap::new(),
        body: bytes::Bytes::from_static(b"{\"model\":\"claude-opus-4-6\",\"messages\":[]}"),
        peers_path: peers,
        session_key: None,
        session_kind: teamclaude_rs::stats::SessionKind::Fallback,
        wire_session_id: None,
        model: None,
        tool_uses: &[],
        tool_results: &[],
    }
}

/// **The positive control for every assertion below.** An account with no pin
/// leaves from this Mac, exactly as it did before the exit lock existed.
///
/// Without this, "the local exit saw nothing" in the next test would be
/// satisfied just as well by a fixture that can never reach the local exit at
/// all.
///
/// Watched red by making `AccountEgress::parse` answer `Local` for every
/// input: unchanged, still green, which is the point of a control. It is
/// watched red the other way, by pinning the account, in
/// `a_pinned_account_never_takes_the_direct_path`.
#[tokio::test]
async fn an_unpinned_account_leaves_from_this_mac() {
    let exit = loopback().await;
    let addr = exit.local_addr().expect("the local exit address");
    let seen = local_exit(exit);

    let manager = fleet_with(&format!("http://{addr}"), vec![fake_account()]);
    let response = post_messages(Arc::clone(&manager)).await;

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "an unpinned account is served by the direct path"
    );
    assert_eq!(
        count(&seen),
        1,
        "the request left from this Mac, which is what an unpinned account does"
    );
}

/// **A pinned account never takes the direct path, on any request.**
///
/// The account is locked to a Mac and `egressStrict` is on, so the exit lock
/// answers before the direct attempt is ever built. The gate is the local
/// exit's counter: three requests, zero connections from this box. A 503 alone
/// would be the same answer whether the hunk ran or the upstream merely refused
/// us.
///
/// The named refusal is asserted in the same breath, because "it did not leave"
/// and "and the operator was told why" are one promise.
///
/// Watched red by commenting out the whole `pinned_egress` match in
/// `src/proxy.rs` (the hunk immediately below `let Some(token) =
/// manager.access_token(idx)`): the local exit sees 3 connections and the
/// status is 200, failing on the counter.
#[tokio::test]
async fn a_pinned_account_never_takes_the_direct_path() {
    let gateway = node();
    let exit = loopback().await;
    let addr = exit.local_addr().expect("the local exit address");
    let seen = local_exit(exit);

    let manager = fleet_with(
        &format!("http://{addr}"),
        vec![pinned_account(&gateway.id, true)],
    );

    for request in 1..=3 {
        let response = post_messages(Arc::clone(&manager)).await;
        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "request {request}: a pin that cannot be honoured on a strict account refuses"
        );
        assert_eq!(
            response
                .headers()
                .get("x-should-retry")
                .and_then(|value| value.to_str().ok()),
            Some("true"),
            "request {request}: the Mac may be back in a moment, so the client is told to retry"
        );
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("the refusal body reads");
        let body = String::from_utf8_lossy(&body);
        assert!(
            body.contains("alice@example.com") && body.contains(&gateway.id.display()),
            "request {request}: the refusal names the account and the Mac it is locked to: \
             {body}"
        );
        assert_eq!(
            count(&seen),
            0,
            "request {request}: a locked account must not have left from this Mac"
        );
    }
}

/// **A pinned request leaves through its Mac, on every request.**
///
/// Three requests, one live gateway, and the origin counts the connections it
/// received: all three arrived through the pinned Mac, and the local exit
/// beside it was never touched. That is the exit lock as decisions row 15
/// states it, "lock where accounts go out from which server", measured at the
/// far end rather than inferred from a log line.
///
/// It drives `pinned_egress` directly rather than `proxy::handle`, for one
/// reason: the fleet's upstream has to be a host the mesh carries
/// (`api.anthropic.com`), and through the full handler a regression in the hunk
/// would put a real request on the real internet. `pinned_egress` has no direct
/// path in it at all, so the worst a break can do here is answer nothing.
///
/// Watched red by making `pinned_egress` treat every account as unpinned (`let
/// Some(peer) = pin.egress.peer()` replaced with `let Some(peer) = None`): the
/// origin's counter stays at 0 and the first assertion names it.
#[tokio::test]
async fn a_pinned_request_leaves_through_its_peer_on_every_request() {
    let dir = scratch("pinned-route");
    let peers = dir.join("tcr-peers.json");
    // The requester's key is the one `pinned_egress` will mint in this scratch
    // directory, so the gateway has to pin THAT id rather than one the test
    // chose: minting it here first is what makes the two agree.
    let requester = teamclaude_rs::peer::id::NodeKey::load_or_mint(&dir)
        .expect("mint this node's keypair in the scratch dir");
    let gateway = node();

    let origin = loopback().await;
    let origin_addr = origin.local_addr().expect("the origin address");
    let arrived = origin_exit(origin);

    let local = loopback().await;
    let local_addr = local.local_addr().expect("the local exit address");
    let locally = local_exit(local);

    let gateway_listener = loopback().await;
    let gateway_addr = gateway_listener.local_addr().expect("the gateway address");
    let gateway_task = gateway_serving(
        gateway_listener,
        gateway.secret,
        vec![row(&requester.id(), "requester", Vec::new(), true)],
        origin_addr,
        1024 * 1024,
        3,
    );

    let mut file = PeerFile::default();
    file.peers.push(row(
        &gateway.id,
        "gateway",
        vec![gateway_addr.to_string()],
        false,
    ));
    file.peers[0].allow.carry = true;
    teamclaude_rs::peer::config::save(&peers, &file).expect("write the peers file");

    let manager = fleet_with(
        &format!("https://{ORIGIN}"),
        vec![pinned_account(&gateway.id, true)],
    );
    for request in 1..=3 {
        match egress::pinned_egress(attempt(&manager, &axum::http::Method::POST, &peers)).await {
            egress::PinnedEgress::Answered(_) => {}
            egress::PinnedEgress::TakeTheDirectPath { .. } => panic!(
                "request {request}: a locked account must never be told to take the direct path \
                 while its Mac is answering"
            ),
        }
    }

    let outcomes = gateway_task.await.expect("the gateway task joins");
    assert_eq!(
        count(&arrived),
        3,
        "every one of the three requests must have reached the origin through the pinned Mac; \
         the gateway saw {outcomes:?}"
    );
    assert_eq!(
        count(&locally),
        0,
        "and none of them may have left from this Mac; the local exit at {local_addr} was \
         never supposed to be touched"
    );
}

/// **A Mac that is not there, on a strict account, refuses by name.**
///
/// The row is pinned and granted the carry and its address is `127.0.0.1:1`,
/// refused by the kernel at once, so the carry is offered and not taken. The
/// assertion is on the words the operator reads: which account, which Mac, and
/// which of the five gaps it was.
///
/// Watched red by changing `Err(err) if pin.strict` to `Err(err) if false` in
/// `pinned_egress`: the answer becomes `TakeTheDirectPath` and the first
/// assertion names it.
#[tokio::test]
async fn a_down_peer_refuses_by_name_when_the_account_is_strict() {
    let dir = scratch("pinned-strict");
    let peers = dir.join("tcr-peers.json");
    let gateway = node();
    let mut file = PeerFile::default();
    file.peers.push(row(
        &gateway.id,
        "gateway",
        vec!["127.0.0.1:1".to_string()],
        false,
    ));
    file.peers[0].allow.carry = true;
    teamclaude_rs::peer::config::save(&peers, &file).expect("write the peers file");

    let manager = fleet_with(
        &format!("https://{ORIGIN}"),
        vec![pinned_account(&gateway.id, true)],
    );
    let answer = egress::pinned_egress(attempt(&manager, &axum::http::Method::POST, &peers)).await;
    let egress::PinnedEgress::Answered(response) = answer else {
        panic!("a strict pin that cannot be honoured refuses; it does not fall back");
    };

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok()),
        Some("5"),
        "a Mac that is asleep is a wait, so the client is given one"
    );
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("the refusal body reads");
    let body = String::from_utf8_lossy(&body);
    assert!(
        body.contains("alice@example.com"),
        "the refusal names the account whose row to open: {body}"
    );
    assert!(
        body.contains(&gateway.id.display()),
        "and the Mac it is locked to: {body}"
    );
    assert!(
        body.contains("it did not answer"),
        "and which of the five gaps it was, not a generic failure: {body}"
    );
    assert!(
        !body.contains(&gateway.id.to_wire()),
        "the short display form, never the full pinned key, in anything a client reads: {body}"
    );
}

/// **A carry the gateway TOOK and that then failed is not sent again from this
/// Mac**, even on an account that did not ask to be refused.
///
/// The review's finding: `pinned_egress`'s non-strict arm answered
/// `TakeTheDirectPath` for every error, including the one that means the
/// gateway had already put the request on the wire. The five-second bound also
/// covers the RESPONSE HEAD, so a slow origin reads exactly like a dead one,
/// and the fallback then sent the same POST directly: executed twice, billed
/// twice, on an `egressStrict: false` account whose operator was promised a
/// preference, not a double charge.
///
/// The gateway here is the real one, and the origin behind it accepts the
/// connection and drops it, so the carry is TAKEN and then fails: that is the
/// `CarryFailed` gap and no other. The positive control is the test above,
/// whose gateway is at `127.0.0.1:1` and never takes the carry at all: that one
/// still falls back, which is what it should do.
///
/// Watched red by deleting the `Err(err) if err.request_may_have_left()` arm
/// from `pinned_egress`: the answer becomes `TakeTheDirectPath`.
#[tokio::test]
async fn a_carry_that_was_taken_and_failed_is_not_sent_again_from_this_mac() {
    let dir = scratch("pinned-delivered");
    let peers = dir.join("tcr-peers.json");
    let requester = teamclaude_rs::peer::id::NodeKey::load_or_mint(&dir)
        .expect("mint this node's keypair in the scratch dir");
    let gateway = node();

    // An origin that accepts and drops: the request crosses the splice and the
    // exchange fails afterwards, which is the whole condition.
    let origin = loopback().await;
    let origin_addr = origin.local_addr().expect("the origin address");
    let arrived = origin_exit(origin);

    let gateway_listener = loopback().await;
    let gateway_addr = gateway_listener.local_addr().expect("the gateway address");
    let gateway_task = gateway_serving(
        gateway_listener,
        gateway.secret,
        vec![row(&requester.id(), "requester", Vec::new(), true)],
        origin_addr,
        1024 * 1024,
        1,
    );

    let mut file = PeerFile::default();
    file.peers.push(row(
        &gateway.id,
        "gateway",
        vec![gateway_addr.to_string()],
        false,
    ));
    file.peers[0].allow.carry = true;
    teamclaude_rs::peer::config::save(&peers, &file).expect("write the peers file");

    let manager = fleet_with(
        &format!("https://{ORIGIN}"),
        vec![pinned_account(&gateway.id, false)],
    );
    let answer = egress::pinned_egress(attempt(&manager, &axum::http::Method::POST, &peers)).await;
    let outcomes = gateway_task.await.expect("the gateway task joins");
    assert_eq!(
        count(&arrived),
        1,
        "the gateway took the carry and the request reached the origin: {outcomes:?}"
    );

    let egress::PinnedEgress::Answered(response) = answer else {
        panic!(
            "a carry that was taken and then failed must be answered, never re-sent from \
             this Mac: {outcomes:?}"
        );
    };
    assert_eq!(
        response.status().as_u16(),
        502,
        "the client is told the outcome is unknown, not handed a retryable 503"
    );
    assert_eq!(
        response
            .headers()
            .get("x-should-retry")
            .and_then(|value| value.to_str().ok()),
        Some("false"),
        "a client that retried this would pay for the request twice"
    );
}

/// **The same Mac, the same outage, on an account that did not ask to be
/// refused: the local path, and exactly one line saying so.**
///
/// One line, not zero and not one per layer: the operator asked for an address
/// and is not getting it, which is worth saying once per request.
///
/// Watched red by deleting the `tracing::warn!` in `pinned_egress`'s non-strict
/// arm: the count is 0 and the second assertion names it.
#[tokio::test]
async fn a_down_peer_falls_back_to_this_mac_with_one_line_when_it_is_not_strict() {
    let dir = scratch("pinned-loose");
    let peers = dir.join("tcr-peers.json");
    let gateway = node();
    let mut file = PeerFile::default();
    file.peers.push(row(
        &gateway.id,
        "gateway",
        vec!["127.0.0.1:1".to_string()],
        false,
    ));
    file.peers[0].allow.carry = true;
    teamclaude_rs::peer::config::save(&peers, &file).expect("write the peers file");

    let (collector, _guard) = capture();
    let manager = fleet_with(
        &format!("https://{ORIGIN}"),
        vec![pinned_account(&gateway.id, false)],
    );
    let answer = egress::pinned_egress(attempt(&manager, &axum::http::Method::POST, &peers)).await;
    assert!(
        matches!(
            answer,
            egress::PinnedEgress::TakeTheDirectPath { paced: true }
        ),
        "without `egressStrict` a pin that cannot be honoured is a preference, not a refusal,          and it says the pacing slot is already spent: this request waited on the account's          own bucket inside the exit lock, and `src/proxy.rs` reads this flag rather than          waiting on it a second time"
    );

    let lines = collector.matching("locked to leave from a peer that did not carry it");
    assert_eq!(
        lines.len(),
        1,
        "exactly one line, and it says which account and which Mac: {:?}",
        collector
            .events()
            .iter()
            .map(|event| event.message.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        lines[0].fields.get("account").map(String::as_str),
        Some("alice@example.com")
    );
    assert_eq!(
        lines[0].fields.get("peer").map(String::as_str),
        Some(gateway.id.display().as_str())
    );
}

/// **A pin this build cannot read fails the config LOAD, on every account,
/// strict or not.**
///
/// It used to be refused one request at a time, because the value was parsed
/// out of a stringly map on the request path. It is a declared field now, so
/// serde is what refuses it, and the operator finds out when the file is read
/// rather than once per request. Either way the thing that must never happen is
/// the same: a value the operator typed wrong read as `local` would send the
/// account out of exactly the address it was written down to avoid.
///
/// The short display form is the case worth naming. `PeerId::parse` refuses it,
/// so a pin copied out of a panel row rather than out of `tcr peer ls` is an
/// error and not a prefix match on somebody's key.
///
/// Watched red by making `Egress::try_from` answer `Ok(Self::Local)` for
/// anything it does not recognise: all three loads succeed and the first
/// assertion names it.
#[test]
fn a_pin_that_is_neither_spelling_fails_the_config_load() {
    let account = |egress: &str| {
        format!(
            r#"{{"upstream":"https://api.anthropic.com","accounts":[{{"name":"alice@example.com",
             "accessToken":"at-alice","egress":"{egress}"}}]}}"#
        )
    };

    for wrong in [
        // The display form: short, lossy, and deliberately not parseable back.
        "tcr-7f3k9m2q4x",
        // A full wire id with no `via ` in front of it.
        "0000000000000000000000000000000000000000000000000000",
        // A word that is not the word.
        "remote",
        // The right prefix over the wrong payload.
        "via not-a-peer-id",
        // A colon where the documented form has a space: near enough to be
        // typed by hand, and refused, because two spellings for one thing is
        // how a config stops meaning one thing.
        "via:0000000000000000000000000000000000000000000000000000",
    ] {
        let err = serde_json::from_str::<Config>(&account(wrong))
            .expect_err("a pin that is neither spelling must fail the load, not read as local");
        assert!(
            err.to_string().contains("egress"),
            "and the error must name the key the operator has to fix: {err}"
        );
    }

    let peer = node().id;
    let good = serde_json::from_str::<Config>(&account(&format!("via {}", peer.to_wire())))
        .expect("the `via ` spelling is the one that loads");
    assert_eq!(
        good.accounts[0].egress,
        teamclaude_rs::config::Egress::Via(peer)
    );
    assert!(
        !good.accounts[0].egress_strict,
        "an absent `egressStrict` is not strict"
    );
}

/// The two fields as an operator writes them, read back as one typed value.
///
/// A round trip, not a parse: what is written is what `tcr` writes back, which
/// is the property an operator who edits this file by hand depends on.
///
/// Watched red by making `Egress`'s `Display` print the bare wire id without
/// its `via ` prefix: the round trip comes back as a load error and the last
/// assertion names it.
#[test]
fn an_account_round_trips_the_exit_lock_it_was_written_with() {
    let peer = node().id;

    let plain = fake_account();
    assert_eq!(
        plain.egress_pin(),
        teamclaude_rs::config::EgressPin::default()
    );
    assert!(plain.egress_pin().egress.is_local());

    let pinned = pinned_account(&peer, true);
    let pin = pinned.egress_pin();
    assert_eq!(pin.egress.peer(), Some(peer));
    assert!(pin.strict);

    let written = serde_json::to_string(&pinned).expect("an account serializes");
    assert!(
        written.contains(&format!(r#""egress":"via {}""#, peer.to_wire())),
        "the pin is written as one prefixed word: {written}"
    );
    assert!(
        written.contains(r#""egressStrict":true"#),
        "and the strictness beside it, under the name the file uses: {written}"
    );

    let read: Account = serde_json::from_str(&written).expect("and it reads back");
    assert_eq!(read.egress_pin(), pin, "what tcr writes, tcr reads");
}

/// **An unpinned account writes no exit-lock keys at all.**
///
/// `tcr` rewrites the whole config on every save, so a field that serializes
/// its own default stamps that default onto every account row the first time
/// an unrelated setting changes. Two keys times every account, on a file a
/// human reads and edits by hand, for a feature nobody on those rows turned
/// on.
///
/// The pinned half in the same test is the positive control: skipping is a
/// property of the DEFAULT and not of the field, so an assertion that only saw
/// the absence would also pass against a field that never serializes.
///
/// Watched red by dropping `skip_serializing_if` from either field: the plain
/// account's JSON carries `"egress":"local"` or `"egressStrict":false` and the
/// matching assertion names it.
#[test]
fn an_unpinned_account_writes_no_egress_keys() {
    let plain = serde_json::to_string(&fake_account()).expect("an account serializes");
    assert!(
        !plain.contains("egress"),
        "an account nobody pinned writes neither key, so an upgrade does not stamp a \
         default onto every row of a hand-edited file: {plain}"
    );

    let peer = node().id;
    let pinned =
        serde_json::to_string(&pinned_account(&peer, true)).expect("an account serializes");
    assert!(
        pinned.contains(&format!(r#""egress":"via {}""#, peer.to_wire()))
            && pinned.contains(r#""egressStrict":true"#),
        "the control: a row that IS pinned still writes both keys, or the skip above is \
         a field that never serializes: {pinned}"
    );

    let back: Account = serde_json::from_str(&plain).expect("the keyless form reads back");
    assert_eq!(
        back.egress_pin(),
        teamclaude_rs::config::EgressPin::default(),
        "and absent reads as the default it stood for, which is the whole of the round trip"
    );
}
