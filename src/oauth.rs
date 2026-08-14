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
///
/// `login_hint`, when present, is passed through to `claude.ai/oauth/authorize`
/// so the login page pre-selects that address — **ergonomics only**, never the
/// source of truth. Measured 2026-08-14 (σ3, N=1, clean incognito session): the
/// endpoint honors it and pre-fills the exact address passed. Untested (σ1):
/// whether it overrides an *already signed-in* browser session — precisely the
/// case `open_browser` produces by handing the URL to the default browser.
/// Because that case is unverified, the caller must never rely on the hint for
/// correctness; the identity assertion after `fetch_profile` returns is what
/// actually enforces the requested account, and depends on nothing external.
fn build_login_flow(redirect_uri: &str, login_hint: Option<&str>) -> anyhow::Result<LoginFlow> {
    let client = BasicClient::new(ClientId::new(CLIENT_ID.to_string()))
        .set_auth_uri(AuthUrl::new(AUTHORIZE_URL.to_string()).context("invalid authorize URL")?)
        .set_token_uri(TokenUrl::new(TOKEN_ENDPOINT.to_string()).context("invalid token URL")?)
        .set_redirect_uri(
            RedirectUrl::new(redirect_uri.to_string()).context("invalid redirect URI")?,
        );

    // oauth2 generates a random verifier and its S256 challenge together.
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();

    let mut request = client
        // 32-byte state, NOT the crate-default `new_random()` (16 bytes / 22
        // chars): Claude's authorize endpoint rejects the short state with
        // "Invalid request format" once a logged-in session validates the
        // request. 32 bytes (43 base64url chars) matches the working JS
        // reference (`randomBytes(32).toString('base64url')`).
        .authorize_url(|| CsrfToken::new_random_len(32))
        .add_scope(Scope::new(OAUTH_SCOPES.to_string()))
        .set_pkce_challenge(challenge)
        // Claude's authorize endpoint requires this non-standard flag.
        .add_extra_param("code", "true");
    if let Some(email) = login_hint {
        request = request.add_extra_param("login_hint", email);
    }
    let (auth_url, csrf) = request.url();

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
/// via [`identity::resolve`] — never a loose first-match — so the legacy
/// two-org tie ([`identity::same_identity_strict`]) is broken the same way
/// [`config::save_account`] and [`crate::manager::Manager::add_or_update_account`]
/// already break it, and an unbreakable tie is REFUSED rather than guessed:
/// guessing here means stamping a fresh credential over a DIFFERENT account's
/// single-use refresh token (see [`identity::Resolved::Many`]'s doc-comment).
/// An existing account with the same identity has its tokens refreshed in
/// place and any absent identity field backfilled (never duplicated);
/// otherwise a new account is appended with the next-highest priority and
/// every identity field populated.
///
/// Backward compatible: when the probe and the stored entry both lack identity
/// fields (today's real config), `identity::resolve` falls back to name
/// equality — so a single-org re-login matches its existing entry exactly as
/// before.
pub fn upsert_account(
    config: &mut Config,
    name: &str,
    tokens: &Tokens,
    account_uuid: Option<String>,
    org_uuid: Option<String>,
    org_name: Option<String>,
) -> anyhow::Result<()> {
    let probe = crate::identity::probe(
        name,
        account_uuid.clone(),
        org_uuid.clone(),
        org_name.clone(),
    );

    match crate::identity::resolve(config.accounts.iter().enumerate(), &probe) {
        crate::identity::Resolved::One(index) => {
            let account = &mut config.accounts[index];
            account.account_type = "oauth".to_string();
            account.access_token = tokens.access_token.clone();
            account.refresh_token = Some(tokens.refresh_token.clone());
            account.expires_at = Some(tokens.expires_at_ms);
            // Backfill any identity field the stored entry was missing (e.g. a
            // legacy pre-org entry newly profiled), without overwriting known
            // values.
            if account.account_uuid.is_none() {
                account.account_uuid = account_uuid;
            }
            if account.org_uuid.is_none() {
                account.org_uuid = org_uuid;
            }
            if account.org_name.is_none() {
                account.org_name = org_name;
            }
            Ok(())
        }
        crate::identity::Resolved::None => {
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
            Ok(())
        }
        // Recompute the loose set for its names: `Resolved::Many` itself
        // carries none, and this is exactly the set `resolve` drew from (same
        // predicate, same candidates) — mirrors
        // `Manager::add_or_update_account`'s identical recomputation.
        crate::identity::Resolved::Many => {
            let candidates: Vec<String> = config
                .accounts
                .iter()
                .filter(|a| crate::identity::same_identity(a, &probe))
                .map(|a| a.name.clone())
                .collect();
            bail!("{}", crate::cli::ambiguous_query_message(name, &candidates));
        }
    }
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
    /// No live proxy on the port at all — the historical file-only login,
    /// unchanged. Also reached under `--force` when the probe could not
    /// confirm a safe live route (no route, or an unusable answer):
    /// `--force` is the escape hatch PAST A REFUSAL, never a way to skip a
    /// live route that IS there — `Live` below takes priority over it. An
    /// api-key rejection (`AddCapability::Unauthorized`) never resolves to
    /// this, under `--force` or otherwise: that is positive evidence the
    /// proxy is alive, so there is no unsafe file write for `--force` to
    /// rescue — see `login_route`'s `Unauthorized` arm.
    File,
    /// A live proxy is there and its add route is confirmed: route the
    /// finished credential through it instead of the file. Wins even against
    /// `--force` — forcing the unsafe file write only makes sense when the
    /// safe live route is not available.
    Live,
    /// A live proxy is there without the route, and `--force` was not given:
    /// refuse outright with this message, unchanged from before this route
    /// existed.
    Refuse(String),
}

/// The pure login-guard DECISION: given whether the HTTP capability probe
/// already confirmed a live account-add route ([`probe_add_capability`], run
/// impure and BEFORE this), any live proxy separately detected on the port
/// (the impure lsof/owner-file detection,
/// [`crate::singleton::live_proxy_server`]), the port, and the `--force`
/// flag, return which [`LoginRoute`] `login()` should take. Split from both
/// impure detectors so the decision itself stays unit-testable, mirroring
/// singleton's pure-decision / impure-executor split.
///
/// `has_add_route` is decided FIRST and is decisive on its own, independent
/// of `incumbent` — including when `incumbent` is `None`. The HTTP probe is
/// first-hand evidence that a live proxy with the route is answering on this
/// exact port; `incumbent` is an INFERENCE from a pid, an argv string, or an
/// owner-file claim, any of which can be stale, absent, or blind to a host
/// application the pid/argv matcher was never taught to recognize (see
/// `ProxyHost::Unknown`'s doc-comment). Gating the live path on `incumbent`
/// agreeing would mean any future regression in that inference silently
/// reintroduces the whole-file clobber this unit exists to remove — as a
/// silent fallback rather than a loud one. So a confirmed route wins even
/// against a `None` incumbent, and `incumbent` is consulted only to build the
/// REFUSAL message, for the one case where the probe did NOT confirm the
/// route.
///
/// The incumbent's KIND decides the refusal instruction, and it is not
/// cosmetic. Since the owner file, detection reaches a proxy served INSIDE a
/// host application, and the pid reported is then the HOST's. "kill {pid}"
/// would SIGTERM a GUI process that installs no handler for it: no
/// `applicationWillTerminate`, no final session→account pin write, and every
/// live session cold-starts its prompt cache at the next boot.
/// `takeover_decision` and `incumbents_to_signal` are both hardened against
/// exactly that signal; a message that ADVISES it would walk around both. So
/// an embedded incumbent is told to be quit, not killed.
fn login_guard_refusal(
    incumbent: Option<singleton::Incumbent>,
    has_add_route: bool,
    port: u16,
    force: bool,
) -> LoginRoute {
    // `has_add_route` is checked BEFORE `force`, deliberately: a confirmed
    // route wins even against the escape hatch. Checking `force` first (the
    // pre-fix order) meant `--force` chose the file write even when the live
    // route was confirmed safe — the exact whole-file clobber this unit
    // exists to remove, reintroduced by the escape hatch meant to route
    // around a REFUSAL, not around a safe route that was never refused.
    if has_add_route {
        return LoginRoute::Live;
    }
    if force {
        return LoginRoute::File;
    }
    match incumbent {
        Some(incumbent) => {
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
        None => LoginRoute::File,
    }
}

/// What a live proxy said, structurally, about whether it can accept a live
/// account add. Read from [`crate::cli::post_add_account`]'s own
/// [`crate::cli::LiveControlError`] classification, which is itself driven by
/// [`crate::proxy::ENDPOINT_HEADER`] — never by matching error text.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AddCapability {
    /// The route answered (stamped, whatever the status) — a live add is
    /// possible.
    Present,
    /// A connection could not even be made — genuinely nothing is listening
    /// on the port. The ONLY outcome that may resolve to `LoginRoute::File`
    /// without `--force` and without `incumbent` separately confirming a
    /// server: there is truly no live process to disagree with the file.
    Absent,
    /// The proxy answered and rejected our api-key. Its own condition — never
    /// folded into `Absent`: that would either wrongly read as an older proxy
    /// with no route, or let `login()` silently fall back to writing the file
    /// out from under a server that is right there, listening.
    Unauthorized,
    /// Something holds the port and is either an older proxy answering
    /// without the route (identified structurally, never by error text) or
    /// accepted the connection and then produced nothing usable before the
    /// deadline — the wedged-port shape `cli::probe_incumbent`'s own tests
    /// document as measured and real (`a_listener_that_never_answers_is_silent_not_answering`).
    /// Both are first-hand evidence a live process holds the port — at least
    /// as strong as the `incumbent` pid heuristic, and stronger exactly where
    /// that heuristic is documented to miss (an embedded host). Split out of
    /// `Absent` because the two are NOT the same conservative answer: folding
    /// them together let a `None` incumbent silently choose `File` beside a
    /// real, wedged server, without `--force` ever being asked for. Carries
    /// the probe's own account of what happened, for the refusal message.
    Unusable(String),
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
        // A connection could not be made at all — the ONLY genuinely-absent
        // case.
        Err(crate::cli::LiveControlError::NoServer) => AddCapability::Absent,
        // Everything else (`NoRoute`, `NoAnswer`, `Unusable`) is first-hand
        // evidence something holds the port, just not usably.
        Err(other) => AddCapability::Unusable(other.why()),
    }
}

