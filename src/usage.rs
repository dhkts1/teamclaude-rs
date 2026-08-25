//! Per-account usage buckets and the append-only usage ledger.
//!
//! # Why the proxy is the right place to count
//!
//! ccusage answers "what did I spend today" by re-parsing every Claude Code
//! transcript under `~/.claude/projects/` on each call: slow, blind to which
//! account served the request, and its "5-hour block" is a guess (first message,
//! floored to the UTC hour). This proxy already sees every request exactly once,
//! already parses all four token dimensions, already knows the model and the
//! serving account, and reads the REAL 5-hour window off Anthropic's headers.
//! Aggregating here at request time makes a poll a lookup instead of a sweep.
//!
//! # The two structures
//!
//! * A **360-slot ring of per-minute buckets** per account — six hours, which
//!   covers any 5-hour window plus the trailing hour with room to spare. A slot
//!   carries the minute it belongs to, so a stale slot is recognised and cleared
//!   rather than read as current, and the ring never needs sweeping.
//! * A **today accumulator** per account, keyed by the local calendar day. It is
//!   NOT derived from the ring: a day is 24 hours and the ring is six.
//!
//! Both are keyed by model, because "what did opus cost me today" is the
//! question, and a per-model split cannot be recovered from a total afterwards.
//!
//! # Minute granularity
//!
//! Window membership is decided per MINUTE, not per millisecond: a request is in
//! the 5-hour window if its minute is at or after the window's opening minute.
//! At the boundary that admits at most 59 seconds of requests either side, which
//! is well inside the resolution anyone reads these numbers at, and it is what
//! makes the ring O(1) per record instead of a sorted list per account.
//!
//! # The ledger
//!
//! In-memory buckets die with the process, and this proxy is restarted for every
//! TcrBar update. One JSON line per served request goes to
//! `~/.cache/teamclaude/usage/<UTC-date>.jsonl` (directory `0700`, files `0600`
//! — the same posture as the session-affinity pin file and the log directory),
//! and boot replays today's and yesterday's files back through the same
//! recording path. Keys are one or two characters on purpose: ~110 bytes a line
//! at ~17k requests a day is ~2 MB, against 40-66 MB/day for the tracing log.
//!
//! A ledger write that fails warns ONCE and is then silent, and never fails the
//! request it was recording. Losing accounting is not worth failing traffic.
//!
//! File names are UTC dates; "today" is a LOCAL day computed from the record
//! timestamps inside the files. The two deliberately need not agree — the file
//! name is a shard key, not a claim about anyone's calendar.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tcr_status_wire::{UsageRow, UsageTotals, UsageWindow};
use time::{Date, OffsetDateTime, UtcOffset};

use crate::pricing::{cost_nanos, PricingTable};

/// Minutes held in the ring: six hours, so a 5-hour window plus the trailing
/// hour is always fully covered.
const RING_MINUTES: i64 = 360;
/// The 5-hour quota window, in milliseconds — the span `window` reports over.
pub const FIVE_HOURS_MS: i64 = 5 * 60 * 60 * 1000;
/// The trailing span `last_hour` reports over.
pub const ONE_HOUR_MS: i64 = 60 * 60 * 1000;
/// Bucket key for a request whose body carried no parseable `model`.
const UNKNOWN_MODEL: &str = "unknown";

/// One served request's usage, as recorded and as persisted.
///
/// `input` is BASE input only — cache creation and cache reads are separate
/// billing dimensions and are carried separately. The quota counter the rest of
/// the manager keeps (`AccountRuntime::input_tokens`) is the SUM of all three,
/// and [`Self::input_total`] reproduces it exactly so no existing counter
/// changes meaning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageRecord {
    pub ts_ms: i64,
    /// `None` when the request body carried no `model` — buckets file it under
    /// `"unknown"` and it prices as unpriced, never as free.
    pub model: Option<String>,
    pub session: Option<u64>,
    pub input: u64,
    pub cache_5m: u64,
    pub cache_1h: u64,
    pub cache_read: u64,
    pub output: u64,
}

