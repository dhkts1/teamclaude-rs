//! Account-management subcommands for the `tcr` CLI.
//!
//! These are the non-server verbs — `accounts`, `remove`, `priority`,
//! `enable`, `disable`, `status` — that read or mutate the drop-in config
//! (`~/.config/teamclaude.json`) directly, mirroring the JS `teamclaude`
//! management commands so friends can curate the fleet without hand-editing
//! JSON.
//!
//! The seam copies [`crate::oauth::login`]: pure functions here `load` → mutate
//! a `&mut Config` → [`config::save`]; `main.rs` is a thin parse-and-delegate
//! layer. Every load/save wrapper WARNS to stderr when a proxy is already
//! listening on the config's port — a running server refreshes tokens and
//! flushes the config on shutdown, so an out-of-band edit races that writer and
//! can be silently clobbered.
//!
//! The read verbs (`accounts`, `status`) build an OFFLINE [`Manager`] with
//! `config_path = None`, so a probe's token refresh is NEVER persisted to disk —
//! reading quota must never mutate the user's file out from under a live server.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context as _};
use time::OffsetDateTime;

use crate::build_info::{self, BuildInfo};
use crate::config::{self, Account, Config};
use crate::identity;
use crate::manager::Manager;
use crate::oauth::{NoRefresh, TokenRefresher};
use crate::probe::{LiveUsageProber, UsageProber};
use crate::stats::{AccountSnapshot, QuotaState, StatsSnapshot};
use crate::status::{StatusPayload, STATUS_KIND};

/// Reject a group label whose CONTENT could corrupt header composition
/// (`main.rs`'s `compose_group_header`) or a config write — a literal newline,
/// any other control character, or a codepoint above U+00FF — and reject
/// empty/whitespace-only.
///
/// The Phase 1 validator (moved here from `main.rs` so `tcr group add`'s
/// surgical write and `tcr run --group`'s header composition share exactly one
/// definition — two validators drift and then disagree). Defense in depth, not
/// a fix for a live vulnerability: Claude Code's own header-value parser
/// already hard-errors on a header value containing a line break, NUL, or a
/// codepoint above U+00FF, so a bad label fails closed downstream today
/// regardless of this check.
///
/// Returns the failing character class in `Err` (never the raw character —
/// most of what this rejects is unprintable) so a call site can say exactly
/// what was wrong.
pub fn validate_group_label_chars(label: &str) -> Result<(), &'static str> {
    if label.trim().is_empty() {
        return Err("empty or whitespace-only");
    }
    for c in label.chars() {
        if c == '\n' || c == '\r' {
            return Err("contains a newline");
        }
        if c.is_control() {
            return Err("contains a control character");
        }
        if (c as u32) > 0xFF {
            return Err("contains a codepoint above U+00FF");
        }
    }
    Ok(())
}

/// How to set an account's priority: an explicit integer, or a relative
/// `--first` / `--last` that recomputes against the existing fleet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriorityArg {
    /// Set the exact integer priority.
    N(i64),
    /// Make this account sort FIRST: `min(0, existing priorities) - 1`.
    First,
    /// Make this account sort LAST: `max(0, existing priorities) + 1`.
    Last,
}

/// Is a proxy already listening on `port`? A cheap loopback connect probe —
/// the same gate `tcr run` uses to decide whether to route through us.
pub fn proxy_is_up(port: u16) -> bool {
    use std::net::{SocketAddr, TcpStream};
    use std::time::Duration;
    TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(300),
    )
    .is_ok()
}

/// Warn (once) when a server is live on the config's port, since our write
/// races its shutdown flush. Best-effort — never blocks the edit.
fn warn_if_server_running(config: &Config) {
    if proxy_is_up(config.proxy.port) {
        eprintln!(
            "[tcr] warning: a proxy is already listening on :{} — it may overwrite this edit when it flushes the config on exit. Stop the server first, or expect to re-apply.",
            config.proxy.port
        );
    }
}

/// Load the config for a management verb, warning if a server is live.
fn load_for_edit(config_path: &Path) -> anyhow::Result<Config> {
    let config = config::load(config_path)
        .with_context(|| format!("load config at {}", config_path.display()))?;
    warn_if_server_running(&config);
    Ok(config)
}

/// Save the config after a management verb, warning if a server is live.
fn save_after_edit(config_path: &Path, config: &Config) -> anyhow::Result<()> {
    warn_if_server_running(config);
    config::save(config_path, config)
        .with_context(|| format!("save config at {}", config_path.display()))
}

/// Resolve a user-supplied account `query` to exactly one index in `accounts`.
///
/// Delegates to [`identity::match_accounts`]: an exact `name` match wins;
/// otherwise the email portion is matched, and `--org` (exact org name, or org
/// uuid exact/prefix) narrows — **even when the name matched**, so a duplicate
/// email across two orgs is disambiguated by `--org` (finding #8) rather than
/// silently resolving to the first entry. Zero or more-than-one surviving
/// candidate is an error — the ambiguous error lists the candidate names so the
/// caller can disambiguate. (`Account.name` IS the email.)
pub fn resolve_account<T: identity::Queryable>(
    accounts: &[T],
    query: &str,
    org: Option<&str>,
) -> anyhow::Result<usize> {
    match identity::match_one(accounts, query, org) {
        identity::Match::One(only) => Ok(only),
        identity::Match::None => {
            let org_note = org
                .map(|o| format!(" (with org matching '{o}')"))
                .unwrap_or_default();
            bail!("no account matches '{query}'{org_note}");
        }
        identity::Match::Ambiguous(names) => {
            bail!("{}", ambiguous_query_message(query, &names));
        }
    }
}

/// The one-line explanation of an ambiguous query, shared by the CLI's own
/// resolution and by the live control endpoint's 409 body, so a caller gets the
/// same actionable sentence — and the same candidate list — whichever answered.
pub fn ambiguous_query_message(query: &str, names: &[String]) -> String {
    format!(
        "'{query}' is ambiguous — matches {} accounts: {}. Narrow with --org or use an exact name.",
        names.len(),
        names.join(", ")
    )
}

/// Load → resolve `(query, org)` to one account → hand it to `mutate` → save.
///
/// Centralizes the load→resolve→save chain (incl. the nonexistent-account error
/// path) shared by every mutating verb. Resolution happens BEFORE `mutate`, so a
/// non-matching query returns an error with the file left byte-identical (no
/// partial write). `mutate` receives the resolved index and the whole config, so
/// a command that needs the fleet (e.g. relative priority) or must `remove` the
/// entry can still do so; it returns whatever the caller wants to report.
fn edit_account<T>(
    config_path: &Path,
    query: &str,
    org: Option<&str>,
    mutate: impl FnOnce(&mut Config, usize) -> T,
) -> anyhow::Result<T> {
    let mut config = load_for_edit(config_path)?;
    let idx = resolve_account(&config.accounts, query, org)?;
    let out = mutate(&mut config, idx);
    save_after_edit(config_path, &config)?;
    Ok(out)
}

/// Remove the account matching `query` from the config and save.
///
/// Resolution happens BEFORE any mutation, so a non-matching query returns an
/// error with the file left byte-identical (no partial write).
pub fn remove_account(config_path: &Path, query: &str, org: Option<&str>) -> anyhow::Result<()> {
    let removed = edit_account(config_path, query, org, |config, idx| {
        config.accounts.remove(idx).name
    })?;
    println!("Removed account '{removed}'.");
    Ok(())
}

/// Set the priority of the account matching `query`.
///
/// `--first` = `min(0, existing priorities) - 1`; `--last` =
/// `max(0, existing priorities) + 1`; an explicit `N` is written verbatim. The
/// `0` seed guarantees a relative move always crosses the default tier even when
/// every existing priority sits on the same side of it.
pub fn set_priority(
    config_path: &Path,
    query: &str,
    priority: PriorityArg,
    org: Option<&str>,
) -> anyhow::Result<()> {
    let (name, value) = edit_account(config_path, query, org, |config, idx| {
        let value = match priority {
            PriorityArg::N(n) => n,
            PriorityArg::First => {
                config
                    .accounts
                    .iter()
                    .filter_map(|a| a.priority)
                    .chain(std::iter::once(0))
                    .min()
                    .expect("chained 0 guarantees a min")
                    - 1
            }
            PriorityArg::Last => {
                config
                    .accounts
                    .iter()
                    .filter_map(|a| a.priority)
                    .chain(std::iter::once(0))
                    .max()
                    .expect("chained 0 guarantees a max")
                    + 1
            }
        };
        config.accounts[idx].priority = Some(value);
        (config.accounts[idx].name.clone(), value)
    })?;
    println!("Set priority of '{name}' to {value}.");
    Ok(())
}

/// Printed after every group mutation that actually changed the file. There is
/// no live route to push a `groups` edit into a running proxy — it reads
/// `groups` once, at boot, exactly like `disabled` before the live-control
/// routes existed for it — so a successful `tcr group add`/`rm` that silently
/// looked live would be the same class of bug `set_enabled`'s doc-comment
/// describes: two accounts measured on the live fleet with a config saying one
/// thing and the serving process doing another.
const GROUP_RESTART_NOTE: &str = "note: the running proxy will not see this until it restarts";

/// Resolve `name` to exactly one configured account by EXACT match on
/// `Account.name` (which IS the email — see [`resolve_account`]'s doc-comment).
///
/// Deliberately not [`resolve_account`]: `tcr group add`/`rm`'s contract with
/// the TcrBar panel (its argument shape is pinned, not ours to change) has no
/// `--org` disambiguator, and an unknown name must list every configured
/// account rather than [`resolve_account`]'s "no account matches" — the panel
/// shells out blind and needs the full roster in the error to act on it.
fn find_account_by_name(accounts: &[Account], name: &str) -> anyhow::Result<usize> {
    match accounts.iter().position(|a| a.name == name) {
        Some(idx) => Ok(idx),
        None => {
            let configured: Vec<&str> = accounts.iter().map(|a| a.name.as_str()).collect();
            bail!(
                "no account named '{name}' in the config. Configured accounts: {}",
                configured.join(", ")
            );
        }
    }
}

/// `tcr group add <group> <account>` — label `account` with `group`.
///
/// Idempotent: adding a label an account already carries succeeds and says so,
/// so the panel can call it without first checking state. The group label
/// itself is validated with [`validate_group_label_chars`] — the same Phase 1
/// validator `tcr run --group` uses — because `add` is the one path that can
/// put a fresh, attacker- or typo-controlled string into the config; `rm` must
/// NOT run this check (see [`remove_from_group`]'s doc-comment).
pub fn add_to_group(config_path: &Path, group: &str, account: &str) -> anyhow::Result<()> {
    if let Err(reason) = validate_group_label_chars(group) {
        bail!("group {group:?}: invalid group label — {reason}");
    }
    // Deliberately NOT `load_for_edit` — that helper's warning ("a proxy is
    // already listening — it may overwrite this edit when it flushes the
    // config on exit") describes a hazard THIS write does not have.
    // `Manager::persist_now` flushes via `config::save_tokens`, not a whole
    // `config::save` of its boot-time snapshot (see persist_now's and
    // save_tokens' doc-comments) — it re-reads the CURRENT on-disk document
    // at shutdown and merges in only the token fields, so a group label this
    // surgical write already put on disk survives that flush untouched. The
    // warning would be false on every ordinary run, and TcrBar calls this
    // command with the proxy always up, which is exactly the case that makes
    // a false-but-constant warning turn into wallpaper.
    let config = config::load(config_path)
        .with_context(|| format!("load config at {}", config_path.display()))?;
    let idx = find_account_by_name(&config.accounts, account)?;
    let target = &config.accounts[idx];
    let outcome = config::save_group_membership(config_path, target, group, true)
        .with_context(|| format!("save config at {}", config_path.display()))?;
    match outcome {
        config::GroupWrite::Updated => {
            println!("Added '{account}' to group '{group}'.");
            println!("{GROUP_RESTART_NOTE}");
        }
        config::GroupWrite::Unchanged => {
            println!("'{account}' is already in group '{group}'; nothing changed.");
        }
        config::GroupWrite::NoEntry => bail!(
            "'{account}' vanished from {} between load and write (concurrent edit?) — nothing changed",
            config_path.display()
        ),
        config::GroupWrite::Ambiguous => bail!(
            "more than one entry in {} carries '{account}''s identity — nothing changed",
            config_path.display()
        ),
    }
    Ok(())
}

/// `tcr group rm <group> <account>` / `tcr group rm <group> --all` — remove
/// `account`'s (or every member's, with `--all`) `group` label.
///
/// `account == None` iff `all` — clap's `ArgGroup` on `GroupRmArgs` refuses
/// "both" and "neither" at parse time before this ever runs, so the `expect`
/// below documents that invariant rather than guessing around it.
///
/// Deliberately does NOT run [`validate_group_label_chars`] on `group`: unlike
/// `add`, `rm` must be able to remove a label that somehow got into the config
/// with a bad character (hand-edited, or written by an older/buggy version) —
/// refusing to remove a bad label would make it permanent.
pub fn remove_from_group(
    config_path: &Path,
    group: &str,
    account: Option<&str>,
    all: bool,
) -> anyhow::Result<()> {
    // Same reasoning as `add_to_group`: no `load_for_edit` warning here either
    // — `save_group_membership`'s surgical write survives the proxy's
    // shutdown flush (`persist_now` -> `save_tokens`) for the same reason.
    let config = config::load(config_path)
        .with_context(|| format!("load config at {}", config_path.display()))?;

    if all {
        let mut removed_from: Vec<String> = Vec::new();
        for acct in &config.accounts {
            if !acct.in_group(group) {
                continue;
            }
            let outcome = config::save_group_membership(config_path, acct, group, false)
                .with_context(|| format!("save config at {}", config_path.display()))?;
            match outcome {
                config::GroupWrite::Updated => removed_from.push(acct.name.clone()),
                config::GroupWrite::Unchanged => {}
                config::GroupWrite::NoEntry => bail!(
                    "'{}' vanished from {} mid-removal (concurrent edit?) — {} account(s) already updated: {}",
                    acct.name,
                    config_path.display(),
                    removed_from.len(),
                    removed_from.join(", ")
                ),
                config::GroupWrite::Ambiguous => bail!(
                    "more than one entry in {} carries '{}''s identity — {} account(s) already updated: {}",
                    config_path.display(),
                    acct.name,
                    removed_from.len(),
                    removed_from.join(", ")
                ),
            }
        }
        if removed_from.is_empty() {
            println!("Group '{group}' has no members; nothing changed.");
        } else {
            println!(
                "Removed group '{group}' from {} account(s): {}.",
                removed_from.len(),
                removed_from.join(", ")
            );
            println!("{GROUP_RESTART_NOTE}");
        }
        return Ok(());
    }

    // clap's ArgGroup on `GroupRmArgs` guarantees exactly one of
    // `account`/`--all` — see this function's doc-comment.
    let account = account.expect("clap guarantees `account` is Some when `all` is false");
    let idx = find_account_by_name(&config.accounts, account)?;
    let target = &config.accounts[idx];
    let outcome = config::save_group_membership(config_path, target, group, false)
        .with_context(|| format!("save config at {}", config_path.display()))?;
    match outcome {
        config::GroupWrite::Updated => {
            println!("Removed '{account}' from group '{group}'.");
            println!("{GROUP_RESTART_NOTE}");
        }
        config::GroupWrite::Unchanged => {
            println!("'{account}' was not in group '{group}'; nothing changed.");
        }
        config::GroupWrite::NoEntry => bail!(
            "'{account}' vanished from {} between load and write (concurrent edit?) — nothing changed",
            config_path.display()
        ),
        config::GroupWrite::Ambiguous => bail!(
            "more than one entry in {} carries '{account}''s identity — nothing changed",
            config_path.display()
        ),
    }
    Ok(())
}

/// `tcr group ls [--json]` — list every group and its members, plus the
/// ungrouped count.
///
/// Read-only: plain [`config::load`], no clobber-warning (we never save).
/// Text output is greppable, one line per group; an account in several groups
/// appears under each — same on both paths.
pub fn list_groups(config_path: &Path, json: bool) -> anyhow::Result<()> {
    let config = config::load(config_path)
        .with_context(|| format!("load config at {}", config_path.display()))?;
    if json {
        println!("{}", render_groups_json(&config)?);
    } else {
        print!("{}", render_groups_text(&config));
    }
    Ok(())
}

/// Group name -> the (in-order) names of every account carrying that label,
/// plus the count of accounts carrying NO label at all. An account in several
/// groups appears once per group it is in — this is the shared aggregation
/// both [`render_groups_text`] and [`render_groups_json`] render from, so the
/// two outputs can never disagree about which account is in which group.
fn group_membership(config: &Config) -> (std::collections::BTreeMap<&str, Vec<&str>>, usize) {
    let mut groups: std::collections::BTreeMap<&str, Vec<&str>> = std::collections::BTreeMap::new();
    let mut ungrouped = 0usize;
    for account in &config.accounts {
        match account.groups.as_deref() {
            Some(labels) if !labels.is_empty() => {
                for label in labels {
                    groups
                        .entry(label.as_str())
                        .or_default()
                        .push(account.name.as_str());
                }
            }
            _ => ungrouped += 1,
        }
    }
    (groups, ungrouped)
}

/// `tcr group ls` text: one greppable line per group, then `ungrouped`.
fn render_groups_text(config: &Config) -> String {
    use std::fmt::Write as _;
    let (groups, ungrouped) = group_membership(config);
    let width = groups.keys().map(|g| g.len()).max().unwrap_or(0);
    let mut out = String::new();
    for (group, members) in &groups {
        let _ = writeln!(
            out,
            "group {group:width$} accounts={} members={}",
            members.len(),
            members.join(","),
        );
    }
    let _ = writeln!(out, "ungrouped accounts={ungrouped}");
    out
}

/// `tcr group ls --json`: the structured equivalent of [`render_groups_text`]
/// for the TcrBar panel.
fn render_groups_json(config: &Config) -> anyhow::Result<String> {
    let (groups, ungrouped) = group_membership(config);
    let rows: Vec<serde_json::Value> = groups
        .iter()
        .map(|(group, members)| {
            serde_json::json!({
                "group": group,
                "accounts": members.len(),
                "members": members,
            })
        })
        .collect();
    let out = serde_json::json!({ "groups": rows, "ungrouped": ungrouped });
    Ok(serde_json::to_string_pretty(&out)?)
}

