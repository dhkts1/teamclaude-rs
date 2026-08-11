//! OAuth token refresh, ported faithfully from `teamclaude/src/oauth.js`.
//!
//! The request shape (endpoint, client id, JSON body, headers) matches the JS
//! reference exactly so it stays a drop-in against the same Anthropic OAuth
//! backend. The one behaviour that matters downstream is the error split: a
//! `400/401/403` means the refresh token itself is dead (the account must drop
//! out of rotation until re-login), whereas a `5xx`/network failure is transient
//! (keep the current token and fail this one request over to another account).

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Anthropic OAuth token endpoint (from the JS `DEFAULT_TOKEN_ENDPOINT`).
pub const TOKEN_ENDPOINT: &str = "https://platform.claude.com/v1/oauth/token";
/// Claude Code OAuth client id (from the JS `DEFAULT_CLIENT_ID`).
pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// Threshold used by [`is_expiring_soon`]: refresh proactively 5 minutes out.
pub const EXPIRING_SOON_MS: i64 = 5 * 60 * 1000;

const MAX_RETRIES: u32 = 2;
const BASE_DELAY_MS: u64 = 500;
const REFRESH_TIMEOUT: Duration = Duration::from_secs(30);

/// Freshly-minted OAuth credentials returned by a refresh.
#[derive(Debug, Clone)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    /// Epoch **milliseconds** at which `access_token` expires.
    pub expires_at_ms: i64,
}

/// Why a refresh failed. The variant drives account state: [`OAuthError::AuthRejected`]
/// sidelines the account (dead refresh token), [`OAuthError::Transient`] does not.
#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("token refresh rejected by auth (HTTP {status}) — re-login needed")]
    AuthRejected { status: u16 },
    #[error("token refresh failed transiently: {0}")]
    Transient(String),
}

#[derive(Serialize)]
struct RefreshRequest<'a> {
    grant_type: &'a str,
    refresh_token: &'a str,
    client_id: &'a str,
}

#[derive(Debug, Deserialize)]
struct RefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_at: Option<i64>,
    #[serde(default)]
    expires_in: Option<i64>,
}

/// Normalize an `expires_at` to milliseconds. OAuth backends may report seconds;
/// Claude Code credentials use milliseconds. Values below ~`1e12` are seconds
/// (that boundary is year 2001 in ms, far in the future in seconds).
pub fn normalize_expires_at(value: i64) -> i64 {
    if value < 1_000_000_000_000 {
        value * 1000
    } else {
        value
    }
}

/// Absolute expiry (ms) from `now_ms` and an optional `expires_in` (seconds,
/// default 3600). Saturating so a hostile/huge `expires_in` can never overflow
/// the i64 multiply into a negative expiry (which would make [`is_expired`]
/// always true → a single-use-refresh-token storm).
fn expires_at_from(now_ms: i64, expires_in: Option<i64>) -> i64 {
    now_ms.saturating_add(expires_in.unwrap_or(3600).saturating_mul(1000))
}

/// Has the token already expired at `now_ms`? A `None` expiry is treated as
/// non-expiring (nothing to refresh).
pub fn is_expired(expires_at_ms: Option<i64>, now_ms: i64) -> bool {
    match expires_at_ms {
        Some(exp) => now_ms >= exp,
        None => false,
    }
}

/// Is the token within [`EXPIRING_SOON_MS`] of expiry at `now_ms`?
pub fn is_expiring_soon(expires_at_ms: Option<i64>, now_ms: i64) -> bool {
    match expires_at_ms {
        Some(exp) => now_ms + EXPIRING_SOON_MS >= exp,
        None => false,
    }
}

/// Refresh an access token against [`TOKEN_ENDPOINT`], retrying `5xx`/network
/// failures with exponential backoff. Auth rejections are returned immediately.
pub async fn refresh_access_token(
    client: &reqwest::Client,
    refresh_token: &str,
) -> Result<Tokens, OAuthError> {
    refresh_access_token_at(client, refresh_token, TOKEN_ENDPOINT).await
}

