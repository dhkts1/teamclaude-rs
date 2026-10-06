//! The `ttl: 1h` rewrite, measured at the wire: what the UPSTREAM receives, not
//! what the proxy intended. A fake upstream records every body; the proxy in
//! front of it is the real `mitm::serve` with a one-account config. No real
//! account data anywhere here: this repository is public.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::response::Response;
use axum::routing::any;
use axum::Router;

/// Every request body the fake upstream saw, in arrival order.
type Bodies = Arc<Mutex<Vec<Vec<u8>>>>;

async fn spawn_upstream_recording(seen: Bodies) -> String {
    let app = Router::new().fallback(any(move |req: axum::extract::Request| {
        let seen = Arc::clone(&seen);
        async move {
            let body = axum::body::to_bytes(req.into_body(), 4 * 1024 * 1024)
                .await
                .expect("read the forwarded body");
            seen.lock()
                .expect("the body log is never poisoned")
                .push(body.to_vec());
            Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .body(Body::from(br#"{"type":"message"}"#.to_vec()))
                .expect("build the canned answer")
        }
    }));
    let listening = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the fake upstream");
    let addr = listening.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        let _ = axum::serve(listening, app).await;
    });
    format!("http://{addr}")
}

/// A manager with ONE fake account pointed at `upstream`; `extra` is spliced
/// into the top-level config object (the unmodelled flags live there).
fn manager(upstream: &str, extra: &str) -> Arc<teamclaude_rs::manager::Manager> {
    let config: teamclaude_rs::config::Config = serde_json::from_str(&format!(
        r#"{{
            "proxy": {{ "port": 0 }},
            "upstream": "{upstream}",
            "quotaProbeSeconds": 0,
            "warmupSeconds": 0,
            {extra}
            "accounts": [
                {{
                    "name": "rewrite-fake",
                    "accessToken": "at-fake-rewrite",
                    "accountUuid": "11111111-1111-1111-1111-111111111111",
                    "orgUuid": "22222222-2222-2222-2222-222222222222"
                }}
            ]
        }}"#
    ))
    .expect("the inline config parses");
    teamclaude_rs::manager::Manager::with_live_refresher(config, None)
}

async fn spawn_proxy(manager: Arc<teamclaude_rs::manager::Manager>) -> String {
    let listening = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the proxy");
    let addr = listening.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        teamclaude_rs::mitm::serve(listening, manager, None).await;
    });
    format!("http://{addr}")
}

/// A request shaped like Claude Code's: a system breakpoint on the default
/// window, a tool with an explicit `5m`, and a message block with none.
const CLIENT_BODY: &str = r#"{"model":"claude-opus-5-5","max_tokens":1,"system":[{"type":"text","text":"sys","cache_control":{"type":"ephemeral"}}],"tools":[{"name":"t","input_schema":{"type":"object"},"cache_control":{"type":"ephemeral","ttl":"5m"}}],"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#;

async fn send_through(proxy: &str) -> u16 {
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("the local client builds");
    client
        .post(format!("{proxy}/v1/messages"))
        .header("content-type", "application/json")
        .body(CLIENT_BODY)
        .send()
        .await
        .expect("the proxy answered")
        .status()
        .as_u16()
}

fn only_body(seen: &Bodies) -> serde_json::Value {
    let bodies = seen.lock().expect("the body log is never poisoned");
    assert_eq!(bodies.len(), 1, "exactly one request reached upstream");
    serde_json::from_slice(&bodies[0]).expect("upstream received JSON")
}

/// Default config: every breakpoint arrives upstream as `1h`, the block with no
/// breakpoint stays bare, and the rest of the body is intact.
///
/// Watched red with `extend_all_ttls` short-circuited to `None`: the first
/// assertion failed on `"ttl"` being absent.
#[tokio::test]
async fn breakpoints_arrive_upstream_as_1h_by_default() {
    let seen: Bodies = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn_upstream_recording(Arc::clone(&seen)).await;
    let proxy = spawn_proxy(manager(&upstream, "")).await;

    assert_eq!(send_through(&proxy).await, 200);

    let body = only_body(&seen);
    assert_eq!(body["system"][0]["cache_control"]["ttl"], "1h");
    assert_eq!(
        body["tools"][0]["cache_control"]["ttl"], "1h",
        "an explicit 5m becomes 1h"
    );
    assert!(body["messages"][0]["content"][0]
        .get("cache_control")
        .is_none());
    assert_eq!(body["model"], "claude-opus-5-5");
    assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
}

/// `"cacheTtlRewrite": false` forwards the client's bytes exactly as sent.
#[tokio::test]
async fn rewrite_off_forwards_the_body_byte_for_byte() {
    let seen: Bodies = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn_upstream_recording(Arc::clone(&seen)).await;
    let proxy = spawn_proxy(manager(&upstream, r#""cacheTtlRewrite": false,"#)).await;

    assert_eq!(send_through(&proxy).await, 200);

    let bodies = seen.lock().expect("the body log is never poisoned");
    assert_eq!(bodies.len(), 1);
    assert_eq!(
        std::str::from_utf8(&bodies[0]).expect("utf-8"),
        CLIENT_BODY,
        "with the rewrite off, upstream sees the client's own bytes"
    );
}
