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
use crate::oauth::{NoRefresh, TokenRefresher};
use crate::probe::{LiveUsageProber, UsageProber};
use crate::stats::{AccountSnapshot, StatsSnapshot};

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
    let name = edit_account(config_path, query, org, |config, idx| {
        config.accounts[idx].disabled = if disabled { Some(true) } else { None };
        config.accounts[idx].name.clone()
    })?;
    println!(
        "{} account '{name}'.",
        if disabled { "Disabled" } else { "Enabled" }
    );
    Ok(())
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

/// The ` held=<win> resets=<HH:MMZ>(<+countdown>)` segment(s) appended to an
/// account's status line — one per held window, empty when the account holds none.
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

/// Render a [`StatsSnapshot`] as plain text — ONE LINE PER ACCOUNT — so the
/// output is greppable (`account NAME priority=P quota=Q% status=S ...`). The
/// ratatui TUI renderer is unusable for stdout, and per Gil's greppable-output
/// rule a naive grep must be able to match any single field.
pub fn render_accounts(snapshot: &StatsSnapshot, thresholds: &[f64]) -> String {
    if snapshot.accounts.is_empty() {
        return "no accounts configured\n".to_string();
    }
    let now = OffsetDateTime::now_utc();
    let mut out = String::new();
    for (i, a) in snapshot.accounts.iter().enumerate() {
        let quota = gating_quota(a);
        let quota_str = match quota {
            Some(u) => format!("{:.0}%", u * 100.0),
            None => "n/a".to_string(),
        };
        // A missing threshold (slice shorter than accounts) can only fail closed:
        // 1.0 means "only a fully-exhausted window reads as held", never a false hold.
        let threshold = thresholds.get(i).copied().unwrap_or(1.0);
        out.push_str(&format!(
            "account {} priority={} quota={} status={} probe={}{}{}\n",
            a.name,
            a.priority,
            quota_str,
            a.status,
            a.probe_status.as_str(),
            if a.disabled { " disabled" } else { "" },
            held_suffix(a, threshold, now),
        ));
    }
    out
}

/// Render a [`StatsSnapshot`] as a JSON array, one object per account.
fn render_accounts_json(snapshot: &StatsSnapshot, thresholds: &[f64]) -> String {
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
                "held": held,
            })
        })
        .collect();
    serde_json::to_string_pretty(&rows).unwrap_or_else(|_| "[]".to_string())
}

/// `tcr accounts [--probe]` — list configured accounts (offline). With
/// `--probe`, first refresh every account's live quota (never persisted).
pub async fn list_accounts(config_path: &Path, probe: bool) -> anyhow::Result<()> {
    let config = load_for_edit(config_path)?;
    let thresholds = resolve_thresholds(&config);
    let snapshot = snapshot_offline(
        config,
        Arc::new(NoRefresh),
        Arc::new(LiveUsageProber::new()),
        probe,
    )
    .await;
    print!("{}", render_accounts(&snapshot, &thresholds));
    Ok(())
}

/// `tcr status [--json]` — probe every account's live quota (offline, never
/// persisted) and render the fleet as greppable text or a JSON array.
pub async fn status(config_path: &Path, json: bool) -> anyhow::Result<()> {
    let config = load_for_edit(config_path)?;
    let thresholds = resolve_thresholds(&config);
    let snapshot = snapshot_offline(
        config,
        Arc::new(NoRefresh),
        Arc::new(LiveUsageProber::new()),
        true,
    )
    .await;
    if json {
        println!("{}", render_accounts_json(&snapshot, &thresholds));
    } else {
        print!("{}", render_accounts(&snapshot, &thresholds));
    }
    Ok(())
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
        let thresholds = resolve_thresholds(&config);
        let snapshot = snapshot_offline(
            config,
            Arc::new(NoRefresh),
            Arc::new(FixedProber { util: 0.25 }),
            true,
        )
        .await;
        let text = render_accounts(&snapshot, &thresholds);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "one line per account");
        // Each line is greppable: name + priority + quota%.
        assert!(lines[0].contains("alice@example.com"));
        assert!(lines[0].contains("priority=0"));
        assert!(lines[0].contains("quota=25%"));
        assert!(lines[1].contains("bob@example.com"));
        assert!(lines[1].contains("priority=1"));
        // At 25% util, well under the 0.9 default threshold, no account is held.
        assert!(
            !text.contains("held="),
            "under-threshold accounts show no hold: {text}"
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
        let thresholds = resolve_thresholds(&config);
        let calls = Arc::new(AtomicU64::new(0));
        let refresher = Arc::new(RecordingRefresher {
            calls: calls.clone(),
        });
        let snapshot = snapshot_offline(config, refresher, Arc::new(RejectingProber), true).await;
        let text = render_accounts(&snapshot, &thresholds);
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
        // 3a: a held account's line carries `held=<win> resets=<HH:MMZ>(<+countdown>)`,
        // gated on the same per-account threshold `eligible` uses (0.9 default here).
        let config = load_from(TWO_ACCOUNTS);
        let thresholds = resolve_thresholds(&config);
        let snapshot =
            snapshot_offline(config, Arc::new(NoRefresh), Arc::new(WindowProber), true).await;
        let text = render_accounts(&snapshot, &thresholds);
        let alice = text
            .lines()
            .find(|l| l.contains("alice@example.com"))
            .expect("alice line");
        let bob = text
            .lines()
            .find(|l| l.contains("bob@example.com"))
            .expect("bob line");
        // alice (at-a) is 5h-held; bob (at-b) is 7d-held. Each shows its own window.
        assert!(alice.contains("held=5h"), "5h hold labelled: {alice}");
        assert!(
            alice.contains("resets=") && alice.contains("(+"),
            "reset+countdown: {alice}"
        );
        assert!(!alice.contains("held=7d"), "alice is not 7d-held: {alice}");
        assert!(bob.contains("held=7d"), "7d hold labelled: {bob}");
        assert!(bob.contains("resets="), "reset shown: {bob}");
        assert!(!bob.contains("held=5h"), "bob is not 5h-held: {bob}");
        // Greppable one-line-per-account contract survives the new fields.
        assert_eq!(
            text.lines().count(),
            2,
            "still one line per account: {text}"
        );

        // JSON mirrors the held fields for machine consumers.
        let json = render_accounts_json(&snapshot, &thresholds);
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
}