/// [`refresh_access_token`] against an explicit endpoint (for tests).
pub async fn refresh_access_token_at(
    client: &reqwest::Client,
    refresh_token: &str,
    endpoint: &str,
) -> Result<Tokens, OAuthError> {
    let body = RefreshRequest {
        grant_type: "refresh_token",
        refresh_token,
        client_id: CLIENT_ID,
    };

    for attempt in 0..=MAX_RETRIES {
        if attempt > 0 {
            let delay = BASE_DELAY_MS * (1u64 << (attempt - 1));
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }

        let send = client
            .post(endpoint)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/plain, */*")
            .header("User-Agent", "axios/1.13.6")
            .timeout(REFRESH_TIMEOUT)
            .json(&body)
            .send()
            .await;

        let response = match send {
            Ok(r) => r,
            Err(err) => {
                // Network/timeout: retry while attempts remain, else transient.
                if attempt < MAX_RETRIES {
                    continue;
                }
                return Err(OAuthError::Transient(err.to_string()));
            }
        };

        let status = response.status();
        if !status.is_success() {
            let code = status.as_u16();
            // 5xx is retryable; exhausting retries leaves it transient (not auth).
            if status.is_server_error() && attempt < MAX_RETRIES {
                continue;
            }
            if matches!(code, 400 | 401 | 403) {
                return Err(OAuthError::AuthRejected { status: code });
            }
            let detail = response.text().await.unwrap_or_default();
            return Err(OAuthError::Transient(format!("HTTP {code}: {detail}")));
        }

        let data: RefreshResponse = response
            .json()
            .await
            .map_err(|e| OAuthError::Transient(e.to_string()))?;

        let expires_at_ms = data.expires_at.map_or_else(
            || expires_at_from(crate::now_ms(), data.expires_in),
            normalize_expires_at,
        );

        return Ok(Tokens {
            access_token: data.access_token,
            refresh_token: data
                .refresh_token
                .unwrap_or_else(|| refresh_token.to_string()),
            expires_at_ms,
        });
    }

    Err(OAuthError::Transient("refresh retries exhausted".into()))
}

/// Future returned by [`TokenRefresher::refresh`]. `'static` so it can be awaited
/// after the manager's state lock is released.
pub type RefreshFuture = Pin<Box<dyn Future<Output = Result<Tokens, OAuthError>> + Send>>;

/// Abstraction over "turn a refresh token into fresh tokens", so the manager can
/// be exercised in tests without hitting the network.
pub trait TokenRefresher: Send + Sync {
    fn refresh(&self, refresh_token: String) -> RefreshFuture;
}

/// The production refresher: a real HTTPS call to Anthropic's OAuth endpoint.
pub struct LiveRefresher {
    client: reqwest::Client,
    endpoint: String,
}

impl LiveRefresher {
    pub fn new() -> Self {
        Self {
            // A dedicated client: the upstream proxy client must NOT carry a
            // total timeout (streams run long), but a refresh must not hang.
            //
            // no_proxy(): reqwest honors HTTPS_PROXY/HTTP_PROXY by default. We ARE the
            // proxy — routing our own upstream through an ambient proxy (e.g. a
            // teamclaude on :3456) loops us through the very thing we replace and fails
            // as "upstream unreachable". Always reach Anthropic directly.
            client: reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("build reqwest client"),
            endpoint: TOKEN_ENDPOINT.to_string(),
        }
    }
}

impl Default for LiveRefresher {
    fn default() -> Self {
        Self::new()
    }
}

impl TokenRefresher for LiveRefresher {
    fn refresh(&self, refresh_token: String) -> RefreshFuture {
        let client = self.client.clone();
        let endpoint = self.endpoint.clone();
        Box::pin(async move { refresh_access_token_at(&client, &refresh_token, &endpoint).await })
    }
}

/// The error a [`NoRefresh`] attempt fails with, surfaced when an offline snapshot
/// (`tcr status`, `tcr accounts --probe`) meets an already-expired token.
pub const NO_REFRESH_MESSAGE: &str = "offline snapshot never refreshes — token expired; check the running server's TUI or re-run after tcr login";

/// A refresher that NEVER contacts the OAuth token endpoint: every attempt fails
/// with [`NO_REFRESH_MESSAGE`] (a *transient* error, so it never falsely sidelines
/// the credential as dead). Injected by [`crate::cli::snapshot_offline`] so a
/// second process (`tcr status`) can never perform a real OAuth refresh — which,
/// because refresh tokens are SINGLE-USE, would rotate and thereby REVOKE the copy
/// the running server still holds, killing that account (observed live 2026-07-19).
/// Accounts with a still-valid access token probe normally (no refresh is
/// attempted); expired ones surface a visible probe error instead of refreshing.
pub struct NoRefresh;

impl TokenRefresher for NoRefresh {
    fn refresh(&self, _refresh_token: String) -> RefreshFuture {
        Box::pin(async { Err(OAuthError::Transient(NO_REFRESH_MESSAGE.to_string())) })
    }
}

// ===========================================================================
// Browser OAuth login (PKCE), ported from `teamclaude/src/oauth.js`
// (`loginOAuth` / `startCallbackServer` / `raceWithStdinCode` / `openBrowser` /
// `fetchProfile`). Adds a new account to the drop-in config so friends can
// onboard without hand-editing JSON.
// ===========================================================================

use std::io::Write as _;
use std::path::Path;

use anyhow::{anyhow, bail, Context as _};
use oauth2::basic::BasicClient;
use oauth2::{AuthUrl, ClientId, CsrfToken, PkceCodeChallenge, RedirectUrl, Scope, TokenUrl};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::TcpListener;

use crate::config::{self, Account, Config};
use crate::singleton;

/// Authorize endpoint (from the JS `OAUTH_AUTHORIZE`).
pub const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
/// Profile endpoint used to name the account (from the JS `PROFILE_URL`).
pub const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
/// OAuth scopes requested at login (from the JS `OAUTH_SCOPES`).
pub const OAUTH_SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

/// Overall login timeout matching the JS 2-minute callback-server deadline.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(120);

/// PKCE + CSRF material for one login attempt, produced by the `oauth2` crate.
/// Holds the plaintext `verifier` and `state` secrets we replay at the manual
/// token exchange, plus the fully-built authorize URL.
struct LoginFlow {
    /// The PKCE `code_verifier` secret, replayed at token exchange.
    verifier: String,
    /// The anti-CSRF `state` secret, validated on the callback.
    state: String,
    /// The authorize URL to open in the browser.
    auth_url: String,
}

/// Build the authorize URL, PKCE challenge and CSRF state via the standard
/// `oauth2` client. PKCE (S256), `state`, and standard params (`response_type`,
/// `client_id`, `redirect_uri`, `scope`, `code_challenge*`) are all produced by
/// the library; only Claude's non-standard `code=true` is added by hand.
fn build_login_flow(redirect_uri: &str) -> anyhow::Result<LoginFlow> {
    let client = BasicClient::new(ClientId::new(CLIENT_ID.to_string()))
        .set_auth_uri(AuthUrl::new(AUTHORIZE_URL.to_string()).context("invalid authorize URL")?)
        .set_token_uri(TokenUrl::new(TOKEN_ENDPOINT.to_string()).context("invalid token URL")?)
        .set_redirect_uri(
            RedirectUrl::new(redirect_uri.to_string()).context("invalid redirect URI")?,
        );

    // oauth2 generates a random verifier and its S256 challenge together.
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();

    let (auth_url, csrf) = client
        // 32-byte state, NOT the crate-default `new_random()` (16 bytes / 22
        // chars): Claude's authorize endpoint rejects the short state with
        // "Invalid request format" once a logged-in session validates the
        // request. 32 bytes (43 base64url chars) matches the working JS
        // reference (`randomBytes(32).toString('base64url')`).
        .authorize_url(|| CsrfToken::new_random_len(32))
        .add_scope(Scope::new(OAUTH_SCOPES.to_string()))
        .set_pkce_challenge(challenge)
        // Claude's authorize endpoint requires this non-standard flag.
        .add_extra_param("code", "true")
        .url();

    Ok(LoginFlow {
        verifier: verifier.secret().clone(),
        state: csrf.secret().clone(),
        auth_url: auth_url.to_string(),
    })
}

/// Extract `(code, state, error)` from a URL query string (`k=v&k2=v2`).
///
/// Decoding is [`form_urlencoded`]'s — the same parser `url`, `axum` and
/// `reqwest` already pull into this tree, so it costs no package — rather than
/// the hand-rolled `%XX`/`+` decoder this replaces. It is lossy on invalid
/// UTF-8, so a hostile callback still cannot panic the CLI.
///
/// The explicit loop is kept deliberately over a `collect()` into a map: it
/// preserves **last-occurrence-wins** on a duplicated key, which is what the
/// hand-rolled version did.
///
/// One disclosed behaviour change: `form_urlencoded` percent-decodes the KEY as
/// well as the value, so `%63ode=x` now matches `code` where it previously did
/// not. That is the more spec-correct reading and is unreachable from a real
/// Claude redirect. A differential run over 4,132 inputs (36 hand-picked edges
/// plus a 4,096-case brute-force sweep) found this as the ONLY divergence —
/// nothing changed for `;` separators, bare keys, `code=a=b`, lone surrogates,
/// `%00`, `%FF`, a trailing `%`, or `+`-vs-`%2B`.
fn parse_oauth_query(query: &str) -> (Option<String>, Option<String>, Option<String>) {
    let (mut code, mut state, mut error) = (None, None, None);
    for (k, v) in form_urlencoded::parse(query.as_bytes()) {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            "error" => error = Some(v.into_owned()),
            _ => {}
        }
    }
    (code, state, error)
}

/// Resolve a callback into the authorization code, enforcing OAuth security:
/// an `error` fails; `state` MUST be present and equal to ours (mismatch is a
/// CSRF signal and is rejected); a missing code fails.
fn resolve_callback_code(
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    expected_state: &str,
) -> anyhow::Result<String> {
    if let Some(err) = error {
        bail!("OAuth error: {err}");
    }
    if state.as_deref() != Some(expected_state) {
        bail!("OAuth state mismatch");
    }
    code.ok_or_else(|| anyhow!("callback missing authorization code"))
}

/// Open `url` in the default browser (best-effort; ignores failure like the JS).
fn open_browser(url: &str) {
    let cmd = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "start"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(cmd).arg(url).spawn();
}

/// Minimal local callback server on `listener` (already bound to 127.0.0.1).
/// Accepts connections until a valid `GET /callback?...` arrives, then replies
/// `302` to the success page and returns the code. A callback carrying an
/// `error` or a mismatched `state` fails immediately.
async fn run_callback_server(
    listener: TcpListener,
    expected_state: &str,
) -> anyhow::Result<String> {
    loop {
        let (mut stream, _) = listener.accept().await?;

        // Read one chunk — the request line + headers arrive together; we only
        // need the request line (`GET /callback?query HTTP/1.1`).
        let mut buf = vec![0u8; 8192];
        let n = stream.read(&mut buf).await.unwrap_or(0);
        let request = String::from_utf8_lossy(&buf[..n]);
        let target = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("");
        let (path, query) = target.split_once('?').unwrap_or((target, ""));

        if path != "/callback" {
            let _ = stream
                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nNot found")
                .await;
            continue;
        }

        let (code, state, error) = parse_oauth_query(query);
        match resolve_callback_code(code, state, error, expected_state) {
            Ok(code) => {
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 302 Found\r\nLocation: https://platform.claude.com/oauth/code/success?app=claude-code\r\nContent-Length: 0\r\n\r\n",
                    )
                    .await;
                let _ = stream.flush().await;
                return Ok(code);
            }
            Err(err) => {
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<html><body><h2>Authentication failed</h2><p>You can close this tab.</p></body></html>",
                    )
                    .await;
                let _ = stream.flush().await;
                return Err(err);
            }
        }
    }
}

/// Fallback path: accept a pasted callback URL or raw code from stdin. Empty
/// lines keep waiting; a full URL has its `state` validated (reject on
/// mismatch) exactly like the callback. On a non-TTY / EOF stdin, this never
/// settles so the browser callback or the timeout wins (mirrors the JS
/// `isTTY` guard).
async fn read_stdin_code(expected_state: &str) -> anyhow::Result<String> {
    eprint!("Paste authorization code here (or wait for browser callback): ");
    let _ = std::io::stderr().flush();

    let mut reader = BufReader::new(tokio::io::stdin());
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line).await?;
        if read == 0 {
            // EOF (piped/closed stdin): never settle from here.
            std::future::pending::<()>().await;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // A pasted callback URL: validate state, extract code.
        if let Some((_, query)) = trimmed.split_once('?') {
            let (code, state, _error) = parse_oauth_query(query);
            if let Some(code) = code {
                // Only reject when a state was supplied and does not match —
                // a bare code without state stays acceptable (JS parity).
                if let Some(state) = state {
                    if state != expected_state {
                        bail!("OAuth state mismatch");
                    }
                }
                return Ok(code);
            }
        }
        // Otherwise treat the whole line as the raw authorization code.
        return Ok(trimmed.to_string());
    }
}

/// Wait for the authorization code from whichever source arrives first — the
/// browser callback or a manual stdin paste — bounded by the 2-minute timeout.
async fn wait_for_code(listener: TcpListener, expected_state: &str) -> anyhow::Result<String> {
    tokio::select! {
        result = run_callback_server(listener, expected_state) => result,
        result = read_stdin_code(expected_state) => result,
        _ = tokio::time::sleep(LOGIN_TIMEOUT) => Err(anyhow!("Login timed out after 2 minutes")),
    }
}

/// Exchange the authorization code for tokens (`POST {TOKEN_ENDPOINT}`).
///
/// This is the ONE step kept as a manual `reqwest` JSON POST rather than
/// oauth2's `exchange_code`: Claude's token endpoint is non-standard — it wants
/// a JSON body with `Content-Type: application/json` (as the JS reference
/// sends), not the RFC 6749 form-encoding oauth2 emits, and it echoes `state`
/// in the body. Uses a `.no_proxy()` client so an ambient `HTTPS_PROXY` cannot
/// swallow the request.
async fn exchange_code(
    code: &str,
    verifier: &str,
    state: &str,
    redirect_uri: &str,
) -> anyhow::Result<Tokens> {
    #[derive(Serialize)]
    struct ExchangeRequest<'a> {
        code: &'a str,
        state: &'a str,
        grant_type: &'a str,
        client_id: &'a str,
        redirect_uri: &'a str,
        code_verifier: &'a str,
    }

    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .context("build OAuth token-exchange client")?;

    let response = client
        .post(TOKEN_ENDPOINT)
        .header("Content-Type", "application/json")
        .json(&ExchangeRequest {
            code,
            state,
            grant_type: "authorization_code",
            client_id: CLIENT_ID,
            redirect_uri,
            code_verifier: verifier,
        })
        .send()
        .await
        .context("token exchange request failed")?;

    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("Token exchange failed ({}): {text}", status.as_u16());
    }

    tokens_from_exchange_body(&text)
}

/// Turn a raw token-endpoint response body into [`Tokens`], failing the exchange
/// when no usable `refresh_token` is present.
///
/// Unlike the *refresh* path — which has the prior refresh token to fall back on
/// (`refresh_token.unwrap_or_else(|| refresh_token.to_string())`) — a code→token
/// *exchange* has no prior token. So an absent or empty/whitespace `refresh_token`
/// here would mint an account whose stored token is `""`; that account dies with
/// `AuthRejected` on its first refresh (the refresh POST sends the empty token).
/// Fail the exchange instead of persisting a self-destructing account.
fn tokens_from_exchange_body(text: &str) -> anyhow::Result<Tokens> {
    let data: RefreshResponse =
        serde_json::from_str(text).context("parse token-exchange response")?;

    let refresh_token = match data.refresh_token {
        Some(token) if !token.trim().is_empty() => token,
        _ => bail!("token exchange returned no refresh_token"),
    };

    let expires_at_ms = data.expires_at.map_or_else(
        || expires_at_from(crate::now_ms(), data.expires_in),
        normalize_expires_at,
    );

    Ok(Tokens {
        access_token: data.access_token,
        refresh_token,
        expires_at_ms,
    })
}

/// The account+org identity fetched from the profile endpoint. Every field is
/// optional so a partial or failed fetch degrades gracefully: `email` names the
/// account (falls back to a prompt when absent) and `account_uuid`/`org_uuid`/
/// `org_name` key the account's identity so multi-org logins stay distinct.
struct Profile {
    email: Option<String>,
    account_uuid: Option<String>,
    org_uuid: Option<String>,
    org_name: Option<String>,
}

/// Fetch the account+org identity from the profile endpoint. Returns an
/// all-`None` [`Profile`] on any failure (network, non-2xx, or malformed body)
/// so the caller can still prompt for a name and login without org info. Serde
/// ignores unknown fields, so extra profile keys are harmless.
async fn fetch_profile(access_token: &str) -> Profile {
    #[derive(Deserialize)]
    struct ProfileResponse {
        account: Option<ProfileAccount>,
        organization: Option<ProfileOrg>,
    }
    #[derive(Deserialize)]
    struct ProfileAccount {
        uuid: Option<String>,
        email: Option<String>,
    }
    #[derive(Deserialize)]
    struct ProfileOrg {
        uuid: Option<String>,
        name: Option<String>,
    }

    async fn inner(access_token: &str) -> Option<ProfileResponse> {
        let client = reqwest::Client::builder().no_proxy().build().ok()?;
        let response = client
            .get(PROFILE_URL)
            .header("Authorization", format!("Bearer {access_token}"))
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        response.json().await.ok()
    }

    let non_empty = |s: Option<String>| s.filter(|v| !v.is_empty());
    match inner(access_token).await {
        Some(profile) => {
            let account = profile.account;
            let organization = profile.organization;
            Profile {
                email: non_empty(account.as_ref().and_then(|a| a.email.clone())),
                account_uuid: non_empty(account.and_then(|a| a.uuid)),
                org_uuid: non_empty(organization.as_ref().and_then(|o| o.uuid.clone())),
                org_name: non_empty(organization.and_then(|o| o.name)),
            }
        }
        None => Profile {
            email: None,
            account_uuid: None,
            org_uuid: None,
            org_name: None,
        },
    }
}

/// Prompt for an account name on stdin, falling back to `fallback` on empty
/// input or unreadable stdin.
fn prompt_account_name(fallback: &str) -> String {
    eprint!("Name this account [{fallback}]: ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(_) => {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                fallback.to_string()
            } else {
                trimmed.to_string()
            }
        }
        Err(_) => fallback.to_string(),
    }
}

/// Add or update an OAuth account in `config`, keyed on `(account_uuid, org)`
/// via [`identity::same_identity`] rather than name alone. An existing account
/// with the same identity has its tokens refreshed in place and any absent
/// identity field backfilled (never duplicated); otherwise a new account is
/// appended with the next-highest priority and every identity field populated.
///
/// Backward compatible: when the probe and the stored entry both lack identity
/// fields (today's real config), `same_identity` falls back to name equality —
/// so a single-org re-login matches its existing entry exactly as before.
pub fn upsert_account(
    config: &mut Config,
    name: &str,
    tokens: &Tokens,
    account_uuid: Option<String>,
    org_uuid: Option<String>,
    org_name: Option<String>,
) {
    let probe = crate::identity::probe(
        name,
        account_uuid.clone(),
        org_uuid.clone(),
        org_name.clone(),
    );

    if let Some(account) = config
        .accounts
        .iter_mut()
        .find(|a| crate::identity::same_identity(a, &probe))
    {
        account.account_type = "oauth".to_string();
        account.access_token = tokens.access_token.clone();
        account.refresh_token = Some(tokens.refresh_token.clone());
        account.expires_at = Some(tokens.expires_at_ms);
        // Backfill any identity field the stored entry was missing (e.g. a legacy
        // pre-org entry newly profiled), without overwriting known values.
        if account.account_uuid.is_none() {
            account.account_uuid = account_uuid;
        }
        if account.org_uuid.is_none() {
            account.org_uuid = org_uuid;
        }
        if account.org_name.is_none() {
            account.org_name = org_name;
        }
        return;
    }

    let next_priority = config
        .accounts
        .iter()
        .filter_map(|a| a.priority)
        .max()
        .map_or(0, |max| max + 1);

    config.accounts.push(Account {
        name: name.to_string(),
        account_type: "oauth".to_string(),
        account_uuid,
        org_uuid,
        org_name,
        access_token: tokens.access_token.clone(),
        refresh_token: Some(tokens.refresh_token.clone()),
        expires_at: Some(tokens.expires_at_ms),
        priority: Some(next_priority),
        switch_threshold: None,
        disabled: None,
        extra: serde_json::Map::new(),
    });
}

/// The proxy port a login must guard against — the port the running server binds.
/// Reads `proxy.port` from the config (falling back to the serde default when the
/// file is missing or unreadable), so the guard checks the SAME port `tcr server`
/// takes over.
fn login_target_port(config_path: &Path) -> u16 {
    config::load(config_path)
        .unwrap_or_else(|_| {
            serde_json::from_str("{}").expect("empty object is a valid default config")
        })
        .proxy
        .port
}

/// What `login()` should do with the finished credential, decided purely from
/// the incumbent detected on the guarded port, whether a probe already
/// confirmed THAT incumbent carries the live account-add route, and
/// `--force`. Returned by [`login_guard_refusal`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum LoginRoute {
    /// No live proxy on the port, or `--force`: the historical file-only
    /// login. `--force` always means this, even when the live route WAS
    /// confirmed — it is the deliberate escape hatch past the live
    /// coordination below, not just past the old refusal.
    File,
    /// A live proxy is there and its add route is confirmed: route the
    /// finished credential through it instead of the file.
    Live,
    /// A live proxy is there without the route, and `--force` was not given:
    /// refuse outright with this message, unchanged from before this route
    /// existed.
    Refuse(String),
}

/// The pure login-guard DECISION: given any live proxy server detected on the
/// port, whether that incumbent's live account-add route was already
/// confirmed (never probed in here — see [`probe_add_capability`]), the port,
/// and the `--force` flag, return which [`LoginRoute`] `login()` should take.
/// Split from the impure lsof-based detection
/// ([`crate::singleton::live_proxy_server`]) AND from the impure HTTP
/// capability probe so the decision itself stays unit-testable, mirroring
/// singleton's pure-decision / impure-executor split.
///
/// The incumbent's KIND decides the REFUSAL instruction, and it is not
/// cosmetic. Since the owner file, detection reaches a proxy served INSIDE a
/// host application, and the pid reported is then the HOST's. "kill {pid}"
/// would SIGTERM a GUI process that installs no handler for it: no
/// `applicationWillTerminate`, no final session→account pin write, and every
/// live session cold-starts its prompt cache at the next boot.
/// `takeover_decision` and `incumbents_to_signal` are both hardened against
/// exactly that signal; a message that ADVISES it would walk around both. So
/// an embedded incumbent is told to be quit, not killed — whether or not it
/// has the add route, since that's the refusal-message case regardless.
fn login_guard_refusal(
    incumbent: Option<singleton::Incumbent>,
    has_add_route: bool,
    port: u16,
    force: bool,
) -> LoginRoute {
    match incumbent {
        Some(incumbent) if !force && !has_add_route => {
            let pid = incumbent.pid;
            LoginRoute::Refuse(match incumbent.kind {
                singleton::ProxyKind::TcrEmbedded => format!(
                    "a tcr server is already running on port {port} (pid {pid}); logging in now would be overwritten by the server's next token refresh — that pid is the HOST APPLICATION serving the proxy in-process, so do not signal it: quit the host application (killing it skips its shutdown and loses the session pin map), run 'tcr login', then start it again. Re-run with --force to log in anyway."
                ),
                singleton::ProxyKind::Tcr | singleton::ProxyKind::LegacyJs => format!(
                    "a tcr server is already running on port {port} (pid {pid}); logging in now would be overwritten by the server's next token refresh — stop it (kill {pid}, or Ctrl-C in its terminal), run 'tcr login', then restart 'tcr server'. Re-run with --force to log in anyway."
                ),
            })
        }
        Some(_) if !force => LoginRoute::Live,
        _ => LoginRoute::File,
    }
}

/// What a live proxy said, structurally, about whether it can accept a live
/// account add. Read from [`crate::cli::post_add_account`]'s own
/// [`crate::cli::LiveControlError`] classification, which is itself driven by
/// [`crate::proxy::ENDPOINT_HEADER`] — never by matching error text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AddCapability {
    /// The route answered (stamped, whatever the status) — a live add is
    /// possible.
    Present,
    /// Nothing usable answered: no server, a route-less 404/405 (an older
    /// tcr), or the probe timed out / returned something unreadable. All
    /// three collapse to the same conservative answer: `login_guard_refusal`
    /// treats "confirmed absent" and "could not confirm" identically, always
    /// falling back to the historical refusal rather than guessing a route is
    /// there.
    Absent,
    /// The proxy answered and rejected our api-key. Its own condition — never
    /// folded into `Absent`: that would either wrongly read as an older proxy
    /// with no route, or let `login()` silently fall back to writing the file
    /// out from under a server that is right there, listening.
    Unauthorized,
}

/// Probe whether the proxy `config` points at can accept a live account add,
/// via a deliberately-invalid POST to [`crate::proxy::ADD_ACCOUNT_PATH`] — a
/// blank name and access token, always rejected by the route's own
/// validation — so this can run BEFORE any OAuth tokens exist and before the
/// browser opens. Never probes by adding a real account.
async fn probe_add_capability(config: &Config) -> AddCapability {
    let probe = Account {
        name: String::new(),
        account_type: "oauth".to_string(),
        account_uuid: None,
        org_uuid: None,
        org_name: None,
        access_token: String::new(),
        refresh_token: None,
        expires_at: None,
        priority: None,
        switch_threshold: None,
        disabled: None,
        extra: serde_json::Map::new(),
    };
    match crate::cli::post_add_account(config, &probe).await {
        // A blank name/token can never be added, so `Ok` cannot happen in
        // practice — but a stamped success is just as much "the route
        // exists" as a stamped rejection, so it counts as `Present` too.
        Ok(_) => AddCapability::Present,
        // The route validated the (deliberately bad) body and said so,
        // stamped — exactly the positive capability signal.
        Err(crate::cli::LiveControlError::Rejected(_)) => AddCapability::Present,
        Err(crate::cli::LiveControlError::Unauthorized) => AddCapability::Unauthorized,
        Err(_) => AddCapability::Absent,
    }
}

/// Resolve which [`LoginRoute`] `login()` should take, running the impure
/// capability probe only when it can matter — a live incumbent is on the port
/// and `--force` was not given. Fully resolves before `login()` starts the
/// OAuth dance, so a user is never told to stop a server AFTER completing a
/// full browser round-trip, and never silently falls back to the file when
/// the incumbent answered but rejected our api-key.
async fn login_route(
    config_path: &Path,
    incumbent: Option<singleton::Incumbent>,
    port: u16,
    force: bool,
) -> anyhow::Result<LoginRoute> {
    if force || incumbent.is_none() {
        return Ok(login_guard_refusal(incumbent, false, port, force));
    }
    let config = load_or_default(config_path)?;
    match probe_add_capability(&config).await {
        AddCapability::Unauthorized => bail!(
            "the proxy on :{port} rejected the api-key in {} while checking whether it could \
             take a live login — no browser was opened and nothing was changed. Fix \
             `proxy.apiKey` and retry, or re-run with --force to log in via the file instead \
             (this still risks the running server overwriting it on its next token refresh).",
            config_path.display()
        ),
        AddCapability::Present => Ok(login_guard_refusal(incumbent, true, port, force)),
        AddCapability::Absent => Ok(login_guard_refusal(incumbent, false, port, force)),
    }
}

/// Run the full browser OAuth login and persist the account. Returns the
/// account name on success. Never logs or prints the tokens.
///
/// Refuses (unless `force`) when a live proxy server already holds the
/// configured port AND that proxy has no live account-add route: an older
/// tcr reads config only at boot, and its next `persist_tokens` writes its
/// boot-time TOKENS back over the file, silently clobbering the fresh ones
/// this login writes (observed live 2026-07-19 — a server refresh overwrote a
/// re-login within seconds). When the live route IS there, this routes the
/// finished credential through the server instead ([`finish_login`]) — the
/// server owns the write with its own current state, so the window above
/// does not exist on that path at all. Detection is read-only; the server is
/// never signalled.
pub async fn login(config_path: &Path, force: bool) -> anyhow::Result<String> {
    let port = login_target_port(config_path);
    let incumbent = singleton::live_proxy_server(port);
    let route = login_route(config_path, incumbent, port, force).await?;
    if let LoginRoute::Refuse(msg) = route {
        bail!("{}", msg);
    }

    // Bind the callback server on a random loopback port (127.0.0.1 only).
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("bind local OAuth callback server")?;
    let callback_port = listener.local_addr()?.port();
    let redirect_uri = format!("http://localhost:{callback_port}/callback");

    // PKCE + CSRF state + authorize URL, built by the oauth2 crate.
    let flow = build_login_flow(&redirect_uri)?;
    println!("Opening browser for authentication...");
    println!("If it doesn't open, visit:\n  {}\n", flow.auth_url);
    open_browser(&flow.auth_url);

    let code = wait_for_code(listener, &flow.state).await?;

    println!("Exchanging authorization code for tokens...");
    let tokens = exchange_code(&code, &flow.verifier, &flow.state, &redirect_uri).await?;

    // Load the existing config once (missing file → empty default; a corrupt
    // file surfaces as an error so login never clobbers it with defaults).
    let mut config = load_or_default(config_path)?;

    // Fetch the account+org identity; name from the profile email, else prompt.
    let profile = fetch_profile(&tokens.access_token).await;
    let fallback = format!("account-{}", config.accounts.len() + 1);
    let name = match &profile.email {
        Some(email) => email.clone(),
        None => prompt_account_name(&fallback),
    };

    finish_login(config_path, route, &mut config, &name, &tokens, profile).await
}

/// The file half of a login: `upsert_account` + whole-file [`config::save`].
/// Exactly what `login()` always did, kept as its own function so both
/// [`LoginRoute::File`] and every live-path fallback in [`finish_login`] run
/// the identical sequence rather than duplicating it.
fn persist_via_file(
    config_path: &Path,
    config: &mut Config,
    name: &str,
    tokens: &Tokens,
    profile: Profile,
) -> anyhow::Result<String> {
    upsert_account(
        config,
        name,
        tokens,
        profile.account_uuid,
        profile.org_uuid,
        profile.org_name,
    );
    config::save(config_path, config).context("save config after login")?;
    println!("Saved account '{name}' to {}", config_path.display());
    Ok(name.to_string())
}

/// Persist the finished OAuth credential per `route`: through the live proxy
/// when it is [`LoginRoute::Live`], via [`persist_via_file`] when it is
/// [`LoginRoute::File`]. Split out of [`login`] so WHERE the credential lands
/// is testable without driving a real browser.
///
/// On the live path the running server owns the durable write — this must
/// NOT also whole-file [`config::save`] the (possibly already-stale) `config`
/// this process is holding; that write is exactly the clobber this unit
/// exists to remove. Falls back to the file only on the quiet
/// [`crate::cli::LiveControlError::NoServer`] case, mirroring
/// [`crate::cli::set_enabled`]'s discipline: a server that answered and
/// REFUSED us (`Unauthorized`, `Rejected`) must surface, never silently write
/// the file instead, because the two halves would then disagree about what
/// the account's credentials are.
async fn finish_login(
    config_path: &Path,
    route: LoginRoute,
    config: &mut Config,
    name: &str,
    tokens: &Tokens,
    profile: Profile,
) -> anyhow::Result<String> {
    match route {
        LoginRoute::Refuse(_) => {
            unreachable!("login() bails on a refusal before fetching tokens")
        }
        LoginRoute::File => persist_via_file(config_path, config, name, tokens, profile),
        LoginRoute::Live => {
            let account = Account {
                name: name.to_string(),
                account_type: "oauth".to_string(),
                account_uuid: profile.account_uuid.clone(),
                org_uuid: profile.org_uuid.clone(),
                org_name: profile.org_name.clone(),
                access_token: tokens.access_token.clone(),
                refresh_token: Some(tokens.refresh_token.clone()),
                expires_at: Some(tokens.expires_at_ms),
                priority: None,
                switch_threshold: None,
                disabled: None,
                extra: serde_json::Map::new(),
            };
            match crate::cli::post_add_account(config, &account).await {
                Ok(applied) => {
                    println!(
                        "Saved account '{}' to the running proxy on :{}",
                        applied.name, config.proxy.port
                    );
                    if let Some(warning) = &applied.warning {
                        eprintln!("[tcr] warning: {warning}");
                    }
                    if !applied.persisted {
                        eprintln!(
                            "[tcr] warning: this account is live but NOT saved to {} — it will not survive a restart.",
                            config_path.display()
                        );
                    }
                    Ok(applied.name)
                }
                // Nothing is listening any more (it exited between the probe
                // and here): the quiet fallback, same discipline as
                // `set_enabled`.
                Err(crate::cli::LiveControlError::NoServer) => {
                    persist_via_file(config_path, config, name, tokens, profile)
                }
                // The proxy rejected our api-key. Writing the file here would
                // be the clobber this unit exists to remove, in a new place:
                // the running server keeps whatever it already had while the
                // file quietly disagrees. Change nothing, exit non-zero.
                Err(crate::cli::LiveControlError::Unauthorized) => bail!(
                    "the proxy on :{} rejected the api-key in {} — the config was NOT changed, \
                     because writing it would leave the running proxy unaware of this login. \
                     Fix `proxy.apiKey` and retry.",
                    config.proxy.port,
                    config_path.display()
                ),
                // The route ran and refused the submission itself (e.g. the
                // identity matched more than one live account). Do not fall
                // back — the file's own resolution could land on a different
                // account than the one the server was talking about.
                Err(crate::cli::LiveControlError::Rejected(message)) => bail!(
                    "the proxy running on :{} refused this: {message} Nothing was changed.",
                    config.proxy.port
                ),
                // The route is missing: the capability probe said it was
                // there and it no longer is (a race, or a downgrade mid-run).
                // The file write is all we can do, and it is HALF a login —
                // say so loudly, mirroring `set_enabled`'s NoRoute arm.
                Err(crate::cli::LiveControlError::NoRoute) => {
                    let saved = persist_via_file(config_path, config, name, tokens, profile)?;
                    eprintln!(
                        "[tcr] WARNING: the proxy running on :{} is too old to accept a live login (no {} route), so only the config file was changed. Run `tcr restart` when a cold prompt cache is acceptable.",
                        config.proxy.port,
                        crate::proxy::ADD_ACCOUNT_PATH,
                    );
                    Ok(saved)
                }
                // It answered something unusable, or did not answer in time.
                // Same shape of consequence as NoRoute, different cause, and
                // equally never silent.
                Err(other) => {
                    let why = other.why();
                    let saved = persist_via_file(config_path, config, name, tokens, profile)?;
                    eprintln!(
                        "[tcr] WARNING: could not apply this login to the proxy running on :{} ({why}), so only the config file was changed.",
                        config.proxy.port,
                    );
                    Ok(saved)
                }
            }
        }
    }
}

/// Load the config, treating a missing file as an empty default (so the very
/// first login creates it) while surfacing genuine parse/permission errors so
/// a corrupt file is never overwritten.
fn load_or_default(config_path: &Path) -> anyhow::Result<Config> {
    match config::load(config_path) {
        Ok(config) => Ok(config),
        Err(config::ConfigError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
            Ok(serde_json::from_str("{}").expect("empty object is a valid default config"))
        }
        Err(err) => Err(err).context("load config for login"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_treats_small_values_as_seconds() {
        // 1_700_000_000 s → ms.
        assert_eq!(normalize_expires_at(1_700_000_000), 1_700_000_000_000);
    }

    #[test]
    fn normalize_leaves_millisecond_values_unchanged() {
        assert_eq!(normalize_expires_at(1_700_000_000_000), 1_700_000_000_000);
    }

    #[test]
    fn is_expired_true_when_now_past_expiry() {
        assert!(is_expired(Some(1000), 2000));
    }

    #[test]
    fn is_expired_false_for_none() {
        assert!(!is_expired(None, 2000));
    }

    #[test]
    fn is_expiring_soon_within_window() {
        // Expiry 1 minute out is inside the 5-minute window.
        let now = 10_000_000;
        assert!(is_expiring_soon(Some(now + 60_000), now));
        assert!(!is_expiring_soon(Some(now + 10 * 60_000), now));
    }

    // --- PKCE (via the oauth2 crate) ---------------------------------------

    #[test]
    fn pkce_challenge_matches_rfc7636_vector() {
        // The canonical RFC 7636 Appendix B S256 example: the oauth2 crate must
        // map this fixed verifier to exactly this base64url (no-pad) challenge.
        let verifier = oauth2::PkceCodeVerifier::new(
            "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".to_string(),
        );
        let challenge = PkceCodeChallenge::from_code_verifier_sha256(&verifier);
        assert_eq!(
            challenge.as_str(),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn login_flow_produces_verifier_and_state() {
        // The oauth2-built flow yields a non-empty verifier and CSRF state, and
        // an authorize URL rooted at Claude's endpoint.
        let flow = build_login_flow("http://localhost:12345/callback").unwrap();
        assert!(!flow.verifier.is_empty());
        assert!(!flow.state.is_empty());
        assert!(flow.auth_url.starts_with(AUTHORIZE_URL));
        // Claude's authorize endpoint rejects short states ("Invalid request
        // format", observed live 2026-07-17 with the 22-char crate default).
        // 32 random bytes -> 43 base64url chars, matching the JS reference.
        assert_eq!(
            flow.state.len(),
            43,
            "state must be 32 bytes base64url (43 chars), got {} chars",
            flow.state.len()
        );
    }

    // --- callback / URL parsing --------------------------------------------

    #[test]
    fn parse_callback_extracts_code_and_state() {
        let (code, state, error) = parse_oauth_query("code=abc123&state=xyz789");
        assert_eq!(code.as_deref(), Some("abc123"));
        assert_eq!(state.as_deref(), Some("xyz789"));
        assert!(error.is_none());
    }

    #[test]
    fn parse_callback_percent_decodes_values() {
        let (code, _state, _error) = parse_oauth_query("code=a%2Fb%2Bc&state=s");
        assert_eq!(code.as_deref(), Some("a/b+c"));
    }

    /// The hostile-decode cases the deleted private `percent_decode` helper
    /// covered, re-pointed through the public parser now that `form_urlencoded`
    /// owns the decoding. Same inputs, same guarantees — coverage is unchanged,
    /// only the entry point moved.
    #[test]
    fn parse_oauth_query_survives_bare_percent_and_multibyte() {
        // Valid %XX still decodes.
        let (code, ..) = parse_oauth_query("code=a%20b");
        assert_eq!(code.as_deref(), Some("a b"));

        // A trailing bare `%` is preserved, never panics.
        let (code, ..) = parse_oauth_query("code=100%");
        assert_eq!(code.as_deref(), Some("100%"));

        // A `%` immediately before a multibyte UTF-8 char (hostile callback)
        // must NOT panic on a char-boundary slice.
        let (code, ..) = parse_oauth_query("code=a%\u{20AC}b");
        let code = code.expect("a % before a multibyte char must still parse");
        assert!(code.contains('a') && code.contains('b'));

        // A lone `%€` (percent right before the euro sign) also survives.
        let _ = parse_oauth_query("code=%\u{20AC}");
    }

    #[test]
    fn expires_at_from_saturates_on_huge_expires_in() {
        // A hostile/huge expires_in must saturate, never overflow into a
        // negative (past) expiry that would force a refresh storm.
        let now = crate::now_ms();
        let saturated = expires_at_from(now, Some(i64::MAX));
        assert_eq!(saturated, i64::MAX);
        assert!(
            saturated >= now,
            "expiry must be in the future, not negative"
        );
        // Normal values are unchanged: default 3600s and an explicit value.
        assert_eq!(expires_at_from(0, None), 3_600 * 1000);
        assert_eq!(expires_at_from(1000, Some(60)), 1000 + 60 * 1000);
    }

    // --- token-exchange refresh_token guard --------------------------------

    #[test]
    fn exchange_body_fails_without_usable_refresh_token() {
        // An exchange response has no prior refresh token to fall back on, so a
        // missing, empty, or whitespace `refresh_token` must FAIL the exchange —
        // never mint a Tokens with an empty refresh_token that dies on first use.
        for body in [
            r#"{"access_token":"at"}"#,                       // absent
            r#"{"access_token":"at","refresh_token":""}"#,    // empty
            r#"{"access_token":"at","refresh_token":"   "}"#, // whitespace
            r#"{"access_token":"at","refresh_token":null}"#,  // explicit null
        ] {
            assert!(
                tokens_from_exchange_body(body).is_err(),
                "expected Err for body: {body}"
            );
        }
    }

    #[test]
    fn exchange_body_ok_with_refresh_token() {
        // A response carrying a real refresh_token yields Ok with that token
        // preserved verbatim.
        let tokens = tokens_from_exchange_body(
            r#"{"access_token":"at","refresh_token":"rt","expires_at":1700000000000}"#,
        )
        .expect("a present refresh_token must return Ok");
        assert_eq!(tokens.access_token, "at");
        assert_eq!(tokens.refresh_token, "rt");
        assert_eq!(tokens.expires_at_ms, 1_700_000_000_000);
    }

    #[test]
    fn resolve_callback_accepts_matching_state() {
        let code = resolve_callback_code(
            Some("the-code".into()),
            Some("expected".into()),
            None,
            "expected",
        )
        .expect("matching state should resolve");
        assert_eq!(code, "the-code");
    }

    #[test]
    fn resolve_callback_rejects_state_mismatch() {
        let result = resolve_callback_code(
            Some("the-code".into()),
            Some("attacker".into()),
            None,
            "expected",
        );
        assert!(result.is_err(), "mismatched state must be rejected");
    }

    #[test]
    fn resolve_callback_rejects_missing_state() {
        // A callback with no state at all is a CSRF signal → reject.
        let result = resolve_callback_code(Some("the-code".into()), None, None, "expected");
        assert!(result.is_err());
    }

    #[test]
    fn resolve_callback_rejects_oauth_error() {
        let result = resolve_callback_code(
            None,
            Some("expected".into()),
            Some("access_denied".into()),
            "expected",
        );
        assert!(result.is_err());
    }

    // --- config append / update --------------------------------------------

    fn tokens(access: &str, refresh: &str, expires: i64) -> Tokens {
        Tokens {
            access_token: access.to_string(),
            refresh_token: refresh.to_string(),
            expires_at_ms: expires,
        }
    }

    #[test]
    fn upsert_appends_new_account() {
        let mut config: Config = serde_json::from_str(r#"{ "accounts": [] }"#).unwrap();
        upsert_account(
            &mut config,
            "new@example.com",
            &tokens("at1", "rt1", 111),
            None,
            None,
            None,
        );

        assert_eq!(config.accounts.len(), 1);
        let account = &config.accounts[0];
        assert_eq!(account.name, "new@example.com");
        assert_eq!(account.account_type, "oauth");
        assert_eq!(account.access_token, "at1");
        assert_eq!(account.refresh_token.as_deref(), Some("rt1"));
        assert_eq!(account.expires_at, Some(111));
        assert_eq!(account.priority, Some(0));
    }

    #[test]
    fn upsert_updates_existing_email_without_duplicating() {
        let mut config: Config = serde_json::from_str(
            r#"{ "accounts": [
                { "name": "me@example.com", "type": "oauth", "accessToken": "old-at",
                  "refreshToken": "old-rt", "expiresAt": 100, "priority": 0 }
            ] }"#,
        )
        .unwrap();

        upsert_account(
            &mut config,
            "me@example.com",
            &tokens("new-at", "new-rt", 999),
            None,
            None,
            None,
        );

        // Same account count — updated in place, not duplicated.
        assert_eq!(config.accounts.len(), 1);
        let account = &config.accounts[0];
        assert_eq!(account.access_token, "new-at");
        assert_eq!(account.refresh_token.as_deref(), Some("new-rt"));
        assert_eq!(account.expires_at, Some(999));
        // Existing priority is preserved on update.
        assert_eq!(account.priority, Some(0));
    }

    #[test]
    fn upsert_assigns_next_priority_for_additional_account() {
        let mut config: Config = serde_json::from_str(
            r#"{ "accounts": [
                { "name": "first@example.com", "type": "oauth", "accessToken": "at",
                  "refreshToken": "rt", "expiresAt": 100, "priority": 3 }
            ] }"#,
        )
        .unwrap();

        upsert_account(
            &mut config,
            "second@example.com",
            &tokens("at2", "rt2", 200),
            None,
            None,
            None,
        );

        assert_eq!(config.accounts.len(), 2);
        // max existing priority (3) + 1.
        assert_eq!(config.accounts[1].priority, Some(4));
    }

    #[test]
    fn upsert_second_org_same_email_appends_distinct_account() {
        // #0: the same email logging into a DIFFERENT org must NOT overwrite the
        // first — once both entries carry org keys they are distinct accounts.
        let mut config: Config = serde_json::from_str(r#"{ "accounts": [] }"#).unwrap();
        upsert_account(
            &mut config,
            "me@example.com",
            &tokens("at-corp", "rt-corp", 100),
            Some("uuid-person".into()),
            Some("org-corp".into()),
            Some("Corp".into()),
        );
        upsert_account(
            &mut config,
            "me@example.com",
            &tokens("at-pers", "rt-pers", 200),
            Some("uuid-person".into()),
            Some("org-personal".into()),
            Some("Personal".into()),
        );

        assert_eq!(config.accounts.len(), 2, "second org is a distinct account");
        assert_eq!(config.accounts[0].access_token, "at-corp");
        assert_eq!(config.accounts[1].access_token, "at-pers");
        assert_eq!(config.accounts[1].org_uuid.as_deref(), Some("org-personal"));
    }

    #[test]
    fn upsert_backfills_legacy_entry_org_without_duplicating() {
        // A legacy entry (uuid but no org) meeting a freshly-profiled login of the
        // same person backfills the org in place rather than duplicating.
        let mut config: Config = serde_json::from_str(
            r#"{ "accounts": [
                { "name": "me@example.com", "type": "oauth", "accountUuid": "uuid-person",
                  "accessToken": "old-at", "refreshToken": "old-rt", "expiresAt": 100, "priority": 0 }
            ] }"#,
        )
        .unwrap();

        upsert_account(
            &mut config,
            "me@example.com",
            &tokens("new-at", "new-rt", 999),
            Some("uuid-person".into()),
            Some("org-corp".into()),
            Some("Corp".into()),
        );

        assert_eq!(
            config.accounts.len(),
            1,
            "legacy entry backfilled, not duplicated"
        );
        let account = &config.accounts[0];
        assert_eq!(account.access_token, "new-at");
        assert_eq!(account.org_uuid.as_deref(), Some("org-corp"));
        assert_eq!(account.org_name.as_deref(), Some("Corp"));
    }

    // --- authorize URL ------------------------------------------------------

    #[test]
    fn authorize_url_carries_required_params() {
        // The oauth2-built authorize URL must carry every required OAuth param
        // plus Claude's non-standard `code=true`.
        let url = build_login_flow("http://localhost:12345/callback")
            .unwrap()
            .auth_url;
        assert!(url.starts_with(AUTHORIZE_URL));
        assert!(url.contains("code=true"));
        assert!(url.contains(&format!("client_id={CLIENT_ID}")));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("code_challenge="));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state="));
        // redirect_uri is percent-encoded by oauth2.
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A12345%2Fcallback"));
    }

    #[tokio::test]
    async fn no_refresh_never_hits_the_endpoint_and_fails_transiently() {
        // The whole point of Fix 1: an offline snapshot's refresher must fail
        // locally (no OAuth token-endpoint call) so a `tcr status` beside a live
        // server can never rotate/revoke its single-use refresh token. Transient,
        // never AuthRejected — it must NOT falsely mark the credential dead.
        let err = NoRefresh
            .refresh("rt-should-never-be-sent".to_string())
            .await
            .expect_err("NoRefresh must always fail — it never contacts the endpoint");
        assert!(matches!(err, OAuthError::Transient(_)));
        assert_eq!(
            err.to_string(),
            format!("token refresh failed transiently: {NO_REFRESH_MESSAGE}")
        );
    }

    /// A live proxy of the given kind, as `singleton::live_proxy_server` reports
    /// it — pid 4242 throughout, so a message assertion naming it is unambiguous.
    fn incumbent(kind: singleton::ProxyKind) -> Option<singleton::Incumbent> {
        Some(singleton::Incumbent { pid: 4242, kind })
    }

    /// Unwrap a [`LoginRoute::Refuse`]'s message, or panic naming what came back
    /// instead — every existing refusal test still wants the bare message
    /// string, and this is the one place that unwrap lives now that the guard
    /// returns a three-way enum instead of `Option<String>`.
    fn expect_refuse(route: LoginRoute) -> String {
        match route {
            LoginRoute::Refuse(msg) => msg,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn login_guard_refusal_message_carries_pid_and_stop_login_restart_sequence() {
        let msg = expect_refuse(login_guard_refusal(
            incumbent(singleton::ProxyKind::Tcr),
            false,
            3456,
            false,
        ));
        // Actionable + greppable: names the port, the PID, and the escape hatch.
        assert!(msg.contains("3456"), "names the port: {msg}");
        assert!(msg.contains("4242"), "names the pid: {msg}");
        assert!(msg.contains("--force"), "names the escape hatch: {msg}");
        assert_eq!(
            msg.lines().count(),
            1,
            "the refusal is a single line: {msg}"
        );
        // The recovery is the ORDERED stop → login → restart sequence: stop names the
        // exact PID, then `tcr login`, then restart `tcr server`.
        let stop = msg.find("kill 4242").expect("stop step names the pid");
        let login = msg.find("tcr login").expect("login step present");
        let restart = msg.find("restart").expect("restart step present");
        assert!(
            stop < login && login < restart,
            "stop → login → restart, in order: {msg}"
        );
        assert!(
            msg.contains("tcr server"),
            "restart targets the server: {msg}"
        );
    }

    #[test]
    fn login_guard_allows_with_force_or_no_server() {
        // --force is the deliberate escape hatch even when a server is live —
        // it takes the file path regardless of whether the route is there.
        assert_eq!(
            login_guard_refusal(incumbent(singleton::ProxyKind::Tcr), false, 3456, true),
            LoginRoute::File
        );
        // No server on the port → nothing to refuse.
        assert_eq!(
            login_guard_refusal(None, false, 3456, false),
            LoginRoute::File
        );
    }

    /// THE NEW OUTCOME. A live proxy that HAS the add route is no longer
    /// refused at all — the whole point of this unit.
    #[test]
    fn login_guard_proceeds_live_when_the_incumbent_has_the_add_route() {
        assert_eq!(
            login_guard_refusal(incumbent(singleton::ProxyKind::Tcr), true, 3456, false),
            LoginRoute::Live
        );
        // Even an embedded host, whose absent-route refusal has the special
        // "quit, don't kill" wording — once the route is confirmed there is
        // nothing left to refuse, embedded or not.
        assert_eq!(
            login_guard_refusal(
                incumbent(singleton::ProxyKind::TcrEmbedded),
                true,
                3456,
                false
            ),
            LoginRoute::Live
        );
    }

    /// THE ADVICE MUST BE SURVIVABLE. `live_proxy_server` can now name the pid of
    /// the host application that serves the proxy in-process, and telling the
    /// operator to `kill` it is the one action `singleton` is hardened against
    /// twice over: AppKit installs no SIGTERM handler, so the app dies without
    /// `applicationWillTerminate` and the final session→account pin write is lost.
    /// A guard that refuses correctly and then advises the destructive fix is worse
    /// than no guard, because the operator does what the tool says.
    #[test]
    fn the_login_guard_never_tells_an_operator_to_kill_a_host_application() {
        let msg = expect_refuse(login_guard_refusal(
            incumbent(singleton::ProxyKind::TcrEmbedded),
            false,
            3456,
            false,
        ));
        assert!(
            !msg.contains("kill 4242"),
            "must not advise signalling the host application: {msg}"
        );
        assert!(
            !msg.contains("Ctrl-C"),
            "the host application is not a foreground process to interrupt: {msg}"
        );
        assert!(
            msg.contains("quit the host application"),
            "must name the one action that stops an embedded proxy safely: {msg}"
        );
        // Everything the CLI refusal is load-bearing for still holds: one line,
        // names the port, the pid and the escape hatch.
        assert!(
            msg.contains("3456") && msg.contains("4242") && msg.contains("--force"),
            "{msg}"
        );
        assert_eq!(
            msg.lines().count(),
            1,
            "the refusal is a single line: {msg}"
        );

        // And the control: a plain CLI peer, which the operator CAN signal, still
        // gets the kill instruction — so the assertions above are about the kind,
        // not about the message having been softened for everyone.
        let cli = expect_refuse(login_guard_refusal(
            incumbent(singleton::ProxyKind::Tcr),
            false,
            3456,
            false,
        ));
        assert!(cli.contains("kill 4242"), "{cli}");
    }

    #[test]
    fn login_refuses_over_proxy_command_lines_but_allows_an_unrelated_holder() {
        use crate::singleton::is_proxy_server;
        // The real chain: a port holder blocks login IFF its argv is a
        // recognized tcr/teamclaude *server*. Compose the pure classifier with the
        // pure guard decision — refuse / refuse / allow. Argv, not a joined
        // command line: `singleton::classify_proxy_server` takes pre-split argv
        // (as `sysinfo::Process::cmd()` returns it) so a space in an executable's
        // path can never be re-split into the wrong tokens.
        let refuses = |cmd: &[&str]| {
            let argv: Vec<String> = cmd.iter().map(|s| s.to_string()).collect();
            let server = singleton::classify_proxy_server(&argv)
                .map(|kind| singleton::Incumbent { pid: 4242, kind });
            assert_eq!(
                server.is_some(),
                is_proxy_server(&argv),
                "the bool and the classifying form must agree: {cmd:?}"
            );
            matches!(
                login_guard_refusal(server, false, 3456, false),
                LoginRoute::Refuse(_)
            )
        };
        assert!(
            refuses(&["/opt/teamclaude-rs/target/release/tcr", "server"]),
            "a live tcr server must block login"
        );
        assert!(
            refuses(&["node", "/opt/nvm/bin/teamclaude", "server"]),
            "a live JS teamclaude server must block login"
        );
        assert!(
            !refuses(&["python3", "-m", "http.server", "3456"]),
            "an unrelated process on the port must NOT block login"
        );
    }

    #[test]
    fn login_target_port_reads_config_and_defaults_when_missing() {
        // Reads proxy.port so the guard checks the SAME port the server binds.
        let path =
            std::env::temp_dir().join(format!("tcr-oauth-login-port-{}.json", std::process::id()));
        std::fs::write(&path, r#"{ "proxy": { "port": 4999 } }"#).unwrap();
        assert_eq!(login_target_port(&path), 4999);
        std::fs::remove_file(&path).ok();
        // A missing/unreadable config falls back to the serde default (3456).
        let missing = std::env::temp_dir().join("tcr-oauth-login-port-does-not-exist.json");
        assert_eq!(login_target_port(&missing), 3456);
    }

    // --- probe_add_capability -----------------------------------------------

    #[tokio::test]
    async fn probe_add_capability_reads_a_stamped_400_as_present() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config: Config =
            serde_json::from_str(&format!(r#"{{ "proxy": {{ "port": {port} }} }}"#)).unwrap();
        let manager = crate::manager::Manager::with_live_refresher(config.clone(), None);
        tokio::spawn(async move { crate::mitm::serve(listener, manager, None).await });

        assert_eq!(probe_add_capability(&config).await, AddCapability::Present);
    }

    #[tokio::test]
    async fn probe_add_capability_unstamped_404_is_absent() {
        // An "older tcr": something answers, but not on this route at all — no
        // `ENDPOINT_HEADER`, unlike every response this crate's own router
        // produces (even its 404s and 405s).
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, axum::Router::new()).await;
        });
        let config: Config =
            serde_json::from_str(&format!(r#"{{ "proxy": {{ "port": {port} }} }}"#)).unwrap();
        assert_eq!(probe_add_capability(&config).await, AddCapability::Absent);
    }

    #[tokio::test]
    async fn probe_add_capability_wrong_api_key_is_unauthorized_not_absent() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server_config: Config = serde_json::from_str(&format!(
            r#"{{ "proxy": {{ "port": {port}, "apiKey": "correct-key" }} }}"#
        ))
        .unwrap();
        let manager = crate::manager::Manager::with_live_refresher(server_config, None);
        tokio::spawn(async move { crate::mitm::serve(listener, manager, None).await });

        // The caller's config carries the WRONG key.
        let client_config: Config = serde_json::from_str(&format!(
            r#"{{ "proxy": {{ "port": {port}, "apiKey": "wrong-key" }} }}"#
        ))
        .unwrap();
        assert_eq!(
            probe_add_capability(&client_config).await,
            AddCapability::Unauthorized,
            "a rejected api-key must surface as itself, never read as 'no route'"
        );
    }

    // --- finish_login ---------------------------------------------------------

    /// A [`Profile`] carrying only an email — the common case, no org info.
    fn profile_named(email: &str) -> Profile {
        Profile {
            email: Some(email.to_string()),
            account_uuid: None,
            org_uuid: None,
            org_name: None,
        }
    }

    /// THE BITING TEST for the whole-file-write hazard `finish_login` exists to
    /// remove. The documented failure: `login()` loads a config snapshot, does
    /// slow async work (a profile fetch, an unbounded stdin prompt), and THEN
    /// whole-file `config::save`s that now-stale snapshot — silently reverting
    /// any account the live server rotated in the meantime. Refresh tokens are
    /// single-use, so that does not just lose a write, it bricks the reverted
    /// account.
    ///
    /// Reproduced here without a real browser: seed a live server + config file
    /// with two accounts, take a CLIENT-SIDE snapshot (what `login()` would be
    /// holding across its own async window), then have the LIVE SERVER itself
    /// rotate one of those accounts' credentials — via the same
    /// `/_tcr/accounts` route, a real re-login — AFTER the snapshot was taken.
    /// Only then does `finish_login` run, on the STALE snapshot, adding a THIRD
    /// account live. If it whole-file-saved that snapshot, the rotation would
    /// be reverted. It must not be.
    #[tokio::test]
    async fn finish_login_live_path_does_not_clobber_a_concurrent_rotation() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-live-clobber-{}-{port}.json",
            std::process::id()
        ));
        let seed = format!(
            r#"{{ "proxy": {{ "port": {port} }}, "accounts": [
                {{ "name": "alice@example.com", "type": "oauth", "accessToken": "at-alice",
                  "refreshToken": "rt-alice", "expiresAt": 1893456000000, "priority": 0 }},
                {{ "name": "bob@example.com", "type": "oauth", "accessToken": "at-bob-STALE",
                  "refreshToken": "rt-bob-STALE", "expiresAt": 1893456000000, "priority": 1 }}
            ] }}"#
        );
        std::fs::write(&path, &seed).unwrap();

        let server_config: Config = serde_json::from_str(&seed).unwrap();
        let manager =
            crate::manager::Manager::with_live_refresher(server_config, Some(path.clone()));
        tokio::spawn(async move { crate::mitm::serve(listener, manager, None).await });

        // The stale snapshot: what `login()` would have loaded before its own
        // async window (profile fetch / stdin prompt) opened.
        let mut client_config = load_or_default(&path).unwrap();

        // The live server rotates bob's credentials WHILE that snapshot is
        // held — a real re-login through the same route this unit adds,
        // exactly the race `login()`'s doc-comment describes.
        let rotated_bob = Account {
            name: "bob@example.com".to_string(),
            account_type: "oauth".to_string(),
            account_uuid: None,
            org_uuid: None,
            org_name: None,
            access_token: "at-bob-ROTATED".to_string(),
            refresh_token: Some("rt-bob-ROTATED".to_string()),
            expires_at: Some(1_893_456_000_000),
            priority: None,
            switch_threshold: None,
            disabled: None,
            extra: serde_json::Map::new(),
        };
        let rotate_config = load_or_default(&path).unwrap();
        crate::cli::post_add_account(&rotate_config, &rotated_bob)
            .await
            .expect("the live re-login must succeed");

        // Sanity: the rotation really did land on disk before the race window
        // (the `finish_login` call below) closes.
        let after_rotation = std::fs::read_to_string(&path).unwrap();
        assert!(
            after_rotation.contains("rt-bob-ROTATED"),
            "setup: the rotation must be on disk first: {after_rotation}"
        );

        // Now finish a THIRD account's login, live, using the STALE snapshot.
        let name = finish_login(
            &path,
            LoginRoute::Live,
            &mut client_config,
            "charlie@example.com",
            &tokens("at-charlie", "rt-charlie", 1_893_456_000_000),
            profile_named("charlie@example.com"),
        )
        .await
        .unwrap();
        assert_eq!(name, "charlie@example.com");

        let final_doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let accounts = final_doc["accounts"].as_array().unwrap();
        assert_eq!(
            accounts.len(),
            3,
            "alice, rotated bob, and charlie: {final_doc}"
        );
        let bob = accounts
            .iter()
            .find(|a| a["name"] == "bob@example.com")
            .expect("bob still on disk");
        assert_eq!(
            bob["refreshToken"],
            serde_json::json!("rt-bob-ROTATED"),
            "the live add must NOT whole-file-write a stale snapshot back over \
             bob's rotation: {final_doc}"
        );
        assert!(
            accounts.iter().any(|a| a["name"] == "charlie@example.com"),
            "charlie was added live: {final_doc}"
        );

        std::fs::remove_file(&path).ok();
    }

    /// A server that answered and REFUSED us (wrong api-key) must surface,
    /// never silently fall back to writing the file — that silent fallback is
    /// how the two halves end up disagreeing about an account's credentials.
    #[tokio::test]
    async fn finish_login_live_unauthorized_does_not_fall_back_to_file() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seed = format!(r#"{{ "proxy": {{ "port": {port}, "apiKey": "correct-key" }} }}"#);
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-live-unauthorized-{}-{port}.json",
            std::process::id()
        ));
        std::fs::write(&path, &seed).unwrap();

        let server_config: Config = serde_json::from_str(&seed).unwrap();
        let manager =
            crate::manager::Manager::with_live_refresher(server_config, Some(path.clone()));
        tokio::spawn(async move { crate::mitm::serve(listener, manager, None).await });

        // The caller's config carries the WRONG key.
        let mut client_config: Config = serde_json::from_str(&format!(
            r#"{{ "proxy": {{ "port": {port}, "apiKey": "wrong-key" }} }}"#
        ))
        .unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let err = finish_login(
            &path,
            LoginRoute::Live,
            &mut client_config,
            "dave@example.com",
            &tokens("at-dave", "rt-dave", 1_893_456_000_000),
            profile_named("dave@example.com"),
        )
        .await
        .expect_err("a rejected api-key must not silently write the file");
        assert!(err.to_string().contains("api-key"), "{err}");

        assert_eq!(
            before,
            std::fs::read_to_string(&path).unwrap(),
            "the file must be byte-identical after a surfaced Unauthorized"
        );
        std::fs::remove_file(&path).ok();
    }

    /// The quiet fallback: nothing is listening at all, so there is no live
    /// rotation to disagree with the file — `finish_login` writes it exactly
    /// as the historical file-only path always has.
    #[tokio::test]
    async fn finish_login_no_server_falls_back_to_file_quietly() {
        let dead_port = {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            listener.local_addr().unwrap().port()
        };
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-live-noserver-{}-{dead_port}.json",
            std::process::id()
        ));
        std::fs::write(
            &path,
            format!(r#"{{ "proxy": {{ "port": {dead_port} }}, "accounts": [] }}"#),
        )
        .unwrap();
        let mut config = load_or_default(&path).unwrap();

        let name = finish_login(
            &path,
            LoginRoute::Live,
            &mut config,
            "erin@example.com",
            &tokens("at-erin", "rt-erin", 1_893_456_000_000),
            profile_named("erin@example.com"),
        )
        .await
        .unwrap();
        assert_eq!(name, "erin@example.com");

        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            doc["accounts"][0]["name"],
            serde_json::json!("erin@example.com")
        );
        std::fs::remove_file(&path).ok();
    }
}