/// Enable or disable the account matching `query`, **in the running proxy first**.
///
/// A file-only write was the bug. The proxy reads `disabled` from the config once,
/// at `Manager::new`, and never again — so `tcr disable alice` exited 0, printed
/// "Disabled account 'alice'.", and the serving process kept handing that account
/// live traffic. `tcr status` prefers the live server, so nothing anywhere said
/// otherwise. Two accounts were measured in that state on the live fleet.
///
/// So: ask the server ([`crate::proxy::DISABLED_PATH`]), which applies the flag to
/// its in-memory rotation AND persists it, and fall back to the file only when no
/// server answered. The fallback is not silent when a server DID answer and could
/// not do the job — that silence is the entire reason the defect was invisible.
///
/// `disabled = false` on the file path sets the field to `None` so
/// `skip_serializing_if = Option::is_none` DROPS the key entirely — matching the
/// JS `delete account.disabled`, not a `false` literal. `Manager::set_disabled`'s
/// persist does the same, so both paths leave the same document.
pub async fn set_enabled(
    config_path: &Path,
    query: &str,
    org: Option<&str>,
    disabled: bool,
) -> anyhow::Result<()> {
    // A config we cannot read has no port and no api-key to reach a server with;
    // fall through to the file path, which reports the load failure as it always
    // has. Never a silent skip of the live attempt for any other reason.
    if let Ok(config) = config::load(config_path) {
        match post_set_disabled(&config, query, org, disabled).await {
            Ok(applied) => {
                println!(
                    "{} account '{}'.",
                    if disabled { "Disabled" } else { "Enabled" },
                    applied.name
                );
                if let Some(warning) = &applied.warning {
                    eprintln!("[tcr] warning: {warning}");
                }
                return Ok(());
            }
            // Nothing is listening: the historical case, and the only quiet one.
            // There is no live rotation to disagree with the file.
            Err(LiveControlError::NoServer) => {}
            // A server is there and REFUSED us. Writing the file here would be the
            // old lie in a new place: the config would say benched while the proxy
            // we could not talk to keeps rotating. Change nothing, exit non-zero.
            Err(LiveControlError::Unauthorized) => {
                bail!(
                    "the proxy on :{} rejected the api-key in {} — the config was NOT changed, because writing it would leave the running proxy still rotating this account. Fix `proxy.apiKey` and retry.",
                    config.proxy.port,
                    config_path.display()
                );
            }
            // The route is missing: an older tcr is serving. The file write is all
            // we can do, and it is HALF a disable — say so loudly. This arm is the
            // one that used to be the whole function, silently.
            Err(LiveControlError::NoRoute) => {
                let name = write_disabled_flag(config_path, query, org, disabled)?;
                eprintln!(
                    "[tcr] WARNING: the proxy running on :{} is too old to accept live account control (no {} route), so only the config file was changed. It will KEEP {} '{name}' until it restarts. Run `tcr restart` when a cold prompt cache is acceptable.",
                    config.proxy.port,
                    crate::proxy::DISABLED_PATH,
                    if disabled { "routing to" } else { "benching" },
                );
                return Ok(());
            }
            // The route ran and refused the QUERY: it matched no live account, or
            // matched several. Do not fall back — the file's own resolution could
            // land on a different account than the one the server was talking
            // about, and a disable applied to the wrong row is worse than none.
            Err(LiveControlError::Rejected(message)) => {
                bail!(
                    "the proxy running on :{} refused this: {message} Nothing was changed.",
                    config.proxy.port
                );
            }
            // It answered something we cannot use, or did not answer at all. Same
            // shape of consequence as the arm above, different cause, and equally
            // never silent.
            Err(other) => {
                let name = write_disabled_flag(config_path, query, org, disabled)?;
                eprintln!(
                    "[tcr] WARNING: could not apply this to the proxy running on :{} ({}), so only the config file was changed. It may KEEP {} '{name}' until it restarts.",
                    config.proxy.port,
                    other.why(),
                    if disabled { "routing to" } else { "benching" },
                );
                return Ok(());
            }
        }
    }

    let name = write_disabled_flag(config_path, query, org, disabled)?;
    println!(
        "{} account '{name}'.",
        if disabled { "Disabled" } else { "Enabled" }
    );
    Ok(())
}

/// The file half of enable/disable: resolve, set the flag, save, return the
/// resolved name. Exactly what `set_enabled` did before it learned to ask the
/// server.
fn write_disabled_flag(
    config_path: &Path,
    query: &str,
    org: Option<&str>,
    disabled: bool,
) -> anyhow::Result<String> {
    edit_account(config_path, query, org, |config, idx| {
        config.accounts[idx].disabled = if disabled { Some(true) } else { None };
        config.accounts[idx].name.clone()
    })
}

/// Why a live account-control request did not apply.
///
/// The arms are the CLI's decision table, not decoration: one of them must NOT
/// write the config (`Unauthorized`), one writes it quietly (`NoServer`), and the
/// rest write it and shout. Collapsing them is how a half-applied disable becomes
/// invisible again.
///
/// `pub(crate)`: [`crate::oauth`]'s live-login half reuses this exact enum for
/// [`post_add_account`] rather than defining a parallel one — see that
/// function's doc-comment.
#[derive(Debug)]
pub(crate) enum LiveControlError {
    /// Nothing is listening on the configured port. No live rotation exists, so
    /// the file IS the whole truth — the ordinary offline case.
    NoServer,
    /// A server answered, but the account-control route is not there: an older
    /// tcr, identified structurally by the absent
    /// [`crate::proxy::ENDPOINT_HEADER`] on a 404/405, never by error text.
    NoRoute,
    /// The proxy rejected our api-key. The one arm that must not touch the file.
    Unauthorized,
    /// Something is listening but produced no usable response before the deadline.
    NoAnswer(String),
    /// It answered and the answer was not one we can act on: a 4xx/5xx from the
    /// route itself, or a body that is not ours.
    Unusable(String),
    /// The route resolved our query to nothing, or to more than one account. The
    /// server's own message is carried through verbatim — it names the candidates.
    Rejected(String),
}

impl LiveControlError {
    pub(crate) fn why(&self) -> String {
        match self {
            LiveControlError::NoServer => "nothing is listening".to_string(),
            LiveControlError::NoRoute => {
                "the running proxy has no account-control route".to_string()
            }
            LiveControlError::Unauthorized => "the proxy api-key was rejected".to_string(),
            LiveControlError::NoAnswer(why)
            | LiveControlError::Unusable(why)
            | LiveControlError::Rejected(why) => why.clone(),
        }
    }
}

/// Ask the running proxy to park/unpark an account. Mirrors [`fetch_live_status`]:
/// same `no_proxy()` client (`HTTP_PROXY` commonly points AT tcr, so honouring it
/// would send this command through the proxy it is about), same timeouts, same
/// api-key header — the control route has no loopback exemption either.
async fn post_set_disabled(
    config: &Config,
    query: &str,
    org: Option<&str>,
    disabled: bool,
) -> Result<crate::proxy::SetDisabledResponse, LiveControlError> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| LiveControlError::Unusable(format!("http client: {e}")))?;

    let url = format!(
        "http://127.0.0.1:{}{}",
        config.proxy.port,
        crate::proxy::DISABLED_PATH
    );
    let mut request = client.post(&url).json(&serde_json::json!({
        "query": query,
        "org": org,
        "disabled": disabled,
    }));
    if let Some(key) = config.proxy.api_key.as_deref() {
        request = request.header("x-api-key", key);
    }

    let response = match request.send().await {
        Ok(r) => r,
        // `is_timeout()` first: a connect TIMEOUT (a blackholed address, SYN
        // dropped rather than refused) reports `is_connect() == true` as
        // well, so checking `is_connect()` first would misclassify it as
        // `NoServer` — "nothing is listening" — when the truer reading is
        // "something is there but not answering", `NoAnswer` below.
        Err(e) if e.is_timeout() => {
            return Err(LiveControlError::NoAnswer(
                "the server did not answer within 5s".to_string(),
            ))
        }
        Err(e) if e.is_connect() => return Err(LiveControlError::NoServer),
        Err(e) => return Err(LiveControlError::NoAnswer(e.to_string())),
    };

    let status = response.status();
    // Whether THIS route produced the answer, decided structurally. A 404 from a
    // tcr without the route and a 404 meaning "no such account" are the same status
    // code with opposite consequences, and the header is the only difference that
    // is not a matched error string.
    let from_route = response
        .headers()
        .get(crate::proxy::ENDPOINT_HEADER)
        .and_then(|v| v.to_str().ok())
        == Some(crate::proxy::DISABLED_ENDPOINT);
    let body = response.text().await.unwrap_or_default();

    if status.is_success() {
        return serde_json::from_str(&body).map_err(|e| {
            LiveControlError::Unusable(format!(
                "the response was not a tcr account-control payload ({e})"
            ))
        });
    }
    if status.as_u16() == 401 {
        return Err(LiveControlError::Unauthorized);
    }
    if !from_route && matches!(status.as_u16(), 404 | 405) {
        return Err(LiveControlError::NoRoute);
    }
    if from_route && matches!(status.as_u16(), 404 | 409) {
        return Err(LiveControlError::Rejected(
            error_message_of(&body).unwrap_or_else(|| format!("HTTP {status}")),
        ));
    }
    Err(LiveControlError::Unusable(format!("HTTP {status}")))
}

/// Ask the running proxy to add an account to the live rotation, or — when the
/// submitted identity already matches one — replace its credentials in place.
/// No restart, so no session pin is invalidated. Mirrors [`post_set_disabled`]:
/// same `no_proxy()` client (`HTTP_PROXY` commonly points AT tcr), same
/// timeouts, same api-key header — the control route has no loopback
/// exemption either.
///
/// `account` is POSTed directly as the request body: [`config::Account`]
/// already serializes to the exact wire shape
/// [`crate::proxy::ADD_ACCOUNT_PATH`] expects (see that route's
/// `AddAccountRequest` doc-comment), so a caller builds the credential once —
/// for a real login, or [`crate::oauth::probe_add_capability`]'s deliberately
/// blank probe — and this function never re-lists its fields.
///
/// Reuses [`LiveControlError`] rather than a parallel enum, extended with one
/// more structural case [`post_set_disabled`] never needs: a `400` FROM the
/// route (stamped) is this route's own request-validation failure — e.g. a
/// blank name or token — and is exactly as much a "the route said no" signal
/// as the `404`/`409` [`post_set_disabled`] already treats that way. The
/// [`crate::proxy::ENDPOINT_HEADER`] stamp remains the only thing that
/// decides "no such route" vs "the route said no" — never error text.
pub(crate) async fn post_add_account(
    config: &Config,
    account: &Account,
) -> Result<crate::proxy::AddAccountResponse, LiveControlError> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| LiveControlError::Unusable(format!("http client: {e}")))?;

    let url = format!(
        "http://127.0.0.1:{}{}",
        config.proxy.port,
        crate::proxy::ADD_ACCOUNT_PATH
    );
    let mut request = client.post(&url).json(account);
    if let Some(key) = config.proxy.api_key.as_deref() {
        request = request.header("x-api-key", key);
    }

    let response = match request.send().await {
        Ok(r) => r,
        // `is_timeout()` first: a connect TIMEOUT (a blackholed address, SYN
        // dropped rather than refused) reports `is_connect() == true` as
        // well, so checking `is_connect()` first would misclassify it as
        // `NoServer` — "nothing is listening" — when the truer reading is
        // "something is there but not answering", `NoAnswer` below.
        Err(e) if e.is_timeout() => {
            return Err(LiveControlError::NoAnswer(
                "the server did not answer within 5s".to_string(),
            ))
        }
        Err(e) if e.is_connect() => return Err(LiveControlError::NoServer),
        Err(e) => return Err(LiveControlError::NoAnswer(e.to_string())),
    };

    let status = response.status();
    // Same structural discriminator `post_set_disabled` uses: whether THIS
    // route produced the answer, decided by the header, never by status code
    // or error text alone.
    let from_route = response
        .headers()
        .get(crate::proxy::ENDPOINT_HEADER)
        .and_then(|v| v.to_str().ok())
        == Some(crate::proxy::ADD_ACCOUNT_ENDPOINT);
    let body = response.text().await.unwrap_or_default();

    if status.is_success() {
        return serde_json::from_str(&body).map_err(|e| {
            LiveControlError::Unusable(format!(
                "the response was not a tcr account-add payload ({e})"
            ))
        });
    }
    if status.as_u16() == 401 {
        return Err(LiveControlError::Unauthorized);
    }
    if !from_route && matches!(status.as_u16(), 404 | 405) {
        return Err(LiveControlError::NoRoute);
    }
    // 400: the route's own request validation (blank name/token) — the
    // positive capability signal `probe_add_capability` looks for. 409: the
    // submitted identity matched more than one live account.
    if from_route && matches!(status.as_u16(), 400 | 409) {
        return Err(LiveControlError::Rejected(
            error_message_of(&body).unwrap_or_else(|| format!("HTTP {status}")),
        ));
    }
    Err(LiveControlError::Unusable(format!("HTTP {status}")))
}

/// Ask the running proxy to set or clear the identity-bound control account.
/// Cloned from [`post_set_disabled`]: same `no_proxy()` client, same timeouts,
/// same api-key header, same `is_timeout()`-before-`is_connect()` ordering,
/// and `from_route` decided ONLY by [`crate::proxy::ENDPOINT_HEADER`] ==
/// [`crate::proxy::CONTROL_ENDPOINT`] — never by status code or error text.
///
/// `query = None` is the CLEAR request; it is sent through exactly like a
/// name, since [`crate::proxy::SetControlRequest`] treats `query: null` as a
/// complete operation on its own, not an error.
async fn post_set_control(
    config: &Config,
    query: Option<&str>,
    org: Option<&str>,
) -> Result<crate::proxy::SetControlResponse, LiveControlError> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| LiveControlError::Unusable(format!("http client: {e}")))?;

    let url = format!(
        "http://127.0.0.1:{}{}",
        config.proxy.port,
        crate::proxy::CONTROL_PATH
    );
    let mut request = client.post(&url).json(&serde_json::json!({
        "query": query,
        "org": org,
    }));
    if let Some(key) = config.proxy.api_key.as_deref() {
        request = request.header("x-api-key", key);
    }

    let response = match request.send().await {
        Ok(r) => r,
        // `is_timeout()` first — see `post_set_disabled`'s identical comment
        // on why checking `is_connect()` first would misclassify a blackholed
        // address as `NoServer`.
        Err(e) if e.is_timeout() => {
            return Err(LiveControlError::NoAnswer(
                "the server did not answer within 5s".to_string(),
            ))
        }
        Err(e) if e.is_connect() => return Err(LiveControlError::NoServer),
        Err(e) => return Err(LiveControlError::NoAnswer(e.to_string())),
    };

    let status = response.status();
    let from_route = response
        .headers()
        .get(crate::proxy::ENDPOINT_HEADER)
        .and_then(|v| v.to_str().ok())
        == Some(crate::proxy::CONTROL_ENDPOINT);
    let body = response.text().await.unwrap_or_default();

    if status.is_success() {
        return serde_json::from_str(&body).map_err(|e| {
            LiveControlError::Unusable(format!(
                "the response was not a tcr control-account payload ({e})"
            ))
        });
    }
    if status.as_u16() == 401 {
        return Err(LiveControlError::Unauthorized);
    }
    if !from_route && matches!(status.as_u16(), 404 | 405) {
        return Err(LiveControlError::NoRoute);
    }
    if from_route && matches!(status.as_u16(), 404 | 409) {
        return Err(LiveControlError::Rejected(
            error_message_of(&body).unwrap_or_else(|| format!("HTTP {status}")),
        ));
    }
    Err(LiveControlError::Unusable(format!("HTTP {status}")))
}

/// The file half of `tcr control`: resolve OFFLINE (when `query` is `Some`) →
/// persist ONLY the top-level `controlAccount` key via
/// [`config::save_control_account`] — mirroring [`write_disabled_flag`]'s
/// shape, but never a whole-config [`config::save`]; see
/// `save_control_account`'s doc-comment and `1d978ce` for why that clobbers
/// out-of-band keys.
fn write_control_account(
    config_path: &Path,
    query: Option<&str>,
    org: Option<&str>,
) -> anyhow::Result<Option<String>> {
    let config = load_for_edit(config_path)?;
    let name = match query {
        None => None,
        Some(q) => Some(
            config.accounts[resolve_account(&config.accounts, q, org)?]
                .name
                .clone(),
        ),
    };
    config::save_control_account(config_path, name.as_deref())
        .with_context(|| format!("save config at {}", config_path.display()))?;
    Ok(name)
}

/// Set or clear the identity-bound control account, **in the running proxy
/// first** — same posture as [`set_enabled`], and for the same reason: a
/// file-only write would leave the running process still resolving identity
/// traffic to its OLD control account (or none) until it restarts.
pub async fn set_control(
    config_path: &Path,
    query: Option<&str>,
    org: Option<&str>,
) -> anyhow::Result<()> {
    // A config we cannot read has no port and no api-key to reach a server
    // with; fall through to the file path, which reports the load failure as
    // it always has. Never a silent skip of the live attempt for any other
    // reason.
    if let Ok(config) = config::load(config_path) {
        match post_set_control(&config, query, org).await {
            Ok(applied) => {
                match &applied.name {
                    Some(name) => println!("Set control account to '{name}'."),
                    None => println!("Cleared the control account."),
                }
                if let Some(warning) = &applied.warning {
                    eprintln!("[tcr] warning: {warning}");
                }
                return Ok(());
            }
            // Nothing is listening: the historical case, and the only quiet
            // one. There is no live rotation to disagree with the file.
            Err(LiveControlError::NoServer) => {}
            // A server is there and REFUSED us. Writing the file here would
            // be the old lie in a new place: the config would name a new
            // control account while the running proxy — the one that actually
            // resolves identity traffic — keeps using its old one (or none).
            Err(LiveControlError::Unauthorized) => {
                bail!(
                    "the proxy on :{} rejected the api-key in {} — the config was NOT changed, because writing it would leave the running proxy still resolving identity traffic against its old control account. Fix `proxy.apiKey` and retry.",
                    config.proxy.port,
                    config_path.display()
                );
            }
            // The route is missing: an older tcr is serving. The file write
            // is all we can do, and it is HALF a control-account change — say
            // so loudly.
            Err(LiveControlError::NoRoute) => {
                write_control_account(config_path, query, org)?;
                eprintln!(
                    "[tcr] WARNING: the proxy running on :{} is too old to accept live account control (no {} route), so only the config file was changed. It will KEEP its OLD control account until it restarts. Run `tcr restart` when a cold prompt cache is acceptable.",
                    config.proxy.port,
                    crate::proxy::CONTROL_PATH,
                );
                return Ok(());
            }
            // The route ran and refused the QUERY: it matched no live
            // account, or matched several. Do not fall back — the file's own
            // offline resolution could land on a different account than the
            // one the server was talking about.
            Err(LiveControlError::Rejected(message)) => {
                bail!(
                    "the proxy running on :{} refused this: {message} Nothing was changed.",
                    config.proxy.port
                );
            }
            // It answered something we cannot use, or did not answer at all.
            Err(other) => {
                write_control_account(config_path, query, org)?;
                eprintln!(
                    "[tcr] WARNING: could not apply this to the proxy running on :{} ({}), so only the config file was changed. It may KEEP its OLD control account until it restarts.",
                    config.proxy.port,
                    other.why(),
                );
                return Ok(());
            }
        }
    }

    let name = write_control_account(config_path, query, org)?;
    match name {
        Some(name) => println!("Set control account to '{name}'."),
        None => println!("Cleared the control account."),
    }
    Ok(())
}

/// `tcr control --show` — print the current control account, preferring the
/// LIVE server's answer (it may differ from the file if the server has not
/// been restarted since a config edit) and falling back to the file.
pub async fn show_control(config_path: &Path) -> anyhow::Result<()> {
    let config = config::load(config_path)
        .with_context(|| format!("load config at {}", config_path.display()))?;
    let control = match fetch_live_status(&config).await {
        Ok(payload) => payload.control,
        Err(_) => config.control_account.clone(),
    };
    match control {
        Some(name) => println!("{name}"),
        None => println!("(none)"),
    }
    Ok(())
}

/// Pull `error.message` out of the proxy's standard error envelope.
fn error_message_of(body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()?
        .get("error")?
        .get("message")?
        .as_str()
        .map(str::to_string)
}

/// Build an OFFLINE snapshot from `config`: a [`Manager`] with `config_path =
/// None` (so a probe's token refresh is NEVER written to disk) AND a [`NoRefresh`]
/// refresher (so a probe NEVER performs an OAuth refresh at all), optionally
/// probing every account's live quota first.
///
/// The no-refresh guarantee is load-bearing and independent of the no-persist one:
/// refresh tokens are SINGLE-USE, so a refresh from this second process would
/// revoke the running server's copy and kill the account — even though we would
/// never write the rotated token back. Accounts with a valid access token probe
/// normally; expired ones surface a visible probe error instead of refreshing.
///
/// Split out from [`list_accounts`] / [`status`] so tests can inject a scripted
/// [`UsageProber`] and a scripted [`TokenRefresher`] — asserting both that the
/// on-disk config is byte-unchanged (the no-race guarantee) and that an expired
/// account is handled by the injected refresher, never a hidden [`LiveRefresher`]
/// (the no-refresh guarantee). Production callers pass [`NoRefresh`].
pub async fn snapshot_offline(
    config: Config,
    refresher: Arc<dyn TokenRefresher>,
    prober: Arc<dyn UsageProber>,
    probe: bool,
) -> StatsSnapshot {
    // config_path = None → persist_now / persist_tokens are silent no-ops, and the
    // caller's refresher (production: NoRefresh) can never reach the OAuth token
    // endpoint, so a probe here can neither mutate the user's file nor revoke the
    // live server's tokens.
    let manager = Manager::new(
        config,
        refresher,
        prober,
        Arc::new(crate::warmer::LiveWarmer::new()),
        None,
    );
    if probe {
        manager.probe_all().await;
    }
    manager.snapshot(OffsetDateTime::now_utc())
}

