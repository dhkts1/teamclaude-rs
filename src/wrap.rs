//! `tcr wrap` — a weekly (or N-day) usage report, read straight off the usage
//! ledger `src/usage.rs` writes.
//!
//! # Why this reads the ledger and not `tcr status`
//!
//! `tcr status` answers "what is happening right now" from the live process's
//! in-memory buckets (today, the last hour, the 5-hour window). This answers
//! "what happened this week", which is a question about history the running
//! process does not keep — the ring only covers six hours and `today` rolls
//! over at midnight. The ledger's day files are the only durable record, so
//! this reads them directly, offline, whether or not a proxy is running.
//!
//! # Reuse, not a second parser
//! [`crate::usage::parse_ledger_file`] and the `LedgerLine` shape are shared
//! with [`crate::usage::UsageTracker`]'s own boot-time replay — this module
//! never re-implements the line format. Pricing is [`crate::pricing`], the
//! exact table and formula the live proxy uses.
//!
//! # Grouping by account, not by identity
//!
//! `tcr status` resolves a ledger line back to a *configured* account
//! (`resolve_account`), because it must attribute a line to a specific,
//! currently-live rotation slot. This report has no such need — it just adds
//! up what each stored name did — so it groups on `LedgerLine::a` verbatim,
//! with no config or account-identity resolution at all. That also means it
//! runs with no config file present, and reports an account that has since
//! been removed from the fleet.
//!
//! # Days are UTC calendar days
//!
//! One file per UTC day is what is on disk, so "day" here means the UTC day a
//! record's own ledger file is named after ([`crate::usage::ledger_date`]),
//! not the local calendar day `tcr status`'s `today` uses. That is also what
//! makes every number in this report reproducible with one `jq` command over
//! the files directly — see `docs/cli.md`.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;
use time::Date;

use crate::pricing::{cost_nanos, PricingOverride, PricingTable};
use crate::usage::{self, LedgerLine};

/// How many of the longest-running sessions to list.
const TOP_SESSIONS: usize = 3;

/// One bucket's running totals — requests, the five token dimensions, and cost
/// in nanodollars (see `pricing::cost_nanos` on why nanodollars). Mirrors
/// `usage::Totals` in shape, deliberately: this module aggregates the same
/// dimensions, just sliced by model/account/day instead of by minute.
#[derive(Debug, Clone, Copy, Default)]
struct Agg {
    requests: u64,
    input: u64,
    output: u64,
    cache_read: u64,
    cost_nanos: u64,
}

impl Agg {
    fn add(&mut self, pricing: &PricingTable, line: &LedgerLine) {
        self.requests += 1;
        self.input += line.i;
        self.output += line.o;
        self.cache_read += line.r;
        if let Some(price) = line.m.as_deref().and_then(|m| pricing.lookup(m)) {
            self.cost_nanos += cost_nanos(&price, line.i, line.c5, line.c1, line.r, line.o);
        }
    }

    fn cost_usd(&self) -> f64 {
        self.cost_nanos as f64 / 1e9
    }
}

/// A period's headline numbers, and the wire shape for `--json`'s `totals` and
/// `previous` fields.
#[derive(Debug, Clone, Copy, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Totals {
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cost_usd: f64,
    /// `cache_read_tokens / input_tokens`, or `None` when `input_tokens` is 0
    /// — nothing served is not a measured 0% hit rate. Same honest-null idiom
    /// `cli.rs::cache_hit_ratio` uses for `tcr status`.
    pub cache_hit_ratio: Option<f64>,
}

