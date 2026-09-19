//! `tcr peer graph`, through the shipped binary.
//!
//! `status::peer_graph` and its schema are already covered end to end in
//! `tests/peer_status.rs` (the flattened-edge shape, the masking, the
//! lease-direction rule, the expired-lease drop): that file owns
//! `src/status.rs`'s derivation. This file owns the CLI SEAM: the verb that
//! opens the two files, mints or loads the node key, and either prints the
//! result or serves it, which is the part `peer_status.rs`'s pure-function
//! tests cannot reach.
//!
//! Nothing here touches the operator's real config directory or the live
//! proxy on 127.0.0.1:3456: every fixture is a scratch dir this process owns,
//! and every server this file starts binds `127.0.0.1:0` (or a fixed high
//! port for the loopback-refusal check, which never actually binds).

use std::path::Path;

/// A scratch directory of this test's own, so a sibling test binary running at
/// the same instant cannot read or write this one's peers file. Same shape as
/// `tests/peer_reach.rs::scratch`.
fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tcr-peer-graph-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the scratch dir");
    dir
}

/// A peers file with two pinned peers and two paths on one of them, written as
/// JSON text rather than through `PeerFile`: this test is about the CLI's
/// output, not about the config structs, and going through them would fail
/// to COMPILE on every field added to those structs elsewhere, a different failure from
/// the one this file exists to report.
fn write_two_peer_fixture(dir: &Path) -> std::path::PathBuf {
    let studio = tcr_peer_wire::PeerId([0x11; 32]).to_wire();
    let attic = tcr_peer_wire::PeerId([0x22; 32]).to_wire();
    let body = format!(
        r#"{{
  "peers": [
    {{
      "node": "{studio}",
      "label": "studio-mac",
      "endpoints": [
        {{ "kind": "direct", "addr": "127.0.0.1:7749", "observedAtMs": 1700000000000, "source": "paired" }},
        {{ "kind": "direct", "addr": "10.0.1.24:7749", "observedAtMs": 1699999000000, "source": "paired" }}
      ],
      "addedAt": 1
    }},
    {{
      "node": "{attic}",
      "label": "attic-nuc",
      "endpoints": [
        {{ "kind": "direct", "addr": "10.0.1.31:7749", "observedAtMs": 1700000000000, "source": "paired" }}
      ],
      "addedAt": 1
    }}
  ]
}}"#
    );
    let path = dir.join("tcr-peers.json");
    std::fs::write(&path, body).expect("write the peers file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("0600 on the peers file");
    }
    path
}

/// Run `tcr peer graph` with the given extra args: `(stdout, stderr, ok)`.
fn run_graph(peers: &Path, extra: &[&str]) -> (String, String, bool) {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_tcr"))
        .args(["peer", "graph"])
        .args(["--peers", peers.to_str().expect("a utf-8 path")])
        .args(extra)
        .output()
        .expect("spawn tcr peer graph");
    (
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.success(),
    )
}

// ---------------------------------------------------------------------------
// Item 1: `tcr peer graph --json`
// ---------------------------------------------------------------------------

/// **The gate for item 1**: the verb runs against a real peers file, minting
/// this Mac's own node key on first use, the same as every other peer verb,
/// and prints a document `tests/peer_status.rs`'s schema (`graph_schema_errors`)
/// would accept: one `self` node, one node per pinned peer, one `path` edge per
/// endpoint.
#[test]
fn tcr_peer_graph_json_runs_against_a_fixture_and_parses() {
    let dir = scratch("json");
    let peers = write_two_peer_fixture(&dir);

    let (out, err, ok) = run_graph(&peers, &["--json"]);
    assert!(ok, "tcr peer graph --json exited non-zero: {err}");
    let parsed: serde_json::Value =
        serde_json::from_str(out.trim()).unwrap_or_else(|why| panic!("invalid JSON: {why}\n{out}"));

    assert_eq!(
        parsed["kind"], "tcr.peer.graph.v1",
        "the CLI's own kind must be the one `status::peer_graph` mints: {parsed}"
    );
    let nodes = parsed["nodes"].as_array().expect("nodes is an array");
    assert_eq!(
        nodes.len(),
        3,
        "this Mac plus the two pinned peers: {parsed}"
    );
    let selves = nodes.iter().filter(|n| n["role"] == "self").count();
    assert_eq!(
        selves, 1,
        "exactly one node is the Mac that ran the verb: {parsed}"
    );
    let names: Vec<&str> = nodes.iter().filter_map(|n| n["name"].as_str()).collect();
    assert!(names.contains(&"studio-mac"), "{names:?}");
    assert!(names.contains(&"attic-nuc"), "{names:?}");

    let edges = parsed["edges"].as_array().expect("edges is an array");
    assert_eq!(
        edges.len(),
        3,
        "two endpoints on studio-mac plus one on attic-nuc, no leases: {parsed}"
    );
    assert!(
        edges.iter().all(|e| e["kind"] == "path"),
        "no lease is pinned in this fixture, so every edge is a path: {parsed}"
    );
}

/// A peers file with nobody pinned yet is not a refusal: it is the ordinary
/// state of a Mac that has never paired, and the graph is just this Mac alone.
#[test]
fn tcr_peer_graph_json_with_no_peers_file_reports_just_this_mac() {
    let dir = scratch("empty");
    let peers = dir.join("tcr-peers.json");

    let (out, err, ok) = run_graph(&peers, &["--json"]);
    assert!(ok, "tcr peer graph --json exited non-zero: {err}");
    let parsed: serde_json::Value =
        serde_json::from_str(out.trim()).unwrap_or_else(|why| panic!("invalid JSON: {why}\n{out}"));
    let nodes = parsed["nodes"].as_array().expect("nodes is an array");
    assert_eq!(
        nodes.len(),
        1,
        "no peers file, no peers, this Mac only: {parsed}"
    );
    assert_eq!(nodes[0]["role"], "self");
    assert_eq!(
        parsed["edges"].as_array().expect("edges is an array").len(),
        0
    );
}

