//! `tcr mint` — mint a long-lived (365-day) OAuth token for an existing
//! account, or for every account carrying a group label, and put the result
//! on the clipboard. Nothing is written to the config file: this command
//! exports, it never stores (`docs/design/long-lived-tokens.md`).
//!
//! Deliberately separate from [`crate::oauth`]'s login flow rather than a new
//! branch inside it: the authorize host (`claude.com/cai/oauth/authorize`,
//! not [`oauth::AUTHORIZE_URL`]) and redirect target (an out-of-band paste
//! page, not a local callback server) both differ, and `login_hint` here is
//! load-bearing ergonomics for a KNOWN row rather than the add-a-new-account
//! convenience it is in [`oauth::login`]. What IS shared — [`oauth::CLIENT_ID`],
//! [`oauth::TOKEN_ENDPOINT`], [`oauth::OAUTH_SCOPES`],
//! [`oauth::LOGIN_TOKEN_LIFETIME_SECS`] and the two headers the endpoint
//! requires to avoid a bogus 429 — is reused by name, never re-typed.

use std::io::IsTerminal as _;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::Context as _;
use oauth2::basic::BasicClient;
use oauth2::{AuthUrl, ClientId, CsrfToken, PkceCodeChallenge, RedirectUrl, Scope, TokenUrl};
use serde::Deserialize;

use crate::config::{self, Account};
use crate::identity;
use crate::oauth;

/// Authorize endpoint used by `tcr mint`. Measured against the live endpoint
/// (`docs/design/long-lived-tokens.md`) — distinct from [`oauth::AUTHORIZE_URL`],
/// which the browser-login flow uses.
pub const MINT_AUTHORIZE_URL: &str = "https://claude.com/cai/oauth/authorize";

/// The out-of-band redirect target: a paste page, not a callback server. Mint
/// runs headless-friendly (no local port, no browser auto-open assumption) —
/// the operator opens the URL themselves and pastes back `code#state`.
pub const MINT_REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";

/// PKCE + CSRF material for one mint attempt, plus the built authorize URL.
struct MintFlow {
    verifier: String,
    state: String,
    auth_url: String,
}

/// Build the authorize URL, a fresh PKCE challenge and a fresh CSRF state for
/// one account. Called once per account in [`mint_one`] — never reused across
/// accounts, so one account's leaked/expired code can never be replayed
/// against another's exchange.
fn build_mint_flow(login_hint: &str) -> anyhow::Result<MintFlow> {
    let client = BasicClient::new(ClientId::new(oauth::CLIENT_ID.to_string()))
        .set_auth_uri(
            AuthUrl::new(MINT_AUTHORIZE_URL.to_string()).context("invalid mint authorize URL")?,
        )
        .set_token_uri(
            TokenUrl::new(oauth::TOKEN_ENDPOINT.to_string()).context("invalid token URL")?,
        )
        .set_redirect_uri(
            RedirectUrl::new(MINT_REDIRECT_URI.to_string()).context("invalid mint redirect URI")?,
        );

    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();

    let (auth_url, csrf) = client
        .authorize_url(|| CsrfToken::new_random_len(32))
        .add_scope(Scope::new(oauth::OAUTH_SCOPES.to_string()))
        .set_pkce_challenge(challenge)
        .add_extra_param("code", "true")
        .add_extra_param("login_hint", login_hint)
        .url();

    Ok(MintFlow {
        verifier: verifier.secret().clone(),
        state: csrf.secret().clone(),
        auth_url: auth_url.to_string(),
    })
}

/// Build the JSON body for [`exchange_mint_code`]'s `POST {TOKEN_ENDPOINT}`, as
/// a pure function so the request shape is assertable without the network —
/// mirrors `warm_request_spec` in `src/warmer.rs` and `exchange_request_body`
/// in `src/oauth.rs`.
fn mint_exchange_request_body(code: &str, verifier: &str, state: &str) -> serde_json::Value {
    serde_json::json!({
        "code": code,
        "state": state,
        "grant_type": "authorization_code",
        "client_id": oauth::CLIENT_ID,
        "redirect_uri": MINT_REDIRECT_URI,
        "code_verifier": verifier,
        "expires_in": oauth::LOGIN_TOKEN_LIFETIME_SECS,
    })
}