/// Gating quota for one account row: the most-spent of the three known windows
/// (`5h`, `7d`, `7d_oi`), matching the rotation eligibility gate. `None` when
/// nothing has been learned yet.
///
/// NOTE: this is deliberately 3-window and stays cli-scoped — do NOT unify with
/// manager.rs's 2-window eligibility gate (`[five_hour, seven_day]`), which
/// intentionally excludes `7d_oi` to mirror `eligible`'s dims. The display gate
/// and the routing gate are different by design.
fn gating_quota(a: &AccountSnapshot) -> Option<f64> {
    [a.five_hour, a.seven_day, a.seven_day_oi]
        .into_iter()
        .flatten()
        .reduce(f64::max)
}

/// Per-account gating threshold — the account's `switchThreshold`, else the global
/// one — collected in config order (which is the snapshot's account order), so the
/// held-window display gates on exactly the threshold `Manager::eligible` routes on.
fn resolve_thresholds(config: &Config) -> Vec<f64> {
    config
        .accounts
        .iter()
        .map(|a| a.switch_threshold.unwrap_or(config.switch_threshold))
        .collect()
}

/// A gating window (5h or 7d) currently holding an account out of rotation: its
/// effective utilization is at/over the account's threshold and it still resets in
/// the future. Mirrors `Quota::is_near`'s dimensions (5h + 7d; the model-scoped
/// `7d_oi` never gates shared rotation, so it is not shown as a hold here).
struct HeldWindow {
    label: &'static str,
    reset: OffsetDateTime,
}

/// The windows holding account `a` out of rotation, evaluated against its own
/// `threshold`. `a.five_hour`/`a.seven_day` are already the EFFECTIVE utilizations
/// and `*_reset` the live (future-only) resets — both baked in at snapshot time —
/// so a held window always carries a future reset to show.
fn held_windows(a: &AccountSnapshot, threshold: f64) -> Vec<HeldWindow> {
    let mut held = Vec::new();
    for (label, util, reset) in [
        ("5h", a.five_hour, a.five_hour_reset),
        ("7d", a.seven_day, a.seven_day_reset),
    ] {
        if let (Some(u), Some(reset)) = (util, reset) {
            if u >= threshold {
                held.push(HeldWindow { label, reset });
            }
        }
    }
    held
}

/// A window's reset as a bare `HH:MMZ` wall clock. Rendered in UTC (the `Z` is
/// explicit): local-time rendering would need the `time` crate's `local-offset`
/// feature, which is unset and is unsound under the multi-threaded runtime anyway —
/// the countdown beside it is timezone-independent and carries the actionable "when".
fn format_reset_clock(reset: OffsetDateTime) -> String {
    format!("{:02}:{:02}Z", reset.hour(), reset.minute())
}

/// Countdown from `now` to a future `reset`, one unit: minutes under two hours
/// (`+92m`), hours under two days (`+5h`), else days (`+3d`). Clamped at `+0m` so a
/// reset that just elapsed never reads negative.
fn format_reset_countdown(reset: OffsetDateTime, now: OffsetDateTime) -> String {
    let mins = (reset - now).whole_minutes().max(0);
    if mins < 120 {
        format!("+{mins}m")
    } else if mins < 60 * 48 {
        format!("+{}h", mins / 60)
    } else {
        format!("+{}d", mins / (60 * 24))
    }
}

/// The ` held=<win> resets=<HH:MMZ>(<+countdown>)` segment(s) for an account's
/// held windows — one per held window, empty when the account holds none.
///
/// Retained deliberately: the text status line now renders per-window inline
/// `(+countdown)` tokens instead, but `held_windows` still backs the JSON
/// `held` array and this suffix stays available for that parity / future reuse.
#[allow(dead_code)]
fn held_suffix(a: &AccountSnapshot, threshold: f64, now: OffsetDateTime) -> String {
    let mut s = String::new();
    for h in held_windows(a, threshold) {
        s.push_str(&format!(
            " held={} resets={}({})",
            h.label,
            format_reset_clock(h.reset),
            format_reset_countdown(h.reset, now),
        ));
    }
    s
}

/// A short, grep-matchable token for an account's live [`QuotaState`] — surfaced
/// on the status line and in the JSON row so a held-but-has-headroom account
/// (`near`) is never mistaken for a truly spent one (`spent`). `ok` is an
/// in-rotation account comfortably under its threshold.
fn quota_state_token(state: QuotaState) -> &'static str {
    match state {
        QuotaState::Normal => "ok",
        QuotaState::NearLimit => "near",
        QuotaState::Exhausted => "spent",
    }
}

/// Render one quota window as a `key=value` token: `5h=100%(+93m)`, `wk=67%`, or
/// `5h=n/a`. The inline `(+countdown)` appears whenever the window has a live
/// (future) reset — a window's reset is a property of the window, not of
/// whether it is currently a binding hold, so it is no longer gated on the
/// account's threshold.
fn render_window(
    label: &str,
    util: Option<f64>,
    reset: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> String {
    match util {
        None => format!("{label}=n/a"),
        Some(u) => {
            let hold = match reset {
                Some(r) if r > now => format!("({})", format_reset_countdown(r, now)),
                _ => String::new(),
            };
            format!("{label}={:.0}%{hold}", u * 100.0)
        }
    }
}

/// Minimum served requests before [`cache_hit_ratio`] reports a number rather
/// than no-signal.
///
/// Grounded in two read measurements, not taste: `docs/plans/divert-budget-swarm-state.md`
/// records the median distinct divert destinations per hold episode as 2.0, so
/// an account whose only traffic is "a few cold diverts" sits at single-digit
/// requests; a live read of the running fleet on 2026-08-15 (`tcr status
/// --json`) showed every account with a real, sustained cache ratio at 376+
/// requests, three orders of magnitude above that. 20 sits comfortably above
/// the divert-only noise floor and nowhere near real serving volume, so it
/// cannot suppress an actual measurement while it reliably catches the
/// near-empty-denominator case this constant exists to silence. This is a
/// design judgment (σ2 — architecturally grounded in real data, not a
/// controlled A/B against the incident it targets), not a derived fact.
const CACHE_SIGNAL_FLOOR_REQUESTS: u64 = 20;

/// The account's prompt-cache hit ratio, or `None` when there is nothing
/// reliable to report — never a placeholder number.
///
/// Two distinct reasons collapse to the same `None`, deliberately: nothing to
/// divide by (`input_tokens == 0` — the offline / never-served case, the
/// original honesty fix) and too little served to trust the ratio
/// (`requests` under [`CACHE_SIGNAL_FLOOR_REQUESTS`] — an account whose only
/// real traffic was a handful of cold diverts, which is what "the low
/// `cacheHitRatio` means the cache is broken" falsely read as). Both are "not
/// a measurement", and the wire's existing contract — `null`, never a
/// placeholder `0.0` — already reads as unambiguously distinct from a real 0%
/// to every consumer (`offline_status_reports_null_not_zero_hit_ratio`), so
/// folding the floor into the same `None` needs no new field on the wire: a
/// no-signal account and an offline account both render `"cacheHitRatio":
/// null`, and a consumer that already treats null as "not measured" handles
/// both without a code change.
///
/// The ratio itself is unchanged from the original fix — `cache_read_tokens /
/// input_tokens`. `input_tokens`/`cache_read_tokens` are populated
/// exclusively from served-request usage (`Manager::update_usage`, called
/// only from real proxied responses in `proxy.rs`); the background quota
/// prober and keep-warm path update `apply_usage`/`update_quota` instead,
/// which never touch these counters (verified by reading every call site,
/// 2026-08-15). So the denominator was already inference-only traffic before
/// this change — the false "cache is broken" signal was a sample-size
/// problem, not a wrong-traffic-class problem, and the floor above is the
/// fix for it.
///
/// Takes `source` and checks it FIRST, ahead of the floor: an offline
/// snapshot's counters are structurally zero regardless of `requests`, so the
/// floor check alone happened to null them too — but relying on that
/// coincidence would leave the two guards silently redundant instead of each
/// covering the case it names. Checking `source` first also makes the
/// contract order-independent of any future change to what "offline" counts
/// as zero: the moment `source` says "not the serving process", nothing else
/// about the row's numbers matters.
fn cache_hit_ratio(a: &AccountSnapshot, source: StatusSource) -> Option<f64> {
    if source == StatusSource::Offline {
        return None;
    }
    if a.input_tokens == 0 || a.requests < CACHE_SIGNAL_FLOOR_REQUESTS {
        return None;
    }
    Some(a.cache_read_tokens as f64 / a.input_tokens as f64)
}

/// Render a [`StatsSnapshot`] as plain text — ONE LINE PER ACCOUNT — so the
/// output is greppable (`account NAME priority=P quota=Q% status=S ...`). The
/// ratatui TUI renderer is unusable for stdout, and per Gil's greppable-output
/// rule a naive grep must be able to match any single field.
///
/// `source` is taken rather than inferred because one token depends on it:
/// `stream_errors` is a SERVING counter, so an offline snapshot's zero is
/// structurally unmeasured (see [`StatusSource`]) and renders `n/a`, never `0`.
///
/// Takes no per-account threshold: the inline `(+countdown)` on each window
/// token is now a function of "does this window have a live reset", not of
/// whether it is a binding hold, so there is nothing left in this renderer to
/// gate on it. `held_windows`/`held[]` (JSON) keep their own threshold — a
/// different question this text line never answered.
pub fn render_accounts(snapshot: &StatsSnapshot, source: StatusSource) -> String {
    if snapshot.accounts.is_empty() {
        return "no accounts configured\n".to_string();
    }
    let now = OffsetDateTime::now_utc();
    let mut out = String::new();
    for a in snapshot.accounts.iter() {
        let five_hour = render_window("5h", a.five_hour, a.five_hour_reset, now);
        let seven_day = render_window("wk", a.seven_day, a.seven_day_reset, now);
        // Fable's model-scoped weekly never gates the general view: no reset field,
        // no countdown, and the whole token is omitted when it was never learned.
        let fable = match a.seven_day_oi {
            Some(u) => format!(" fable={:.0}%", u * 100.0),
            None => String::new(),
        };
        // Prompt-cache hit ratio. `n/a` — never `0%` — when there is no signal
        // to report (R3: no NaN either): see [`cache_hit_ratio`] for the two
        // cases that collapse to it, matching the `5h=n/a` / `wk=n/a` idiom.
        // An OFFLINE snapshot's counters live in the server's process and are
        // structurally zero here, and rendering that as a measured 0% is
        // exactly the lie that hid a real prompt-cache catastrophe — same
        // reasoning extends to a live account whose only traffic was a
        // handful of cold diverts. Greppable `cache=NN%` token for parity
        // with the JSON field.
        let cache = match cache_hit_ratio(a, source) {
            None => " cache=n/a".to_string(),
            Some(ratio) => format!(" cache={:.0}%", ratio * 100.0),
        };
        // Stream failures this account's streams carried (decayed window): an
        // in-band SSE `error` event, or a stream that hit EOF without Anthropic's
        // `message_stop` terminator (recorded as `"truncated"`). This is the only
        // place that class of failure is visible at all.
        //
        // `n/a` — never `0` — on an OFFLINE snapshot, for the same reason
        // `cache=n/a` exists: the counter lives in the serving process, so a fresh
        // one reads structurally zero, and "0 stream errors" is an affirmative
        // all-clear in a way "0 requests" is not. Publishing an unmeasured
        // all-clear on an error counter is precisely the failure this field was
        // added to end.
        let stream_errors = match source {
            StatusSource::Offline => " stream_errors=n/a".to_string(),
            StatusSource::Live => format!(" stream_errors={}", a.stream_error_count),
        };
        // The latest error's `error.type`, alongside the count. Omitted entirely
        // when none has been seen, matching the `fable=` idiom — the count above
        // already distinguishes "none" from "not measured".
        let last_stream_error = match (source, a.last_stream_error.as_deref()) {
            (StatusSource::Live, Some(kind)) => format!(" last_stream_error={kind}"),
            _ => String::new(),
        };
        // WHEN it comes back — the question `status=` cannot answer. `free_at` is
        // the instant ALL of this account's active gates clear, so it never
        // promises a return the weekly cap will not honour.
        //
        // Omitted (not `n/a`) when there is no instant to name, matching the
        // `fable=`/`last_stream_error=` idiom: a terminal gate (REJECTED / LOGIN /
        // disabled) is cleared only by a human, and a gate whose reset upstream
        // never reported has no time to give. Absent means "no time can be
        // promised" — printing `free_in=0s` there would read as "returns now".
        let free_in = match a.free_at {
            Some(f) if f > now => format!(" free_in={}s", (f - now).whole_seconds().max(1)),
            _ => String::new(),
        };
        // Group labels, greppable via `groups=codereview,dev`. Omitted entirely
        // when the account carries none, matching the `fable=`/`last_stream_error=`
        // idiom for "nothing to say" rather than printing `groups=`.
        let groups = if a.groups.is_empty() {
            String::new()
        } else {
            format!(" groups={}", a.groups.join(","))
        };
        out.push_str(&format!(
            "account {} priority={} {} {}{}{} state={} status={}{} probe={}{}{}{}{}\n",
            a.name,
            a.priority,
            five_hour,
            seven_day,
            fable,
            cache,
            quota_state_token(a.quota_state),
            a.status,
            free_in,
            a.probe_status.as_str(),
            stream_errors,
            last_stream_error,
            if a.disabled { " disabled" } else { "" },
            groups,
        ));
    }
    out
}

/// Where a rendered fleet view's numbers came from.
///
/// The distinction is not cosmetic. Only [`StatusSource::Live`] carries real
/// serving counters (requests, tokens, cache hit ratio) — those live in the
/// running proxy's process. [`StatusSource::Offline`] is a fresh process's view:
/// its quota bars are real (they come from a live probe) but every counter is
/// structurally zero because nothing has served through it. Labelling the two
/// apart is the whole point: a structurally-zero counter must never again be
/// mistaken for a measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusSource {
    /// Read from the running proxy's `/_tcr/status` endpoint.
    Live,
    /// Computed in this process from the config, with no server to ask.
    Offline,
}

impl StatusSource {
    fn as_str(self) -> &'static str {
        match self {
            StatusSource::Live => "live",
            StatusSource::Offline => "offline",
        }
    }
}

/// Render a [`StatsSnapshot`] as a JSON array, one object per account.
///
/// `source` and `serverSha` are stamped on EVERY row rather than wrapped around
/// the array: the output has always been a bare array, and a `jq '.[].name'`
/// that works today must keep working. One row is one account is one line to
/// grep.
///
/// `server` is `None` on the offline path, where there is no serving process
/// whose build could be reported — `serverSha` is then `null`, the same
/// "not measured" idiom `cacheHitRatio` uses, never a placeholder that reads
/// like a real sha.
fn render_accounts_json(
    snapshot: &StatsSnapshot,
    thresholds: &[f64],
    source: StatusSource,
    server: Option<&BuildInfo>,
    http1_only: bool,
    control: Option<&str>,
) -> String {
    let now = OffsetDateTime::now_utc();
    let rows: Vec<serde_json::Value> = snapshot
        .accounts
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let quota = gating_quota(a);
            let threshold = thresholds.get(i).copied().unwrap_or(1.0);
            let held: Vec<serde_json::Value> = held_windows(a, threshold)
                .into_iter()
                .map(|h| {
                    serde_json::json!({
                        "window": h.label,
                        "resetAtMs": (h.reset.unix_timestamp_nanos() / 1_000_000) as i64,
                        "minutesUntilReset": (h.reset - now).whole_minutes().max(0),
                    })
                })
                .collect();
            serde_json::json!({
                "source": source.as_str(),
                // Which build produced these numbers. A script watching this
                // output can diff it against `git rev-parse --short HEAD` and
                // know, without an `lsof`, whether the server predates the fix
                // it is verifying.
                "serverSha": server.map(|b| b.sha.as_str()),
                "serverDirty": server.and_then(|b| b.dirty),
                // Server-wide, repeated per row like `serverSha` above — whether
                // this server's upstream clients are forced onto HTTP/1.1. See
                // `Config::http1_only` and `Manager::http1_only`.
                "http1Only": http1_only,
                "name": a.name,
                "priority": a.priority,
                "status": a.status,
                "disabled": a.disabled,
                // Whether THIS row is the identity-bound control account
                // (`Config::control_account` / `Manager::control_name`). Reported
                // on the offline path too, deliberately — it is a config fact,
                // not a serving counter, so it does NOT get the
                // null-when-offline treatment `streamErrorCount` gets below;
                // see `StatusPayload::control`'s doc-comment for why.
                "control": control == Some(a.name.as_str()),
                "quota": quota,
                "quotaState": quota_state_token(a.quota_state),
                // The server's own terminal-gate verdict
                // (`Manager::account_terminal_gate`/`account_gate`), including the
                // one TcrBar could not otherwise see: `rejected` (Anthropic's own
                // `anthropic-ratelimit-unified-status: rejected` header). Before
                // this field existed, `status` was rewritten only for `Throttled`
                // (see `snapshot.rs`), so a rejected account still read `status:
                // "active"` and the panel drew it as eligible when the router will
                // never select it. Kebab-case values ("ok", "hold", "five-hour",
                // "seven-day", "fable-weekly", "standard", "login", "rejected",
                // "disabled") via `GateReason`'s own `Serialize`. New field, so an
                // older reader on either end is unaffected: a client built before
                // this shipped simply never looks at it.
                "gate": a.gate,
                "fiveHour": a.five_hour,
                "sevenDay": a.seven_day,
                "sevenDayOi": a.seven_day_oi,
                // Per-window state, additive alongside the existing combined
                // `quotaState` above (which stays the most-spent-of-both gating
                // verdict and is UNCHANGED by this field's addition — nothing
                // that reads `quota`/`quotaState` today observes a different
                // value). `null` when that window has no reading yet — same
                // "not measured" idiom as `serverSha`/`cacheHitRatio` elsewhere
                // in this row, never a fabricated "ok". Lets TcrBar tint each of
                // the two per-window quota bars independently instead of both
                // by the shared gating state, so a 7d-red account with an empty
                // 5h window doesn't paint its 5h bar red.
                "fiveHourState": a.five_hour.map(|u| {
                    quota_state_token(crate::stats::QuotaState::from_utilization(Some(u), threshold))
                }),
                "sevenDayState": a.seven_day.map(|u| {
                    quota_state_token(crate::stats::QuotaState::from_utilization(Some(u), threshold))
                }),
                // A window's reset is a property of the window, not of whether
                // it is currently pinning the account — UNCONDITIONAL, no
                // threshold gate. `held[]` below stays exactly as it is; it
                // answers a different question ("which window is holding this
                // account out of rotation"). `null` — never `0`, never a
                // fabricated instant — is the same "not measured" idiom as
                // `serverSha`/`cacheHitRatio`/`fiveHourState` above, and
                // collapses two distinct honest cases: the window's reset has
                // already elapsed with nothing learned since, or no reset was
                // ever learned. No `minutesUntilReset` companion (unlike
                // `held[]`) — a server-computed relative figure goes stale
                // between polls, so the client derives the countdown from the
                // timestamp against its own clock instead.
                "fiveHourResetAtMs": a.five_hour_reset.map(|r| (r.unix_timestamp_nanos() / 1_000_000) as i64),
                "sevenDayResetAtMs": a.seven_day_reset.map(|r| (r.unix_timestamp_nanos() / 1_000_000) as i64),
                // `null` — NEVER `0` — on the OFFLINE path, same idiom as
                // `streamErrorCount`/`cacheHitRatio` below and for the same
                // reason: these four are pure serving counters that live in
                // the SERVING process, so a fresh offline `Manager` reads
                // them structurally zero. That used to render as a real `0`,
                // and offline is not only the no-server case — `fetch_live_status`
                // falls back to it on NoAnswer/Unusable too (a slow, wedged,
                // or restarting proxy), which are ordinary states here and
                // ones a human or a `jq` one-liner polling this JSON every few
                // seconds would misread as "this account served nothing"
                // instead of "not measured right now". `fiveHour`/`sevenDay`/
                // `quota` above are deliberately NOT guarded the same way —
                // they come from a live probe the offline path genuinely runs
                // itself, not from the serving Manager's counters, so they are
                // real numbers on both paths.
                "requests": match source {
                    StatusSource::Offline => serde_json::Value::Null,
                    StatusSource::Live => serde_json::json!(a.requests),
                },
                "inputTokens": match source {
                    StatusSource::Offline => serde_json::Value::Null,
                    StatusSource::Live => serde_json::json!(a.input_tokens),
                },
                "outputTokens": match source {
                    StatusSource::Offline => serde_json::Value::Null,
                    StatusSource::Live => serde_json::json!(a.output_tokens),
                },
                "cacheReadTokens": match source {
                    StatusSource::Offline => serde_json::Value::Null,
                    StatusSource::Live => serde_json::json!(a.cache_read_tokens),
                },
                // Prompt-cache hit ratio (0.0-1.0): cache_read / input_total, and
                // `null` — NEVER a literal 0.0 — when there is no reliable signal:
                // offline (see the four fields above), or `inputTokens` is 0 and
                // there is nothing to divide by (also keeps NaN out — R3), or
                // `requests` sits under `CACHE_SIGNAL_FLOOR_REQUESTS` and the
                // ratio would be measuring a near-empty denominator. See
                // [`cache_hit_ratio`], which checks `source` FIRST.
                //
                // The null is the honesty fix, twice over. `source: "offline"`
                // means these counters come from a fresh process, not the serving
                // one, so they are structurally zero; emitting `0.0` there
                // published an unmeasured number as a measured "0% cache hits" for
                // every account forever, and that false zero is precisely why a
                // real prompt-cache catastrophe went unseen. The same false
                // reading recurs on a LIVE account whose only traffic was a
                // handful of cold diverts — a real but statistically meaningless
                // low ratio reads exactly like "the cache is broken" to anyone
                // watching this field. `null` says "not measured"; `source` says
                // which process would have measured it, when it could have.
                "cacheHitRatio": match cache_hit_ratio(a, source) {
                    None => serde_json::Value::Null,
                    Some(ratio) => serde_json::json!(ratio),
                },
                // When the background quota probe last ran for this account
                // (`AccountSnapshot::last_probe`), Unix milliseconds like every
                // other timestamp on this wire. Already crosses the
                // server<->CLI boundary (`status.rs`'s `AccountStatus`
                // reconstructs it) and the TUI already renders it
                // (`tui.rs`'s `ok 45s` age column) — this was simply never
                // added to the JSON output. It matters here specifically: an
                // idle account's quota probe runs on a randomized ~300s
                // cadence, but TcrBar polls this JSON every ~3s, so without
                // this field a stale probe (proxy wedged, probe target
                // unreachable) is invisible to any consumer — the quota bars
                // just silently stop moving with no timestamp to notice by.
                // Real on BOTH paths, like `fiveHour`/`quota` above: the
                // offline path runs its own live probe, so this is not a
                // serving counter and gets no null-on-offline guard.
                "lastProbeMs": a
                    .last_probe
                    .map(|t| (t.unix_timestamp_nanos() / 1_000_000) as i64),
                "probeStatus": a.probe_status.as_str(),
                "probeError": a.probe_error,
                // Decayed count of stream failures — an in-band SSE `error` event,
                // or a stream that hit EOF without `message_stop` (recorded as
                // `"truncated"`). `null`, NEVER 0, on the offline
                // path: the counter lives in the serving process, so a fresh one
                // is structurally zero, and an unmeasured `0` on an ERROR counter
                // publishes an all-clear nobody measured. Same "not measured"
                // idiom as `cacheHitRatio`; `source` says which process would
                // have measured it.
                "streamErrorCount": match source {
                    StatusSource::Offline => serde_json::Value::Null,
                    StatusSource::Live => serde_json::json!(a.stream_error_count),
                },
                // The latest error's `error.type` (e.g. "overloaded_error"), or
                // null when none has been observed / nothing measured.
                "lastStreamError": match source {
                    StatusSource::Offline => None,
                    StatusSource::Live => a.last_stream_error.clone(),
                },
                "held": held,
                // WHEN THE ACCOUNT COMES BACK — the question `status` cannot
                // answer. `status: "throttled"` is a state; this is the instant.
                //
                // It lives here rather than in the TUI's Status cell because that
                // column is Length(9) and "throttled" is exactly 9 characters, so
                // a countdown there is clipped at every terminal width; and it is
                // `free_at`, not the raw hold, because free_at is the instant ALL
                // of this account's active gates clear. Rendering the hold alone
                // would promise a return the weekly cap will not honour.
                //
                // Null is honest and load-bearing: `account_gate` reports no
                // instant for a terminal state (REJECTED / LOGIN / disabled —
                // only a human clears those) and for a gate whose reset upstream
                // never reported. Absent means "no time can be promised", never
                // "returns now".
                "freeAtMs": a.free_at.map(|f| (f.unix_timestamp_nanos() / 1_000_000) as i64),
                "secondsUntilFree": a
                    .free_at
                    .filter(|f| *f > now)
                    .map(|f| (f - now).whole_seconds()),
                // The 429 hold specifically, so an operator can tell a short
                // transient park from a week-scale quota cap without inferring it
                // from `gate`.
                "rateLimitedUntilMs": a
                    .rate_limited_until
                    .map(|t| (t.unix_timestamp_nanos() / 1_000_000) as i64),
                // Group labels (config fact, not a serving counter) — always an
                // array, `[]` when unlabelled, real on BOTH paths like `control`
                // above. Never `null`: "this account has no groups" is known,
                // not unmeasured.
                "groups": a.groups,
            })
        })
        .collect();
    serde_json::to_string_pretty(&rows).unwrap_or_else(|_| "[]".to_string())
}

