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

use anyhow::{bail, Context as _};
use time::OffsetDateTime;

use crate::config::{self, Account, Config};
use crate::identity;
use crate::manager::Manager;
use crate::oauth::LiveRefresher;
use crate::probe::{LiveUsageProber, UsageProber};
use crate::stats::StatsSnapshot;

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
pub fn resolve_account(
    accounts: &[Account],
    query: &str,
    org: Option<&str>,
) -> anyhow::Result<usize> {
    let candidates = identity::match_accounts(accounts, query, org);

    match candidates.as_slice() {
        [] => {
            let org_note = org
                .map(|o| format!(" (with org matching '{o}')"))
                .unwrap_or_default();
            bail!("no account matches '{query}'{org_note}");
        }
        [only] => Ok(*only),
        many => {
            let names: Vec<&str> = many.iter().map(|&i| accounts[i].name.as_str()).collect();
            bail!(
                "'{query}' is ambiguous — matches {} accounts: {}. Narrow with --org or use an exact name.",
                names.len(),
                names.join(", ")
            );
        }
    }
}

/// Remove the account matching `query` from the config and save.
///
/// Resolution happens BEFORE any mutation, so a non-matching query returns an
/// error with the file left byte-identical (no partial write).
pub fn remove_account(config_path: &Path, query: &str, org: Option<&str>) -> anyhow::Result<()> {
    let mut config = load_for_edit(config_path)?;
    let idx = resolve_account(&config.accounts, query, org)?;
    let removed = config.accounts.remove(idx);
    save_after_edit(config_path, &config)?;
    println!("Removed account '{}'.", removed.name);
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
    let mut config = load_for_edit(config_path)?;
    let idx = resolve_account(&config.accounts, query, org)?;

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
    let name = config.accounts[idx].name.clone();
    save_after_edit(config_path, &config)?;
    println!("Set priority of '{name}' to {value}.");
    Ok(())
}

/// Enable or disable the account matching `query`.
///
/// `disabled = true` writes `"disabled": true`; `disabled = false` sets the
/// field to `None` so `skip_serializing_if = Option::is_none` DROPS the key
/// entirely — matching the JS `delete account.disabled`, not a `false` literal.
pub fn set_enabled(
    config_path: &Path,
    query: &str,
    org: Option<&str>,
    disabled: bool,
) -> anyhow::Result<()> {
    let mut config = load_for_edit(config_path)?;
    let idx = resolve_account(&config.accounts, query, org)?;
    config.accounts[idx].disabled = if disabled { Some(true) } else { None };
    let name = config.accounts[idx].name.clone();
    save_after_edit(config_path, &config)?;
    println!(
        "{} account '{name}'.",
        if disabled { "Disabled" } else { "Enabled" }
    );
    Ok(())
}

/// Build an OFFLINE snapshot from `config`: a [`Manager`] with `config_path =
/// None` (so a probe's token refresh is NEVER written to disk), optionally
/// probing every account's live quota first.
///
/// Split out from [`list_accounts`] / [`status`] so tests can inject a scripted
/// [`UsageProber`] and assert the on-disk config is byte-unchanged (the no-race
/// guarantee).
pub async fn snapshot_offline(
    config: Config,
    prober: Arc<dyn UsageProber>,
    probe: bool,
) -> StatsSnapshot {
    // config_path = None → persist_now / persist_tokens are silent no-ops, so a
    // probe here can never mutate the user's file.
    let manager = Manager::new(
        config,
        Arc::new(LiveRefresher::new()),
        prober,
        Arc::new(crate::warmer::LiveWarmer::new()),
        None,
    );
    if probe {
        manager.probe_all().await;
    }
    manager.snapshot(OffsetDateTime::now_utc())
}