/// The token-exchange response fields mint needs. [`oauth`]'s `RefreshResponse`
/// does not model `account`/`organization` — refresh never needed identity —
/// so this is its own struct rather than widening that one for a caller that
/// isn't a refresh.
#[derive(Debug, Deserialize)]
struct MintExchangeResponse {
    access_token: String,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    account: Option<MintAccount>,
    #[serde(default)]
    organization: Option<MintOrg>,
}

#[derive(Debug, Deserialize)]
struct MintAccount {
    #[serde(default)]
    email_address: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MintOrg {
    #[serde(default)]
    name: Option<String>,
}

/// Exchange the pasted authorization code for a long-lived token. A
/// `.no_proxy()` client, exactly like [`oauth::exchange_code`]: an ambient
/// `HTTPS_PROXY` points at tcr itself, and routing this exchange through it
/// would loop back into the very proxy this token is meant to feed.
async fn exchange_mint_code(
    code: &str,
    verifier: &str,
    state: &str,
) -> anyhow::Result<MintExchangeResponse> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .context("build mint token-exchange client")?;

    let response = client
        .post(oauth::TOKEN_ENDPOINT)
        .header("Content-Type", "application/json")
        .header("Accept", oauth::OAUTH_ACCEPT)
        .header("User-Agent", oauth::OAUTH_USER_AGENT)
        .json(&mint_exchange_request_body(code, verifier, state))
        .send()
        .await
        .context("mint token exchange request failed")?;

    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("mint token exchange failed ({}): {text}", status.as_u16());
    }

    serde_json::from_str(&text).context("parse mint token-exchange response")
}

/// What minting one account produced.
#[derive(Debug, Clone, PartialEq)]
enum MintOutcome {
    /// Identity and lifetime both checked out; `token` is safe to present.
    Minted {
        token: String,
        org: String,
        granted_secs: i64,
    },
    /// The whole reason bulk minting is safe: `login_hint` is not proven to
    /// override an already-signed-in browser session, so this is the check
    /// that actually stops a `--group` loop from minting N tokens for one
    /// account (`docs/design/long-lived-tokens.md`, "Identity arrives free at
    /// exchange time"). The token is discarded — it never reaches
    /// [`assemble_clipboard_block`].
    IdentityMismatch { requested: String, actual: String },
    /// The endpoint granted a different lifetime than requested. Reported
    /// loudly rather than silently presenting a short-lived token as
    /// long-lived; the token is still discarded, matching the identity-mismatch
    /// case (an unexpected grant is exactly the moment to trust nothing else
    /// about the response either).
    LifetimeMismatch {
        requested_secs: i64,
        granted_secs: i64,
    },
    /// Network/parse/stdin failure — the exchange never produced a response to
    /// check identity against.
    Failed(String),
}

/// One account's mint attempt: the account name it was minted for, and what
/// came of it.
#[derive(Debug, Clone, PartialEq)]
struct MintResult {
    account_name: String,
    outcome: MintOutcome,
}

/// Evaluate an exchange response against the row it was minted for. Pure and
/// synchronous by design (test 3 in the bridge brief) — network I/O stops at
/// [`exchange_mint_code`], everything after it is a plain function of data.
fn evaluate_mint_response(requested_email: &str, response: MintExchangeResponse) -> MintOutcome {
    let actual_email = response
        .account
        .as_ref()
        .and_then(|a| a.email_address.clone())
        .unwrap_or_default();
    if actual_email != requested_email {
        return MintOutcome::IdentityMismatch {
            requested: requested_email.to_string(),
            actual: actual_email,
        };
    }

    let requested_secs = oauth::LOGIN_TOKEN_LIFETIME_SECS as i64;
    let granted_secs = response.expires_in.unwrap_or(0);
    if granted_secs != requested_secs {
        return MintOutcome::LifetimeMismatch {
            requested_secs,
            granted_secs,
        };
    }

    let org = response
        .organization
        .and_then(|o| o.name)
        .unwrap_or_else(|| "unknown-org".to_string());
    MintOutcome::Minted {
        token: response.access_token,
        org,
        granted_secs,
    }
}

/// Split a pasted `code#state` on `#`, keeping the part before it — exactly as
/// [`oauth::login_with_token`] reads a bare line, per the bridge brief. Falls
/// back to the whole trimmed string when no `#` is present rather than
/// erroring: a caller that pasted a bare code (no state) should not be
/// refused a value [`exchange_mint_code`] can still try.
fn parse_pasted_code(pasted: &str) -> &str {
    pasted.split_once('#').map_or(pasted, |(code, _state)| code)
}