impl UsageRecord {
    /// The quota counter's input figure: base + cache creation + cache reads,
    /// byte-identical to what `sum_input_tokens` produces in `proxy.rs`.
    pub fn input_total(&self) -> u64 {
        self.input + self.cache_5m + self.cache_1h + self.cache_read
    }

    /// Total cache-creation tokens, both TTLs — the existing
    /// `cache_creation_tokens` counter's meaning, unchanged.
    pub fn cache_creation(&self) -> u64 {
        self.cache_5m + self.cache_1h
    }

    fn model_key(&self) -> &str {
        self.model.as_deref().unwrap_or(UNKNOWN_MODEL)
    }
}

/// Accumulated usage for one bucket-and-model pair. Cost is kept in
/// nanodollars so a per-request rounding error cannot accumulate into the day.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Totals {
    requests: u64,
    input: u64,
    cache_5m: u64,
    cache_1h: u64,
    cache_read: u64,
    output: u64,
    cost_nanos: u64,
    unpriced_requests: u64,
}

impl Totals {
    fn add(&mut self, other: &Totals) {
        self.requests += other.requests;
        self.input += other.input;
        self.cache_5m += other.cache_5m;
        self.cache_1h += other.cache_1h;
        self.cache_read += other.cache_read;
        self.output += other.output;
        self.cost_nanos += other.cost_nanos;
        self.unpriced_requests += other.unpriced_requests;
    }

    /// Project onto the wire shape. `costUsd` is `None` when the bucket served
    /// requests and NOT ONE of them could be priced — publishing `0.0` there
    /// would be an unmeasured number wearing a measurement's clothes. A bucket
    /// with some priced and some unpriced requests reports the partial sum, and
    /// `unpricedRequests` is how a reader knows it is partial.
    fn to_wire(self) -> UsageTotals {
        let cost_usd = if self.requests > 0 && self.unpriced_requests >= self.requests {
            None
        } else {
            Some(self.cost_nanos as f64 / 1e9)
        };
        UsageTotals {
            requests: self.requests,
            input_tokens: self.input,
            cache_creation_tokens: self.cache_5m,
            cache_creation_1h_tokens: self.cache_1h,
            cache_read_tokens: self.cache_read,
            output_tokens: self.output,
            cost_usd,
            unpriced_requests: self.unpriced_requests,
        }
    }
}

/// One minute of one account's traffic, split by model.
#[derive(Debug, Clone, Default)]
struct MinuteBucket {
    /// Which minute (`ts_ms / 60_000`) this slot currently holds. `None` when
    /// the slot has never been written.
    minute: Option<i64>,
    models: BTreeMap<String, Totals>,
}

/// One account's usage state.
#[derive(Debug)]
struct AccountUsage {
    ring: Vec<MinuteBucket>,
    /// The local calendar day `today` holds, or `None` before the first record.
    day: Option<Date>,
    today: BTreeMap<String, Totals>,
}

impl Default for AccountUsage {
    fn default() -> Self {
        Self {
            ring: vec![MinuteBucket::default(); RING_MINUTES as usize],
            day: None,
            today: BTreeMap::new(),
        }
    }
}

fn add_into(map: &mut BTreeMap<String, Totals>, model: &str, totals: &Totals) {
    map.entry(model.to_string()).or_default().add(totals);
}

fn sum(map: &BTreeMap<String, Totals>) -> Totals {
    let mut out = Totals::default();
    for t in map.values() {
        out.add(t);
    }
    out
}