/// `tcr accounts [--probe]` — list configured accounts (offline). With
/// `--probe`, first refresh every account's live quota (never persisted).
pub async fn list_accounts(config_path: &Path, probe: bool) -> anyhow::Result<()> {
    // Read-only verb: plain load, no clobber-warning (we never save).
    let config = config::load(config_path)
        .with_context(|| format!("load config at {}", config_path.display()))?;
    let snapshot = snapshot_offline(
        config,
        Arc::new(NoRefresh),
        Arc::new(LiveUsageProber::new()),
        probe,
    )
    .await;
    // `tcr accounts` is the offline verb by construction — it builds its own
    // Manager and never asks the server — so its serving counters are structurally
    // zero and must render as unmeasured, not as a measurement.
    print!("{}", render_accounts(&snapshot, StatusSource::Offline));
    Ok(())
}

/// Why a live status read did not produce a payload.
///
/// The three variants are not interchangeable, and collapsing them is what made
/// a wedged proxy unrecoverable: [`incumbent_liveness`] turns them into
/// "is anything actually serving on this port", and that answer is the
/// difference between "leave a working proxy alone" and "this process is holding
/// the socket and answering nothing".
enum LiveStatusError {
    /// Nothing is listening on the configured port — the ORDINARY case (`tcr
    /// status` with no server running). Falling back is expected, so it is
    /// reported by the `offline` label alone rather than by a warning.
    NoServer,
    /// Something is listening but produced no response before the deadline: no
    /// bytes back within the 2s connect / 5s total budget, or the connection
    /// dropped mid-read. This is the WEDGED shape.
    NoAnswer(String),
    /// A server answered — an HTTP response came back — but the read was not
    /// usable: a rejected api-key, an older tcr with no status route, a payload
    /// that is not ours. The process is SERVING; we just cannot read it. Always
    /// warned about: a silently-swallowed rejection here would look exactly like
    /// "no server", which is how an api-key typo becomes a mysterious all-zero
    /// status.
    Unusable(String),
}

impl LiveStatusError {
    /// Did an HTTP response come back at all?
    ///
    /// The load-bearing distinction for the startup stand-down. A 401 or an
    /// unparseable body is a process that ACCEPTED the connection, routed the
    /// request and wrote a response — the opposite of wedged — while a timeout
    /// means the listening socket is all that is left of it.
    fn answered(&self) -> bool {
        matches!(self, LiveStatusError::Unusable(_))
    }

    fn why(&self) -> String {
        match self {
            LiveStatusError::NoServer => "nothing is listening".to_string(),
            LiveStatusError::NoAnswer(why) | LiveStatusError::Unusable(why) => why.clone(),
        }
    }
}

/// Whether the incumbent holding the port is actually serving anything.
///
/// Deliberately NOT `Option<BuildInfo>::is_none()`. That conflates four states,
/// two of which are a perfectly healthy proxy: an api-key mismatch and an older
/// `tcr` with no status route both answer, and killing either would be a
/// takeover of a live server on the strength of a diagnostic read failing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Liveness {
    /// It answered. Whatever else is true, it is serving requests.
    Answering,
    /// Nothing came back before the deadline. It holds the socket and serves
    /// nothing; `why` is the probe's own account of what happened, for the
    /// operator-facing line.
    Silent { why: String },
}

/// Read the live fleet snapshot from the running proxy's [`crate::proxy::STATUS_PATH`].
///
/// Sends the configured proxy api-key: unlike the forwarding path, the status
/// endpoint has no loopback exemption. Only a body that deserializes AND names
/// the exact [`STATUS_KIND`] is accepted — a tcr built before the endpoint
/// existed has no such route, so it forwards this path UPSTREAM and hands back
/// Anthropic's own error JSON, which must never be rendered as a fleet status.
async fn fetch_live_status(config: &Config) -> Result<StatusPayload, LiveStatusError> {
    let client = reqwest::Client::builder()
        // Never route our own loopback read through a system proxy: `HTTP_PROXY`
        // very commonly points AT tcr, so honouring it would send this query
        // through the endpoint it is asking about.
        .no_proxy()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| LiveStatusError::Unusable(format!("http client: {e}")))?;

    let url = format!(
        "http://127.0.0.1:{}{}",
        config.proxy.port,
        crate::proxy::STATUS_PATH
    );
    let mut request = client.get(&url);
    if let Some(key) = config.proxy.api_key.as_deref() {
        request = request.header("x-api-key", key);
    }

    let response = match request.send().await {
        Ok(r) => r,
        Err(e) if e.is_connect() => return Err(LiveStatusError::NoServer),
        Err(e) if e.is_timeout() => {
            return Err(LiveStatusError::NoAnswer(
                "the server did not answer within 5s".to_string(),
            ))
        }
        // No response object came back, and it was neither a refused connect nor
        // the deadline: a reset or a truncated read. Nothing was served.
        Err(e) => return Err(LiveStatusError::NoAnswer(e.to_string())),
    };

    let status = response.status();
    if !status.is_success() {
        let hint = if status.as_u16() == 401 {
            " — the proxy api-key in the config was rejected"
        } else {
            ""
        };
        return Err(LiveStatusError::Unusable(format!("HTTP {status}{hint}")));
    }

    let body = response
        .text()
        .await
        .map_err(|e| LiveStatusError::Unusable(format!("reading the response body: {e}")))?;
    let payload: StatusPayload = serde_json::from_str(&body).map_err(|e| {
        LiveStatusError::Unusable(format!(
            "the response was not a tcr status payload ({e}) — an older tcr forwards this path upstream"
        ))
    })?;
    if payload.kind != STATUS_KIND {
        return Err(LiveStatusError::Unusable(format!(
            "unexpected payload kind '{}' (expected '{STATUS_KIND}')",
            payload.kind
        )));
    }
    Ok(payload)
}

/// One probe of the incumbent on the port, answering BOTH questions the startup
/// stand-down has to decide on.
///
/// * `build` — which commit it is executing, so the user learns whether the
///   binary they just compiled is the one serving. `None` on any failure to read
///   it; this is a diagnostic on a path that is already exiting.
/// * `liveness` — whether it responded AT ALL. Separate from `build` on purpose:
///   `build == None` is true for a healthy proxy behind a rejected api-key and
///   for one wedged solid, and the stand-down must not treat those alike.
///
/// One probe, not two, so the decision and the message can never disagree about
/// what the incumbent did. Bounded by [`fetch_live_status`]'s own 2s connect /
/// 5s total timeouts, so it cannot hang.
pub struct IncumbentProbe {
    pub build: Option<BuildInfo>,
    pub liveness: Liveness,
}

pub async fn probe_incumbent(config: &Config) -> IncumbentProbe {
    match fetch_live_status(config).await {
        Ok(payload) => IncumbentProbe {
            build: Some(payload.build),
            liveness: Liveness::Answering,
        },
        Err(err) => IncumbentProbe {
            build: None,
            liveness: if err.answered() {
                Liveness::Answering
            } else {
                Liveness::Silent { why: err.why() }
            },
        },
    }
}

/// `tcr status [--json]` — render the fleet as greppable text or a JSON array,
/// preferring the RUNNING proxy's own numbers.
///
/// The live endpoint is tried first because it is the only place the serving
/// counters exist: requests, tokens and the prompt-cache hit ratio are per-process
/// state, and an offline snapshot's copies are structurally zero. When no server
/// answers we fall back to the historical offline path — a fresh `Manager` plus a
/// live quota probe, never persisted — and every rendering says which of the two
/// it is, so a zero counter can never again pass for a measurement.
///
/// The live path deliberately does NOT probe. Quota there comes from the server's
/// own probe loop (per account, on a random schedule centred on
/// `quotaProbeSeconds`, 300s by default), which is both
/// fresher in practice than a cold one-shot probe and one fewer caller hitting the
/// usage endpoint — that endpoint rate-limits, and a second prober racing the
/// server's is what makes a whole fleet read `probe=rate-limited`. Only the
/// offline fallback, which has no server to inherit quota from, probes.
pub async fn status(config_path: &Path, json: bool) -> anyhow::Result<()> {
    // Read-only verb: plain load, no clobber-warning (we never save).
    let config = config::load(config_path)
        .with_context(|| format!("load config at {}", config_path.display()))?;

    let (source, server_build, snapshot, thresholds, http1_only, control) = match fetch_live_status(
        &config,
    )
    .await
    {
        Ok(payload) => {
            let build = payload.build.clone();
            let http1_only = payload.http1_only;
            let control = payload.control.clone();
            let (snapshot, thresholds) = payload.into_snapshot();
            (
                StatusSource::Live,
                Some(build),
                snapshot,
                thresholds,
                http1_only,
                control,
            )
        }
        Err(reason) => {
            // Both "it answered something unusable" and "it answered nothing"
            // warn; only the ordinary no-server case stays quiet.
            if !matches!(reason, LiveStatusError::NoServer) {
                let why = reason.why();
                eprintln!(
                        "[tcr] warning: could not read live status from the proxy on :{} ({why}) — falling back to an offline snapshot, whose serving counters are all zero.",
                        config.proxy.port
                    );
            }
            let thresholds = resolve_thresholds(&config);
            // `config` is consumed by `snapshot_offline` below, so read
            // `http1_only`/`control_account` off it first — this is the
            // config FILE's value, not a running server's (there is none
            // in this branch), which is the honest answer for an offline
            // snapshot. `control` is a config fact either way, so this is
            // the same reading a live server would derive at boot.
            let http1_only = config.http1_only;
            let control = config.control_account.clone();
            let snapshot = snapshot_offline(
                config,
                Arc::new(NoRefresh),
                Arc::new(LiveUsageProber::new()),
                true,
            )
            .await;
            (
                StatusSource::Offline,
                None,
                snapshot,
                thresholds,
                http1_only,
                control,
            )
        }
    };

    // Build skew goes to STDERR in both modes: `--json` output is a bare array
    // that scripts pipe into jq, and a diagnostic on stdout would corrupt it.
    // One channel for the warning also means one place to look for it.
    if let Some(line) = skew_report(server_build.as_ref()) {
        eprintln!("{line}");
    }

    if json {
        println!(
            "{}",
            render_accounts_json(
                &snapshot,
                &thresholds,
                source,
                server_build.as_ref(),
                http1_only,
                control.as_deref()
            )
        );
    } else {
        // One greppable `source=` line above the account lines, in the same
        // key=value idiom, so the provenance is visible without --json.
        // `http1Only` rides here rather than per-account: it is a server-wide
        // fact, like `source` itself, not a per-account gating detail — see
        // `Manager::http1_only`'s doc-comment for why this must NOT be
        // re-derived from the config file when `source=live`.
        println!(
            "status source={}{}{} http1Only={}",
            source.as_str(),
            match source {
                StatusSource::Live => String::new(),
                StatusSource::Offline =>
                    " note=serving-counters-unavailable-no-server-answered".to_string(),
            },
            build_fields(server_build.as_ref()),
            http1_only,
        );
        print!("{}", render_accounts(&snapshot, source));
    }
    Ok(())
}

/// The build `key=value` tail of the `status source=` line.
///
/// `client_sha` is always present, and is not redundant with `server_sha`: the
/// two are the same binary only until someone rebuilds without restarting, which
/// is the normal state of affairs here (`tcr update` rebuilds on purpose without
/// restarting). When they differ, the CLI you just ran is newer than the server
/// answering it — worth seeing before trusting either.
fn build_fields(server: Option<&BuildInfo>) -> String {
    let client = BuildInfo::current();
    match server {
        Some(server) => format!(
            " server_sha={} server_dirty={} server_built_at={} client_sha={}",
            server.sha,
            server.dirty_str(),
            server.built_at,
            client.sha,
        ),
        None => format!(" client_sha={}", client.sha),
    }
}