/// Every account in `accounts` carrying `group` — exactly
/// [`Account::in_group`], never a looser substring or prefix match.
fn select_group_accounts<'a>(accounts: &'a [Account], group: &str) -> Vec<&'a Account> {
    accounts.iter().filter(|a| a.in_group(group)).collect()
}

/// Assemble the WHOLE clipboard block: one `<account name>\t<token>` line per
/// successfully minted account, in the order minted. Accounts that failed,
/// mismatched identity, or mismatched lifetime contribute no line — their
/// token (if any was ever held) never reaches this function's output.
fn assemble_clipboard_block(results: &[MintResult]) -> String {
    results
        .iter()
        .filter_map(|r| match &r.outcome {
            MintOutcome::Minted { token, .. } => Some(format!("{}\t{}", r.account_name, token)),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The stdout line for one account. Never the token — see this module's
/// top-level doc comment and the bridge brief's "Output rules".
///
/// The second column is always `OK` or `REFUSED`, so a reader scanning a
/// `--group` run of several accounts sees at a glance which rows did not work
/// without reading every reason — the failing ones are also the ones worth
/// reading closely.
fn format_report_line(result: &MintResult) -> String {
    match &result.outcome {
        MintOutcome::Minted {
            org, granted_secs, ..
        } => {
            let days = *granted_secs as f64 / 86_400.0;
            format!("{}\tOK\t{days:.0}d\t{org}", result.account_name)
        }
        MintOutcome::IdentityMismatch { requested, actual } => {
            format!(
                "{}\tREFUSED\tidentity mismatch: requested {requested}, exchange returned {actual}",
                result.account_name
            )
        }
        MintOutcome::LifetimeMismatch {
            requested_secs,
            granted_secs,
        } => {
            format!(
                "{}\tREFUSED\tlifetime mismatch: requested {requested_secs}s, granted \
                 {granted_secs}s — not presented as long-lived",
                result.account_name
            )
        }
        MintOutcome::Failed(err) => format!("{}\tREFUSED\t{err}", result.account_name),
    }
}

/// Read one pasted `code#state` line from stdin, prompting on stderr so stdout
/// stays scoped to the account/org/lifetime report (see this module's
/// top-level doc comment).
fn read_pasted_code() -> anyhow::Result<String> {
    eprint!("Paste code#state: ");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("read pasted code from stdin")?;
    let code = parse_pasted_code(line.trim());
    if code.is_empty() {
        anyhow::bail!("no authorization code pasted");
    }
    Ok(code.to_string())
}

/// Mint one account: build the flow, print the URL, block for the pasted
/// code, exchange it, and check the response against `login_hint`. Never
/// concurrent with another account's mint — the bridge brief is explicit that
/// each needs a human at a browser, and [`run_mint`] calls this in a plain
/// sequential loop, never `join_all`.
async fn mint_one(account_name: &str, login_hint: &str) -> MintResult {
    let outcome = match mint_one_inner(login_hint).await {
        Ok(outcome) => outcome,
        Err(err) => MintOutcome::Failed(err.to_string()),
    };
    MintResult {
        account_name: account_name.to_string(),
        outcome,
    }
}

async fn mint_one_inner(login_hint: &str) -> anyhow::Result<MintOutcome> {
    let flow = build_mint_flow(login_hint)?;
    eprintln!("\n=== {login_hint} ===");
    eprintln!("Open this URL, sign in as {login_hint}, then paste the resulting code#state:");
    eprintln!("  {}\n", flow.auth_url);

    let code = read_pasted_code()?;
    let response = exchange_mint_code(&code, &flow.verifier, &flow.state).await?;
    Ok(evaluate_mint_response(login_hint, response))
}

/// Pipe `block` to `pbcopy`'s stdin — never argv, which would put a token in
/// `ps` output and shell history.
fn copy_to_clipboard(block: &str) -> anyhow::Result<()> {
    let mut child = Command::new("pbcopy")
        .stdin(Stdio::piped())
        .spawn()
        .context("spawn pbcopy")?;
    child
        .stdin
        .as_mut()
        .context("pbcopy stdin")?
        .write_all(block.as_bytes())
        .context("write to pbcopy")?;
    let status = child.wait().context("wait for pbcopy")?;
    if !status.success() {
        anyhow::bail!("pbcopy exited with {status}");
    }
    Ok(())
}

/// `tcr mint --account <name> | --group <name>` — mint a long-lived token for
/// one account or every account carrying a group label, sequentially, and put
/// the successfully-minted tokens on the clipboard. Returns `Ok(true)` only
/// when every targeted account minted cleanly; the caller turns `Ok(false)`
/// into a non-zero exit so a UI driving this can surface the failure.
///
/// Reads the config but never writes it: mint stores nothing
/// (`docs/design/long-lived-tokens.md`; the operator explicitly chose
/// export-only over storing).
pub async fn run_mint(
    config_path: &Path,
    account: Option<&str>,
    group: Option<&str>,
) -> anyhow::Result<bool> {
    let cfg = config::load(config_path).context("load config for mint")?;

    let targets: Vec<&Account> = match (account, group) {
        (Some(query), None) => {
            let idx = crate::cli::resolve_account(&cfg.accounts, query)?;
            vec![&cfg.accounts[idx]]
        }
        (None, Some(group)) => {
            let matches = select_group_accounts(&cfg.accounts, group);
            if matches.is_empty() {
                anyhow::bail!("no account carries group '{group}'");
            }
            matches
        }
        (Some(_), Some(_)) | (None, None) => {
            anyhow::bail!("provide exactly one of --account or --group")
        }
    };

    let mut results = Vec::with_capacity(targets.len());
    for target in targets {
        let login_hint = identity::email_of(&target.name).to_string();
        results.push(mint_one(&target.name, &login_hint).await);
    }

    for result in &results {
        println!("{}", format_report_line(result));
    }
    let minted = results
        .iter()
        .filter(|r| matches!(r.outcome, MintOutcome::Minted { .. }))
        .count();
    println!("{minted}/{} minted", results.len());

    let block = assemble_clipboard_block(&results);
    if !block.is_empty() {
        copy_to_clipboard(&block).context("copy minted tokens to clipboard")?;
    }

    wait_for_return_on_a_tty();

    Ok(minted == results.len())
}

/// Hold the process open after the summary when stdin is a TTY, so a `--group`
/// run launched via `exec` from a Terminal window (as TcrBar's menu item
/// does — the same shape `tcr login` already runs under) does not have its
/// per-account report vanish the instant the window closes. Mint stores
/// nothing, so this report is the ONLY record of which rows were refused.
///
/// Guarded on [`IsTerminal`] exactly like `read_setup_token` in `src/oauth.rs`
/// decides whether to prompt: a piped or scripted invocation has no one to
/// wait for and must exit immediately. That TTY branch itself has no
/// existing test in this codebase (`read_setup_token`'s is untested too) —
/// driving a real terminal is outside what a unit test can assert, so this
/// is exercised by hand rather than under `cargo test`.
fn wait_for_return_on_a_tty() {
    if !std::io::stdin().is_terminal() {
        return;
    }
    eprint!("\nPress Return to close. ");
    std::io::stderr().flush().ok();
    let mut discard = String::new();
    let _ = std::io::stdin().read_line(&mut discard);
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- authorize URL -------------------------------------------------

    #[test]
    fn mint_authorize_url_carries_required_params() {
        let flow = build_mint_flow("alice@example.com").unwrap();
        let url = &flow.auth_url;
        // Hardcoded, not `MINT_AUTHORIZE_URL` — asserting a constant against
        // itself never catches the constant drifting to the wrong host.
        assert!(url.starts_with("https://claude.com/cai/oauth/authorize"));
        assert!(url.contains("code=true"));
        assert!(url.contains("code_challenge="));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("login_hint=alice%40example.com"));
        // The out-of-band redirect, percent-encoded by oauth2.
        assert!(url
            .contains("redirect_uri=https%3A%2F%2Fplatform.claude.com%2Foauth%2Fcode%2Fcallback"));

        // Decode the `scope` param rather than substring-matching the raw URL:
        // `:` and the space between scopes are both percent-encoded, so a raw
        // `contains("user:inference")` check would never match.
        let query = url.split_once('?').map(|(_, q)| q).unwrap_or_default();
        let scope = form_urlencoded::parse(query.as_bytes())
            .find(|(k, _)| k == "scope")
            .map(|(_, v)| v.into_owned())
            .expect("scope param present");
        assert_eq!(scope, oauth::OAUTH_SCOPES);
    }

    // --- exchange body ---------------------------------------------------

    #[test]
    fn mint_exchange_body_requests_the_long_lived_lifetime() {
        let body = mint_exchange_request_body("code123", "verifier", "state");
        assert_eq!(body["expires_in"], oauth::LOGIN_TOKEN_LIFETIME_SECS);
        assert_eq!(body["grant_type"], "authorization_code");
        assert_eq!(body["redirect_uri"], MINT_REDIRECT_URI);
    }

    // --- identity guard ----------------------------------------------------

    fn response(email: Option<&str>, expires_in: Option<i64>) -> MintExchangeResponse {
        MintExchangeResponse {
            access_token: "secret-token".to_string(),
            expires_in,
            account: Some(MintAccount {
                email_address: email.map(str::to_string),
            }),
            organization: Some(MintOrg {
                name: Some("acme-corp".to_string()),
            }),
        }
    }

    #[test]
    fn matching_identity_and_lifetime_is_minted() {
        let outcome = evaluate_mint_response(
            "alice@example.com",
            response(
                Some("alice@example.com"),
                Some(oauth::LOGIN_TOKEN_LIFETIME_SECS as i64),
            ),
        );
        assert!(matches!(outcome, MintOutcome::Minted { .. }));
    }

    #[test]
    fn mismatched_identity_is_rejected_and_the_token_never_reaches_the_block() {
        let outcome = evaluate_mint_response(
            "alice@example.com",
            response(
                Some("mallory@example.com"),
                Some(oauth::LOGIN_TOKEN_LIFETIME_SECS as i64),
            ),
        );
        assert!(matches!(outcome, MintOutcome::IdentityMismatch { .. }));

        let results = vec![MintResult {
            account_name: "alice@example.com".to_string(),
            outcome,
        }];
        let block = assemble_clipboard_block(&results);
        assert!(!block.contains("secret-token"), "block was: {block:?}");
        assert_eq!(block, "", "a mismatch contributes no line at all");
    }

    #[test]
    fn mismatched_lifetime_is_rejected_and_the_token_never_reaches_the_block() {
        // The endpoint granted the default 8h instead of the requested 365d —
        // must be reported loudly, never presented as long-lived.
        let outcome = evaluate_mint_response(
            "alice@example.com",
            response(Some("alice@example.com"), Some(28_800)),
        );
        assert!(matches!(outcome, MintOutcome::LifetimeMismatch { .. }));

        let results = vec![MintResult {
            account_name: "alice@example.com".to_string(),
            outcome,
        }];
        assert_eq!(assemble_clipboard_block(&results), "");
    }

    #[test]
    fn a_minted_token_reaches_the_assembled_block() {
        let results = vec![MintResult {
            account_name: "alice@example.com".to_string(),
            outcome: MintOutcome::Minted {
                token: "secret-token".to_string(),
                org: "acme-corp".to_string(),
                granted_secs: oauth::LOGIN_TOKEN_LIFETIME_SECS as i64,
            },
        }];
        assert_eq!(
            assemble_clipboard_block(&results),
            "alice@example.com\tsecret-token"
        );
    }

    // --- pasted code -------------------------------------------------------

    #[test]
    fn pasted_code_is_split_on_hash_before_the_state() {
        assert_eq!(parse_pasted_code("abc123#the-state"), "abc123");
    }

    #[test]
    fn pasted_bare_code_with_no_hash_is_used_verbatim() {
        assert_eq!(parse_pasted_code("abc123"), "abc123");
    }

    // --- target selection ----------------------------------------------

    fn account(name: &str, groups: &[&str]) -> Account {
        let mut value = serde_json::json!({
            "name": name,
            "accessToken": "t",
            "refreshToken": "r",
        });
        if !groups.is_empty() {
            value["groups"] = serde_json::json!(groups);
        }
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn group_selects_exactly_the_members_and_account_selects_one() {
        let accounts = vec![
            account("alice@example.com", &["team-a"]),
            account("bob@example.com", &["team-a", "team-b"]),
            account("carol@example.com", &["team-b"]),
        ];

        let team_a = select_group_accounts(&accounts, "team-a");
        assert_eq!(
            team_a.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
            vec!["alice@example.com", "bob@example.com"]
        );

        let team_b = select_group_accounts(&accounts, "team-b");
        assert_eq!(
            team_b.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
            vec!["bob@example.com", "carol@example.com"]
        );

        let none = select_group_accounts(&accounts, "no-such-group");
        assert!(none.is_empty());

        let idx = crate::cli::resolve_account(&accounts, "carol@example.com").unwrap();
        assert_eq!(accounts[idx].name, "carol@example.com");
    }
}
