//! Opt-in "keep-warm" — periodically start the rolling 5-hour session window on
//! idle accounts so a cold account is never the one that has to serve the moment
//! the active one runs out.
//!
//! Ported in spirit from `teamclaude/src/warmer.js` (issue #76). It is the SECOND
//! sanctioned active-upstream feature (the quota probe is the first) and differs
//! in one important way, which is why it is strictly **opt-in and OFF by default**:
//! the 5h timer only starts on *real usage*, so — unlike the zero-spend
//! `/api/oauth/usage` probe — warming genuinely consumes a little quota (a few
//! tokens, a slice of the 5h window) per account per window. To keep that cost
//! minimal we warm an account only when its 5h window is not already running, and
//! we use the cheapest model ([`WARM_MODEL`]).
//!
//! Unlike the JS mechanism (spawn a one-shot `claude` pointed back at the proxy),
//! this warms **in process**: a direct minimal `POST {upstream}/v1/messages` with
//! the account's token, mirroring [`crate::probe`]'s prober shape. A warm FAILURE
//! is NON-FATAL — logged at `warn` and stepped over — never a crash of the loop.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use serde_json::Value;

use crate::probe::OAUTH_USAGE_BETA;

/// The cheapest model to warm with (verified working). Warming spends real quota,
/// so we deliberately pick the smallest model and a 1-token cap.
pub const WARM_MODEL: &str = "claude-haiku-4-5-20251001";
/// `anthropic-version` value every Anthropic Messages request carries.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Per-warm wall-clock ceiling; a warm that outlives it reads as a failure.
pub const WARM_TIMEOUT: Duration = Duration::from_secs(30);

/// Why a warm request failed. `status` carries the HTTP code when there was one.
/// A warm failure is a *soft* signal — the caller logs it and moves on.
#[derive(Debug, Clone, thiserror::Error)]
#[error("keep-warm request failed{}: {message}", .status.map(|s| format!(" (HTTP {s})")).unwrap_or_default())]
pub struct WarmError {
    pub status: Option<u16>,
    pub message: String,
}

/// Build the exact byte-shape of a warm request, WITHOUT the token or the network,
/// so the request shape is unit-testable in isolation. Returns the target URL, the
/// JSON body, and the non-auth headers (the `Authorization: Bearer …` header is
/// added by [`LiveWarmer`] from the account's live token).
///
/// Shape: `POST {upstream}/v1/messages`, body a 1-token single-turn message on
/// `model`, headers `anthropic-version`, `content-type: application/json`, and the
/// OAuth `anthropic-beta` (replicating the forward path — an OAuth token to the
/// Messages endpoint carries the same beta the usage probe uses).
pub fn warm_request_spec(upstream: &str, model: &str) -> (String, Value, HeaderMap) {
    let url = format!("{upstream}/v1/messages");
    let body = serde_json::json!({
        "model": model,
        "max_tokens": 1,
        "messages": [{ "role": "user", "content": "warm" }],
    });
    let mut headers = HeaderMap::new();
    headers.insert(
        "anthropic-version",
        HeaderValue::from_static(ANTHROPIC_VERSION),
    );
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert("anthropic-beta", HeaderValue::from_static(OAUTH_USAGE_BETA));
    (url, body, headers)
}

/// Future returned by [`AccountWarmer::warm`]. `'static` so it can be awaited
/// after any manager lock is released. On success it yields the response's headers
/// so the caller can fold the just-started 5h window into the account's quota.
pub type WarmFuture = Pin<Box<dyn Future<Output = Result<HeaderMap, WarmError>> + Send>>;

/// Abstraction over "warm one account with its access token", so the warm loop can
/// be exercised in tests without hitting the network (mirrors [`crate::probe::UsageProber`]).
pub trait AccountWarmer: Send + Sync {
    fn warm(&self, access_token: String, upstream: String) -> WarmFuture;
}

/// The production warmer: a real minimal `POST {upstream}/v1/messages`.
pub struct LiveWarmer {
    client: reqwest::Client,
}

impl LiveWarmer {
    pub fn new() -> Self {
        Self {
            // no_proxy(): reqwest honors HTTPS_PROXY/HTTP_PROXY by default. We ARE the
            // proxy — an ambient proxy env var would route the warm request through
            // another proxy and silently fail it (mirrors LiveUsageProber).
            client: reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("build reqwest client"),
        }
    }
}

impl Default for LiveWarmer {
    fn default() -> Self {
        Self::new()
    }
}

impl AccountWarmer for LiveWarmer {
    fn warm(&self, access_token: String, upstream: String) -> WarmFuture {
        let client = self.client.clone();
        Box::pin(async move {
            let (url, body, headers) = warm_request_spec(&upstream, WARM_MODEL);
            let body_bytes = serde_json::to_vec(&body).map_err(|e| WarmError {
                status: None,
                message: e.to_string(),
            })?;
            let resp = client
                .post(&url)
                .headers(headers)
                .header("Authorization", format!("Bearer {access_token}"))
                .body(body_bytes)
                .timeout(WARM_TIMEOUT)
                .send()
                .await
                .map_err(|e| WarmError {
                    status: None,
                    message: e.to_string(),
                })?;

            let status = resp.status();
            let response_headers = resp.headers().clone();
            if !status.is_success() {
                let detail = resp.text().await.unwrap_or_default();
                let detail = detail.chars().take(200).collect::<String>();
                return Err(WarmError {
                    status: Some(status.as_u16()),
                    message: if detail.is_empty() {
                        format!("HTTP {}", status.as_u16())
                    } else {
                        detail
                    },
                });
            }
            // The response's rate-limit headers describe the now-live 5h window; the
            // caller folds them so the next sweep sees this account as already warm.
            Ok(response_headers)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warm_request_spec_targets_messages_endpoint() {
        let (url, _body, _headers) = warm_request_spec("https://api.anthropic.com", WARM_MODEL);
        assert_eq!(url, "https://api.anthropic.com/v1/messages");
    }

    #[test]
    fn warm_request_spec_body_is_minimal_single_turn() {
        let (_url, body, _headers) = warm_request_spec("https://up", WARM_MODEL);
        assert_eq!(body["model"], WARM_MODEL);
        assert_eq!(body["max_tokens"], 1);
        let messages = body["messages"].as_array().expect("messages is an array");
        assert_eq!(messages.len(), 1, "exactly one user turn");
        assert_eq!(messages[0]["role"], "user");
    }

    #[test]
    fn warm_request_spec_headers_carry_anthropic_version() {
        let (_url, _body, headers) = warm_request_spec("https://up", WARM_MODEL);
        assert_eq!(
            headers.get("anthropic-version").unwrap().to_str().unwrap(),
            ANTHROPIC_VERSION
        );
        assert_eq!(
            headers.get(CONTENT_TYPE).unwrap().to_str().unwrap(),
            "application/json"
        );
    }
}