/// Compare the RUNNING server's build against the checkout we are standing in,
/// and return the line to print — `None` when there is nothing to say.
///
/// Two silences are deliberate. With no live server there is no running code to
/// be stale (the offline path's numbers come from this very process). Outside
/// tcr's own checkout there is no HEAD that means anything here — see
/// [`build_info::find_tcr_checkout`], which refuses to compare against an
/// unrelated repository's HEAD.
fn skew_report(server: Option<&BuildInfo>) -> Option<String> {
    let server = server?;
    let cwd = std::env::current_dir().ok()?;
    let root = build_info::find_tcr_checkout(&cwd)?;
    let checkout = build_info::read_checkout_state(&root, &server.sha);
    build_info::compare(server, &checkout).report_line()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::{OAuthError, RefreshFuture};
    use crate::probe::{ProbeFuture, Usage, UsageBucket};
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A unique temp config path per call so parallel tests never collide.
    fn temp_config_path(tag: &str) -> std::path::PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "tcr-cli-test-{tag}-{}-{seq}.json",
            std::process::id()
        ))
    }

    /// Write `json` to a fresh temp path and return it.
    fn write_config(tag: &str, json: &str) -> std::path::PathBuf {
        let path = temp_config_path(tag);
        fs::write(&path, json).unwrap();
        path
    }

    const TWO_ACCOUNTS: &str = r#"{
      "proxy": { "port": 3456 },
      "quotaProbeSeconds": 120,
      "routes": [{ "name": "r1", "match": "*fable*" }],
      "accounts": [
        { "name": "alice@example.com", "type": "oauth", "orgName": "Org A",
          "orgUuid": "uuid-a", "accessToken": "at-a", "refreshToken": "rt-a",
          "expiresAt": 1893456000000, "priority": 0 },
        { "name": "bob@example.com", "type": "oauth", "orgName": "Org B",
          "orgUuid": "uuid-b", "accessToken": "at-b", "refreshToken": "rt-b",
          "expiresAt": 1893456000000, "priority": 1 }
      ]
    }"#;

    fn load(path: &Path) -> Config {
        config::load(path).unwrap()
    }

    // --- remove ------------------------------------------------------------

    #[test]
    fn remove_deletes_named_account_leaving_siblings() {
        let path = write_config("remove", TWO_ACCOUNTS);
        remove_account(&path, "alice@example.com", None).unwrap();
        let config = load(&path);
        assert_eq!(config.accounts.len(), 1);
        assert_eq!(config.accounts[0].name, "bob@example.com");
        fs::remove_file(&path).ok();
    }

    #[test]
    fn remove_preserves_unmodelled_extra_fields() {
        let path = write_config("remove-extra", TWO_ACCOUNTS);
        remove_account(&path, "alice@example.com", None).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // The #1 silent-loss risk: unmodelled top-level keys survive the edit.
        assert_eq!(value["quotaProbeSeconds"], serde_json::json!(120));
        assert!(value["routes"].is_array());
        fs::remove_file(&path).ok();
    }

    #[test]
    fn remove_nonexistent_errors_and_leaves_file_byte_identical() {
        let path = write_config("remove-miss", TWO_ACCOUNTS);
        let before = fs::read_to_string(&path).unwrap();
        let result = remove_account(&path, "nobody@example.com", None);
        assert!(result.is_err());
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(before, after, "a failed resolve must not write the config");
        fs::remove_file(&path).ok();
    }

    // --- priority ----------------------------------------------------------

    #[test]
    fn set_priority_explicit_writes_int() {
        let path = write_config("prio-n", TWO_ACCOUNTS);
        set_priority(&path, "bob@example.com", PriorityArg::N(7), None).unwrap();
        let config = load(&path);
        assert_eq!(config.accounts[1].priority, Some(7));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn set_priority_first_is_min0_minus_one() {
        // Existing priorities are 0 and 1 → min(0, {0,1}) - 1 = -1.
        let path = write_config("prio-first", TWO_ACCOUNTS);
        set_priority(&path, "bob@example.com", PriorityArg::First, None).unwrap();
        let config = load(&path);
        assert_eq!(config.accounts[1].priority, Some(-1));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn set_priority_last_is_max0_plus_one() {
        // Existing priorities are 0 and 1 → max(0, {0,1}) + 1 = 2.
        let path = write_config("prio-last", TWO_ACCOUNTS);
        set_priority(&path, "alice@example.com", PriorityArg::Last, None).unwrap();
        let config = load(&path);
        assert_eq!(config.accounts[0].priority, Some(2));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn set_priority_nonexistent_errors_and_leaves_file_byte_identical() {
        let path = write_config("prio-miss", TWO_ACCOUNTS);
        let before = fs::read_to_string(&path).unwrap();
        let result = set_priority(&path, "nobody@example.com", PriorityArg::N(3), None);
        assert!(result.is_err());
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(before, after);
        fs::remove_file(&path).ok();
    }

    // --- group ---------------------------------------------------------

    /// Three accounts: alice in two groups, bob in one (shared with alice),
    /// carol in none — enough to cover multi-group membership, a shared
    /// group, and an ungrouped account in one fixture. Carries `routes` (an
    /// unmodelled top-level key) and per-account `models` (an unmodelled
    /// per-account key) so a mutation's flatten round-trip can be checked
    /// without a second fixture.
    const GROUPED_ACCOUNTS: &str = r#"{
      "proxy": { "port": 3456 },
      "quotaProbeSeconds": 120,
      "routes": [{ "name": "r1", "match": "*fable*" }],
      "accounts": [
        { "name": "alice@example.com", "type": "oauth", "orgName": "Org A",
          "orgUuid": "uuid-a", "accessToken": "at-a", "refreshToken": "rt-a",
          "expiresAt": 1893456000000, "priority": 0,
          "groups": ["codereview", "dev"], "models": ["opus"] },
        { "name": "bob@example.com", "type": "oauth", "orgName": "Org B",
          "orgUuid": "uuid-b", "accessToken": "at-b", "refreshToken": "rt-b",
          "expiresAt": 1893456000000, "priority": 1,
          "groups": ["codereview"] },
        { "name": "carol@example.com", "type": "oauth", "orgName": "Org C",
          "orgUuid": "uuid-c", "accessToken": "at-c", "refreshToken": "rt-c",
          "expiresAt": 1893456000000, "priority": 2 }
      ]
    }"#;

    #[test]
    fn group_add_labels_named_account_and_leaves_siblings_byte_identical() {
        let path = write_config("group-add", TWO_ACCOUNTS);
        add_to_group(&path, "codereview", "alice@example.com").unwrap();
        let config = load(&path);
        assert_eq!(
            config.accounts[0].groups,
            Some(vec!["codereview".to_string()])
        );
        // bob is untouched — same identity fields, still no `groups` key.
        assert_eq!(config.accounts[1].name, "bob@example.com");
        assert_eq!(config.accounts[1].groups, None);
        let raw = fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(
            value["accounts"][1].get("groups").is_none(),
            "bob's raw JSON entry must not gain a groups key: {value}"
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn group_add_twice_is_idempotent() {
        let path = write_config("group-add-twice", TWO_ACCOUNTS);
        add_to_group(&path, "codereview", "alice@example.com").unwrap();
        add_to_group(&path, "codereview", "alice@example.com").unwrap();
        let config = load(&path);
        assert_eq!(
            config.accounts[0].groups,
            Some(vec!["codereview".to_string()]),
            "adding the same label twice must not duplicate it"
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn group_rm_removes_only_named_label_leaving_others_intact() {
        let path = write_config("group-rm-one", GROUPED_ACCOUNTS);
        remove_from_group(&path, "codereview", Some("alice@example.com"), false).unwrap();
        let config = load(&path);
        assert_eq!(config.accounts[0].groups, Some(vec!["dev".to_string()]));
        // bob still carries codereview — only alice's membership changed.
        assert_eq!(
            config.accounts[1].groups,
            Some(vec!["codereview".to_string()])
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn group_rm_all_clears_label_from_every_member_and_nothing_else() {
        let path = write_config("group-rm-all", GROUPED_ACCOUNTS);
        remove_from_group(&path, "codereview", None, true).unwrap();
        let config = load(&path);
        // alice keeps `dev`, loses `codereview`.
        assert_eq!(config.accounts[0].groups, Some(vec!["dev".to_string()]));
        // bob had only `codereview` — the key drops entirely (not `[]`).
        assert_eq!(config.accounts[1].groups, None);
        // carol was never in it and stays untouched.
        assert_eq!(config.accounts[2].groups, None);
        // No other field moved — priorities and identities intact.
        assert_eq!(config.accounts[0].priority, Some(0));
        assert_eq!(config.accounts[1].priority, Some(1));
        assert_eq!(config.accounts[2].priority, Some(2));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn group_rm_all_on_a_group_with_no_members_is_a_success_no_op() {
        let path = write_config("group-rm-all-empty", GROUPED_ACCOUNTS);
        let before = fs::read_to_string(&path).unwrap();
        remove_from_group(&path, "nonexistent-group", None, true).unwrap();
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(
            before, after,
            "an --all on an empty group must write nothing"
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn group_add_unknown_account_errors_and_names_the_configured_accounts() {
        let path = write_config("group-add-unknown", GROUPED_ACCOUNTS);
        let err = add_to_group(&path, "codereview", "nobody@example.com").unwrap_err();
        let message = err.to_string();
        for name in ["alice@example.com", "bob@example.com", "carol@example.com"] {
            assert!(
                message.contains(name),
                "unknown-account error must name every configured account, missing {name}: {message}"
            );
        }
        fs::remove_file(&path).ok();
    }

    #[test]
    fn group_rm_unknown_account_errors_and_names_the_configured_accounts() {
        let path = write_config("group-rm-unknown", GROUPED_ACCOUNTS);
        let err =
            remove_from_group(&path, "codereview", Some("nobody@example.com"), false).unwrap_err();
        let message = err.to_string();
        for name in ["alice@example.com", "bob@example.com", "carol@example.com"] {
            assert!(message.contains(name), "missing {name}: {message}");
        }
        fs::remove_file(&path).ok();
    }

    #[test]
    fn group_add_rejects_bad_label_but_rm_still_accepts_it() {
        // A control character somehow already in the config (hand-edited, or
        // written by an older/buggy version) — `rm` must be able to strip it.
        let with_bad_label = r#"{
          "proxy": { "port": 3456 },
          "accounts": [
            { "name": "alice@example.com", "type": "oauth",
              "accessToken": "at-a", "priority": 0,
              "groups": ["good", "bad\u0000label"] }
          ]
        }"#;
        let bad_label = "bad\u{0}label";

        let path = write_config("group-add-bad-label", with_bad_label);
        let err = add_to_group(&path, bad_label, "alice@example.com").unwrap_err();
        assert!(
            err.to_string().contains("control character"),
            "add must name the character class at fault: {err}"
        );
        // Nothing was written by the rejected `add`.
        let config = load(&path);
        assert_eq!(
            config.accounts[0].groups,
            Some(vec!["good".to_string(), bad_label.to_string()])
        );

        // `rm` does NOT run the validator — it must still remove the bad label.
        remove_from_group(&path, bad_label, Some("alice@example.com"), false).unwrap();
        let config = load(&path);
        assert_eq!(config.accounts[0].groups, Some(vec!["good".to_string()]));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn group_mutation_preserves_unmodelled_extra_keys() {
        let path = write_config("group-extra", GROUPED_ACCOUNTS);
        add_to_group(&path, "burst", "carol@example.com").unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // Top-level unmodelled key.
        assert!(
            value["routes"].is_array(),
            "top-level `routes` must survive: {value}"
        );
        assert_eq!(value["quotaProbeSeconds"], serde_json::json!(120));
        // Per-account unmodelled key on an entry NOT even touched by this edit.
        assert_eq!(value["accounts"][0]["models"], serde_json::json!(["opus"]));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn group_ls_json_lists_an_account_in_two_groups_under_both() {
        let config = config::load(&write_config("group-ls-json", GROUPED_ACCOUNTS)).unwrap();
        let rendered = render_groups_json(&config).unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        let groups = value["groups"].as_array().unwrap();
        let codereview = groups
            .iter()
            .find(|g| g["group"] == "codereview")
            .expect("codereview group must be present");
        let members: Vec<&str> = codereview["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m.as_str().unwrap())
            .collect();
        assert!(
            members.contains(&"alice@example.com"),
            "alice is in codereview AND dev — she must appear under codereview too: {members:?}"
        );
        assert!(members.contains(&"bob@example.com"));
        let dev = groups
            .iter()
            .find(|g| g["group"] == "dev")
            .expect("dev group must be present");
        let dev_members: Vec<&str> = dev["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m.as_str().unwrap())
            .collect();
        assert!(
            dev_members.contains(&"alice@example.com"),
            "alice must ALSO appear under dev: {dev_members:?}"
        );
        assert_eq!(value["ungrouped"], serde_json::json!(1)); // carol only
    }

    #[test]
    fn group_ls_text_reports_ungrouped_count() {
        let config = config::load(&write_config("group-ls-text", GROUPED_ACCOUNTS)).unwrap();
        let rendered = render_groups_text(&config);
        assert!(rendered.contains("ungrouped accounts=1"), "{rendered}");
        assert!(rendered.contains("group codereview"), "{rendered}");
        assert!(rendered.contains("group dev"), "{rendered}");
    }

    // --- enable / disable --------------------------------------------------

    /// A free port nothing is listening on: bound, then dropped.
    /// An ephemeral port nothing is listening on — and **never 3456**.
    ///
    /// Drawing 3456 here is not a cosmetic collision. `TWO_ACCOUNTS` names 3456
    /// and [`config_on_a_dead_port`] substitutes it away precisely so a test can
    /// never POST an account-control command at the REAL proxy; if the draw IS
    /// 3456 the substitution is a no-op, and the assert that guards it fires.
    ///
    /// This only ever reproduces in CI, which is what made it look like a flake
    /// and then like a regression. On a developer Mac a live tcr holds 3456, so
    /// the OS cannot offer it; on a Linux runner the port is free and gets drawn
    /// occasionally. Measured 2026-08-14: two consecutive runs of the same
    /// commit failed on two DIFFERENT tests of this family, which is the
    /// signature of a shared helper losing a dice roll rather than of a broken
    /// assertion.
    ///
    /// Bounded rather than `loop`: an unbounded retry against an OS that somehow
    /// only offers 3456 would hang the suite with no diagnosis, and a test that
    /// hangs is strictly worse than one that fails with a reason.
    async fn dead_port() -> u16 {
        for _ in 0..64 {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            if port != 3456 {
                return port;
            }
        }
        panic!("could not draw an ephemeral port other than 3456 in 64 attempts");
    }

    /// `TWO_ACCOUNTS` pointed at a port nothing serves, written to a temp file.
    ///
    /// Load-bearing safety, not tidiness: `set_enabled` is live-first now, and
    /// `TWO_ACCOUNTS` names port **3456** — the port a REAL tcr serves Gil's fleet
    /// on. A test left on that port would POST an account-control command at the
    /// live proxy. The substitution is asserted below because a silent no-op here
    /// would aim every one of these tests at the running server.
    async fn config_on_a_dead_port(tag: &str) -> std::path::PathBuf {
        let port = dead_port().await;
        let json = TWO_ACCOUNTS.replace("\"port\": 3456", &format!("\"port\": {port}"));
        assert!(
            !json.contains("\"port\": 3456") && json.contains(&format!("\"port\": {port}")),
            "the port substitution must apply, or this test talks to the live proxy"
        );
        write_config(tag, &json)
    }

    #[tokio::test]
    async fn set_enabled_true_writes_disabled_true() {
        let path = config_on_a_dead_port("disable").await;
        set_enabled(&path, "alice@example.com", None, true)
            .await
            .unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["accounts"][0]["disabled"], serde_json::json!(true));
        fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn set_enabled_false_drops_the_disabled_key() {
        let path = config_on_a_dead_port("enable").await;
        // First disable, then re-enable — the key must vanish entirely.
        set_enabled(&path, "alice@example.com", None, true)
            .await
            .unwrap();
        set_enabled(&path, "alice@example.com", None, false)
            .await
            .unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(
            value["accounts"][0].get("disabled").is_none(),
            "re-enable must DROP the disabled key, not write false"
        );
        fs::remove_file(&path).ok();
    }

    /// THE BITING TEST for the CLI half, end to end through the PRODUCTION path:
    /// a real hybrid listener (the thing that injects `ClientAddr`), a real socket,
    /// the real router, and `set_enabled` as `tcr disable` calls it.
    ///
    /// The two assertions after the call are the whole bug. Pre-change `set_enabled`
    /// wrote the file and stopped, so the RUNNING manager's `disabled` stayed
    /// `false` and the account it named kept being handed traffic — measured on the
    /// live fleet, two accounts deep. A file-only assertion passes against that
    /// defect; the in-memory one does not.
    #[tokio::test]
    async fn set_enabled_parks_the_account_in_the_running_proxy() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let path = config_on_a_dead_port("live-disable").await;
        let mut config = load(&path);
        config.proxy.port = port;

        // The server owns the SAME config file the CLI is pointed at, exactly as
        // the real proxy does — so the durable half lands where `tcr` looks.
        let manager = Manager::with_live_refresher(config.clone(), Some(path.clone()));
        let served = Arc::clone(&manager);
        tokio::spawn(async move { crate::mitm::serve(listener, served, None).await });

        // Point the CLI at the live port by rewriting the file it will load.
        let raw = fs::read_to_string(&path).unwrap();
        let mut doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        doc["proxy"]["port"] = serde_json::json!(port);
        fs::write(&path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();

        set_enabled(&path, "alice@example.com", None, true)
            .await
            .unwrap();

        // 1. THE RUNNING ROTATION. Not a fresh Manager, not the file — the process
        //    that would serve the next request.
        let live = manager.snapshot(OffsetDateTime::now_utc());
        assert_eq!(live.accounts[0].name, "alice@example.com");
        assert!(
            live.accounts[0].disabled,
            "the account is parked IN THE SERVING PROCESS, not just on disk"
        );
        assert!(
            !live.accounts[1].disabled,
            "and only the resolved account was touched"
        );

        // 2. …and the file carries it, so the bench survives a restart.
        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            after["accounts"][0]["disabled"],
            serde_json::json!(true),
            "the durable half landed too: {after}"
        );

        // 3. Re-enable, live, and the key is DROPPED from the document — the same
        //    contract the file-only path has always had.
        set_enabled(&path, "alice@example.com", None, false)
            .await
            .unwrap();
        assert!(
            !manager.snapshot(OffsetDateTime::now_utc()).accounts[0].disabled,
            "re-enable reaches the live rotation too"
        );
        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(
            after["accounts"][0].get("disabled").is_none(),
            "re-enable DROPS the key rather than writing false: {after}"
        );
        fs::remove_file(&path).ok();
    }

    /// An ambiguous query is refused by the SERVER and the CLI does not fall back:
    /// the file's own resolution could land on a different row than the one the
    /// server was talking about, and a disable applied to the wrong account is
    /// worse than none. Both accounts share an email here, so `--org` is the fix
    /// the message must name.
    #[tokio::test]
    async fn set_enabled_refuses_an_ambiguous_query_without_writing() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let path = config_on_a_dead_port("live-ambiguous").await;
        // Same email in two orgs — the shape `--org` exists for.
        let raw = fs::read_to_string(&path).unwrap();
        let mut doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        doc["proxy"]["port"] = serde_json::json!(port);
        doc["accounts"][1]["name"] = serde_json::json!("alice@example.com");
        fs::write(&path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
        let before = fs::read_to_string(&path).unwrap();

        let config = load(&path);
        let manager = Manager::with_live_refresher(config, Some(path.clone()));
        tokio::spawn(async move { crate::mitm::serve(listener, manager, None).await });

        let err = set_enabled(&path, "alice@example.com", None, true)
            .await
            .expect_err("an ambiguous query must not be applied");
        let text = err.to_string();
        assert!(
            text.contains("ambiguous") && text.contains("--org"),
            "the refusal names the candidates and the fix: {text}"
        );
        assert_eq!(
            before,
            fs::read_to_string(&path).unwrap(),
            "a refused command leaves the config byte-identical"
        );
        fs::remove_file(&path).ok();
    }

    // --- post_add_account ---------------------------------------------------

    /// A minimal valid live server config: the port, no accounts, no api-key.
    fn add_route_config(port: u16) -> Config {
        serde_json::from_str(&format!(r#"{{ "proxy": {{ "port": {port} }} }}"#)).unwrap()
    }

    /// A blank credential — the deliberately-invalid probe body — always
    /// rejected by [`crate::proxy::add_account_handler`]'s own validation, so
    /// this can never add a real account.
    fn blank_account() -> Account {
        Account {
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
            groups: None,
            extra: serde_json::Map::new(),
        }
    }

    #[tokio::test]
    async fn post_add_account_reads_a_stamped_400_as_rejected() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = add_route_config(port);
        let manager = Manager::with_live_refresher(config.clone(), None);
        tokio::spawn(async move { crate::mitm::serve(listener, manager, None).await });

        let err = post_add_account(&config, &blank_account())
            .await
            .expect_err("a blank name/token must be refused by the route's own validation");
        assert!(
            matches!(err, LiveControlError::Rejected(ref msg) if msg.contains("required")),
            "a stamped 400 is a structural Rejected, not a guess from status alone: {err:?}"
        );
    }

    #[tokio::test]
    async fn post_add_account_unstamped_404_is_no_route() {
        // An "older tcr": something answers, but not on this route — no
        // `ENDPOINT_HEADER`, unlike every response this crate's own router
        // produces (even its 404s and 405s).
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, axum::Router::new()).await;
        });
        let config = add_route_config(port);

        let err = post_add_account(&config, &blank_account())
            .await
            .expect_err("nothing is registered at this path");
        assert!(
            matches!(err, LiveControlError::NoRoute),
            "an unstamped 404 must read as NoRoute, never Rejected: {err:?}"
        );
    }

    #[tokio::test]
    async fn post_add_account_wrong_api_key_is_unauthorized() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server_config: Config = serde_json::from_str(&format!(
            r#"{{ "proxy": {{ "port": {port}, "apiKey": "correct-key" }} }}"#
        ))
        .unwrap();
        let manager = Manager::with_live_refresher(server_config, None);
        tokio::spawn(async move { crate::mitm::serve(listener, manager, None).await });

        let client_config: Config = serde_json::from_str(&format!(
            r#"{{ "proxy": {{ "port": {port}, "apiKey": "wrong-key" }} }}"#
        ))
        .unwrap();
        let err = post_add_account(&client_config, &blank_account())
            .await
            .expect_err("a wrong api-key must be refused");
        assert!(
            matches!(err, LiveControlError::Unauthorized),
            "must surface as Unauthorized, never NoRoute: {err:?}"
        );
    }

    /// THE ROUND TRIP: a real account, POSTed to a real live server, lands in
    /// the SERVING rotation — not just in the response body.
    #[tokio::test]
    async fn post_add_account_adds_a_new_account_to_the_live_rotation() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = add_route_config(port);
        let manager = Manager::with_live_refresher(config.clone(), None);
        let served = Arc::clone(&manager);
        tokio::spawn(async move { crate::mitm::serve(listener, served, None).await });

        let account = Account {
            name: "new@example.com".to_string(),
            account_type: "oauth".to_string(),
            account_uuid: None,
            org_uuid: None,
            org_name: None,
            access_token: "at-new".to_string(),
            refresh_token: Some("rt-new".to_string()),
            expires_at: Some(1_893_456_000_000),
            priority: None,
            switch_threshold: None,
            disabled: None,
            groups: None,
            extra: serde_json::Map::new(),
        };
        let applied = post_add_account(&config, &account)
            .await
            .expect("a fresh identity must be added, not rejected");
        assert!(
            applied.added,
            "a nonexistent identity is ADDED: {applied:?}"
        );
        assert_eq!(applied.name, "new@example.com");

        let live = manager.snapshot(OffsetDateTime::now_utc());
        assert!(
            live.accounts.iter().any(|a| a.name == "new@example.com"),
            "the account must be in the SERVING rotation, not just the response"
        );
    }

    /// An ambiguous identity is refused by the route, structurally — never
    /// guessed onto one of the candidates.
    #[tokio::test]
    async fn post_add_account_ambiguous_identity_is_rejected_not_guessed() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // Two live accounts share a name and neither carries an org — the
        // legacy shape `same_identity`/`match_one` cannot break a tie on.
        let seed = format!(
            r#"{{ "proxy": {{ "port": {port} }}, "accounts": [
                {{ "name": "dup@example.com", "type": "oauth", "accessToken": "at-1",
                  "refreshToken": "rt-1", "priority": 0 }},
                {{ "name": "dup@example.com", "type": "oauth", "accessToken": "at-2",
                  "refreshToken": "rt-2", "priority": 1 }}
            ] }}"#
        );
        let config: Config = serde_json::from_str(&seed).unwrap();
        let manager = Manager::with_live_refresher(config.clone(), None);
        tokio::spawn(async move { crate::mitm::serve(listener, manager, None).await });

        let account = Account {
            name: "dup@example.com".to_string(),
            account_type: "oauth".to_string(),
            account_uuid: None,
            org_uuid: None,
            org_name: None,
            access_token: "at-new".to_string(),
            refresh_token: Some("rt-new".to_string()),
            expires_at: None,
            priority: None,
            switch_threshold: None,
            disabled: None,
            groups: None,
            extra: serde_json::Map::new(),
        };
        let err = post_add_account(&config, &account)
            .await
            .expect_err("an ambiguous identity must be refused, not guessed");
        assert!(
            matches!(err, LiveControlError::Rejected(ref msg) if msg.contains("ambiguous")),
            "{err:?}"
        );
    }

    // --- resolve_account ---------------------------------------------------

    fn load_from(json: &str) -> Config {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn resolve_exact_name_wins() {
        // An exact display-name match resolves outright; a distinct name is not
        // matched by the email fallback.
        let config = load_from(
            r#"{ "accounts": [
                { "name": "user", "type": "oauth", "accessToken": "at1" },
                { "name": "user-two", "type": "oauth", "accessToken": "at2" }
            ] }"#,
        );
        let idx = resolve_account(&config.accounts, "user", None).unwrap();
        assert_eq!(idx, 0, "exact name resolves to that account");
    }

    /// Two accounts sharing the SAME email in different orgs — the finding #8
    /// scenario. Keying on name alone silently returned the first; `--org` must
    /// now disambiguate.
    const DUP_EMAIL_TWO_ORGS: &str = r#"{
      "accounts": [
        { "name": "me@example.com", "type": "oauth", "accountUuid": "uuid-person",
          "orgName": "Org A", "orgUuid": "uuid-a", "accessToken": "at-a", "priority": 0 },
        { "name": "me@example.com", "type": "oauth", "accountUuid": "uuid-person",
          "orgName": "Org B", "orgUuid": "uuid-b", "accessToken": "at-b", "priority": 1 }
      ]
    }"#;

    #[test]
    fn resolve_ambiguous_across_two_orgs_errors_listing_candidates() {
        // The same email in two orgs → ambiguous without --org.
        let accounts = load_from(DUP_EMAIL_TWO_ORGS).accounts;
        let err = resolve_account(&accounts, "me@example.com", None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ambiguous"), "ambiguous error: {msg}");
        assert!(
            msg.matches("me@example.com").count() >= 2,
            "error lists both candidates: {msg}"
        );
    }

    #[test]
    fn resolve_org_narrows_to_one() {
        let accounts = load_from(DUP_EMAIL_TWO_ORGS).accounts;
        // The ambiguous email, narrowed by org name, resolves uniquely.
        let idx = resolve_account(&accounts, "me@example.com", Some("Org B")).unwrap();
        assert_eq!(idx, 1);
        // And --org matching the uuid (exact or prefix) works too.
        let idx = resolve_account(&accounts, "me@example.com", Some("uuid-a")).unwrap();
        assert_eq!(idx, 0);
    }

    // --- render_accounts ---------------------------------------------------

    /// A prober that returns a fixed 5-hour bar for any token — lets a snapshot
    /// carry a known quota% without touching the network.
    struct FixedProber {
        util: f64,
    }
    impl UsageProber for FixedProber {
        fn probe(&self, _access_token: String) -> ProbeFuture {
            let util = self.util;
            Box::pin(async move {
                Ok(Usage {
                    five_hour: Some(UsageBucket {
                        utilization: Some(util),
                        reset_at_ms: Some(crate::now_ms() + 3_600_000),
                    }),
                    seven_day: None,
                    seven_day_oi: None,
                })
            })
        }
    }

    #[tokio::test]
    async fn render_accounts_emits_one_greppable_line_per_account() {
        let config = load_from(TWO_ACCOUNTS);
        let snapshot = snapshot_offline(
            config,
            Arc::new(NoRefresh),
            Arc::new(FixedProber { util: 0.25 }),
            true,
        )
        .await;
        // Offline is the honest source for a snapshot built by `snapshot_offline`,
        // and it is the exact line shape `tcr accounts` emits — that verb is
        // offline by construction, so this is the greppable line a human reads
        // most often.
        let text = render_accounts(&snapshot, StatusSource::Offline);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "one line per account");
        // Each line is greppable: name + priority + per-window utilization.
        assert!(lines[0].contains("alice@example.com"));
        assert!(lines[0].contains("priority=0"));
        // FixedProber sets five_hour=0.25, seven_day/seven_day_oi=None.
        assert!(lines[0].contains("5h=25%"), "5h window: {}", lines[0]);
        assert!(lines[0].contains("wk=n/a"), "wk unknown: {}", lines[0]);
        // seven_day_oi is None → the fable token is omitted entirely.
        assert!(!lines[0].contains("fable="), "no fable token: {}", lines[0]);
        // Well under the 0.9 default threshold → an honest `ok` state.
        assert!(lines[0].contains("state=ok"), "healthy state: {}", lines[0]);
        assert!(lines[1].contains("bob@example.com"));
        assert!(lines[1].contains("priority=1"));
        // At 25% util no window is a binding hold, but it still has a live
        // reset — and a window's reset is a property of the window, not of
        // whether it is pinning the account, so the inline countdown still
        // shows. The legacy `held=`/`quota=`/`quota_state=` fields are gone
        // for good.
        assert!(
            text.contains("5h=25%(+"),
            "under-threshold accounts still carry their window's countdown: {text}"
        );
        assert!(
            !text.contains("held=") && !text.contains("quota="),
            "legacy quota/held fields are gone: {text}"
        );
        // Every token stays `key=value`, so the new stream-error token is greppable
        // wherever it sits in the line and does not displace its neighbours.
        for line in &lines {
            assert!(
                line.contains("probe=") && line.contains("stream_errors=n/a"),
                "the offline line keeps probe= and carries an unmeasured \
                 stream-error token: {line}"
            );
        }
    }

    /// THE HONESTY TEST. An offline snapshot's serving counters live in the
    /// SERVER's process, so `input_tokens` here is structurally zero — there is
    /// nothing to divide by and no measurement was taken. It must therefore emit
    /// `null` (JSON) / `n/a` (human), never a `0.0` that reads as a measured "0%
    /// cache hits".
    ///
    /// That false zero is the entire reason this bug hid: `tcr status --json`
    /// reported `"cacheHitRatio": 0.0` for every account forever, so the one
    /// surface that should have shown a prompt-cache catastrophe reported
    /// "cache fine" straight through it. The `source` field is the other half —
    /// it names the process the counters would have come from.
    #[tokio::test]
    async fn offline_status_reports_null_not_zero_hit_ratio() {
        let config = load_from(TWO_ACCOUNTS);
        let thresholds = resolve_thresholds(&config);
        let snapshot = snapshot_offline(
            config,
            Arc::new(NoRefresh),
            Arc::new(FixedProber { util: 0.25 }),
            true,
        )
        .await;

        let json = render_accounts_json(
            &snapshot,
            &thresholds,
            StatusSource::Offline,
            None,
            false,
            None,
        );
        let rows: Vec<serde_json::Value> = serde_json::from_str(&json).expect("valid json");
        assert_eq!(rows.len(), 2, "one row per account");
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(
                snapshot.accounts[i].input_tokens, 0,
                "the premise: an offline snapshot has counted nothing"
            );
            // The four serving counters read `null` on the offline path, not
            // a real `0` — same false-zero fix as `cacheHitRatio` below, one
            // field family over: these live in the SERVING process, so a
            // fresh offline `Manager` has structurally never measured them.
            for key in ["requests", "inputTokens", "outputTokens", "cacheReadTokens"] {
                assert!(
                    row[key].is_null(),
                    "{key} is unmeasured on the offline path, never a false 0: {row}"
                );
                assert_ne!(
                    row[key],
                    serde_json::json!(0),
                    "the literal 0 that would masquerade as a measurement is gone: {row}"
                );
            }
            assert!(
                row["cacheHitRatio"].is_null(),
                "an uncounted ratio is null, not a number: {row}"
            );
            assert_ne!(
                row["cacheHitRatio"],
                serde_json::json!(0.0),
                "the literal 0.0 that masqueraded as a measurement is gone: {row}"
            );
            assert_eq!(
                row["source"], "offline",
                "every row names the process its counters came from: {row}"
            );
        }

        // A ratio that IS measured still renders as a number — the null is about
        // absence of data, not a blanket suppression. `requests` must clear
        // `CACHE_SIGNAL_FLOOR_REQUESTS` too, or the floor added below would
        // null this out regardless of the token counts.
        let mut counted = snapshot.clone();
        counted.accounts[0].requests = CACHE_SIGNAL_FLOOR_REQUESTS;
        counted.accounts[0].input_tokens = 1_000;
        counted.accounts[0].cache_read_tokens = 750;
        let live =
            render_accounts_json(&counted, &thresholds, StatusSource::Live, None, false, None);
        let rows: Vec<serde_json::Value> = serde_json::from_str(&live).expect("valid json");
        assert_eq!(rows[0]["cacheHitRatio"], serde_json::json!(0.75));
        assert_eq!(rows[0]["source"], "live");
        assert!(
            rows[1]["cacheHitRatio"].is_null(),
            "the still-uncounted account stays null: {}",
            rows[1]
        );

        // The human view uses the same `n/a` idiom the unknown quota windows use.
        let text = render_accounts(&snapshot, StatusSource::Offline);
        for line in text.lines() {
            assert!(
                line.contains("cache=n/a"),
                "uncounted cache reads as n/a, never 0%: {line}"
            );
            assert!(!line.contains("cache=0%"), "no false measured zero: {line}");
        }
        assert!(
            render_accounts(&counted, StatusSource::Live).contains("cache=75%"),
            "a measured ratio still renders as a percentage"
        );
    }

    /// THE SIGNAL-FLOOR TEST. An account whose only real traffic is a
    /// handful of cold diverts has genuinely nonzero, genuinely LIVE token
    /// counters — this is not the `input_tokens == 0` case above — but too
    /// few served requests to trust the ratio it would produce. Before this
    /// fix, `tcr status` reported that account's near-empty-denominator ratio
    /// as a confident measured number (as low as single digits of percent),
    /// which read exactly like "this account's cache is broken" and sent a
    /// lead chasing a regression that was never there
    /// (`docs/plans/divert-budget-swarm-state.md`, "Falsified on the way").
    /// This pins the fix: under `CACHE_SIGNAL_FLOOR_REQUESTS`, the ratio is
    /// `null`/`n/a` — no-signal — same as the honest-zero case, never a
    /// number.
    #[tokio::test]
    async fn an_account_under_the_signal_floor_reports_no_signal_not_a_number() {
        let config = load_from(TWO_ACCOUNTS);
        let thresholds = resolve_thresholds(&config);
        let snapshot = snapshot_offline(
            config,
            Arc::new(NoRefresh),
            Arc::new(FixedProber { util: 0.25 }),
            true,
        )
        .await;

        // Real, live, nonzero tokens — a genuine measurement, not an unset
        // counter — but ONE below the floor's request count. A single cold
        // divert can plausibly write tens of thousands of cache_creation
        // tokens in one request, so a small `requests` count with a large
        // token count is exactly the shape this guards, not a fixture
        // artefact.
        let mut sparse = snapshot.clone();
        sparse.accounts[0].requests = CACHE_SIGNAL_FLOOR_REQUESTS - 1;
        sparse.accounts[0].input_tokens = 42_000;
        sparse.accounts[0].cache_read_tokens = 100;

        let live =
            render_accounts_json(&sparse, &thresholds, StatusSource::Live, None, false, None);
        let rows: Vec<serde_json::Value> = serde_json::from_str(&live).expect("valid json");
        assert!(
            rows[0]["cacheHitRatio"].is_null(),
            "under the floor, a real nonzero ratio still reads as no-signal, \
             not a misleadingly precise number: {}",
            rows[0]
        );
        assert_ne!(
            rows[0]["cacheHitRatio"],
            serde_json::json!(0.0),
            "no-signal must never collapse to the false '0% cache hits' this \
             module already fixed once: {}",
            rows[0]
        );

        let text = render_accounts(&sparse, StatusSource::Live);
        let line = text
            .lines()
            .find(|l| l.contains(&sparse.accounts[0].name))
            .expect("the sparse account's line");
        assert!(
            line.contains("cache=n/a"),
            "the human view reports the same no-signal state: {line}"
        );

        // One request OVER the floor, same tokens: the ratio is measured and
        // renders as the real number — this is the boundary, not a blanket
        // suppression of light accounts.
        let mut at_floor = sparse.clone();
        at_floor.accounts[0].requests = CACHE_SIGNAL_FLOOR_REQUESTS;
        let live = render_accounts_json(
            &at_floor,
            &thresholds,
            StatusSource::Live,
            None,
            false,
            None,
        );
        let rows: Vec<serde_json::Value> = serde_json::from_str(&live).expect("valid json");
        assert_eq!(
            rows[0]["cacheHitRatio"],
            serde_json::json!(100.0 / 42_000.0),
            "at the floor the same tokens now render as a real measurement: {}",
            rows[0]
        );
    }

    /// The stream-error counter obeys the same rule as the cache ratio above, and
    /// for a sharper reason: `0` on an ERROR counter is an affirmative all-clear.
    /// Offline it is structurally zero — the count lives in the serving process —
    /// so publishing it as `0` would claim "no truncated streams" about a process
    /// this one never spoke to.
    #[tokio::test]
    async fn offline_status_reports_null_not_zero_stream_errors() {
        let config = load_from(TWO_ACCOUNTS);
        let thresholds = resolve_thresholds(&config);
        let snapshot = snapshot_offline(
            config,
            Arc::new(NoRefresh),
            Arc::new(FixedProber { util: 0.25 }),
            true,
        )
        .await;

        let offline = render_accounts_json(
            &snapshot,
            &thresholds,
            StatusSource::Offline,
            None,
            false,
            None,
        );
        let rows: Vec<serde_json::Value> = serde_json::from_str(&offline).expect("valid json");
        for row in &rows {
            // `get`, not `row[...]`: indexing a MISSING key also yields Null, so
            // `row["streamErrorCount"].is_null()` would pass against the very bug
            // this test exists for — the field never being rendered at all.
            assert_eq!(
                row.get("streamErrorCount"),
                Some(&serde_json::Value::Null),
                "the key is present AND null — unmeasured, never a 0 all-clear: {row}"
            );
            assert_eq!(
                row.get("lastStreamError"),
                Some(&serde_json::Value::Null),
                "nothing measured: {row}"
            );
        }

        // Measured and genuinely clean is a DIFFERENT state, and renders as 0.
        let live = render_accounts_json(
            &snapshot,
            &thresholds,
            StatusSource::Live,
            None,
            false,
            None,
        );
        let rows: Vec<serde_json::Value> = serde_json::from_str(&live).expect("valid json");
        assert_eq!(rows[0]["streamErrorCount"], serde_json::json!(0));

        // Measured and dirty carries both the count and the latest error type.
        let mut errored = snapshot.clone();
        errored.accounts[0].stream_error_count = 3;
        errored.accounts[0].last_stream_error = Some("overloaded_error".to_string());
        let live =
            render_accounts_json(&errored, &thresholds, StatusSource::Live, None, false, None);
        let rows: Vec<serde_json::Value> = serde_json::from_str(&live).expect("valid json");
        assert_eq!(rows[0]["streamErrorCount"], serde_json::json!(3));
        assert_eq!(
            rows[0]["lastStreamError"],
            serde_json::json!("overloaded_error")
        );

        // The text view carries the same three states as greppable tokens.
        let text = render_accounts(&snapshot, StatusSource::Offline);
        for line in text.lines() {
            assert!(
                line.contains("stream_errors=n/a"),
                "unmeasured reads n/a, never 0: {line}"
            );
            assert!(
                !line.contains("stream_errors=0"),
                "no false all-clear: {line}"
            );
        }
        assert!(
            render_accounts(&snapshot, StatusSource::Live).contains("stream_errors=0"),
            "a measured clean fleet still renders a real 0"
        );
        let text = render_accounts(&errored, StatusSource::Live);
        assert!(
            text.contains("stream_errors=3"),
            "the count is greppable: {text}"
        );
        assert!(
            text.contains("last_stream_error=overloaded_error"),
            "the latest error type is greppable: {text}"
        );
    }

    /// END-TO-END through the PRODUCTION path, which is the claim that actually
    /// matters: a real hybrid listener (the thing that injects `ClientAddr`), a
    /// real HTTP round trip on a real socket, and the real client parser.
    ///
    /// The counters below exist ONLY in the served manager's process — exactly the
    /// situation that made `tcr status` lie, since the offline path builds a fresh
    /// `Manager` that can never see them. Binds port 0 (a free ephemeral port) and
    /// overrides the config's 3456, so this never touches a real running proxy.
    #[tokio::test]
    async fn live_status_reads_the_running_servers_counters() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut config = load_from(TWO_ACCOUNTS);
        config.proxy.port = port;

        let manager = Manager::with_live_refresher(config.clone(), None);
        // 750 of 1000 input tokens were prompt-cache reads: a real 75% hit ratio,
        // held in this process only. `record_served` clears
        // `CACHE_SIGNAL_FLOOR_REQUESTS` so the ratio below is a reported number,
        // not the new floor's no-signal null — this test is about the ratio
        // crossing the wire from the live server, not about the floor itself.
        manager.update_usage(0, 1_000, 200, 750, 50);
        let now = OffsetDateTime::now_utc();
        for _ in 0..CACHE_SIGNAL_FLOOR_REQUESTS {
            manager.record_served(0, now, None, crate::stats::SessionKind::Fallback);
        }
        tokio::spawn(async move { crate::mitm::serve(listener, manager, None).await });

        let payload = match fetch_live_status(&config).await {
            Ok(p) => p,
            Err(LiveStatusError::NoServer) => panic!("the spawned server did not answer"),
            Err(err) => panic!("live status unusable: {}", err.why()),
        };
        let build = payload.build.clone();
        let (snapshot, thresholds) = payload.into_snapshot();

        let json = render_accounts_json(
            &snapshot,
            &thresholds,
            StatusSource::Live,
            Some(&build),
            false,
            None,
        );
        let rows: Vec<serde_json::Value> = serde_json::from_str(&json).expect("valid json");
        assert_eq!(
            rows[0]["cacheHitRatio"],
            serde_json::json!(0.75),
            "the ratio is the SERVER's real measurement, not a structural zero: {}",
            rows[0]
        );
        assert_eq!(rows[0]["inputTokens"], 1_000);
        assert_eq!(rows[0]["cacheReadTokens"], 750);
        assert_eq!(rows[0]["source"], "live");
        // The account that served nothing still reports an honest null, not a 0.0.
        assert!(rows[1]["cacheHitRatio"].is_null(), "{}", rows[1]);
        // Thresholds came from the SERVER, not from re-reading the config file.
        assert_eq!(thresholds.len(), 2);

        // END-TO-END on the build stamp too: the sha crossed a real socket from
        // the serving process, and reaches every rendered row.
        assert_eq!(
            build,
            BuildInfo::current(),
            "the served payload names the build that served it"
        );
        for row in &rows {
            assert_eq!(row["serverSha"], serde_json::json!(build_info::SHA));
        }
    }

    /// The offline path has no serving process, so `serverSha` is `null` — the
    /// same "not measured" idiom as `cacheHitRatio`, and never this CLI's own sha
    /// standing in for a server's.
    #[tokio::test]
    async fn offline_rows_report_a_null_server_sha() {
        let config = load_from(TWO_ACCOUNTS);
        let thresholds = resolve_thresholds(&config);
        let snapshot = snapshot_offline(
            config,
            Arc::new(NoRefresh),
            Arc::new(FixedProber { util: 0.25 }),
            false,
        )
        .await;

        let json = render_accounts_json(
            &snapshot,
            &thresholds,
            StatusSource::Offline,
            None,
            false,
            None,
        );
        let rows: Vec<serde_json::Value> = serde_json::from_str(&json).expect("valid json");
        for row in &rows {
            assert!(
                row["serverSha"].is_null(),
                "no server answered, so no server sha: {row}"
            );
            assert!(row["serverDirty"].is_null(), "{row}");
        }
    }

    /// The text line's provenance tail. With a server it names both builds; with
    /// none it still names the CLI's own, so "which binary am I running" is
    /// always answerable from the first line of output.
    #[test]
    fn build_fields_name_the_server_and_the_client() {
        let server = BuildInfo {
            sha: "cd146ce".to_string(),
            dirty: Some(false),
            built_at: "2026-07-26T00:00:00Z".to_string(),
        };
        let live = build_fields(Some(&server));
        assert!(live.contains("server_sha=cd146ce"), "{live}");
        assert!(live.contains("server_dirty=false"), "{live}");
        assert!(
            live.contains("server_built_at=2026-07-26T00:00:00Z"),
            "{live}"
        );
        assert!(
            live.contains(&format!("client_sha={}", build_info::SHA)),
            "{live}"
        );

        let offline = build_fields(None);
        assert!(
            !offline.contains("server_sha="),
            "no server, no server fields: {offline}"
        );
        assert!(
            offline.contains(&format!("client_sha={}", build_info::SHA)),
            "{offline}"
        );

        // An unknown dirty flag reads as `unknown`, never as `false`.
        let murky = build_fields(Some(&BuildInfo::default()));
        assert!(murky.contains("server_dirty=unknown"), "{murky}");
    }

    /// With no live server there is nothing whose code could be stale, so the
    /// skew check stays silent regardless of what the checkout looks like.
    #[test]
    fn skew_report_is_silent_without_a_server() {
        assert_eq!(skew_report(None), None);
    }

    /// The fallback half of the same contract: with nothing listening, the live
    /// read reports `NoServer` (the ordinary case, which warns about nothing) and
    /// `tcr status` keeps working exactly as before — just labelled `offline`.
    #[tokio::test]
    async fn live_status_falls_back_when_no_server_answers() {
        // Bind and immediately drop, so the port is free and reliably refuses.
        let port = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            listener.local_addr().unwrap().port()
        };
        let mut config = load_from(TWO_ACCOUNTS);
        config.proxy.port = port;

        match fetch_live_status(&config).await {
            Err(LiveStatusError::NoServer) => {}
            Err(err) => panic!(
                "a dead port is the ordinary no-server case, not a warning: {}",
                err.why()
            ),
            Ok(_) => panic!("nothing is listening on {port}"),
        }
    }

    // --- incumbent liveness -------------------------------------------------

    /// THE CASE THAT MUST NOT BE A TAKEOVER. A healthy proxy whose api-key we do
    /// not have answers `401` — it accepted the connection, routed the request
    /// and wrote a response, which is the opposite of wedged. `build` is `None`
    /// here exactly as it is for a wedged proxy, so a stand-down that decided on
    /// `build.is_none()` would SIGKILL a server that is serving every request
    /// fine. The liveness verdict is what separates them.
    #[tokio::test]
    async fn a_rejected_api_key_is_an_answering_incumbent_not_a_wedged_one() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut served = load_from(TWO_ACCOUNTS);
        served.proxy.port = port;
        served.proxy.api_key = Some("the-real-key".to_string());

        let manager = Manager::with_live_refresher(served.clone(), None);
        tokio::spawn(async move { crate::mitm::serve(listener, manager, None).await });

        // Same port, WRONG key — what an operator with a stale config has.
        let mut probing = served.clone();
        probing.proxy.api_key = Some("the-wrong-key".to_string());
        let probe = probe_incumbent(&probing).await;

        assert!(
            probe.build.is_none(),
            "the build read fails, which is precisely why it cannot be the liveness signal"
        );
        assert_eq!(
            probe.liveness,
            Liveness::Answering,
            "a 401 is a serving process; taking its port would be a takeover of a healthy proxy"
        );
    }

    /// The WEDGED shape: something holds the listening socket, the connect even
    /// succeeds off the kernel backlog, and no response is ever written. This is
    /// the state a deadlocked proxy leaves the port in, and the one the startup
    /// stand-down has to be able to name — before this, `tcr` reported it as a
    /// healthy incumbent and exited 0, and the port was unrecoverable.
    ///
    /// Costs the probe's own 5s deadline by construction: the assertion IS that
    /// we wait for it and then call it silent.
    #[tokio::test]
    async fn a_listener_that_never_answers_is_silent_not_answering() {
        // Bound and never accepted: connections queue in the backlog forever.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut config = load_from(TWO_ACCOUNTS);
        config.proxy.port = port;

        let probe = probe_incumbent(&config).await;
        assert!(probe.build.is_none());
        match probe.liveness {
            Liveness::Silent { why } => assert!(
                !why.is_empty(),
                "the operator-facing line needs the probe's own account of what happened"
            ),
            Liveness::Answering => {
                panic!("a socket that never wrote a byte is not an answering proxy")
            }
        }
        drop(listener);
    }

    /// The classification the two tests above exercise, pinned directly so a
    /// future variant cannot be added on the wrong side of it by accident.
    #[test]
    fn only_an_http_response_counts_as_answered() {
        assert!(
            !LiveStatusError::NoServer.answered(),
            "nothing listening answered nothing"
        );
        assert!(
            !LiveStatusError::NoAnswer("the server did not answer within 5s".into()).answered(),
            "a deadline with no bytes back is the wedged shape"
        );
        assert!(
            LiveStatusError::Unusable("HTTP 401 Unauthorized".into()).answered(),
            "an HTTP response — any HTTP response — means the process is serving"
        );
    }

    // --- no-persist guarantee ----------------------------------------------

    #[tokio::test]
    async fn status_offline_does_not_persist_the_config() {
        // A scripted prober returns usage; snapshot_offline builds a manager with
        // config_path = None, so nothing is written back — the temp config must
        // be byte-unchanged after a full probe sweep.
        let path = write_config("no-persist", TWO_ACCOUNTS);
        let before = fs::read_to_string(&path).unwrap();
        let config = load(&path);
        let _snapshot = snapshot_offline(
            config,
            Arc::new(NoRefresh),
            Arc::new(FixedProber { util: 0.5 }),
            true,
        )
        .await;
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(
            before, after,
            "an offline status probe must never persist to the config file"
        );
        fs::remove_file(&path).ok();
    }

    // --- no-refresh guarantee (Fix 1) --------------------------------------

    /// A prober that always fails as if the usage endpoint rejected the token
    /// (HTTP 401) — the shape an EXPIRED account's stale access token gets once
    /// [`NoRefresh`] has declined to refresh it.
    struct RejectingProber;
    impl UsageProber for RejectingProber {
        fn probe(&self, _access_token: String) -> ProbeFuture {
            Box::pin(async {
                Err(crate::probe::ProbeError {
                    status: Some(401),
                    message: "HTTP 401: token expired".to_string(),
                })
            })
        }
    }

    /// An INSTRUMENTED refresher: records every refresh attempt and, like
    /// [`NoRefresh`], never contacts the network. Injected into `snapshot_offline`
    /// to prove the offline snapshot routes an expired account's refresh through the
    /// caller's refresher — never a hidden [`crate::oauth::LiveRefresher`] that would
    /// hit the OAuth endpoint and revoke the live server's single-use token.
    struct RecordingRefresher {
        calls: Arc<AtomicU64>,
    }
    impl TokenRefresher for RecordingRefresher {
        fn refresh(&self, _refresh_token: String) -> RefreshFuture {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Err(OAuthError::Transient(
                    "recorded — never networked".to_string(),
                ))
            })
        }
    }

    /// One OAuth account whose token expired in 1970 — probing it would, before the
    /// fix, trigger a real OAuth refresh that revokes the running server's copy.
    const EXPIRED_ACCOUNT: &str = r#"{
      "proxy": { "port": 3456 },
      "accounts": [
        { "name": "stale@example.com", "type": "oauth", "orgName": "Org X",
          "orgUuid": "uuid-x", "accessToken": "at-stale", "refreshToken": "rt-stale",
          "expiresAt": 1000, "priority": 0 }
      ]
    }"#;

    #[tokio::test]
    async fn offline_snapshot_over_expired_account_routes_through_injected_refresher() {
        // Biting test for Fix 1: over an EXPIRED account, the offline snapshot must
        // hand the refresh to the INJECTED refresher (production wires NoRefresh,
        // which never networks) rather than a hidden LiveRefresher. A recording
        // refresher proves the routing — if snapshot_offline ignored its refresher
        // param and hardcoded LiveRefresher, `calls` would stay 0 and this fails.
        // The row must still surface a clear, greppable probe=error, and the config
        // stays byte-identical.
        let path = write_config("expired", EXPIRED_ACCOUNT);
        let before = fs::read_to_string(&path).unwrap();
        let config = load(&path);
        let calls = Arc::new(AtomicU64::new(0));
        let refresher = Arc::new(RecordingRefresher {
            calls: calls.clone(),
        });
        let snapshot = snapshot_offline(config, refresher, Arc::new(RejectingProber), true).await;
        let text = render_accounts(&snapshot, StatusSource::Offline);
        assert!(
            text.contains("stale@example.com"),
            "renders the account: {text}"
        );
        assert!(
            text.contains("probe=error"),
            "surfaces a clear probe status for the expired account: {text}"
        );
        assert!(
            calls.load(Ordering::SeqCst) >= 1,
            "the expired account's refresh was routed through the injected refresher, \
             never a hidden LiveRefresher (which would have hit the OAuth endpoint)"
        );
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(
            before, after,
            "an offline probe of an expired account must never persist"
        );
        fs::remove_file(&path).ok();
    }

    // --- reset countdowns (3a) ---------------------------------------------

    #[test]
    fn reset_countdown_tiers_minutes_then_hours_then_days() {
        let now = OffsetDateTime::now_utc();
        use time::Duration;
        // Minutes under two hours (matches the spec example `+92m`).
        assert_eq!(
            format_reset_countdown(now + Duration::minutes(5), now),
            "+5m"
        );
        assert_eq!(
            format_reset_countdown(now + Duration::minutes(92), now),
            "+92m"
        );
        // Hours from two hours up to two days.
        assert_eq!(format_reset_countdown(now + Duration::hours(5), now), "+5h");
        // Days beyond two days.
        assert_eq!(format_reset_countdown(now + Duration::days(3), now), "+3d");
        assert_eq!(format_reset_countdown(now + Duration::days(7), now), "+7d");
        // A reset that already elapsed clamps to +0m, never negative.
        assert_eq!(
            format_reset_countdown(now - Duration::minutes(5), now),
            "+0m"
        );
    }

    /// `free_in=` answers "when does it come back" — the question
    /// `status=throttled` cannot. It lives on THIS surface because the TUI's
    /// Status column is Length(9) and "throttled" is exactly 9 characters, so a
    /// countdown there is clipped at every terminal width.
    ///
    /// Asserts the RENDERED LINE and the emitted JSON, never a formatting helper.
    /// A unit test on the helper is what let a clipped countdown ship green.
    #[tokio::test]
    async fn free_in_names_the_instant_and_is_omitted_when_none_can_be_promised() {
        // WindowProber gates alice on 5h (~+92m) and bob on 7d (+3d) — real gates
        // with known resets, so account_gate yields a free_at for both.
        let config = load_from(TWO_ACCOUNTS);
        let thresholds = resolve_thresholds(&config);
        let gated =
            snapshot_offline(config, Arc::new(NoRefresh), Arc::new(WindowProber), true).await;
        let text = render_accounts(&gated, StatusSource::Live);
        for line in text.lines() {
            assert!(
                line.contains("free_in="),
                "a gated account names its instant: {line}"
            );
            let secs: i64 = line
                .split("free_in=")
                .nth(1)
                .and_then(|t| t.split('s').next())
                .and_then(|n| n.parse().ok())
                .expect("free_in carries whole seconds");
            assert!(secs > 0, "a promised instant is in the future: {line}");
        }
        // JSON mirrors it for machine consumers, in ms like every other instant.
        let json = render_accounts_json(&gated, &thresholds, StatusSource::Live, None, false, None);
        assert!(
            json.contains("\"freeAtMs\""),
            "json carries freeAtMs: {json}"
        );
        assert!(
            json.contains("\"secondsUntilFree\""),
            "and the countdown: {json}"
        );

        // UNGATED: nothing binds, so there is no instant to promise. The token is
        // OMITTED, never rendered as zero — `free_in=0s` would read as "returns
        // now", which is the same false promise `free_at = None` exists to avoid.
        let config = load_from(TWO_ACCOUNTS);
        let open = snapshot_offline(
            config,
            Arc::new(NoRefresh),
            Arc::new(FixedProber { util: 0.25 }),
            true,
        )
        .await;
        let text = render_accounts(&open, StatusSource::Live);
        assert!(
            !text.contains("free_in="),
            "an account in rotation promises nothing: {text}"
        );
    }

    #[test]
    fn reset_clock_is_bare_hh_mm_utc() {
        // 18:50:00 UTC → "18:50Z" regardless of seconds.
        let reset = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
            + time::Duration::seconds(0);
        let clock = format_reset_clock(reset);
        assert!(clock.ends_with('Z'), "explicit UTC marker: {clock}");
        assert_eq!(clock.len(), 6, "HH:MMZ shape: {clock}");
        assert!(clock.contains(':'), "clock shape: {clock}");
    }

    /// A prober that drives one account over its 5h threshold and another over its
    /// 7d threshold, each with a future reset — so both windows read as held.
    struct WindowProber;
    impl UsageProber for WindowProber {
        fn probe(&self, access_token: String) -> ProbeFuture {
            let now = crate::now_ms();
            Box::pin(async move {
                if access_token == "at-a" {
                    Ok(Usage {
                        five_hour: Some(UsageBucket {
                            utilization: Some(0.95),
                            reset_at_ms: Some(now + 92 * 60 * 1000),
                        }),
                        seven_day: None,
                        seven_day_oi: None,
                    })
                } else {
                    Ok(Usage {
                        five_hour: None,
                        seven_day: Some(UsageBucket {
                            utilization: Some(0.97),
                            reset_at_ms: Some(now + 3 * 24 * 60 * 60 * 1000),
                        }),
                        seven_day_oi: None,
                    })
                }
            })
        }
    }

    #[tokio::test]
    async fn render_accounts_shows_held_window_and_reset_for_over_threshold_accounts() {
        // 3a: a held account's binding window carries an inline `(+countdown)`,
        // gated on the same per-account threshold `eligible` uses.
        let config = load_from(TWO_ACCOUNTS);
        let thresholds = resolve_thresholds(&config);
        let snapshot =
            snapshot_offline(config, Arc::new(NoRefresh), Arc::new(WindowProber), true).await;
        let text = render_accounts(&snapshot, StatusSource::Live);
        let alice = text
            .lines()
            .find(|l| l.contains("alice@example.com"))
            .expect("alice line");
        let bob = text
            .lines()
            .find(|l| l.contains("bob@example.com"))
            .expect("bob line");
        // alice (at-a) is 5h-held at 0.95 with a ~+92m reset; her 5h window carries
        // the inline countdown while her (unlearned) weekly reads n/a. The exact
        // minute drifts with wall-clock between probe and render, so match the `(+`.
        assert!(alice.contains("5h=95%(+"), "5h hold + countdown: {alice}");
        assert!(alice.contains("wk=n/a"), "alice weekly unknown: {alice}");
        // bob (at-b) is 7d-held at 0.97 with a +3d reset; his weekly carries the
        // countdown while his (unlearned) 5h reads n/a.
        assert!(bob.contains("wk=97%(+"), "wk hold + countdown: {bob}");
        assert!(bob.contains("5h=n/a"), "bob 5h unknown: {bob}");
        // Neither is over its 1.0 exhaustion line, so both read `near`, not `spent`.
        assert!(alice.contains("state=near"), "alice near-limit: {alice}");
        assert!(bob.contains("state=near"), "bob near-limit: {bob}");
        // The legacy suffix format is gone from the text line.
        assert!(!text.contains("held="), "no legacy held= suffix: {text}");
        // Greppable one-line-per-account contract survives the new fields.
        assert_eq!(
            text.lines().count(),
            2,
            "still one line per account: {text}"
        );

        // JSON mirrors the held fields for machine consumers.
        let json = render_accounts_json(
            &snapshot,
            &thresholds,
            StatusSource::Offline,
            None,
            false,
            None,
        );
        assert!(json.contains("\"held\""), "json carries held array: {json}");
        assert!(
            json.contains("\"window\": \"5h\""),
            "json names the 5h window: {json}"
        );
        assert!(
            json.contains("\"window\": \"7d\""),
            "json names the 7d window: {json}"
        );
        assert!(
            json.contains("minutesUntilReset"),
            "json carries the countdown: {json}"
        );
    }

    /// A prober that puts alice's 5h window comfortably BELOW her threshold (a
    /// live reset that was previously invisible on the wire), and leaves bob's
    /// 5h window entirely unlearned (no live reset at all — the other
    /// invisible case).
    struct BelowThresholdWindowProber;
    impl UsageProber for BelowThresholdWindowProber {
        fn probe(&self, access_token: String) -> ProbeFuture {
            let now = crate::now_ms();
            Box::pin(async move {
                if access_token == "at-a" {
                    Ok(Usage {
                        five_hour: Some(UsageBucket {
                            utilization: Some(0.3),
                            reset_at_ms: Some(now + 42 * 60 * 1000),
                        }),
                        seven_day: None,
                        seven_day_oi: None,
                    })
                } else {
                    Ok(Usage {
                        five_hour: None,
                        seven_day: None,
                        seven_day_oi: None,
                    })
                }
            })
        }
    }

    #[tokio::test]
    async fn render_accounts_json_carries_reset_for_a_below_threshold_window() {
        // This is the exact case the fix restores: a window well under its
        // threshold (so it never appears in `held[]`) still has a live reset,
        // and that reset must now reach the JSON wire unconditionally.
        let config = load_from(TWO_ACCOUNTS);
        let thresholds = resolve_thresholds(&config);
        let snapshot = snapshot_offline(
            config,
            Arc::new(NoRefresh),
            Arc::new(BelowThresholdWindowProber),
            true,
        )
        .await;

        let json = render_accounts_json(
            &snapshot,
            &thresholds,
            StatusSource::Offline,
            None,
            false,
            None,
        );
        let rows: Vec<serde_json::Value> =
            serde_json::from_str(&json).expect("json is a bare array");
        let alice_row = rows
            .iter()
            .find(|r| r["name"] == "alice@example.com")
            .expect("alice row");
        // 0.3 is far under the 0.9 default threshold, so alice's 5h window is
        // never a binding hold, yet its reset must still reach the wire.
        assert!(
            alice_row["fiveHourResetAtMs"].is_i64(),
            "below-threshold window's reset reaches the wire: {alice_row}"
        );
        // The below-threshold window never binds, so it is absent from `held[]`
        // even though its reset is now unconditionally on the wire.
        assert!(
            alice_row["held"]
                .as_array()
                .expect("held is an array")
                .is_empty(),
            "held[] stays empty for a non-binding window: {alice_row}"
        );
    }

    #[tokio::test]
    async fn render_accounts_json_omits_reset_when_no_live_reset_exists() {
        let config = load_from(TWO_ACCOUNTS);
        let thresholds = resolve_thresholds(&config);
        let snapshot = snapshot_offline(
            config,
            Arc::new(NoRefresh),
            Arc::new(BelowThresholdWindowProber),
            true,
        )
        .await;

        let json = render_accounts_json(
            &snapshot,
            &thresholds,
            StatusSource::Offline,
            None,
            false,
            None,
        );
        let rows: Vec<serde_json::Value> =
            serde_json::from_str(&json).expect("json is a bare array");
        let bob_row = rows
            .iter()
            .find(|r| r["name"] == "bob@example.com")
            .expect("bob row");
        assert_eq!(
            bob_row["fiveHourResetAtMs"],
            serde_json::Value::Null,
            "no live reset renders as null, never 0 or a fabricated instant: {bob_row}"
        );
        assert_eq!(
            bob_row["sevenDayResetAtMs"],
            serde_json::Value::Null,
            "no live reset renders as null, never 0 or a fabricated instant: {bob_row}"
        );
    }

    #[tokio::test]
    async fn render_accounts_json_held_array_is_unchanged_for_over_threshold_account() {
        // The additive claim, asserted rather than assumed: adding the two new
        // reset fields must not change a single byte of the existing `held[]`
        // shape for an account whose window WAS already binding.
        let config = load_from(TWO_ACCOUNTS);
        let thresholds = resolve_thresholds(&config);
        let snapshot =
            snapshot_offline(config, Arc::new(NoRefresh), Arc::new(WindowProber), true).await;
        let json = render_accounts_json(
            &snapshot,
            &thresholds,
            StatusSource::Offline,
            None,
            false,
            None,
        );
        let rows: Vec<serde_json::Value> =
            serde_json::from_str(&json).expect("json is a bare array");
        let alice_row = rows
            .iter()
            .find(|r| r["name"] == "alice@example.com")
            .expect("alice row");
        let held = alice_row["held"].as_array().expect("held is an array");
        assert_eq!(
            held.len(),
            1,
            "alice's 5h window is the sole hold: {held:?}"
        );
        let entry = &held[0];
        assert_eq!(entry["window"], "5h");
        assert!(
            entry["resetAtMs"].is_i64(),
            "held entry keeps resetAtMs: {entry}"
        );
        assert!(
            entry["minutesUntilReset"].is_i64(),
            "held entry keeps minutesUntilReset: {entry}"
        );
        // held[]'s own two keys are unaffected by the new top-level fields.
        assert_eq!(
            entry.as_object().expect("held entry is an object").len(),
            3,
            "held entry shape (window/resetAtMs/minutesUntilReset) is unchanged: {entry}"
        );
    }

    #[tokio::test]
    async fn render_accounts_text_carries_countdown_for_a_below_threshold_window() {
        // The text-line half of the same restored invariant: a window well
        // under its threshold still has a live reset, so the greppable status
        // line must carry its `(+countdown)` even though the window never
        // binds a hold.
        let config = load_from(TWO_ACCOUNTS);
        let snapshot = snapshot_offline(
            config,
            Arc::new(NoRefresh),
            Arc::new(BelowThresholdWindowProber),
            true,
        )
        .await;
        let text = render_accounts(&snapshot, StatusSource::Live);
        let alice = text
            .lines()
            .find(|l| l.contains("alice@example.com"))
            .expect("alice line");
        assert!(
            alice.contains("5h=30%(+"),
            "below-threshold window still carries a countdown: {alice}"
        );
    }

    #[tokio::test]
    async fn render_accounts_text_omits_countdown_when_no_live_reset_exists() {
        let config = load_from(TWO_ACCOUNTS);
        let snapshot = snapshot_offline(
            config,
            Arc::new(NoRefresh),
            Arc::new(BelowThresholdWindowProber),
            true,
        )
        .await;
        let text = render_accounts(&snapshot, StatusSource::Live);
        let bob = text
            .lines()
            .find(|l| l.contains("bob@example.com"))
            .expect("bob line");
        // bob's 5h window was never learned at all: n/a, no `(+…)` token.
        assert!(bob.contains("5h=n/a"), "unlearned window reads n/a: {bob}");
        assert!(
            !bob.contains("5h=n/a("),
            "no countdown token on an unlearned window: {bob}"
        );
    }

    /// A prober that drives every account's 5h window fully over the 1.0
    /// exhaustion line, with a future reset — so the window binds and reads `spent`.
    struct ExhaustedProber;
    impl UsageProber for ExhaustedProber {
        fn probe(&self, _access_token: String) -> ProbeFuture {
            let now = crate::now_ms();
            Box::pin(async move {
                Ok(Usage {
                    five_hour: Some(UsageBucket {
                        utilization: Some(1.0),
                        reset_at_ms: Some(now + 45 * 60 * 1000),
                    }),
                    seven_day: None,
                    seven_day_oi: None,
                })
            })
        }
    }

    #[tokio::test]
    async fn render_accounts_marks_fully_exhausted_account_as_spent_with_countdown() {
        let config = load_from(TWO_ACCOUNTS);
        let snapshot =
            snapshot_offline(config, Arc::new(NoRefresh), Arc::new(ExhaustedProber), true).await;
        let text = render_accounts(&snapshot, StatusSource::Live);
        let line = text
            .lines()
            .find(|l| l.contains("alice@example.com"))
            .expect("alice line");
        // At 1.0 util the account is truly spent (never masquerading as `full`),
        // and its binding 5h window carries an inline `(+…)` countdown. The exact
        // minute drifts with wall-clock between probe and render, so match the `(+`.
        assert!(line.contains("state=spent"), "exhausted → spent: {line}");
        assert!(
            line.contains("5h=100%(+"),
            "5h exhausted + countdown: {line}"
        );
    }

    // ---------------------------------------------------------------------
    // The golden `tcr status --json` contract fixture.
    // ---------------------------------------------------------------------

    /// Path of the fixture BOTH sides read. One file, never a copy: the whole
    /// point is that the Swift decoder and this renderer cannot drift apart, and
    /// two files that must stay equal are the drift this exists to prevent.
    /// `apps/macos/Tests/TcrBarTests/RealWorldDecodeTests.swift` reaches the same
    /// path from `#filePath`.
    fn status_contract_fixture_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/status-contract.json")
    }

    /// One fully-specified account row for the fixture.
    ///
    /// Every field is passed explicitly rather than defaulted, so a new field on
    /// [`AccountSnapshot`] breaks this call site and forces a decision about what
    /// the contract sample should say about it.
    #[allow(clippy::too_many_arguments)]
    fn contract_account(
        name: &str,
        priority: i64,
        status: &str,
        disabled: bool,
        five_hour: Option<f64>,
        five_hour_reset: Option<OffsetDateTime>,
        seven_day: Option<f64>,
        seven_day_reset: Option<OffsetDateTime>,
        seven_day_oi: Option<f64>,
        probe_status: crate::probe::ProbeStatus,
        probe_error: Option<&str>,
        quota_state: QuotaState,
        groups: &[&str],
    ) -> AccountSnapshot {
        AccountSnapshot {
            name: name.to_string(),
            priority,
            status: status.to_string(),
            disabled,
            five_hour,
            five_hour_reset,
            seven_day,
            seven_day_reset,
            seven_day_oi,
            requests: if five_hour.is_some() { 102 } else { 0 },
            input_tokens: if five_hour.is_some() { 8_000_000 } else { 0 },
            output_tokens: if five_hour.is_some() { 31_860 } else { 0 },
            cache_read_tokens: if five_hour.is_some() { 6_000_000 } else { 0 },
            cache_creation_tokens: 0,
            last_used: None,
            rate_limited_until: None,
            probe_status,
            last_probe: None,
            probe_error: probe_error.map(str::to_string),
            quota_state,
            gate: crate::stats::GateReason::Ok,
            free_at: None,
            stream_error_count: if five_hour.is_some() { 2 } else { 0 },
            last_stream_error: if five_hour.is_some() {
                Some("overloaded_error".to_string())
            } else {
                None
            },
            groups: groups.iter().map(|g| g.to_string()).collect(),
        }
    }

    /// A fixed instant in the PAST, so the two clock-derived fields render
    /// deterministically.
    ///
    /// `minutesUntilReset` is `(reset - now).whole_minutes().max(0)` and
    /// `secondsUntilFree` is dropped entirely once `free_at` has elapsed, so
    /// every reset instant here is historical and both render as their clamped /
    /// absent value. That buys a byte-for-byte comparison against raw renderer
    /// output with **no normalisation layer** — and a normalisation layer is
    /// exactly where a contract pin rots, because it is the one part of the
    /// comparison nothing checks.
    fn fixture_instant(unix_ms: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp_nanos(unix_ms as i128 * 1_000_000)
            .expect("fixture instant is a valid timestamp")
    }

    /// The snapshot the fixture is rendered from. Obviously-fake data only —
    /// this repository is public.
    fn status_contract_snapshot() -> (StatsSnapshot, Vec<f64>) {
        // 2026-01-01T00:00:00Z and 2026-01-02T00:00:00Z, both long past.
        let five_hour_reset = fixture_instant(1_767_225_600_000);
        let seven_day_reset = fixture_instant(1_767_312_000_000);

        let accounts = vec![
            // `ok`: probed, measured, in rotation — the ordinary row.
            contract_account(
                "alice@example.com",
                0,
                "active",
                false,
                Some(0.04),
                Some(five_hour_reset),
                Some(0.01),
                Some(seven_day_reset),
                Some(0.0),
                crate::probe::ProbeStatus::Ok,
                None,
                QuotaState::Normal,
                // Non-empty and multi-membership — the only row the Swift decode
                // test has real group data to read, and the one the greppable
                // `groups=codereview,dev` text-line assertion targets.
                &["codereview", "dev"],
            ),
            // `near`: at threshold, so `held` carries a window — the only row
            // that exercises the nested object.
            contract_account(
                "bob@example.com",
                1,
                "active",
                false,
                Some(0.95),
                Some(five_hour_reset),
                Some(0.91),
                Some(seven_day_reset),
                Some(0.10),
                crate::probe::ProbeStatus::RateLimited,
                None,
                QuotaState::NearLimit,
                &[],
            ),
            // `spent`: fully consumed, and carrying a probe error string.
            contract_account(
                "carol@example.com",
                2,
                "throttled",
                false,
                Some(1.0),
                Some(five_hour_reset),
                Some(1.0),
                Some(seven_day_reset),
                Some(0.5),
                crate::probe::ProbeStatus::Error,
                Some("probe failed: connection reset"),
                QuotaState::Exhausted,
                &[],
            ),
            // Never probed AND disabled: the four quota fractions and
            // `cacheHitRatio` are all null. This row is the one that shipped a
            // decode crash — `valueNotFound … Path: [2].quota`. Also the
            // unlabelled row: `groups` must render `[]`, never `null`.
            contract_account(
                "dave@example.com",
                3,
                "active",
                true,
                None,
                None,
                None,
                None,
                None,
                crate::probe::ProbeStatus::Never,
                None,
                QuotaState::Normal,
                &[],
            ),
        ];
        let mut accounts = accounts;
        // `freeAtMs` and `rateLimitedUntilMs` must be pinned as *numbers*
        // somewhere in the sample, or the fixture would only ever prove they can
        // be null — which a deleted key also looks like. Both instants are past,
        // so `secondsUntilFree` (the one clock-derived companion) stays absent
        // and the render stays deterministic.
        accounts[2].free_at = Some(fixture_instant(1_767_312_000_000));
        accounts[2].rate_limited_until = Some(fixture_instant(1_767_225_600_000));

        let thresholds = vec![0.9; accounts.len()];
        (
            StatsSnapshot {
                accounts,
                current: Some(0),
                recent: Vec::new(),
                sessions: Vec::new(),
            },
            thresholds,
        )
    }

    /// The exact bytes the fixture file must hold.
    fn status_contract_rendered() -> String {
        let (snapshot, thresholds) = status_contract_snapshot();
        let build = BuildInfo {
            sha: "abc1234".to_string(),
            dirty: Some(false),
            built_at: "2026-01-01T00:00:00Z".to_string(),
        };
        // Trailing newline so the file is a well-formed text file; the renderer
        // itself emits none.
        format!(
            "{}\n",
            render_accounts_json(
                &snapshot,
                &thresholds,
                StatusSource::Live,
                Some(&build),
                false,
                None
            )
        )
    }

    /// THE CROSS-LANGUAGE CONTRACT PIN.
    ///
    /// `tcr status --json` is decoded by TcrBar (`FleetStatus.swift`), and no
    /// compiler checks that seam — the key names cross as strings. This test
    /// asserts the renderer still produces the committed fixture byte-for-byte,
    /// and `RealWorldDecodeTests.testCommittedContractFixtureDecodes` decodes
    /// **that same file** through `Fleet.decode`.
    ///
    /// So a renamed key cannot land quietly: this test goes red first, and
    /// regenerating the fixture to satisfy it (`TCR_UPDATE_FIXTURES=1 cargo test
    /// status_contract_fixture_matches_committed`) turns the Swift side red in
    /// the same breath. Both reds were observed before this was believed.
    #[test]
    fn status_contract_fixture_matches_committed() {
        let path = status_contract_fixture_path();
        let rendered = status_contract_rendered();

        if std::env::var_os("TCR_UPDATE_FIXTURES").is_some() {
            fs::create_dir_all(path.parent().expect("fixture dir")).expect("create fixture dir");
            fs::write(&path, &rendered).expect("write fixture");
            eprintln!("regenerated {}", path.display());
            return;
        }

        let committed = fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "cannot read the committed contract fixture at {}: {e}. \
                 Regenerate with TCR_UPDATE_FIXTURES=1 cargo test \
                 status_contract_fixture_matches_committed",
                path.display()
            )
        });

        assert_eq!(
            committed,
            rendered,
            "`tcr status --json` no longer renders the committed contract \
             fixture at {}. If the change is intended, regenerate it with \
             TCR_UPDATE_FIXTURES=1 and then run the Swift decode suite \
             (apps/macos: swift test) — that is the half of the contract this \
             process cannot check.",
            path.display()
        );
    }

    /// The fixture is only worth pinning if it is *representative*: a sample
    /// covering one shape would pin one shape. Asserted against the committed
    /// bytes rather than the renderer, so a fixture regenerated from a
    /// narrowed snapshot fails here too.
    #[test]
    fn status_contract_fixture_covers_every_rendered_shape() {
        let committed = fs::read_to_string(status_contract_fixture_path())
            .expect("committed contract fixture is readable");
        let rows: Vec<serde_json::Value> =
            serde_json::from_str(&committed).expect("fixture is a bare JSON array");
        assert_eq!(rows.len(), 4, "one row per covered shape");

        let states: Vec<&str> = rows
            .iter()
            .map(|r| r["quotaState"].as_str().expect("quotaState is a string"))
            .collect();
        assert!(states.contains(&"ok"), "{states:?}");
        assert!(states.contains(&"near"), "{states:?}");
        assert!(states.contains(&"spent"), "{states:?}");

        let probes: Vec<&str> = rows
            .iter()
            .map(|r| r["probeStatus"].as_str().expect("probeStatus is a string"))
            .collect();
        assert!(probes.contains(&"never"), "{probes:?}");
        assert!(probes.contains(&"ok"), "{probes:?}");
        assert!(probes.contains(&"rate-limited"), "{probes:?}");
        assert!(probes.contains(&"error"), "{probes:?}");

        // The never-probed row: every optional the Swift model types as
        // optional really is null somewhere in this sample. `get`, not
        // indexing — indexing a MISSING key also yields Null, which would let a
        // deleted key pass as a null.
        let never = rows
            .iter()
            .find(|r| r["probeStatus"] == "never")
            .expect("a never-probed row");
        for key in [
            "quota",
            "fiveHour",
            "sevenDay",
            "sevenDayOi",
            "cacheHitRatio",
        ] {
            assert_eq!(
                never.get(key),
                Some(&serde_json::Value::Null),
                "{key} is present AND null on the never-probed row: {never}"
            );
        }
        assert_eq!(never["disabled"], serde_json::json!(true));

        // At least one row carries a `held` window, one carries a probe error,
        // and one carries a stream error — the three nested/optional shapes the
        // panel renders differently.
        assert!(
            rows.iter()
                .any(|r| !r["held"].as_array().expect("held is an array").is_empty()),
            "some row is held: {committed}"
        );
        assert!(
            rows.iter().any(|r| r["probeError"].is_string()),
            "some row carries a probe error: {committed}"
        );
        assert!(
            rows.iter().any(|r| r["lastStreamError"].is_string()),
            "some row carries a stream error: {committed}"
        );
        assert!(
            rows.iter().any(|r| r["freeAtMs"].is_i64()),
            "some row pins freeAtMs as a number: {committed}"
        );
        assert!(
            rows.iter().any(|r| r["rateLimitedUntilMs"].is_i64()),
            "some row pins rateLimitedUntilMs as a number: {committed}"
        );
        assert!(
            rows.iter().any(|r| !r["groups"]
                .as_array()
                .expect("groups is an array")
                .is_empty()),
            "some row carries non-empty groups: {committed}"
        );
        assert!(
            rows.iter().any(|r| r["groups"]
                .as_array()
                .expect("groups is an array")
                .is_empty()),
            "some row carries no groups, rendered as [] not null: {committed}"
        );

        // Public repo: the fixture carries no real account data. A positive
        // control on the same probe — the fake domain really is present — so an
        // empty match cannot read as "clean".
        assert!(
            committed.contains("@example.com"),
            "positive control: the fixture uses example.com addresses"
        );
        for row in &rows {
            let name = row["name"].as_str().expect("name is a string");
            assert!(
                name.ends_with("@example.com"),
                "every fixture account is obviously fake: {name}"
            );
        }
    }

    /// `groups` on the JSON path: an array for a labelled account (including
    /// multi-membership), and `[]` — never `null` — for an unlabelled one. This
    /// is config, not a serving counter, so it does not get the
    /// null-on-offline treatment other fields get.
    #[test]
    fn groups_render_as_array_never_null() {
        let (snapshot, thresholds) = status_contract_snapshot();
        let json = render_accounts_json(
            &snapshot,
            &thresholds,
            StatusSource::Live,
            None,
            false,
            None,
        );
        let rows: Vec<serde_json::Value> = serde_json::from_str(&json).expect("valid json");
        let alice = rows
            .iter()
            .find(|r| r["name"] == "alice@example.com")
            .expect("alice row");
        assert_eq!(
            alice["groups"],
            serde_json::json!(["codereview", "dev"]),
            "labelled account carries its groups: {alice}"
        );
        let dave = rows
            .iter()
            .find(|r| r["name"] == "dave@example.com")
            .expect("dave row");
        assert_eq!(
            dave["groups"],
            serde_json::json!([]),
            "unlabelled account renders [], never null: {dave}"
        );
    }

    /// `groups=` on the text path: a greppable `groups=codereview,dev` token for
    /// a labelled account, and the token omitted entirely (not `groups=`) for an
    /// unlabelled one — the same "nothing to say" idiom as `fable=` and
    /// `last_stream_error=`.
    #[test]
    fn groups_text_line_greppable_and_omitted_when_empty() {
        let (snapshot, _thresholds) = status_contract_snapshot();
        let text = render_accounts(&snapshot, StatusSource::Live);
        let alice_line = text
            .lines()
            .find(|l| l.contains("alice@example.com"))
            .expect("alice line");
        assert!(
            alice_line.contains("groups=codereview,dev"),
            "labelled account's groups are greppable: {alice_line}"
        );
        let dave_line = text
            .lines()
            .find(|l| l.contains("dave@example.com"))
            .expect("dave line");
        assert!(
            !dave_line.contains("groups="),
            "unlabelled account omits the token entirely: {dave_line}"
        );
    }
}