/// The local calendar day for a Unix-millisecond instant, on the machine this
/// process runs on.
///
/// `time` 0.3.55 resolves the offset with `localtime_r` for the instant given
/// (`src/sys/local_offset_at/unix.rs`), so this is DST-correct per record and
/// there is no boot-captured offset to go stale. It still needs the crate's
/// `local-offset` feature; without it, and on any platform where the lookup
/// fails, the offset is `Err` and this falls back to UTC — see
/// [`warn_if_local_offset_unavailable`], which says so once at boot rather than
/// letting every day boundary silently mean something else.
pub fn local_day(ts_ms: i64) -> Date {
    let at = OffsetDateTime::from_unix_timestamp_nanos(ts_ms as i128 * 1_000_000)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH);
    let offset = UtcOffset::local_offset_at(at).unwrap_or(UtcOffset::UTC);
    at.to_offset(offset).date()
}

/// Log once, at boot, if the local UTC offset cannot be read — after which
/// "today" is a UTC day and the operator knows why.
pub fn warn_if_local_offset_unavailable() {
    if UtcOffset::local_offset_at(OffsetDateTime::now_utc()).is_err() {
        tracing::warn!(
            "could not read this machine's UTC offset; usage day boundaries will follow UTC"
        );
    }
}

/// The UTC date a record's ledger file is named after.
fn ledger_date(ts_ms: i64) -> Date {
    OffsetDateTime::from_unix_timestamp_nanos(ts_ms as i128 * 1_000_000)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
        .date()
}

fn date_string(date: Date) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}

/// One ledger line. Short keys are deliberate — see the module docs on size.
#[derive(Debug, Serialize, Deserialize)]
struct LedgerLine {
    /// Unix milliseconds.
    t: i64,
    /// The serving account's `name`. Replay resolves it back to an index by
    /// name rather than by position, so reordering or removing an account in
    /// the config cannot silently reattribute yesterday's traffic.
    a: String,
    #[serde(default)]
    m: Option<String>,
    #[serde(default)]
    s: Option<u64>,
    i: u64,
    c5: u64,
    c1: u64,
    r: u64,
    o: u64,
}

/// The append-only file half. Holds the currently-open day's file so the common
/// case is one `write` syscall per request, and rolls over when the UTC date
/// changes.
#[derive(Debug)]
struct Ledger {
    dir: PathBuf,
    open: Option<(Date, std::fs::File)>,
    /// Set after the first write failure so a broken disk cannot turn into a
    /// log flood.
    warned: bool,
}

impl Ledger {
    fn append(&mut self, line: &LedgerLine) {
        if let Err(err) = self.try_append(line) {
            if !self.warned {
                self.warned = true;
                tracing::warn!(
                    dir = %self.dir.display(),
                    error = %err,
                    "could not write the usage ledger; today's totals will not survive a restart"
                );
            }
        }
    }

    fn try_append(&mut self, line: &LedgerLine) -> std::io::Result<()> {
        let date = ledger_date(line.t);
        if self.open.as_ref().is_none_or(|(open, _)| *open != date) {
            self.open = Some((date, open_day_file(&self.dir, date)?));
        }
        let Some((_, file)) = self.open.as_mut() else {
            return Ok(());
        };
        let mut buf = serde_json::to_vec(line)?;
        buf.push(b'\n');
        file.write_all(&buf)
    }
}

/// Create the ledger directory `0700` and open (or create) one day's file
/// `0600`, append-only. Same confidentiality posture as the log directory and
/// the pin file: the parent `~/.cache/teamclaude/` is `0755`, so an owner-only
/// mode has to be asserted here or these files are world-readable on a
/// multi-user box. The mode is requested at creation time rather than
/// `chmod`-ed afterwards, so there is no window in which the file exists at the
/// umask's mode.
fn open_day_file(dir: &Path, date: Date) -> std::io::Result<std::fs::File> {
    ensure_dir(dir)?;
    let path = dir.join(format!("{}.jsonl", date_string(date)));
    let mut options = std::fs::OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn ensure_dir(dir: &Path) -> std::io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// `~/.cache/teamclaude/usage/` (or `$XDG_CACHE_HOME/teamclaude/usage/`) — a
/// cache dir for the same reason the pin file is one: regenerable-ish state
/// written at high frequency has no business next to live OAuth credentials.
pub fn default_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".cache"))
        })
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("teamclaude").join("usage")
}