/// Resolve which [`LoginRoute`] `login()` should take. Runs the impure HTTP
/// capability probe UNCONDITIONALLY — including under `--force` — deliberately
/// never gated on whether [`crate::singleton::live_proxy_server`]
/// (`incumbent`) noticed anything on the port first, NOR on `force`: skipping
/// the probe under `--force` is exactly the bug bridge item C removed, since
/// it is what let `--force` take the file path even when the live route was
/// confirmed and safe. `force` only changes what happens when the probe
/// itself produced an UNUSABLE answer (something holds the port but is not
/// confirmed safe) — that case falls back to `LoginRoute::File` under
/// `--force` instead of erroring out, mirroring the historical "force always
/// succeeds via the file" behaviour. An api-key rejection (`Unauthorized`) is
/// a DIFFERENT case, and `--force` never rescues it: it is positive evidence
/// of a live, healthy proxy — it answered, parsed our request, and rejected
/// the key — which makes it the worst-informed moment to whole-file-write
/// beside it, since a live server rotating tokens is exactly what makes that
/// write destructive. Fully resolves before `login()` starts the OAuth
/// dance, so a user is never told to stop a server AFTER completing a full
/// browser round-trip, and never silently falls back to the file when a live
/// proxy answered but rejected our api-key.
async fn login_route(
    config_path: &Path,
    incumbent: Option<singleton::Incumbent>,
    port: u16,
    force: bool,
) -> anyhow::Result<LoginRoute> {
    let config = load_or_default(config_path)?;
    match probe_add_capability(&config).await {
        // Positive evidence the proxy is alive: it answered, parsed our
        // request, and rejected the key. Refuses regardless of `--force` —
        // unlike `Unusable` below, there is no unsafe-but-forceable file
        // write to fall back to here, only a safe path one config edit away.
        AddCapability::Unauthorized => {
            bail!(
                "the proxy on :{port} rejected the api-key in {} while checking whether it \
                 could take a live login — no browser was opened and nothing was changed. This \
                 refuses even under --force: a rejected api-key is proof the proxy is alive, \
                 which makes this the worst-informed moment to write the config file beside it. \
                 Fix `proxy.apiKey` in the config to match the running server, or stop the \
                 server and log in offline.",
                config_path.display()
            );
        }
        AddCapability::Present => Ok(login_guard_refusal(incumbent, true, port, force)),
        AddCapability::Absent => Ok(login_guard_refusal(incumbent, false, port, force)),
        // Bridge item D: something holds the port but answered unusably —
        // first-hand evidence at least as strong as `incumbent`, so this
        // refuses REGARDLESS of what `incumbent` separately concluded, never
        // falling through to `LoginRoute::File` the way folding this into
        // `Absent` used to when `incumbent` was `None`.
        AddCapability::Unusable(why) => {
            if force {
                return Ok(LoginRoute::File);
            }
            bail!(
                "the proxy on :{port} answered but not usably ({why}) while checking whether \
                 it could take a live login — no browser was opened and nothing was changed. \
                 This is first-hand evidence something holds the port even though process \
                 detection may have missed it, so the historical whole-file-write hazard still \
                 applies. Re-run with --force to log in via the file instead (this still risks \
                 the running server overwriting it on its next token refresh)."
            );
        }
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
pub async fn login(
    config_path: &Path,
    force: bool,
    account: Option<&str>,
) -> anyhow::Result<String> {
    let port = login_target_port(config_path);
    let incumbent = singleton::live_proxy_server(port);
    let route = login_route(config_path, incumbent, port, force).await?;
    if let LoginRoute::Refuse(msg) = route {
        bail!("{}", msg);
    }

    // Resolve --account early, ONLY to compute the login_hint (ergonomics —
    // see build_login_flow's doc-comment). Fail fast here on a typo'd or
    // ambiguous name rather than after a full browser round trip. The SAME
    // resolution runs again below, against a freshly loaded config, where it
    // is load-bearing rather than a convenience.
    let hint = match account {
        Some(query) => {
            let probe_config = load_or_default(config_path)?;
            let idx = crate::cli::resolve_account(&probe_config.accounts, query, None)?;
            Some(crate::identity::email_of(&probe_config.accounts[idx].name).to_string())
        }
        None => None,
    };

    // Bind the callback server on a random loopback port (127.0.0.1 only).
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("bind local OAuth callback server")?;
    let callback_port = listener.local_addr()?.port();
    let redirect_uri = format!("http://localhost:{callback_port}/callback");

    // PKCE + CSRF state + authorize URL, built by the oauth2 crate.
    let flow = build_login_flow(&redirect_uri, hint.as_deref())?;
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

    finish_login_checked(
        config_path,
        route,
        &mut config,
        &name,
        &tokens,
        profile,
        account,
    )
    .await
}

/// [`finish_login`], but first asserting the requested `--account` identity
/// when one was given. Split out so the assertion + write sequence `login()`
/// runs is directly testable without a real browser or network round trip —
/// exactly the reason [`finish_login`] itself was split from [`login`].
async fn finish_login_checked(
    config_path: &Path,
    route: LoginRoute,
    config: &mut Config,
    name: &str,
    tokens: &Tokens,
    profile: Profile,
    requested_account: Option<&str>,
) -> anyhow::Result<String> {
    if let Some(query) = requested_account {
        assert_requested_identity(config, query, &profile)?;
    }
    finish_login(config_path, route, config, name, tokens, profile).await
}

/// The load-bearing half of `tcr login --account <query>`: resolve `query`
/// against `config` via [`crate::cli::resolve_account`] — the exact
/// exact-name-then-email-then-org rule `tcr enable`/`tcr disable` and TcrBar's
/// row buttons already rely on, not a second copy of it — and refuse to
/// proceed unless the identity that came back from `fetch_profile` is the
/// SAME account. `account_uuid` is preferred when both sides have one (an
/// email can be reused across orgs, a UUID cannot); otherwise email is
/// compared. Called strictly BEFORE any write path
/// ([`finish_login`]/[`persist_via_file`]/[`persist_via_account`]): on
/// mismatch, or on an unresolvable requested account, or on a profile that
/// carries neither an email nor a uuid (`fetch_profile`'s all-`None` failure
/// return, `src/oauth.rs:589-592` above — an assertion that cannot be
/// evaluated must fail closed, not pass by default), nothing is written.
fn assert_requested_identity(
    config: &Config,
    query: &str,
    profile: &Profile,
) -> anyhow::Result<()> {
    let idx = crate::cli::resolve_account(&config.accounts, query, None)?;
    let requested = &config.accounts[idx];

    if profile.email.is_none() && profile.account_uuid.is_none() {
        bail!(
            "could not confirm the identity that just authenticated (the profile carried no \
             email and no account id) — refusing to touch '{}'. Nothing was changed.",
            requested.name
        );
    }

    let matches = match (&requested.account_uuid, &profile.account_uuid) {
        (Some(want), Some(got)) => want == got,
        _ => {
            let want_email = crate::identity::email_of(&requested.name);
            profile
                .email
                .as_deref()
                .is_some_and(|got| got == want_email)
        }
    };

    if !matches {
        let got = profile
            .email
            .as_deref()
            .unwrap_or("an account with no email in its profile");
        bail!(
            "requested re-login for '{}', but the browser authenticated as '{got}' instead — \
             nothing was written. Sign out of that account in the browser first, or use a \
             private/incognito window, then retry `tcr login --account {}`.",
            requested.name,
            query
        );
    }

    Ok(())
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
    )?;
    config::save(config_path, config).context("save config after login")?;
    println!("Saved account '{name}' to {}", config_path.display());
    Ok(name.to_string())
}

/// The surgical half of a live-route fallback — [`config::save_account`]'s
/// single-account read-modify-write — used when [`probe_add_capability`]
/// confirmed the route moments earlier but [`crate::cli::post_add_account`]
/// itself then could not apply it (the route disappeared, or the proxy
/// stopped answering before its 5s deadline). Bridge item A: this replaces
/// [`persist_via_file`] on exactly those two arms of [`finish_login`].
///
/// Unlike `persist_via_file`, this never touches any account row but
/// `account`'s own, and it is safe to call even though `account` was built
/// from data gathered before this call's own async window (the profile
/// fetch, the possible stdin prompt, the up-to-5s `post_add_account`
/// timeout): `config::save_account` re-reads the config document FRESH from
/// disk and resolves identity itself, rather than whole-file-saving the
/// (possibly already-stale) in-memory `config` this process is holding — so
/// there is no stale snapshot for it to clobber a concurrent server-side
/// rotation with. This is the same write the server's own persist path uses.
fn persist_via_account(config_path: &Path, account: &Account) -> anyhow::Result<String> {
    let outcome = config::save_account(config_path, account).context("save account after login")?;
    account_write_result(config_path, account, outcome)
}

/// Turn a [`config::AccountWrite`] outcome into `persist_via_account`'s return
/// value, shared with [`persist_via_account_or_file`] so the two callers agree
/// on the exact same success/refusal wording rather than drifting apart.
fn account_write_result(
    config_path: &Path,
    account: &Account,
    outcome: config::AccountWrite,
) -> anyhow::Result<String> {
    match outcome {
        config::AccountWrite::Added | config::AccountWrite::Updated => {
            println!(
                "Saved account '{}' to {}",
                account.name,
                config_path.display()
            );
            Ok(account.name.clone())
        }
        config::AccountWrite::Ambiguous => bail!(
            "'{}' matches more than one account already in {} — narrow with --org or use an \
             exact name. Nothing was changed.",
            account.name,
            config_path.display()
        ),
        config::AccountWrite::Unwritable => bail!(
            "the accounts key in {} is not a JSON array — refusing to write. Nothing was \
             changed.",
            config_path.display()
        ),
    }
}

/// [`persist_via_account`], but falling back to [`persist_via_file`] when
/// there is no file yet for `save_account`'s `read_document` to read —
/// `fs::read_to_string` cannot create a missing file, and a first-ever login
/// must still be able to. Used by `finish_login`'s `NoServer` arm, which
/// otherwise carries the exact same clobber risk `NoRoute` and the
/// timeout/`Unusable` arm already write surgically around: the config
/// snapshot in `config` was taken before the profile fetch, the possible
/// stdin prompt, and the live-add round-trip, so a whole-file write here
/// would revert any rotation a server that exited moments ago made to any
/// OTHER account inside that window.
fn persist_via_account_or_file(
    config_path: &Path,
    config: &mut Config,
    name: &str,
    tokens: &Tokens,
    profile: Profile,
    account: &Account,
) -> anyhow::Result<String> {
    match config::save_account(config_path, account) {
        Err(config::ConfigError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
            persist_via_file(config_path, config, name, tokens, profile)
        }
        result => account_write_result(
            config_path,
            account,
            result.context("save account after login")?,
        ),
    }
}

/// Persist the finished OAuth credential per `route`: through the live proxy
/// when it is [`LoginRoute::Live`], via [`persist_via_file`] when it is
/// [`LoginRoute::File`]. Split out of [`login`] so WHERE the credential lands
/// is testable without driving a real browser.
///
/// On the live path the running server owns the durable write — this must
/// NOT also whole-file [`config::save`] the (possibly already-stale) `config`
/// this process is holding; that write is exactly the clobber this unit
/// exists to remove. The quiet [`crate::cli::LiveControlError::NoServer`]
/// case carries that SAME clobber risk, not a lesser one: the proxy the
/// earlier capability probe confirmed alive can have exited any time in the
/// profile-fetch / stdin-prompt / round-trip window since, so any OTHER
/// account it rotated in that window is still on disk waiting to be
/// reverted. It therefore routes through [`persist_via_account_or_file`]'s
/// surgical write exactly like `NoRoute`/`Unusable` below, never straight to
/// a whole-file one. A server that answered and REFUSED us (`Unauthorized`,
/// `Rejected`) must surface, never silently write the file instead, because
/// the two halves would then disagree about what the account's credentials
/// are.
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
                // and here). NOT the safe "offline the whole time" case
                // `persist_via_file` is for: a proxy WAS confirmed alive
                // moments ago, and it can have rotated any OTHER account's
                // tokens on disk any time in the profile-fetch / stdin-prompt
                // / round-trip window since. Same surgical write as
                // `NoRoute`/`Unusable` below, falling back to a whole-file
                // write only when there is no file yet at all.
                Err(crate::cli::LiveControlError::NoServer) => persist_via_account_or_file(
                    config_path,
                    config,
                    name,
                    tokens,
                    profile,
                    &account,
                ),
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
                // Bridge item A: this is reached with a CONFIRMED-live server
                // on the other end of the port, seconds after the config
                // snapshot above was loaded (a profile fetch, possibly an
                // unbounded stdin prompt, then this call itself came between)
                // — so a whole-file `persist_via_file` here would revert any
                // edit the live server made to any OTHER account in that
                // window. `persist_via_account` writes only this one row,
                // re-reading the file fresh instead of trusting the stale
                // snapshot. It is still HALF a login — say so loudly,
                // mirroring `set_enabled`'s NoRoute arm.
                Err(crate::cli::LiveControlError::NoRoute) => {
                    let saved = persist_via_account(config_path, &account)?;
                    eprintln!(
                        "[tcr] WARNING: the proxy running on :{} is too old to accept a live login (no {} route), so the account was written to {} while the proxy is running — it will not see this account until it restarts. Run `tcr restart` when a cold prompt cache is acceptable.",
                        config.proxy.port,
                        crate::proxy::ADD_ACCOUNT_PATH,
                        config_path.display(),
                    );
                    Ok(saved)
                }
                // It answered something unusable, or did not answer in time
                // (the wedged-port shape `cli::probe_incumbent`'s own tests
                // document as measured and real — the probe confirmed the
                // route seconds ago and `post_add_account` then hit its own
                // 5s deadline). Same shape of consequence as `NoRoute`, same
                // surgical write for the same reason, different cause, and
                // equally never silent.
                Err(other) => {
                    let why = other.why();
                    let saved = persist_via_account(config_path, &account)?;
                    eprintln!(
                        "[tcr] WARNING: could not apply this login to the proxy running on :{} ({why}), so the account was written to {} while the proxy is running — it may not see this account until it restarts.",
                        config.proxy.port,
                        config_path.display(),
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
        let flow = build_login_flow("http://localhost:12345/callback", None).unwrap();
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
        )
        .unwrap();

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
        )
        .unwrap();

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
        )
        .unwrap();

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
        )
        .unwrap();
        upsert_account(
            &mut config,
            "me@example.com",
            &tokens("at-pers", "rt-pers", 200),
            Some("uuid-person".into()),
            Some("org-personal".into()),
            Some("Personal".into()),
        )
        .unwrap();

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
        )
        .unwrap();

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

    /// THE MEASURED FAILURE bridge item B exists to fix: entries
    /// `[{u1, org-a, at-ORG-A/rt-ORG-A}, {u1, no org, at-LEGACY/rt-LEGACY}]`
    /// with a login identity `{u1, no org}`. Under the old `.find()`
    /// first-match-wins, `same_identity` matches BOTH rows — a legacy no-org
    /// entry loosely matches every org of that person — so `.find()` silently
    /// took index 0, stamping the fresh credential over org-a's row and
    /// destroying its single-use refresh token. `identity::resolve` exists
    /// exactly to break this tie via `same_identity_strict`: only the legacy
    /// row (also org-less) is a STRICT match, so the target resolves to it.
    #[test]
    fn upsert_resolves_the_legacy_two_org_tie_onto_the_legacy_row() {
        let mut config: Config = serde_json::from_str(
            r#"{ "accounts": [
                { "name": "me@example.com", "type": "oauth", "accountUuid": "u1",
                  "orgUuid": "org-a", "orgName": "Org A",
                  "accessToken": "at-ORG-A", "refreshToken": "rt-ORG-A",
                  "expiresAt": 100, "priority": 0 },
                { "name": "me@example.com", "type": "oauth", "accountUuid": "u1",
                  "accessToken": "at-LEGACY", "refreshToken": "rt-LEGACY",
                  "expiresAt": 100, "priority": 1 }
            ] }"#,
        )
        .unwrap();

        upsert_account(
            &mut config,
            "me@example.com",
            &tokens("at-NEW", "rt-NEW", 999),
            Some("u1".to_string()),
            None,
            None,
        )
        .unwrap();

        assert_eq!(config.accounts.len(), 2, "no new row appended");
        assert_eq!(
            config.accounts[0].access_token, "at-ORG-A",
            "org-a's row must be untouched: {:?}",
            config.accounts[0]
        );
        assert_eq!(
            config.accounts[0].refresh_token.as_deref(),
            Some("rt-ORG-A"),
            "org-a's refresh token is single-use — clobbering it bricks the account"
        );
        assert_eq!(
            config.accounts[1].access_token, "at-NEW",
            "the legacy row (also org-less) is the strict match, not org-a"
        );
        assert_eq!(config.accounts[1].refresh_token.as_deref(), Some("rt-NEW"));
    }

    /// A genuine, unbreakable tie — two rows sharing the exact same identity —
    /// refuses rather than guesses, and writes NOTHING: the config must be
    /// byte-for-byte unchanged after the refusal.
    #[test]
    fn upsert_refuses_and_writes_nothing_on_an_unbreakable_tie() {
        let mut config: Config = serde_json::from_str(
            r#"{ "accounts": [
                { "name": "me@example.com", "type": "oauth", "accountUuid": "u1",
                  "orgUuid": "org-a", "orgName": "Org A",
                  "accessToken": "at-1", "refreshToken": "rt-1",
                  "expiresAt": 100, "priority": 0 },
                { "name": "me@example.com", "type": "oauth", "accountUuid": "u1",
                  "orgUuid": "org-a", "orgName": "Org A",
                  "accessToken": "at-2", "refreshToken": "rt-2",
                  "expiresAt": 100, "priority": 1 }
            ] }"#,
        )
        .unwrap();
        let before = serde_json::to_value(&config).unwrap();

        let err = upsert_account(
            &mut config,
            "me@example.com",
            &tokens("at-NEW", "rt-NEW", 999),
            Some("u1".to_string()),
            Some("org-a".to_string()),
            Some("Org A".to_string()),
        )
        .expect_err("an unbreakable tie must refuse rather than guess");
        assert!(err.to_string().contains("ambiguous"), "{err}");

        assert_eq!(
            serde_json::to_value(&config).unwrap(),
            before,
            "nothing was written on refusal"
        );
    }

    // --- authorize URL ------------------------------------------------------

    #[test]
    fn authorize_url_carries_required_params() {
        // The oauth2-built authorize URL must carry every required OAuth param
        // plus Claude's non-standard `code=true`.
        let url = build_login_flow("http://localhost:12345/callback", None)
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
        // No --account given: no login_hint at all (add-a-new-account path).
        assert!(!url.contains("login_hint"));
    }

    #[test]
    fn authorize_url_carries_login_hint_when_given() {
        let url = build_login_flow("http://localhost:12345/callback", Some("alice@example.com"))
            .unwrap()
            .auth_url;
        assert!(url.contains("login_hint=alice%40example.com"));
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

    /// THE BRIDGE ITEM C FIX: `--force` overrides only the `Refuse` outcome,
    /// never a confirmed-safe `Live` route. The pre-fix code checked `force`
    /// BEFORE `has_add_route`, so `--force` took the file path even when the
    /// live route was confirmed and safe — the exact bug this unit exists to
    /// remove, self-inflicted by the escape hatch. Nobody wants the file
    /// write *because* it is a file write; they want the login not refused.
    #[test]
    fn login_guard_prefers_live_over_force_when_the_route_is_confirmed() {
        assert_eq!(
            login_guard_refusal(incumbent(singleton::ProxyKind::Tcr), true, 3456, true),
            LoginRoute::Live,
            "--force must not choose the file write when the live route IS available and safe"
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

    /// THE CASE THE PROBE-FIRST ORDERING EXISTS FOR: the HTTP probe confirmed
    /// the route, but pid/argv/owner-file detection found NOTHING on the port
    /// (`incumbent: None`) — a host application `singleton::classify_proxy_server`
    /// was never taught to recognize, or a stale/unreadable owner file. The
    /// probe is first-hand evidence and must win on its own; requiring
    /// `incumbent` to agree would silently fall back to `File` — the
    /// whole-file clobber this unit exists to remove — the moment that
    /// unrelated detection heuristic misses.
    #[test]
    fn login_guard_proceeds_live_on_a_confirmed_route_even_when_incumbent_detection_missed_it() {
        assert_eq!(
            login_guard_refusal(None, true, 3456, false),
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

    /// Bridge item D: an "older tcr" — something answers, but not on this
    /// route at all (no `ENDPOINT_HEADER`, unlike every response this crate's
    /// own router produces, even its 404s and 405s) — is first-hand evidence
    /// a live process holds the port. It must NOT collapse into `Absent`
    /// (renamed from this test's old expectation): folding it in there is
    /// exactly what let `login_route` silently choose `File` beside a real,
    /// answering-but-old server whenever pid detection missed it.
    #[tokio::test]
    async fn probe_add_capability_unstamped_404_is_unusable_not_absent() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, axum::Router::new()).await;
        });
        let config: Config =
            serde_json::from_str(&format!(r#"{{ "proxy": {{ "port": {port} }} }}"#)).unwrap();
        assert!(matches!(
            probe_add_capability(&config).await,
            AddCapability::Unusable(_)
        ));
    }

    /// The genuinely-absent case, for contrast with the test above: nothing
    /// is bound to the port at all, so the connection is refused outright.
    /// This is the ONLY shape that may still resolve to `LoginRoute::File`
    /// without `--force` and without `incumbent` separately confirming a
    /// server.
    #[tokio::test]
    async fn probe_add_capability_connection_refused_is_absent() {
        let dead_port = {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            listener.local_addr().unwrap().port()
        };
        let config: Config =
            serde_json::from_str(&format!(r#"{{ "proxy": {{ "port": {dead_port} }} }}"#)).unwrap();
        assert_eq!(probe_add_capability(&config).await, AddCapability::Absent);
    }

    /// The WEDGED shape `cli::probe_incumbent`'s own tests document as
    /// measured and real (`a_listener_that_never_answers_is_silent_not_answering`):
    /// bound, the connect succeeds off the kernel backlog, and nothing is
    /// ever written back. Costs the probe's own 5s deadline by construction.
    #[tokio::test]
    async fn probe_add_capability_wedged_port_is_unusable() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config: Config =
            serde_json::from_str(&format!(r#"{{ "proxy": {{ "port": {port} }} }}"#)).unwrap();

        assert!(matches!(
            probe_add_capability(&config).await,
            AddCapability::Unusable(_)
        ));
        drop(listener);
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

    // --- login_route ----------------------------------------------------------

    /// THE INTEGRATION-LEVEL VERSION of
    /// `login_guard_proceeds_live_on_a_confirmed_route_even_when_incumbent_detection_missed_it`:
    /// a real live server answering the real route, called with `incumbent:
    /// None` (simulating a pid/argv/owner-file detection MISS — the TcrBar
    /// case, where `argv[0]` ends in `/TcrBar` and
    /// `singleton::classify_proxy_server` recognizes nothing). `login_route`
    /// must still probe and proceed live — it must never gate the probe
    /// itself on `incumbent.is_some()`, which is exactly the shape of bug
    /// this test catches: skipping the probe here would fall through to
    /// `LoginRoute::File` beside a real, answering server.
    #[tokio::test]
    async fn login_route_proceeds_live_even_when_incumbent_detection_missed_a_real_server() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-route-missed-incumbent-{}-{port}.json",
            std::process::id()
        ));
        let seed = format!(r#"{{ "proxy": {{ "port": {port} }} }}"#);
        std::fs::write(&path, &seed).unwrap();

        let server_config: Config = serde_json::from_str(&seed).unwrap();
        let manager = crate::manager::Manager::with_live_refresher(server_config, None);
        tokio::spawn(async move { crate::mitm::serve(listener, manager, None).await });

        let route = login_route(&path, None, port, false)
            .await
            .expect("a confirmed route must not error");
        assert_eq!(
            route,
            LoginRoute::Live,
            "the probe must decide this on its own, independent of incumbent detection"
        );
        std::fs::remove_file(&path).ok();
    }

    /// Bridge item C, at the `login_route` level: `--force` must still probe
    /// and still prefer `Live` when a real server confirms the route — never
    /// skip straight to `File` the way the pre-fix early-return did.
    #[tokio::test]
    async fn login_route_prefers_live_over_force_when_a_real_server_confirms_the_route() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-route-force-live-{}-{port}.json",
            std::process::id()
        ));
        let seed = format!(r#"{{ "proxy": {{ "port": {port} }} }}"#);
        std::fs::write(&path, &seed).unwrap();

        let server_config: Config = serde_json::from_str(&seed).unwrap();
        let manager = crate::manager::Manager::with_live_refresher(server_config, None);
        tokio::spawn(async move { crate::mitm::serve(listener, manager, None).await });

        let route = login_route(&path, None, port, true)
            .await
            .expect("a confirmed route under --force must not error");
        assert_eq!(
            route,
            LoginRoute::Live,
            "--force must still probe and prefer Live when the route is confirmed"
        );
        std::fs::remove_file(&path).ok();
    }

    /// Bridge item D, at the `login_route` level: a wedged port — something
    /// answers the TCP connect and then never responds — must refuse even
    /// when `incumbent` is `None` (pid/argv detection missed it entirely, the
    /// TcrBar shape). Before this fix, `AddCapability::Absent` covered this
    /// case too, and `login_guard_refusal`'s `None => LoginRoute::File` arm
    /// let it through with no `--force` needed at all.
    #[tokio::test]
    async fn login_route_refuses_a_wedged_port_even_when_incumbent_detection_missed_it() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-route-wedged-{}-{port}.json",
            std::process::id()
        ));
        std::fs::write(&path, format!(r#"{{ "proxy": {{ "port": {port} }} }}"#)).unwrap();

        let err = login_route(&path, None, port, false)
            .await
            .expect_err("a wedged port must refuse, not silently choose File");
        assert!(err.to_string().contains("--force"), "{err}");

        std::fs::remove_file(&path).ok();
        drop(listener);
    }

    /// The escape hatch still works past a wedged-port refusal: `--force`
    /// resolves to `File` rather than erroring out.
    #[tokio::test]
    async fn login_route_force_resolves_to_file_past_a_wedged_port_refusal() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-route-wedged-force-{}-{port}.json",
            std::process::id()
        ));
        std::fs::write(&path, format!(r#"{{ "proxy": {{ "port": {port} }} }}"#)).unwrap();

        let route = login_route(&path, None, port, true)
            .await
            .expect("--force must still resolve past a wedged-port refusal");
        assert_eq!(route, LoginRoute::File);

        std::fs::remove_file(&path).ok();
        drop(listener);
    }

    /// THE OVERRULE: `--force` is the escape hatch past a wedged-port
    /// refusal (above), but it must NOT be one past `AddCapability::
    /// Unauthorized`. A rejected api-key is positive evidence the proxy is
    /// alive and healthy — the worst-informed moment to fall back to a
    /// whole-file write beside it — so `login_route` must keep refusing even
    /// under `--force`, and nothing along the way may touch the config file.
    #[tokio::test]
    async fn login_route_refuses_unauthorized_even_under_force() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server_config: Config = serde_json::from_str(&format!(
            r#"{{ "proxy": {{ "port": {port}, "apiKey": "correct-key" }} }}"#
        ))
        .unwrap();
        let manager = crate::manager::Manager::with_live_refresher(server_config, None);
        tokio::spawn(async move { crate::mitm::serve(listener, manager, None).await });

        // The file `login_route` reads carries the WRONG key.
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-route-unauthorized-force-{}-{port}.json",
            std::process::id()
        ));
        let seed = format!(r#"{{ "proxy": {{ "port": {port}, "apiKey": "wrong-key" }} }}"#);
        std::fs::write(&path, &seed).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let err = login_route(&path, None, port, true)
            .await
            .expect_err("--force must not rescue a rejected api-key");
        assert!(err.to_string().contains("api-key"), "{err}");
        assert!(
            err.to_string().contains("even under --force"),
            "the message must say --force does not apply here: {err}"
        );

        assert_eq!(
            before,
            std::fs::read_to_string(&path).unwrap(),
            "the file must be byte-identical — --force must not trigger a write here"
        );
        std::fs::remove_file(&path).ok();
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

    // --- `--account` identity assertion (`tcr login --account`) ------------

    fn two_account_seed() -> String {
        r#"{ "accounts": [
            { "name": "alice@example.com", "type": "oauth", "accessToken": "at-alice",
              "refreshToken": "rt-alice", "expiresAt": 1893456000000, "priority": 0 },
            { "name": "bob@example.com", "type": "oauth", "accessToken": "at-bob",
              "refreshToken": "rt-bob", "expiresAt": 1893456000000, "priority": 1 }
        ] }"#
            .to_string()
    }

    /// A mismatch between the requested account and the identity that came
    /// back writes NOTHING — the config file is byte-identical afterwards —
    /// and the error names both sides.
    #[tokio::test]
    async fn finish_login_checked_mismatch_writes_nothing() {
        let seed = two_account_seed();
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-account-mismatch-{}-{}.json",
            std::process::id(),
            line!()
        ));
        std::fs::write(&path, &seed).unwrap();
        let mut config = load_or_default(&path).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let err = finish_login_checked(
            &path,
            LoginRoute::File,
            &mut config,
            "alice@example.com",
            &tokens("at-new", "rt-new", 1_893_456_000_000),
            profile_named("mallory@example.com"),
            Some("alice@example.com"),
        )
        .await
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("alice@example.com"), "names requested: {msg}");
        assert!(msg.contains("mallory@example.com"), "names returned: {msg}");

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(before, after, "a mismatch must leave the config untouched");

        std::fs::remove_file(&path).ok();
    }

    /// A match writes normally, through the exact same `finish_login` path as
    /// today's no-`--account` flow.
    #[tokio::test]
    async fn finish_login_checked_match_writes_normally() {
        let seed = two_account_seed();
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-account-match-{}-{}.json",
            std::process::id(),
            line!()
        ));
        std::fs::write(&path, &seed).unwrap();
        let mut config = load_or_default(&path).unwrap();

        let name = finish_login_checked(
            &path,
            LoginRoute::File,
            &mut config,
            "alice@example.com",
            &tokens("at-fresh", "rt-fresh", 1_893_456_000_000),
            profile_named("alice@example.com"),
            Some("alice@example.com"),
        )
        .await
        .unwrap();
        assert_eq!(name, "alice@example.com");

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("rt-fresh"),
            "a matching identity must be written: {after}"
        );

        std::fs::remove_file(&path).ok();
    }

    /// `fetch_profile`'s all-`None` failure return (`src/oauth.rs:589-592`)
    /// must refuse rather than pass by default — an assertion that cannot be
    /// evaluated is not evidence of a match.
    #[tokio::test]
    async fn finish_login_checked_all_none_profile_refuses() {
        let seed = two_account_seed();
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-account-allnone-{}-{}.json",
            std::process::id(),
            line!()
        ));
        std::fs::write(&path, &seed).unwrap();
        let mut config = load_or_default(&path).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let all_none = Profile {
            email: None,
            account_uuid: None,
            org_uuid: None,
            org_name: None,
        };
        let err = finish_login_checked(
            &path,
            LoginRoute::File,
            &mut config,
            "alice@example.com",
            &tokens("at-new", "rt-new", 1_893_456_000_000),
            all_none,
            Some("alice@example.com"),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("alice@example.com"),
            "{}",
            err.to_string()
        );

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            before, after,
            "an unconfirmable identity must write nothing"
        );

        std::fs::remove_file(&path).ok();
    }

    /// An ambiguous `--account` query is an error, not a guess — same
    /// zero/one/many rule `crate::cli::resolve_account` already enforces for
    /// every other command.
    #[tokio::test]
    async fn finish_login_checked_ambiguous_query_is_an_error() {
        let seed = r#"{ "accounts": [
            { "name": "dup@example.com", "type": "oauth", "accessToken": "at-1",
              "refreshToken": "rt-1", "expiresAt": 1893456000000, "priority": 0,
              "orgUuid": "org-a", "orgName": "Corp A" },
            { "name": "dup@example.com", "type": "oauth", "accessToken": "at-2",
              "refreshToken": "rt-2", "expiresAt": 1893456000000, "priority": 1,
              "orgUuid": "org-b", "orgName": "Corp B" }
        ] }"#;
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-account-ambiguous-{}-{}.json",
            std::process::id(),
            line!()
        ));
        std::fs::write(&path, seed).unwrap();
        let mut config = load_or_default(&path).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let err = finish_login_checked(
            &path,
            LoginRoute::File,
            &mut config,
            "dup@example.com",
            &tokens("at-new", "rt-new", 1_893_456_000_000),
            profile_named("dup@example.com"),
            Some("dup@example.com"),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("more than one")
                || err.to_string().to_lowercase().contains("ambiguous"),
            "{}",
            err.to_string()
        );

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(before, after, "an ambiguous query must not guess and write");

        std::fs::remove_file(&path).ok();
    }

    /// No `--account` behaves exactly as before: no identity check runs at
    /// all, so an unrelated returned identity still writes normally (this is
    /// the add-a-new-account path, which must not grow an assertion it
    /// cannot satisfy).
    #[tokio::test]
    async fn finish_login_checked_no_account_skips_the_check() {
        let seed = two_account_seed();
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-account-none-{}-{}.json",
            std::process::id(),
            line!()
        ));
        std::fs::write(&path, &seed).unwrap();
        let mut config = load_or_default(&path).unwrap();

        let name = finish_login_checked(
            &path,
            LoginRoute::File,
            &mut config,
            "carol@example.com",
            &tokens("at-carol", "rt-carol", 1_893_456_000_000),
            profile_named("carol@example.com"),
            None,
        )
        .await
        .unwrap();
        assert_eq!(name, "carol@example.com");

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("carol@example.com"), "{after}");

        std::fs::remove_file(&path).ok();
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

    /// THE σ5 EXPERIMENT bridge item A calls for. The WEDGED shape
    /// `cli::probe_incumbent`'s own tests document as measured and real
    /// (`a_listener_that_never_answers_is_silent_not_answering`): bound, the
    /// connect succeeds off the kernel backlog, and nothing is ever written
    /// back — reached on the `Live` route when the capability probe confirmed
    /// the route seconds earlier and `post_add_account` then hits its own 5s
    /// deadline.
    ///
    /// Reproduces the same race `finish_login_live_path_does_not_clobber_a_concurrent_rotation`
    /// does — a stale client-side snapshot plus a write that lands on disk
    /// AFTER that snapshot was taken — but via `config::save_account` (the
    /// server's own persist primitive) directly, rather than a live HTTP
    /// round-trip: the port in THIS scenario is deliberately silent for the
    /// test's whole lifetime, so nothing can be routed through it. Before
    /// this fix, the `NoAnswer` arm fell through to `persist_via_file`'s
    /// WHOLE-FILE `config::save` of that now-stale snapshot, reverting
    /// alice's rotation — the exact hazard `finish_login` exists to remove,
    /// reintroduced in this one arm (watched failing directly: with that arm
    /// reverted to `persist_via_file`, this test fails with `left:
    /// "rt-alice" right: "rt-alice-ROTATED"`). Fixed, it lands through
    /// `config::save_account`'s surgical single-row write instead, so
    /// alice's rotated row survives and the new account is still added.
    #[tokio::test]
    async fn finish_login_live_timeout_does_not_clobber_other_accounts() {
        // Bound and never accepted: connections queue in the kernel backlog
        // and the connect succeeds, but nothing is ever written back —
        // `post_add_account` times out at its own 5s deadline.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-live-timeout-{}-{port}.json",
            std::process::id()
        ));
        let seed = format!(
            r#"{{ "proxy": {{ "port": {port} }}, "accounts": [
                {{ "name": "alice@example.com", "type": "oauth", "accessToken": "at-alice",
                  "refreshToken": "rt-alice", "expiresAt": 1893456000000, "priority": 0 }}
            ] }}"#
        );
        std::fs::write(&path, &seed).unwrap();

        // The stale snapshot: what `login()` would have loaded before its own
        // async window (profile fetch / stdin prompt) opened.
        let mut client_config = load_or_default(&path).unwrap();

        // A rotation lands on disk AFTER that snapshot was taken — the live
        // server's own persist path, used directly since the port itself is
        // silent in this scenario and cannot carry a real round-trip.
        let rotated_alice = Account {
            name: "alice@example.com".to_string(),
            account_type: "oauth".to_string(),
            account_uuid: None,
            org_uuid: None,
            org_name: None,
            access_token: "at-alice-ROTATED".to_string(),
            refresh_token: Some("rt-alice-ROTATED".to_string()),
            expires_at: Some(1_893_456_000_000),
            priority: None,
            switch_threshold: None,
            disabled: None,
            extra: serde_json::Map::new(),
        };
        config::save_account(&path, &rotated_alice).expect("the rotation must land on disk");
        let after_rotation = std::fs::read_to_string(&path).unwrap();
        assert!(
            after_rotation.contains("rt-alice-ROTATED"),
            "setup: the rotation must be on disk first: {after_rotation}"
        );

        let name = finish_login(
            &path,
            LoginRoute::Live,
            &mut client_config,
            "carol@example.com",
            &tokens("at-carol", "rt-carol", 1_893_456_000_000),
            profile_named("carol@example.com"),
        )
        .await
        .expect("a timed-out live add must still fall back to a file write");
        assert_eq!(name, "carol@example.com");

        let final_doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let accounts = final_doc["accounts"].as_array().unwrap();
        let alice = accounts
            .iter()
            .find(|a| a["name"] == "alice@example.com")
            .expect("alice must still be on disk");
        assert_eq!(
            alice["refreshToken"],
            serde_json::json!("rt-alice-ROTATED"),
            "the timed-out live add must NOT whole-file-write a stale snapshot back over \
             alice's rotation: {final_doc}"
        );
        assert!(
            accounts.iter().any(|a| a["name"] == "carol@example.com"),
            "carol was added: {final_doc}"
        );

        std::fs::remove_file(&path).ok();
        drop(listener);
    }

    /// THE σ5 EXPERIMENT for the `NoServer` arm specifically — same shape as
    /// `finish_login_live_path_does_not_clobber_a_concurrent_rotation` and
    /// `finish_login_live_timeout_does_not_clobber_other_accounts`, but for
    /// the arm those two do not reach: the proxy the capability probe found
    /// alive has exited entirely by the time `post_add_account` connects, so
    /// the connection is refused outright (`is_connect()`, not `is_timeout()`
    /// or a 404/405). Before this fix, this arm alone still called
    /// `persist_via_file` directly regardless of what any other test in this
    /// file exercises — a stale client-side snapshot whole-file-written over
    /// a rotation that landed on disk in the profile-fetch / stdin-prompt /
    /// round-trip window. Watched failing directly: with the `NoServer` arm
    /// reverted to `persist_via_file`, this test fails with `left:
    /// "rt-alice" right: "rt-alice-ROTATED"`.
    #[tokio::test]
    async fn finish_login_no_server_does_not_clobber_a_concurrent_rotation() {
        // Bind to grab a free port, then drop the listener immediately: it
        // was live a moment ago (mirroring the capability probe having
        // confirmed it), but by the time `finish_login` runs, nothing is
        // listening and a connect is refused outright.
        let dead_port = {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            listener.local_addr().unwrap().port()
        };
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-live-noserver-clobber-{}-{dead_port}.json",
            std::process::id()
        ));
        let seed = format!(
            r#"{{ "proxy": {{ "port": {dead_port} }}, "accounts": [
                {{ "name": "alice@example.com", "type": "oauth", "accessToken": "at-alice",
                  "refreshToken": "rt-alice", "expiresAt": 1893456000000, "priority": 0 }}
            ] }}"#
        );
        std::fs::write(&path, &seed).unwrap();

        // The stale snapshot: what `login()` would have loaded before its own
        // async window (profile fetch / stdin prompt) opened.
        let mut client_config = load_or_default(&path).unwrap();

        // A rotation lands on disk AFTER that snapshot was taken — the
        // (now-gone) server's own persist path, used directly since the port
        // is silent for the whole test and cannot carry a real round-trip.
        let rotated_alice = Account {
            name: "alice@example.com".to_string(),
            account_type: "oauth".to_string(),
            account_uuid: None,
            org_uuid: None,
            org_name: None,
            access_token: "at-alice-ROTATED".to_string(),
            refresh_token: Some("rt-alice-ROTATED".to_string()),
            expires_at: Some(1_893_456_000_000),
            priority: None,
            switch_threshold: None,
            disabled: None,
            extra: serde_json::Map::new(),
        };
        config::save_account(&path, &rotated_alice).expect("the rotation must land on disk");
        let after_rotation = std::fs::read_to_string(&path).unwrap();
        assert!(
            after_rotation.contains("rt-alice-ROTATED"),
            "setup: the rotation must be on disk first: {after_rotation}"
        );

        let name = finish_login(
            &path,
            LoginRoute::Live,
            &mut client_config,
            "carol@example.com",
            &tokens("at-carol", "rt-carol", 1_893_456_000_000),
            profile_named("carol@example.com"),
        )
        .await
        .expect("a NoServer live add must still fall back to a surgical write");
        assert_eq!(name, "carol@example.com");

        let final_doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let accounts = final_doc["accounts"].as_array().unwrap();
        let alice = accounts
            .iter()
            .find(|a| a["name"] == "alice@example.com")
            .expect("alice must still be on disk");
        assert_eq!(
            alice["refreshToken"],
            serde_json::json!("rt-alice-ROTATED"),
            "the NoServer live add must NOT whole-file-write a stale snapshot back over \
             alice's rotation: {final_doc}"
        );
        assert!(
            accounts.iter().any(|a| a["name"] == "carol@example.com"),
            "carol was added: {final_doc}"
        );

        std::fs::remove_file(&path).ok();
    }

    /// A direct exercise of `finish_login`'s `NoRoute` arm. Before this test
    /// it was covered only by inference — every existing clobber test in
    /// this file drives either `Live`'s success path, `NoServer`, or the
    /// timeout/`Unusable` catch-all (`Err(other)`), never the specific
    /// unstamped-404 shape `NoRoute` is. Same "older tcr" server as
    /// `probe_add_capability_unstamped_404_is_unusable_not_absent`: an empty
    /// axum router answers every path, so `post_add_account` sees a 404 with
    /// no `ENDPOINT_HEADER` and classifies it structurally as `NoRoute`
    /// rather than `Unusable`.
    #[tokio::test]
    async fn finish_login_no_route_does_not_clobber_other_accounts() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, axum::Router::new()).await;
        });

        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-live-noroute-{}-{port}.json",
            std::process::id()
        ));
        let seed = format!(
            r#"{{ "proxy": {{ "port": {port} }}, "accounts": [
                {{ "name": "alice@example.com", "type": "oauth", "accessToken": "at-alice",
                  "refreshToken": "rt-alice", "expiresAt": 1893456000000, "priority": 0 }}
            ] }}"#
        );
        std::fs::write(&path, &seed).unwrap();

        // The stale snapshot: what `login()` would have loaded before its own
        // async window (profile fetch / stdin prompt) opened.
        let mut client_config = load_or_default(&path).unwrap();

        // A rotation lands on disk AFTER that snapshot was taken — this
        // "older tcr" has no add-account route to have produced it, standing
        // in for any other writer that touched the file in the window, same
        // as every sibling clobber test in this file.
        let rotated_alice = Account {
            name: "alice@example.com".to_string(),
            account_type: "oauth".to_string(),
            account_uuid: None,
            org_uuid: None,
            org_name: None,
            access_token: "at-alice-ROTATED".to_string(),
            refresh_token: Some("rt-alice-ROTATED".to_string()),
            expires_at: Some(1_893_456_000_000),
            priority: None,
            switch_threshold: None,
            disabled: None,
            extra: serde_json::Map::new(),
        };
        config::save_account(&path, &rotated_alice).expect("the rotation must land on disk");

        let name = finish_login(
            &path,
            LoginRoute::Live,
            &mut client_config,
            "dana@example.com",
            &tokens("at-dana", "rt-dana", 1_893_456_000_000),
            profile_named("dana@example.com"),
        )
        .await
        .expect("a NoRoute live add must still fall back to a surgical write");
        assert_eq!(name, "dana@example.com");

        let final_doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let accounts = final_doc["accounts"].as_array().unwrap();
        let alice = accounts
            .iter()
            .find(|a| a["name"] == "alice@example.com")
            .expect("alice must still be on disk");
        assert_eq!(
            alice["refreshToken"],
            serde_json::json!("rt-alice-ROTATED"),
            "the NoRoute live add must NOT whole-file-write a stale snapshot back over \
             alice's rotation: {final_doc}"
        );
        assert!(
            accounts.iter().any(|a| a["name"] == "dana@example.com"),
            "dana was added: {final_doc}"
        );

        std::fs::remove_file(&path).ok();
    }

    /// `persist_via_account_or_file`'s NotFound guard: `config::save_account`'s
    /// `read_document` opens the file with `fs::read_to_string`, which cannot
    /// create one that is not there — so a `NoServer` result reached with NO
    /// config file on disk at all must still fall back to `persist_via_file`
    /// (the same whole-file write `LoginRoute::File` always used) instead of
    /// bailing on the read error. A first-ever login must be able to create
    /// the file exactly as it always has.
    #[tokio::test]
    async fn finish_login_no_server_creates_the_file_when_none_exists() {
        let dead_port = {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            listener.local_addr().unwrap().port()
        };
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-live-noserver-nofile-{}-{dead_port}.json",
            std::process::id()
        ));
        std::fs::remove_file(&path).ok();
        assert!(!path.exists(), "setup: no config file must exist yet");

        // In-memory only — no file backs this, mirroring what `login()` would
        // hold when there is nothing on disk yet. Must carry `dead_port`
        // explicitly: leaving `proxy.port` at its serde default would send
        // this test's request to whatever real port 3456 happens to hold.
        let mut client_config: Config =
            serde_json::from_str(&format!(r#"{{ "proxy": {{ "port": {dead_port} }} }}"#)).unwrap();

        let name = finish_login(
            &path,
            LoginRoute::Live,
            &mut client_config,
            "frank@example.com",
            &tokens("at-frank", "rt-frank", 1_893_456_000_000),
            profile_named("frank@example.com"),
        )
        .await
        .expect("a NoServer live add with no file yet must still create one");
        assert_eq!(name, "frank@example.com");

        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            doc["accounts"][0]["name"],
            serde_json::json!("frank@example.com")
        );

        std::fs::remove_file(&path).ok();
    }
}