// ---------------------------------------------------------------------------
// Item 2: `tcr peer graph --serve`
// ---------------------------------------------------------------------------

/// **Half the gate for item 2**: a non-loopback `--addr` is refused before
/// anything is bound. Watch it fail by deleting the `is_loopback()` check in
/// `serve_peer_graph`: the process would instead try to bind `0.0.0.0`,
/// which succeeds on most machines and serves the mesh, socket addresses and
/// lease spend for every trusted Mac, to the whole LAN.
#[test]
fn tcr_peer_graph_serve_refuses_a_non_loopback_address() {
    let dir = scratch("serve-refuse");
    let peers = write_two_peer_fixture(&dir);

    let (out, err, ok) = run_graph(&peers, &["--serve", "--addr", "0.0.0.0:18080"]);
    assert!(
        !ok,
        "a non-loopback --addr must be refused, not served: stdout={out} stderr={err}"
    );
    assert!(
        err.contains("not loopback"),
        "the refusal must name what it refused and why: {err}"
    );
}

/// **The other half of the gate for item 2**: bound on loopback, the page at
/// `/` is the inline HTML (no external asset, no build step: the file is
/// `include_str!`'d whole) and `/graph.json` is the same document `--json`
/// would have printed, both without the process ever touching a non-loopback
/// socket.
///
/// The server is a long-running process (`axum::serve` never returns), so
/// this test spawns it, polls until it answers, asserts, and kills it:
/// never `.output()`, which would hang forever waiting for a process that
/// does not exit.
#[tokio::test]
async fn tcr_peer_graph_serve_answers_the_page_and_the_json_on_loopback() {
    let dir = scratch("serve-ok");
    let peers = write_two_peer_fixture(&dir);

    // Port 0 is not a thing this CLI supports asking axum for (the address is
    // parsed before it reaches `TcpListener::bind`), so a fixed high port is
    // picked instead, scoped to this one test's scratch tag, so a collision
    // with a sibling test binary running at the same instant is the same
    // non-issue `peer_reach.rs`'s port picks already accept.
    let addr = "127.0.0.1:18081";
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_tcr"))
        .args(["peer", "graph", "--serve", "--addr", addr])
        .args(["--peers", peers.to_str().expect("a utf-8 path")])
        .spawn()
        .expect("spawn tcr peer graph --serve");

    let client = reqwest::Client::new();
    let mut last_err = None;
    let mut page = None;
    for _ in 0..50 {
        match client.get(format!("http://{addr}/")).send().await {
            Ok(response) => {
                page = Some(response.text().await.expect("page body"));
                break;
            }
            Err(err) => {
                last_err = Some(err);
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
    let page = page.unwrap_or_else(|| {
        panic!("the server never answered `/`: {last_err:?}");
    });
    assert!(
        page.contains("tcr peer graph"),
        "the inline page's own title: {page}"
    );
    assert!(
        page.contains("/graph.json"),
        "the page polls the JSON endpoint from its own script: {page}"
    );

    let json = client
        .get(format!("http://{addr}/graph.json"))
        .send()
        .await
        .expect("GET /graph.json")
        .json::<serde_json::Value>()
        .await
        .expect("graph.json is valid JSON");
    assert_eq!(json["kind"], "tcr.peer.graph.v1");
    assert_eq!(
        json["nodes"].as_array().expect("nodes is an array").len(),
        3,
        "the same fixture --json would have reported: {json}"
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// **The DNS-rebinding gate**: binding on
/// loopback stops a stranger from opening the socket, but not a page already
/// open in the operator's own browser whose script is redirected at a DNS
/// name that resolves to `127.0.0.1`. The bind is loopback either way; only
/// the `Host` header tells the two apart. Watch this fail by deleting the
/// `.layer(middleware::from_fn(require_loopback_host))` call in
/// `serve_peer_graph`: every assertion below flips, `evil.example` gets 200
/// and the mesh's addresses and lease spend go to whichever host asked.
#[tokio::test]
async fn tcr_peer_graph_serve_403s_a_non_loopback_host_header_and_200s_a_loopback_one() {
    let dir = scratch("serve-host-gate");
    let peers = write_two_peer_fixture(&dir);

    let addr = "127.0.0.1:18082";
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_tcr"))
        .args(["peer", "graph", "--serve", "--addr", addr])
        .args(["--peers", peers.to_str().expect("a utf-8 path")])
        .spawn()
        .expect("spawn tcr peer graph --serve");

    let client = reqwest::Client::new();
    let mut ready = false;
    for _ in 0..50 {
        if client.get(format!("http://{addr}/")).send().await.is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(ready, "the server never came up to be probed");

    let rebound = client
        .get(format!("http://{addr}/"))
        .header(reqwest::header::HOST, "evil.example")
        .send()
        .await
        .expect("GET / with a rebound Host");
    assert_eq!(
        rebound.status(),
        reqwest::StatusCode::FORBIDDEN,
        "a Host that does not name a loopback address must be refused"
    );

    let loopback = client
        .get(format!("http://{addr}/"))
        .header(reqwest::header::HOST, format!("127.0.0.1:{}", 18082))
        .send()
        .await
        .expect("GET / with a loopback Host");
    assert_eq!(
        loopback.status(),
        reqwest::StatusCode::OK,
        "a loopback Host must still be served"
    );

    let _ = child.kill();
    let _ = child.wait();
}