/// What a boot-time replay found. Reported so the operator can tell "the ledger
/// restored the day" apart from "the ledger was empty and today starts at zero".
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReplayReport {
    /// Lines replayed into the buckets.
    pub replayed: usize,
    /// Lines skipped because no configured account carries that name.
    pub unresolved: usize,
    /// Lines that were not parseable JSON of the expected shape.
    pub malformed: usize,
    /// Day files deleted for being older than the retention window.
    pub pruned: usize,
}

/// Per-account usage buckets, the pricing table, and the ledger.
///
/// One of these hangs off `Manager`. Recording takes the buckets lock for an
/// O(1) update; a snapshot takes it for a read. Neither is ever held across the
/// manager's accounts lock — `Manager::record_usage` updates the counters,
/// releases that lock, and only then records here.
#[derive(Debug)]
pub struct UsageTracker {
    accounts: Mutex<Vec<AccountUsage>>,
    pricing: PricingTable,
    ledger: Mutex<Option<Ledger>>,
    /// Whether a ledger is attached at all, readable without taking the lock.
    persisting: AtomicBool,
}

impl UsageTracker {
    pub fn new(account_count: usize, pricing: PricingTable) -> Self {
        let mut accounts = Vec::with_capacity(account_count);
        accounts.resize_with(account_count, AccountUsage::default);
        Self {
            accounts: Mutex::new(accounts),
            pricing,
            ledger: Mutex::new(None),
            persisting: AtomicBool::new(false),
        }
    }

    /// Whether this tracker is writing a ledger — i.e. whether today's totals
    /// will survive a restart.
    pub fn is_persisting(&self) -> bool {
        self.persisting.load(Ordering::Relaxed)
    }

    /// Price one record and fold it into the ring and the day accumulator.
    /// `now`-independent: everything is decided from `record.ts_ms`, which is
    /// what lets replay run the very same path.
    fn record_in_memory(&self, idx: usize, record: &UsageRecord) {
        let price = record
            .model
            .as_deref()
            .and_then(|model| self.pricing.lookup(model));
        let mut totals = Totals {
            requests: 1,
            input: record.input,
            cache_5m: record.cache_5m,
            cache_1h: record.cache_1h,
            cache_read: record.cache_read,
            output: record.output,
            cost_nanos: 0,
            unpriced_requests: 0,
        };
        match price {
            Some(price) => {
                totals.cost_nanos = cost_nanos(
                    &price,
                    record.input,
                    record.cache_5m,
                    record.cache_1h,
                    record.cache_read,
                    record.output,
                );
            }
            None => totals.unpriced_requests = 1,
        }

        let model = record.model_key().to_string();
        let minute = record.ts_ms.div_euclid(60_000);
        let day = local_day(record.ts_ms);

        let mut accounts = self.accounts.lock().expect("usage lock poisoned");
        let Some(account) = accounts.get_mut(idx) else {
            return;
        };

        // The ring. A slot holding a NEWER minute than this record's is not
        // ours to clear — that happens when a replayed line is older than the
        // six hours the ring covers, or when two records land out of order.
        // Dropping the record from the ring is right there: it is genuinely
        // outside every window the ring answers for. The day accumulator below
        // still takes it.
        let slot = minute.rem_euclid(RING_MINUTES) as usize;
        if let Some(bucket) = account.ring.get_mut(slot) {
            match bucket.minute {
                Some(held) if held > minute => {}
                Some(held) if held == minute => add_into(&mut bucket.models, &model, &totals),
                _ => {
                    bucket.minute = Some(minute);
                    bucket.models.clear();
                    add_into(&mut bucket.models, &model, &totals);
                }
            }
        }

        // The day. A record for a LATER day than the one held rolls the
        // accumulator over; a record for an EARLIER day (replaying yesterday's
        // file after today's, or a clock step) is not today's traffic and must
        // not be added to it.
        match account.day {
            Some(held) if held == day => add_into(&mut account.today, &model, &totals),
            Some(held) if held > day => {}
            _ => {
                account.day = Some(day);
                account.today.clear();
                add_into(&mut account.today, &model, &totals);
            }
        }
    }