/// Render a [`StatsSnapshot`] as plain text — ONE LINE PER ACCOUNT — so the
/// output is greppable (`account NAME priority=P quota=Q% status=S ...`). The
/// ratatui TUI renderer is unusable for stdout, and per Gil's greppable-output
/// rule a naive grep must be able to match any single field.
pub fn render_accounts(snapshot: &StatsSnapshot) -> String {
    if snapshot.accounts.is_empty() {
        return "no accounts configured\n".to_string();
    }
    let mut out = String::new();
    for a in &snapshot.accounts {
        // Gating quota = the most-spent of the known windows (matches the
        // rotation eligibility gate). `n/a` when nothing has been learned yet.
        let quota = [a.five_hour, a.seven_day, a.seven_day_oi]
            .into_iter()
            .flatten()
            .reduce(f64::max);
        let quota_str = match quota {
            Some(u) => format!("{:.0}%", u * 100.0),
            None => "n/a".to_string(),
        };
        out.push_str(&format!(
            "account {} priority={} quota={} status={} probe={}{}\n",
            a.name,
            a.priority,
            quota_str,
            a.status,
            a.probe_status.as_str(),
            if a.disabled { " disabled" } else { "" },
        ));
    }
    out
}

/// Render a [`StatsSnapshot`] as a JSON array, one object per account.
fn render_accounts_json(snapshot: &StatsSnapshot) -> String {
    let rows: Vec<serde_json::Value> = snapshot
        .accounts
        .iter()
        .map(|a| {
            let quota = [a.five_hour, a.seven_day, a.seven_day_oi]
                .into_iter()
                .flatten()
                .reduce(f64::max);
            serde_json::json!({
                "name": a.name,
                "priority": a.priority,
                "status": a.status,
                "disabled": a.disabled,
                "quota": quota,
                "fiveHour": a.five_hour,
                "sevenDay": a.seven_day,
                "sevenDayOi": a.seven_day_oi,
                "requests": a.requests,
                "inputTokens": a.input_tokens,
                "outputTokens": a.output_tokens,
                "probeStatus": a.probe_status.as_str(),
                "probeError": a.probe_error,
            })
        })
        .collect();
    serde_json::to_string_pretty(&rows).unwrap_or_else(|_| "[]".to_string())
}

/// `tcr accounts [--probe]` — list configured accounts (offline). With
/// `--probe`, first refresh every account's live quota (never persisted).
pub async fn list_accounts(config_path: &Path, probe: bool) -> anyhow::Result<()> {
    let config = load_for_edit(config_path)?;
    let snapshot = snapshot_offline(config, Arc::new(LiveUsageProber::new()), probe).await;
    print!("{}", render_accounts(&snapshot));
    Ok(())
}

/// `tcr status [--json]` — probe every account's live quota (offline, never
/// persisted) and render the fleet as greppable text or a JSON array.
pub async fn status(config_path: &Path, json: bool) -> anyhow::Result<()> {
    let config = load_for_edit(config_path)?;
    let snapshot = snapshot_offline(config, Arc::new(LiveUsageProber::new()), true).await;
    if json {
        println!("{}", render_accounts_json(&snapshot));
    } else {
        print!("{}", render_accounts(&snapshot));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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

    // --- enable / disable --------------------------------------------------

    #[test]
    fn set_enabled_true_writes_disabled_true() {
        let path = write_config("disable", TWO_ACCOUNTS);
        set_enabled(&path, "alice@example.com", None, true).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["accounts"][0]["disabled"], serde_json::json!(true));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn set_enabled_false_drops_the_disabled_key() {
        let path = write_config("enable", TWO_ACCOUNTS);
        // First disable, then re-enable — the key must vanish entirely.
        set_enabled(&path, "alice@example.com", None, true).unwrap();
        set_enabled(&path, "alice@example.com", None, false).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(
            value["accounts"][0].get("disabled").is_none(),
            "re-enable must DROP the disabled key, not write false"
        );
        fs::remove_file(&path).ok();
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
        let snapshot = snapshot_offline(config, Arc::new(FixedProber { util: 0.25 }), true).await;
        let text = render_accounts(&snapshot);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "one line per account");
        // Each line is greppable: name + priority + quota%.
        assert!(lines[0].contains("alice@example.com"));
        assert!(lines[0].contains("priority=0"));
        assert!(lines[0].contains("quota=25%"));
        assert!(lines[1].contains("bob@example.com"));
        assert!(lines[1].contains("priority=1"));
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
        let _snapshot = snapshot_offline(config, Arc::new(FixedProber { util: 0.5 }), true).await;
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(
            before, after,
            "an offline status probe must never persist to the config file"
        );
        fs::remove_file(&path).ok();
    }
}