impl From<Agg> for Totals {
    fn from(agg: Agg) -> Self {
        Self {
            requests: agg.requests,
            input_tokens: agg.input,
            output_tokens: agg.output,
            cache_read_tokens: agg.cache_read,
            cost_usd: agg.cost_usd(),
            cache_hit_ratio: if agg.input == 0 {
                None
            } else {
                Some(agg.cache_read as f64 / agg.input as f64)
            },
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRow {
    pub model: String,
    pub requests: u64,
    pub cost_usd: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountRow {
    pub account: String,
    pub requests: u64,
    pub cost_usd: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DayRow {
    pub date: String,
    pub requests: u64,
    pub cost_usd: f64,
    /// Whether this is the busiest day (most requests) in the period. At most
    /// one row carries `true`; a tie is broken by the earliest date, so the
    /// mark never moves depending on map iteration order.
    pub busiest: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRow {
    /// The session id `s` carries — a hash, not a secret, and printed as the
    /// plain number it is.
    pub session: u64,
    pub requests: u64,
}

/// Cost and request-count change against the previous period, as a fraction
/// (`0.12` renders as `+12%`). `None` when the previous period had nothing to
/// compare against (0 requests, or 0 cost), because a percentage change off a
/// zero denominator is not a measurement.
#[derive(Debug, Clone, Copy, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Comparison {
    pub cost_change: Option<f64>,
    pub requests_change: Option<f64>,
}

fn pct_change(current: f64, previous: f64) -> Option<f64> {
    if previous <= 0.0 {
        None
    } else {
        Some((current - previous) / previous)
    }
}

/// The full report `tcr wrap` prints, and the whole of `--json`'s output.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WrapReport {
    pub days: u32,
    /// Inclusive UTC date range of the current period, `YYYY-MM-DD`.
    pub since: String,
    pub until: String,
    pub totals: Totals,
    /// Sorted by cost, highest first.
    pub by_model: Vec<ModelRow>,
    /// Sorted by requests, highest first.
    pub by_account: Vec<AccountRow>,
    /// One row per UTC day in the period, in date order.
    pub by_day: Vec<DayRow>,
    pub distinct_sessions: usize,
    /// The longest-running sessions, by request count, highest first.
    pub top_sessions: Vec<SessionRow>,
    /// The immediately preceding period of the same length.
    pub previous: Totals,
    pub comparison: Comparison,
    /// Lines that were not parseable JSON of the expected shape, over the
    /// current period's files. Counted, never fatal — see
    /// [`crate::usage::parse_ledger_file`].
    pub malformed_lines: usize,
}

/// `n` UTC dates ending at (and including) `end`, ascending.
fn dates_ending_at(end: Date, n: u32) -> Vec<Date> {
    (0..n)
        .rev()
        .map(|back| end - time::Duration::days(i64::from(back)))
        .collect()
}

/// Read one period's files into `model`/`account`/`day`/`sessions`, and fold
/// every line into `total`. Returns the malformed-line count over the period.
#[allow(clippy::too_many_arguments)]
fn accumulate(
    dir: &Path,
    dates: &[Date],
    pricing: &PricingTable,
    total: &mut Agg,
    model: &mut BTreeMap<String, Agg>,
    account: &mut BTreeMap<String, Agg>,
    day: &mut BTreeMap<Date, Agg>,
    sessions: &mut BTreeMap<u64, u64>,
) -> usize {
    let mut malformed = 0;
    for date in dates {
        let path = dir.join(format!("{}.jsonl", usage::date_string(*date)));
        let (lines, bad) = usage::parse_ledger_file(&path);
        malformed += bad;
        for line in &lines {
            total.add(pricing, line);
            model
                .entry(line.m.clone().unwrap_or_else(|| "unknown".to_string()))
                .or_default()
                .add(pricing, line);
            account
                .entry(line.a.clone())
                .or_default()
                .add(pricing, line);
            day.entry(usage::ledger_date(line.t))
                .or_default()
                .add(pricing, line);
            if let Some(session) = line.s {
                *sessions.entry(session).or_default() += 1;
            }
        }
    }
    malformed
}

/// Build the report for the `days` UTC days ending at `now_ms`'s own UTC day,
/// reading `dir` for both this period and the equal-length period immediately
/// before it.
pub fn build_report(dir: &Path, days: u32, now_ms: i64, pricing: &PricingTable) -> WrapReport {
    let days = days.max(1);
    let today = usage::ledger_date(now_ms);
    let current_dates = dates_ending_at(today, days);
    let previous_end = today - time::Duration::days(i64::from(days));
    let previous_dates = dates_ending_at(previous_end, days);

    let mut total = Agg::default();
    let mut model_agg: BTreeMap<String, Agg> = BTreeMap::new();
    let mut account_agg: BTreeMap<String, Agg> = BTreeMap::new();
    let mut day_agg: BTreeMap<Date, Agg> = BTreeMap::new();
    let mut sessions: BTreeMap<u64, u64> = BTreeMap::new();
    let malformed_lines = accumulate(
        dir,
        &current_dates,
        pricing,
        &mut total,
        &mut model_agg,
        &mut account_agg,
        &mut day_agg,
        &mut sessions,
    );

    let mut previous_total = Agg::default();
    let mut unused_model = BTreeMap::new();
    let mut unused_account = BTreeMap::new();
    let mut unused_day = BTreeMap::new();
    let mut unused_sessions = BTreeMap::new();
    accumulate(
        dir,
        &previous_dates,
        pricing,
        &mut previous_total,
        &mut unused_model,
        &mut unused_account,
        &mut unused_day,
        &mut unused_sessions,
    );

    let mut by_model: Vec<ModelRow> = model_agg
        .into_iter()
        .map(|(model, agg)| ModelRow {
            model,
            requests: agg.requests,
            cost_usd: agg.cost_usd(),
        })
        .collect();
    by_model.sort_by(|a, b| {
        b.cost_usd
            .partial_cmp(&a.cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.model.cmp(&b.model))
    });

    let mut by_account: Vec<AccountRow> = account_agg
        .into_iter()
        .map(|(account, agg)| AccountRow {
            account,
            requests: agg.requests,
            cost_usd: agg.cost_usd(),
        })
        .collect();
    by_account.sort_by(|a, b| {
        b.requests
            .cmp(&a.requests)
            .then_with(|| a.account.cmp(&b.account))
    });

    let busiest_date = day_agg
        .iter()
        .max_by(|a, b| a.1.requests.cmp(&b.1.requests).then(b.0.cmp(a.0)))
        .map(|(date, _)| *date);
    let by_day: Vec<DayRow> = day_agg
        .into_iter()
        .map(|(date, agg)| DayRow {
            date: usage::date_string(date),
            requests: agg.requests,
            cost_usd: agg.cost_usd(),
            busiest: Some(date) == busiest_date,
        })
        .collect();

    let mut top_sessions: Vec<SessionRow> = sessions
        .into_iter()
        .map(|(session, requests)| SessionRow { session, requests })
        .collect();
    top_sessions.sort_by(|a, b| {
        b.requests
            .cmp(&a.requests)
            .then_with(|| a.session.cmp(&b.session))
    });
    let distinct_sessions = top_sessions.len();
    top_sessions.truncate(TOP_SESSIONS);

    let totals = Totals::from(total);
    let previous = Totals::from(previous_total);
    let comparison = Comparison {
        cost_change: pct_change(totals.cost_usd, previous.cost_usd),
        requests_change: pct_change(totals.requests as f64, previous.requests as f64),
    };

    WrapReport {
        days,
        since: usage::date_string(current_dates[0]),
        until: usage::date_string(today),
        totals,
        by_model,
        by_account,
        by_day,
        distinct_sessions,
        top_sessions,
        previous,
        comparison,
        malformed_lines,
    }
}

/// Best-effort pricing overrides from the config file at `config_path` — the
/// same table the running proxy would price with, so a request that would
/// price the same live prices the same in a week-old report. Any read/parse
/// failure (no config, a broken one) falls back to the built-in table alone:
/// this is a read-only report and must never fail because a config is
/// missing.
pub fn pricing_table(config_path: &Path) -> PricingTable {
    let overrides: BTreeMap<String, PricingOverride> = crate::config::load(config_path)
        .map(|config| config.pricing.into_iter().collect())
        .unwrap_or_default();
    PricingTable::new(overrides)
}

fn fmt_pct(change: Option<f64>) -> String {
    match change {
        None => "n/a".to_string(),
        Some(change) => format!("{:+.0}%", change * 100.0),
    }
}

fn fmt_ratio(ratio: Option<f64>) -> String {
    match ratio {
        None => "n/a".to_string(),
        Some(ratio) => format!("{:.1}%", ratio * 100.0),
    }
}

/// Render the report as the plain-text terminal output — no colour, no line
/// wider than 100 columns.
pub fn render_text(report: &WrapReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "tcr wrap: last {} day(s), {} to {} (UTC)\n\n",
        report.days, report.since, report.until
    ));
    out.push_str(&format!(
        "totals requests={} input={} output={} cacheRead={} cost=${:.2} cacheHitRatio={}\n\n",
        report.totals.requests,
        report.totals.input_tokens,
        report.totals.output_tokens,
        report.totals.cache_read_tokens,
        report.totals.cost_usd,
        fmt_ratio(report.totals.cache_hit_ratio),
    ));

    out.push_str("by model (sorted by cost):\n");
    for row in &report.by_model {
        out.push_str(&format!(
            "  {:<28} requests={:<8} cost=${:.2}\n",
            row.model, row.requests, row.cost_usd
        ));
    }
    out.push('\n');

    out.push_str("by account (sorted by requests):\n");
    for row in &report.by_account {
        out.push_str(&format!(
            "  {:<28} requests={:<8} cost=${:.2}\n",
            row.account, row.requests, row.cost_usd
        ));
    }
    out.push('\n');

    out.push_str("by day (busiest marked *):\n");
    for row in &report.by_day {
        out.push_str(&format!(
            "  {} {} requests={:<8} cost=${:.2}\n",
            row.date,
            if row.busiest { "*" } else { " " },
            row.requests,
            row.cost_usd
        ));
    }
    out.push('\n');

    out.push_str(&format!(
        "sessions: {} distinct; top {} by requests:\n",
        report.distinct_sessions,
        report.top_sessions.len()
    ));
    for row in &report.top_sessions {
        out.push_str(&format!(
            "  session {} requests={}\n",
            row.session, row.requests
        ));
    }
    out.push('\n');

    out.push_str(&format!(
        "compared with the previous {} day(s): cost={} requests={}\n",
        report.days,
        fmt_pct(report.comparison.cost_change),
        fmt_pct(report.comparison.requests_change),
    ));
    if report.malformed_lines > 0 {
        out.push_str(&format!(
            "note: {} malformed ledger line(s) were skipped, not fatal\n",
            report.malformed_lines
        ));
    }
    out
}

/// Print `tcr wrap`'s report, text or `--json`, reading `dir` for the ledger
/// and `config_path` for pricing overrides only (never for accounts — see the
/// module docs on why this report resolves nothing against the config).
pub fn print_report(dir: &Path, config_path: &Path, days: u32, json: bool) -> anyhow::Result<()> {
    let pricing = pricing_table(config_path);
    let report = build_report(dir, days, crate::now_ms(), &pricing);
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render_text(&report));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_day(dir: &Path, date: &str, lines: &[&str]) {
        fs::write(dir.join(format!("{date}.jsonl")), lines.join("\n") + "\n")
            .expect("write fixture day file");
    }

    /// Milliseconds for a given UTC date at noon — comfortably inside the day
    /// whichever local offset a CI runner happens to have, since `ledger_date`
    /// is UTC-only.
    fn noon_ms(date: Date) -> i64 {
        use time::{OffsetDateTime, Time};
        OffsetDateTime::new_utc(date, Time::from_hms(12, 0, 0).expect("valid time"))
            .unix_timestamp()
            * 1000
    }

    /// Two days, two models, two accounts, exact totals — gate 1 in the
    /// bridge. Fixture accounts are the repo's standard fake ones.
    #[test]
    fn two_days_two_models_two_accounts_exact_totals() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let day1 = Date::from_calendar_date(2026, time::Month::September, 5).unwrap();
        let day2 = Date::from_calendar_date(2026, time::Month::September, 6).unwrap();
        let ts1 = noon_ms(day1);
        let ts2 = noon_ms(day2);

        // Day 1: alice on opus-5, bob on sonnet-5.
        write_day(
            tmp.path(),
            "2026-09-05",
            &[
                &format!(
                    r#"{{"t":{ts1},"a":"alice@example.com","m":"claude-opus-5","s":1,"i":1000000,"c5":0,"c1":0,"r":0,"o":100000}}"#
                ),
                &format!(
                    r#"{{"t":{ts1},"a":"bob@example.com","m":"claude-sonnet-5","s":2,"i":500000,"c5":0,"c1":0,"r":100000,"o":50000}}"#
                ),
            ],
        );
        // Day 2: alice again on opus-5 (two requests, same session as day 1
        // does not matter — sessions are counted globally), plus one
        // unpriced/unknown-model line to prove it still counts as a request.
        write_day(
            tmp.path(),
            "2026-09-06",
            &[
                &format!(
                    r#"{{"t":{ts2},"a":"alice@example.com","m":"claude-opus-5","s":1,"i":1000000,"c5":0,"c1":0,"r":0,"o":0}}"#
                ),
                &format!(
                    r#"{{"t":{ts2},"a":"bob@example.com","m":"gpt-9","s":3,"i":10,"c5":0,"c1":0,"r":0,"o":10}}"#
                ),
            ],
        );

        let report = build_report(tmp.path(), 2, noon_ms(day2), &PricingTable::default());

        assert_eq!(report.totals.requests, 4);
        assert_eq!(report.totals.input_tokens, 2_500_010);
        assert_eq!(report.totals.output_tokens, 150_010);
        assert_eq!(report.totals.cache_read_tokens, 100_000);
        // opus-5 $5/MTok input, $25/MTok output: 2 requests of 1M input each =
        // $10.00 input, one with 100k output = $2.50; sonnet-5 $2/MTok input,
        // $10/MTok output: 500k input = $1.00, 50k output = $0.50, 100k
        // cache-read at 0.1x input ($0.20/MTok) = $0.02. gpt-9 is unpriced.
        assert!(
            (report.totals.cost_usd - 14.02).abs() < 1e-9,
            "{}",
            report.totals.cost_usd
        );

        assert_eq!(report.by_model.len(), 3, "opus-5, sonnet-5, gpt-9");
        assert_eq!(
            report.by_model[0].model, "claude-opus-5",
            "highest cost first"
        );
        assert_eq!(report.by_model[0].requests, 2);

        assert_eq!(report.by_account.len(), 2);
        assert_eq!(report.by_account[0].account, "alice@example.com");
        assert_eq!(report.by_account[0].requests, 2);
        assert_eq!(report.by_account[1].account, "bob@example.com");
        assert_eq!(report.by_account[1].requests, 2);

        assert_eq!(report.by_day.len(), 2);
        assert_eq!(report.by_day[0].date, "2026-09-05");
        assert_eq!(report.by_day[0].requests, 2);
        assert_eq!(report.by_day[1].date, "2026-09-06");
        assert_eq!(report.by_day[1].requests, 2);
        // Both days served 2 requests — a tie, broken by earliest date.
        assert!(report.by_day[0].busiest);
        assert!(!report.by_day[1].busiest);

        assert_eq!(report.distinct_sessions, 3);
        assert_eq!(report.top_sessions[0].session, 1, "session 1 served 2 of 4");
        assert_eq!(report.top_sessions[0].requests, 2);
    }

