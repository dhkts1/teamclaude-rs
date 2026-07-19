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

/// Map a single ASCII hex digit byte to its value, or `None` if not a hex digit.
fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Percent-decode a query-component value (`%XX` and `+` → space), lossy on
/// invalid UTF-8 so a hostile callback can never panic the CLI.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2]))
            {
                (Some(hi), Some(lo)) => {
                    out.push((hi << 4) | lo);
                    i += 3;
                }
                _ => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Extract `(code, state, error)` from a URL query string (`k=v&k2=v2`),
/// percent-decoding each value.
fn parse_oauth_query(query: &str) -> (Option<String>, Option<String>, Option<String>) {
    let (mut code, mut state, mut error) = (None, None, None);
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let v = percent_decode(v);
        match k {
            "code" => code = Some(v),
            "state" => state = Some(v),
            "error" => error = Some(v),
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

/// The pure login-guard DECISION: given the PID of any live proxy server detected
/// on the port, the port, and the `--force` flag, return the one-line refusal
/// message to abort with, or `None` to proceed. Split from the impure lsof-based
/// detection ([`crate::singleton::live_proxy_server`]) so the refuse/allow logic is
/// unit-testable, mirroring singleton's pure-decision / impure-executor split.
fn login_guard_refusal(server_pid: Option<u32>, port: u16, force: bool) -> Option<String> {
    match server_pid {
        Some(pid) if !force => Some(format!(
            "a tcr server is already running on port {port} (pid {pid}); logging in now would be overwritten by the server's next token refresh — stop it (kill {pid}, or Ctrl-C in its terminal), run 'tcr login', then restart 'tcr server'. Re-run with --force to log in anyway."
        )),
        _ => None,
    }
}

/// Run the full browser OAuth login and persist the account to `config_path`.
/// Returns the account name on success. Never logs or prints the tokens.
///
/// Refuses (unless `force`) when a live proxy server already holds the configured
/// port: the server reads config only at boot and its next `persist_tokens`
/// rewrites the WHOLE file from memory, silently clobbering the fresh tokens this
/// login writes (observed live 2026-07-19 — a server refresh overwrote a re-login
/// within seconds). Detection is read-only; the server is never signalled.
pub async fn login(config_path: &Path, force: bool) -> anyhow::Result<String> {
    let port = login_target_port(config_path);
    if let Some(msg) = login_guard_refusal(singleton::live_proxy_server(port), port, force) {
        bail!("{}", msg);
    }

    // Bind the callback server on a random loopback port (127.0.0.1 only).
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("bind local OAuth callback server")?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://localhost:{port}/callback");

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

    upsert_account(
        &mut config,
        &name,
        &tokens,
        profile.account_uuid,
        profile.org_uuid,
        profile.org_name,
    );
    config::save(config_path, &config).context("save config after login")?;

    println!("Saved account '{name}' to {}", config_path.display());
    Ok(name)
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

    #[test]
    fn percent_decode_normal_and_bare_and_multibyte() {
        // Valid %XX is unchanged.
        assert_eq!(percent_decode("a%20b"), "a b");
        // A trailing bare `%` is preserved, never panics.
        assert_eq!(percent_decode("100%"), "100%");
        // A `%` immediately before a multibyte UTF-8 char (hostile callback)
        // must NOT panic on a char-boundary slice — it just returns a String.
        let decoded = percent_decode("a%\u{20AC}b");
        assert!(decoded.contains('a') && decoded.contains('b'));
        // A lone `%€` (percent right before the euro sign) also survives.
        let _ = percent_decode("%\u{20AC}");
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

    #[test]
    fn login_guard_refuses_when_a_server_is_live() {
        let msg = login_guard_refusal(Some(4242), 3456, false)
            .expect("a live server without --force must refuse");
        // Actionable + greppable: names the port, the PID, and the escape hatch.
        assert!(msg.contains("3456"), "names the port: {msg}");
        assert!(msg.contains("4242"), "names the pid: {msg}");
        assert!(msg.contains("--force"), "names the escape hatch: {msg}");
        assert!(msg.contains("tcr login"), "gives the recovery step: {msg}");
        assert_eq!(
            msg.lines().count(),
            1,
            "the refusal is a single line: {msg}"
        );
    }

    #[test]
    fn login_guard_allows_with_force_or_no_server() {
        // --force is the deliberate escape hatch even when a server is live.
        assert!(login_guard_refusal(Some(4242), 3456, true).is_none());
        // No server on the port → nothing to refuse.
        assert!(login_guard_refusal(None, 3456, false).is_none());
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
}