    /// Record one served request: ledger first, then the buckets.
    ///
    /// The ledger write comes first so a crash between the two loses the
    /// cheaper half — in-memory totals die with the process anyway, while a
    /// line that never reached the file is gone from every future boot.
    pub fn record(&self, idx: usize, record: &UsageRecord, account_name: &str) {
        if self.persisting.load(Ordering::Relaxed) {
            let line = LedgerLine {
                t: record.ts_ms,
                a: account_name.to_string(),
                m: record.model.clone(),
                s: record.session,
                i: record.input,
                c5: record.cache_5m,
                c1: record.cache_1h,
                r: record.cache_read,
                o: record.output,
            };
            if let Some(ledger) = self.ledger.lock().expect("ledger lock poisoned").as_mut() {
                ledger.append(&line);
            }
        }
        self.record_in_memory(idx, record);
    }

    /// Attach `dir` as this tracker's ledger: replay today's and yesterday's UTC
    /// files back through the recording path, prune files older than
    /// `retention_days`, then start appending.
    ///
    /// Replay happens BEFORE the writer is armed, so a replayed line is never
    /// written back into the file it came from.
    pub fn attach_ledger(
        &self,
        dir: PathBuf,
        retention_days: u32,
        account_names: &[String],
        now_ms: i64,
    ) -> ReplayReport {
        let mut report = ReplayReport::default();
        let today = ledger_date(now_ms);
        let yesterday = ledger_date(now_ms - 86_400_000);
        for date in [yesterday, today] {
            self.replay_file(
                &dir.join(format!("{}.jsonl", date_string(date))),
                account_names,
                &mut report,
            );
        }
        report.pruned = prune(&dir, today, retention_days);
        *self.ledger.lock().expect("ledger lock poisoned") = Some(Ledger {
            dir,
            open: None,
            warned: false,
        });
        self.persisting.store(true, Ordering::Relaxed);
        report
    }

    fn replay_file(&self, path: &Path, account_names: &[String], report: &mut ReplayReport) {
        let Ok(data) = std::fs::read_to_string(path) else {
            return; // No file for that day is the ordinary case, not a failure.
        };
        for raw in data.lines() {
            if raw.trim().is_empty() {
                continue;
            }
            let Ok(line) = serde_json::from_str::<LedgerLine>(raw) else {
                report.malformed += 1;
                continue;
            };
            let Some(idx) = account_names.iter().position(|n| *n == line.a) else {
                report.unresolved += 1;
                continue;
            };
            self.record_in_memory(
                idx,
                &UsageRecord {
                    ts_ms: line.t,
                    model: line.m,
                    session: line.s,
                    input: line.i,
                    cache_5m: line.c5,
                    cache_1h: line.c1,
                    cache_read: line.r,
                    output: line.o,
                },
            );
            report.replayed += 1;
        }
    }

