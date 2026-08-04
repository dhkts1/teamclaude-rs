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
/// `5h=n/a`. The inline `(+countdown)` appears ONLY when the window is a binding
/// hold — util known, at/over its threshold, and a future reset to count down to.
fn render_window(
    label: &str,
    util: Option<f64>,
    reset: Option<OffsetDateTime>,
    threshold: f64,
    now: OffsetDateTime,
) -> String {
    match util {
        None => format!("{label}=n/a"),
        Some(u) => {
            let held = u >= threshold && reset.is_some_and(|r| r > now);
            let hold = match (held, reset) {
                (true, Some(r)) => format!("({})", format_reset_countdown(r, now)),
                _ => String::new(),
            };
            format!("{label}={:.0}%{hold}", u * 100.0)
        }
    }
}

/// Render a [`StatsSnapshot`] as plain text — ONE LINE PER ACCOUNT — so the
/// output is greppable (`account NAME priority=P quota=Q% status=S ...`). The
/// ratatui TUI renderer is unusable for stdout, and per Gil's greppable-output
/// rule a naive grep must be able to match any single field.
///
/// `source` is taken rather than inferred because one token depends on it:
/// `stream_errors` is a SERVING counter, so an offline snapshot's zero is
/// structurally unmeasured (see [`StatusSource`]) and renders `n/a`, never `0`.
pub fn render_accounts(
    snapshot: &StatsSnapshot,
    thresholds: &[f64],
    source: StatusSource,
) -> String {
    if snapshot.accounts.is_empty() {
        return "no accounts configured\n".to_string();
    }
    let now = OffsetDateTime::now_utc();
    let mut out = String::new();
    for (i, a) in snapshot.accounts.iter().enumerate() {
        // A missing threshold (slice shorter than accounts) can only fail closed:
        // 1.0 means "only a fully-exhausted window reads as held", never a false hold.
        let threshold = thresholds.get(i).copied().unwrap_or(1.0);
        let five_hour = render_window("5h", a.five_hour, a.five_hour_reset, threshold, now);
        let seven_day = render_window("wk", a.seven_day, a.seven_day_reset, threshold, now);
        // Fable's model-scoped weekly never gates the general view: no reset field,
        // no countdown, and the whole token is omitted when it was never learned.
        let fable = match a.seven_day_oi {
            Some(u) => format!(" fable={:.0}%", u * 100.0),
            None => String::new(),
        };
        // Prompt-cache hit ratio. `n/a` — never `0%` — when there is no input to
        // divide by (R3: no NaN), matching the `5h=n/a` / `wk=n/a` idiom: an
        // OFFLINE snapshot's counters live in the server's process and are
        // structurally zero here, and rendering that as a measured 0% is exactly
        // the lie that hid a real prompt-cache catastrophe. Greppable `cache=NN%`
        // token for parity with the JSON field.
        let cache = if a.input_tokens == 0 {
            " cache=n/a".to_string()
        } else {
            format!(
                " cache={:.0}%",
                a.cache_read_tokens as f64 / a.input_tokens as f64 * 100.0
            )
        };
        // In-band SSE `error` events this account's streams carried (decayed
        // window). A truncated 200 books as a clean serve, so this is the only
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
        out.push_str(&format!(
            "account {} priority={} {} {}{}{} state={} status={} probe={}{}{}{}\n",
            a.name,
            a.priority,
            five_hour,
            seven_day,
            fable,
            cache,
            quota_state_token(a.quota_state),
            a.status,
            a.probe_status.as_str(),
            stream_errors,
            last_stream_error,
            if a.disabled { " disabled" } else { "" },
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
                "name": a.name,
                "priority": a.priority,
                "status": a.status,
                "disabled": a.disabled,
                "quota": quota,
                "quotaState": quota_state_token(a.quota_state),
                "fiveHour": a.five_hour,
                "sevenDay": a.seven_day,
                "sevenDayOi": a.seven_day_oi,
                "requests": a.requests,
                "inputTokens": a.input_tokens,
                "outputTokens": a.output_tokens,
                "cacheReadTokens": a.cache_read_tokens,
                // Prompt-cache hit ratio (0.0–1.0): cache_read / input_total, and
                // `null` — NEVER a literal 0.0 — when `inputTokens` is 0 and there
                // is nothing to divide by (also keeps NaN out — R3).
                //
                // The null is the honesty fix. `source: "offline"` means these
                // counters come from a fresh process, not the serving one, so they
                // are structurally zero; emitting `0.0` there published an
                // unmeasured number as a measured "0% cache hits" for every account
                // forever, and that false zero is precisely why a real prompt-cache
                // catastrophe went unseen. `null` says "not measured"; `source`
                // says which process would have measured it.
                "cacheHitRatio": if a.input_tokens == 0 {
                    serde_json::Value::Null
                } else {
                    serde_json::json!(a.cache_read_tokens as f64 / a.input_tokens as f64)
                },
                "probeStatus": a.probe_status.as_str(),
                "probeError": a.probe_error,
                // Decayed count of in-band SSE `error` events — a truncated 200
                // that books as a clean serve. `null`, NEVER 0, on the offline
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
    let thresholds = resolve_thresholds(&config);
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
    print!(
        "{}",
        render_accounts(&snapshot, &thresholds, StatusSource::Offline)
    );
    Ok(())
}

/// Why a live status read did not produce a payload.
enum LiveStatusError {
    /// Nothing is listening on the configured port — the ORDINARY case (`tcr
    /// status` with no server running). Falling back is expected, so it is
    /// reported by the `offline` label alone rather than by a warning.
    NoServer,
    /// A server answered but the read was not usable. Always warned about: a
    /// silently-swallowed rejection here would look exactly like "no server",
    /// which is how an api-key typo becomes a mysterious all-zero status.
    Unusable(String),
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
            return Err(LiveStatusError::Unusable(
                "the server did not answer within 5s".to_string(),
            ))
        }
        Err(e) => return Err(LiveStatusError::Unusable(e.to_string())),
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
/// own probe loop (every `quotaProbeSeconds`, 75s by default), which is both
/// fresher in practice than a cold one-shot probe and one fewer caller hitting the
/// usage endpoint — that endpoint rate-limits, and a second prober racing the
/// server's is what makes a whole fleet read `probe=rate-limited`. Only the
/// offline fallback, which has no server to inherit quota from, probes.
pub async fn status(config_path: &Path, json: bool) -> anyhow::Result<()> {
    // Read-only verb: plain load, no clobber-warning (we never save).
    let config = config::load(config_path)
        .with_context(|| format!("load config at {}", config_path.display()))?;

    let (source, server_build, snapshot, thresholds) = match fetch_live_status(&config).await {
        Ok(payload) => {
            let build = payload.build.clone();
            let (snapshot, thresholds) = payload.into_snapshot();
            (StatusSource::Live, Some(build), snapshot, thresholds)
        }
        Err(reason) => {
            if let LiveStatusError::Unusable(why) = reason {
                eprintln!(
                    "[tcr] warning: could not read live status from the proxy on :{} ({why}) — falling back to an offline snapshot, whose serving counters are all zero.",
                    config.proxy.port
                );
            }
            let thresholds = resolve_thresholds(&config);
            let snapshot = snapshot_offline(
                config,
                Arc::new(NoRefresh),
                Arc::new(LiveUsageProber::new()),
                true,
            )
            .await;
            (StatusSource::Offline, None, snapshot, thresholds)
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
            render_accounts_json(&snapshot, &thresholds, source, server_build.as_ref())
        );
    } else {
        // One greppable `source=` line above the account lines, in the same
        // key=value idiom, so the provenance is visible without --json.
        println!(
            "status source={}{}{}",
            source.as_str(),
            match source {
                StatusSource::Live => String::new(),
                StatusSource::Offline =>
                    " note=serving-counters-unavailable-no-server-answered".to_string(),
            },
            build_fields(server_build.as_ref()),
        );
        print!("{}", render_accounts(&snapshot, &thresholds, source));
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
        // Offline is the honest source for a snapshot built by `snapshot_offline`,
        // and it is the exact line shape `tcr accounts` emits — that verb is
        // offline by construction, so this is the greppable line a human reads
        // most often.
        let text = render_accounts(&snapshot, &thresholds, StatusSource::Offline);
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
        // At 25% util no window is a binding hold → no inline countdown, and the
        // legacy `held=`/`quota=`/`quota_state=` fields are gone for good.
        assert!(
            !text.contains("(+"),
            "under-threshold accounts show no countdown: {text}"
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

        let json = render_accounts_json(&snapshot, &thresholds, StatusSource::Offline, None);
        let rows: Vec<serde_json::Value> = serde_json::from_str(&json).expect("valid json");
        assert_eq!(rows.len(), 2, "one row per account");
        for row in &rows {
            assert_eq!(
                row["inputTokens"], 0,
                "the premise: an offline snapshot has counted nothing"
            );
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
        // absence of data, not a blanket suppression.
        let mut counted = snapshot.clone();
        counted.accounts[0].input_tokens = 1_000;
        counted.accounts[0].cache_read_tokens = 750;
        let live = render_accounts_json(&counted, &thresholds, StatusSource::Live, None);
        let rows: Vec<serde_json::Value> = serde_json::from_str(&live).expect("valid json");
        assert_eq!(rows[0]["cacheHitRatio"], serde_json::json!(0.75));
        assert_eq!(rows[0]["source"], "live");
        assert!(
            rows[1]["cacheHitRatio"].is_null(),
            "the still-uncounted account stays null: {}",
            rows[1]
        );

        // The human view uses the same `n/a` idiom the unknown quota windows use.
        let text = render_accounts(&snapshot, &thresholds, StatusSource::Offline);
        for line in text.lines() {
            assert!(
                line.contains("cache=n/a"),
                "uncounted cache reads as n/a, never 0%: {line}"
            );
            assert!(!line.contains("cache=0%"), "no false measured zero: {line}");
        }
        assert!(
            render_accounts(&counted, &thresholds, StatusSource::Live).contains("cache=75%"),
            "a measured ratio still renders as a percentage"
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

        let offline = render_accounts_json(&snapshot, &thresholds, StatusSource::Offline, None);
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
        let live = render_accounts_json(&snapshot, &thresholds, StatusSource::Live, None);
        let rows: Vec<serde_json::Value> = serde_json::from_str(&live).expect("valid json");
        assert_eq!(rows[0]["streamErrorCount"], serde_json::json!(0));

        // Measured and dirty carries both the count and the latest error type.
        let mut errored = snapshot.clone();
        errored.accounts[0].stream_error_count = 3;
        errored.accounts[0].last_stream_error = Some("overloaded_error".to_string());
        let live = render_accounts_json(&errored, &thresholds, StatusSource::Live, None);
        let rows: Vec<serde_json::Value> = serde_json::from_str(&live).expect("valid json");
        assert_eq!(rows[0]["streamErrorCount"], serde_json::json!(3));
        assert_eq!(
            rows[0]["lastStreamError"],
            serde_json::json!("overloaded_error")
        );

        // The text view carries the same three states as greppable tokens.
        let text = render_accounts(&snapshot, &thresholds, StatusSource::Offline);
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
            render_accounts(&snapshot, &thresholds, StatusSource::Live).contains("stream_errors=0"),
            "a measured clean fleet still renders a real 0"
        );
        let text = render_accounts(&errored, &thresholds, StatusSource::Live);
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
        // held in this process only.
        manager.update_usage(0, 1_000, 200, 750, 50);
        tokio::spawn(async move { crate::mitm::serve(listener, manager, None).await });

        let payload = match fetch_live_status(&config).await {
            Ok(p) => p,
            Err(LiveStatusError::NoServer) => panic!("the spawned server did not answer"),
            Err(LiveStatusError::Unusable(why)) => panic!("live status unusable: {why}"),
        };
        let build = payload.build.clone();
        let (snapshot, thresholds) = payload.into_snapshot();

        let json = render_accounts_json(&snapshot, &thresholds, StatusSource::Live, Some(&build));
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

        let json = render_accounts_json(&snapshot, &thresholds, StatusSource::Offline, None);
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
            Err(LiveStatusError::Unusable(why)) => {
                panic!("a dead port is the ordinary no-server case, not a warning: {why}")
            }
            Ok(_) => panic!("nothing is listening on {port}"),
        }
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
        let text = render_accounts(&snapshot, &thresholds, StatusSource::Offline);
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
        // 3a: a held account's binding window carries an inline `(+countdown)`,
        // gated on the same per-account threshold `eligible` uses.
        let config = load_from(TWO_ACCOUNTS);
        let thresholds = resolve_thresholds(&config);
        let snapshot =
            snapshot_offline(config, Arc::new(NoRefresh), Arc::new(WindowProber), true).await;
        let text = render_accounts(&snapshot, &thresholds, StatusSource::Live);
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
        let json = render_accounts_json(&snapshot, &thresholds, StatusSource::Offline, None);
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
        let thresholds = resolve_thresholds(&config);
        let snapshot =
            snapshot_offline(config, Arc::new(NoRefresh), Arc::new(ExhaustedProber), true).await;
        let text = render_accounts(&snapshot, &thresholds, StatusSource::Live);
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
}