    /// The comparison line: a second period with a known ratio to the first
    /// produces the exact percentage.
    #[test]
    fn comparison_against_the_previous_period_is_exact() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev = Date::from_calendar_date(2026, time::Month::September, 3).unwrap();
        let curr = Date::from_calendar_date(2026, time::Month::September, 5).unwrap();

        write_day(
            tmp.path(),
            "2026-09-03",
            &[&format!(
                r#"{{"t":{},"a":"alice@example.com","m":"claude-opus-5","s":1,"i":1000000,"c5":0,"c1":0,"r":0,"o":0}}"#,
                noon_ms(prev)
            )],
        );
        // Current period: two identical requests, so cost and requests both
        // double relative to the previous period -> +100%.
        write_day(
            tmp.path(),
            "2026-09-05",
            &[
                &format!(
                    r#"{{"t":{},"a":"alice@example.com","m":"claude-opus-5","s":1,"i":1000000,"c5":0,"c1":0,"r":0,"o":0}}"#,
                    noon_ms(curr)
                ),
                &format!(
                    r#"{{"t":{},"a":"alice@example.com","m":"claude-opus-5","s":1,"i":1000000,"c5":0,"c1":0,"r":0,"o":0}}"#,
                    noon_ms(curr)
                ),
            ],
        );