    /// This account's usage as of `now_ms`. `five_hour_reset_ms` is the
    /// account's own window reset as read from Anthropic's headers; `None`
    /// there means the window's start cannot be named, so `window` is `null`
    /// rather than a guessed span.
    pub fn row(&self, idx: usize, now_ms: i64, five_hour_reset_ms: Option<i64>) -> UsageRow {
        let accounts = self.accounts.lock().expect("usage lock poisoned");
        let Some(account) = accounts.get(idx) else {
            return UsageRow::default();
        };
        let day = local_day(now_ms);
        // A day accumulator holding some OTHER day is not today's traffic. It
        // reads as an empty day, which is the truth after midnight on an idle
        // proxy — not as yesterday's total wearing today's label.
        let today_current = account.day == Some(day);
        let today_by_model: BTreeMap<String, UsageTotals> = if today_current {
            account
                .today
                .iter()
                .map(|(model, t)| (model.clone(), t.to_wire()))
                .collect()
        } else {
            BTreeMap::new()
        };
        let today = if today_current {
            sum(&account.today).to_wire()
        } else {
            UsageTotals::default()
        };
        let window = five_hour_reset_ms.map(|reset| {
            let since = reset - FIVE_HOURS_MS;
            UsageWindow {
                since,
                totals: range(account, since, now_ms).to_wire(),
            }
        });
        UsageRow {
            today,
            window,
            last_hour: range(account, now_ms - ONE_HOUR_MS, now_ms).to_wire(),
            today_by_model,
        }
    }
}

/// Sum the ring over `[since_ms, now_ms]`, at minute granularity, ignoring any
/// slot that has fallen out of the six hours the ring covers.
fn range(account: &AccountUsage, since_ms: i64, now_ms: i64) -> Totals {
    let from = since_ms.div_euclid(60_000);
    let to = now_ms.div_euclid(60_000);
    let floor = to - RING_MINUTES + 1;
    let mut out = Totals::default();
    for bucket in &account.ring {
        let Some(minute) = bucket.minute else {
            continue;
        };
        if minute >= from && minute <= to && minute >= floor {
            out.add(&sum(&bucket.models));
        }
    }
    out
}