        // days=1 so the "current" period is just 2026-09-05 and the
        // "previous" period is just 2026-09-03 (one day back from the day
        // before the current period starts): 2026-09-04, empty. To exercise
        // an actual non-empty previous period, use days=2: current =
        // [09-04, 09-05], previous = [09-02, 09-03].
        write_day(tmp.path(), "2026-09-04", &[]);
        write_day(tmp.path(), "2026-09-02", &[]);

        let report = build_report(tmp.path(), 2, noon_ms(curr), &PricingTable::default());
        assert_eq!(report.previous.requests, 1);
        assert_eq!(report.totals.requests, 2);
        assert_eq!(
            report.comparison.requests_change,
            Some(1.0),
            "2 vs 1 request is +100%"
        );
        assert_eq!(
            report.comparison.cost_change,
            Some(1.0),
            "identical per-request cost, double the requests, is +100% cost"
        );
    }

    /// A malformed line is counted, not fatal — matches what
    /// `UsageTracker::replay_file` already does with the same file.
    #[test]
    fn a_malformed_line_is_counted_not_fatal() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let day = Date::from_calendar_date(2026, time::Month::September, 5).unwrap();
        write_day(
            tmp.path(),
            "2026-09-05",
            &[
                "not json at all",
                &format!(
                    r#"{{"t":{},"a":"alice@example.com","m":"claude-opus-5","s":1,"i":1000,"c5":0,"c1":0,"r":0,"o":0}}"#,
                    noon_ms(day)
                ),
            ],
        );
        let report = build_report(tmp.path(), 1, noon_ms(day), &PricingTable::default());
        assert_eq!(report.totals.requests, 1, "the good line is still counted");
        assert_eq!(report.malformed_lines, 1);
    }

    /// An empty ledger directory (no traffic at all) reports honest zeroes and
    /// `None` for ratios/comparisons, never a divide-by-zero panic.
    #[test]
    fn an_empty_ledger_reports_honest_zeroes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let day = Date::from_calendar_date(2026, time::Month::September, 5).unwrap();
        let report = build_report(tmp.path(), 7, noon_ms(day), &PricingTable::default());
        assert_eq!(report.totals.requests, 0);
        assert_eq!(report.totals.cost_usd, 0.0);
        assert!(report.totals.cache_hit_ratio.is_none());
        assert!(report.comparison.cost_change.is_none());
        assert!(report.comparison.requests_change.is_none());
        assert!(report.by_model.is_empty());
        assert!(report.by_day.is_empty());
    }
}