/// Delete `<date>.jsonl` files older than `retention_days` before `today`.
/// Anything in the directory that is not a dated ledger file is left alone —
/// this function deletes only names it can prove it wrote.
fn prune(dir: &Path, today: Date, retention_days: u32) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let cutoff = today - time::Duration::days(i64::from(retention_days));
    let mut pruned = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(stem) = name.strip_suffix(".jsonl") else {
            continue;
        };
        let format = time::macros::format_description!("[year]-[month]-[day]");
        let Ok(date) = Date::parse(stem, &format) else {
            continue;
        };
        if date < cutoff && std::fs::remove_file(entry.path()).is_ok() {
            pruned += 1;
        }
    }
    pruned
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker() -> UsageTracker {
        UsageTracker::new(2, PricingTable::default())
    }

    fn record(ts_ms: i64, model: &str, input: u64) -> UsageRecord {
        UsageRecord {
            ts_ms,
            model: Some(model.to_string()),
            session: None,
            input,
            cache_5m: 0,
            cache_1h: 0,
            cache_read: 0,
            output: 0,
        }
    }

    /// The window boundary, which is the whole reason the ring exists: a request
    /// made after this account's current 5-hour window opened counts against it;
    /// one from before belongs to the window before and must not.
    ///
    /// The window here opened 4h59m ago (its reset is a minute from now), so
    /// 4h58m ago is inside it and 5h01m ago is outside — the pair the bridge
    /// names, either side of a boundary the ring decides at minute granularity.
    #[test]
    fn the_five_hour_window_admits_4h58m_and_excludes_5h01m() {
        let tracker = tracker();
        let now = 1_800_000_000_000;
        // Reset a minute out, so the window opened 4h59m ago.
        let reset = now + 60_000;
        tracker.record_in_memory(
            0,
            &record(now - (4 * 60 + 58) * 60_000, "claude-opus-5", 10),
        );
        tracker.record_in_memory(0, &record(now - (5 * 60 + 1) * 60_000, "claude-opus-5", 99));

        let row = tracker.row(0, now, Some(reset));
        let window = row.window.expect("a known reset names a window");
        assert_eq!(
            window.since,
            reset - FIVE_HOURS_MS,
            "the window opens five hours before its reset"
        );
        assert_eq!(
            window.totals.requests, 1,
            "4h58m ago is in the window, 5h01m ago is not"
        );
        assert_eq!(window.totals.input_tokens, 10, "and it is the right one");
        assert_eq!(row.today.requests, 2, "today keeps both");
    }

    /// An unknown reset cannot name a window start, so there is no window at all
    /// — never a guessed span, and never a zero-filled one.
    #[test]
    fn an_unknown_reset_yields_no_window() {
        let tracker = tracker();
        let now = 1_800_000_000_000;
        tracker.record_in_memory(0, &record(now, "claude-opus-5", 10));
        let row = tracker.row(0, now, None);
        assert!(row.window.is_none());
        assert_eq!(row.today.requests, 1, "today is still measured");
    }

    /// `lastHour` is the trailing 60 minutes, and nothing older.
    #[test]
    fn last_hour_holds_59_minutes_and_drops_61() {
        let tracker = tracker();
        let now = 1_800_000_000_000;
        tracker.record_in_memory(0, &record(now - 59 * 60_000, "claude-opus-5", 7));
        tracker.record_in_memory(0, &record(now - 61 * 60_000, "claude-opus-5", 9));
        let row = tracker.row(0, now, None);
        assert_eq!(row.last_hour.requests, 1, "only the 59-minute-old record");
        assert_eq!(row.last_hour.input_tokens, 7);
        assert_eq!(row.today.requests, 2, "today keeps both");
    }

    /// An unknown model is counted and left unpriced. With NOTHING priced in the
    /// bucket the cost is `null`, not `$0.00`.
    #[test]
    fn an_unknown_model_is_counted_but_unpriced() {
        let tracker = tracker();
        let now = 1_800_000_000_000;
        tracker.record_in_memory(0, &record(now, "some-unknown-model", 1_000));
        let row = tracker.row(0, now, None);
        assert_eq!(row.today.requests, 1);
        assert_eq!(row.today.unpriced_requests, 1);
        assert_eq!(row.today.cost_usd, None, "unpriced is null, never 0.0");

        // One priced request beside it makes the total partial, not absent.
        tracker.record_in_memory(0, &record(now, "claude-opus-5", 1_000_000));
        let row = tracker.row(0, now, None);
        assert_eq!(row.today.requests, 2);
        assert_eq!(row.today.unpriced_requests, 1);
        assert_eq!(row.today.cost_usd, Some(5.0));
    }

    /// The per-model split, which is what a total cannot be decomposed into
    /// afterwards.
    #[test]
    fn today_splits_by_model() {
        let tracker = tracker();
        let now = 1_800_000_000_000;
        tracker.record_in_memory(0, &record(now, "claude-opus-5", 1_000_000));
        tracker.record_in_memory(0, &record(now, "claude-sonnet-5", 1_000_000));
        let row = tracker.row(0, now, None);
        assert_eq!(row.today_by_model.len(), 2);
        assert_eq!(
            row.today_by_model["claude-opus-5"].cost_usd,
            Some(5.0),
            "opus input is $5/MTok"
        );
        assert_eq!(
            row.today_by_model["claude-sonnet-5"].cost_usd,
            Some(2.0),
            "sonnet-5 input is $2/MTok"
        );
        assert_eq!(row.today.cost_usd, Some(7.0));
    }

    /// A day accumulator holding yesterday reads as an empty today, never as
    /// yesterday's number relabelled.
    #[test]
    fn a_stale_day_reads_as_an_empty_today() {
        let tracker = tracker();
        let now = 1_800_000_000_000;
        tracker.record_in_memory(0, &record(now, "claude-opus-5", 1_000_000));
        let tomorrow = now + 86_400_000;
        let row = tracker.row(0, tomorrow, None);
        assert_eq!(row.today.requests, 0);
        assert_eq!(row.today_by_model.len(), 0);
    }

    /// THE LEDGER ROUND TRIP: write N records through a tracker with a ledger
    /// attached, then replay the files into a FRESH tracker and get identical
    /// totals. This is what makes a restart cost nothing.
    #[test]
    fn a_replayed_ledger_reproduces_the_totals_exactly() {
        let dir = std::env::temp_dir().join(format!(
            "tcr-usage-ledger-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let names = vec![
            "alice@example.com".to_string(),
            "bob@example.com".to_string(),
        ];
        let now = crate::now_ms();

        let writer = tracker();
        let report = writer.attach_ledger(dir.clone(), 90, &names, now);
        assert_eq!(report.replayed, 0, "an empty directory replays nothing");
        assert!(writer.is_persisting());
        for i in 0..5 {
            writer.record(
                0,
                &UsageRecord {
                    ts_ms: now - i * 60_000,
                    model: Some("claude-opus-5".to_string()),
                    session: Some(7),
                    input: 1_000,
                    cache_5m: 200,
                    cache_1h: 300,
                    cache_read: 4_000,
                    output: 50,
                },
                &names[0],
            );
        }
        writer.record(1, &record(now, "claude-haiku-4-5-20251001", 10), &names[1]);
        let before = writer.row(0, now, Some(now + 60_000));

        let replayed = tracker();
        let report = replayed.attach_ledger(dir.clone(), 90, &names, now);
        assert_eq!(report.replayed, 6, "every line came back");
        assert_eq!(report.unresolved, 0);
        assert_eq!(report.malformed, 0);
        let after = replayed.row(0, now, Some(now + 60_000));
        assert_eq!(
            before, after,
            "a replayed ledger reproduces the row exactly"
        );
        assert_eq!(after.today.requests, 5);
        assert_eq!(after.today.cache_creation_1h_tokens, 1_500);
        assert_eq!(
            replayed.row(1, now, None).today.requests,
            1,
            "the second account's line was attributed to it, not to the first"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A ledger line naming an account this config no longer has is dropped,
    /// not attributed to whoever now sits at that index.
    #[test]
    fn a_line_for_an_unknown_account_is_not_reattributed() {
        let dir = std::env::temp_dir().join(format!(
            "tcr-usage-unknown-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let now = crate::now_ms();
        let writer = tracker();
        writer.attach_ledger(dir.clone(), 90, &["gone@example.com".to_string()], now);
        writer.record(0, &record(now, "claude-opus-5", 5), "gone@example.com");

        let replayed = tracker();
        let report =
            replayed.attach_ledger(dir.clone(), 90, &["alice@example.com".to_string()], now);
        assert_eq!(report.replayed, 0);
        assert_eq!(report.unresolved, 1);
        assert_eq!(replayed.row(0, now, None).today.requests, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Retention deletes old day files and leaves everything else in the
    /// directory alone.
    #[test]
    fn prune_deletes_only_old_dated_files() {
        let dir = std::env::temp_dir().join(format!(
            "tcr-usage-prune-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        ensure_dir(&dir).expect("temp dir");
        let today = ledger_date(crate::now_ms());
        let old = today - time::Duration::days(100);
        let recent = today - time::Duration::days(2);
        for date in [old, recent] {
            std::fs::write(dir.join(format!("{}.jsonl", date_string(date))), "").expect("write");
        }
        std::fs::write(dir.join("notes.txt"), "keep me").expect("write");

        assert_eq!(prune(&dir, today, 90), 1);
        assert!(!dir.join(format!("{}.jsonl", date_string(old))).exists());
        assert!(dir.join(format!("{}.jsonl", date_string(recent))).exists());
        assert!(
            dir.join("notes.txt").exists(),
            "unrelated files are untouched"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The quota counter's input figure must be reproducible from a record, or
    /// routing this through `record_usage` would change what `inputTokens`
    /// means on a wire that has carried it for months.
    #[test]
    fn input_total_folds_every_input_dimension() {
        let rec = UsageRecord {
            ts_ms: 0,
            model: None,
            session: None,
            input: 10,
            cache_5m: 20,
            cache_1h: 30,
            cache_read: 40,
            output: 50,
        };
        assert_eq!(rec.input_total(), 100);
        assert_eq!(rec.cache_creation(), 50);
    }
}
