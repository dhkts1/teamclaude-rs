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
//! # The ledger write is not on the request path
//!
//! [`UsageTracker::record`] runs inside the proxy's request handling, on a tokio
//! worker. It therefore does NO file I/O: it hands the line to a bounded channel
//! ([`LEDGER_QUEUE_DEPTH`]) and returns, and one dedicated thread owns the open
//! file, the day rollover and every syscall. `~/.cache` on a stalled volume — a
//! network home directory, a full disk, an fsevents stall — then costs the
//! ledger, never the fleet: a multi-second `write` blocks that one thread while
//! requests keep being served. A full queue DROPS the line and counts the drop
//! rather than applying backpressure to traffic, because losing accounting is
//! not worth failing (or delaying) traffic.
//!
//! That is also why the in-memory update no longer comes second. The old
//! ordering ("ledger first, so a crash between the two loses the cheaper half")
//! bought a window measured in microseconds and cost a blocking syscall per
//! request; the line is now queued, so there is no ordering left to trade.
//!
//! A ledger that is not keeping up is reported on every TRANSITION — healthy to
//! failing and back — with the dropped-line count, and
//! [`UsageTracker::is_persisting`] answers FALSE for as long as it lasts. Three
//! separate states count as "not keeping up", because an operator asking "will
//! today's totals survive a restart" gets the same answer — no — from all three:
//! a write that FAILED, a queue that was FULL when a line arrived, and a writer
//! thread that has GONE. A ledger that dies at 10:00 must not go on claiming
//! today's totals will survive a restart, and it must say so at 10:00 rather
//! than at the next boot.
//!
//! # Shutdown
//!
//! The queue is the whole reason a shutdown has work to do: lines sitting in it
//! are counted, priced and visible in `tcr status`, and until the writer takes
//! them they are not on disk. [`UsageTracker::shutdown_ledger`] therefore
//! flushes the queue and JOINS the writer, both inside the caller's budget, so
//! the day file is complete and closed before the process exits —
//! `ServerHandle::shutdown_within` calls it after the affinity pins.
//! A writer that does not drain inside that budget is abandoned rather than
//! waited on, and the result says which happened: a `tcr` that quits into a
//! stalled volume must still quit.
//!
//! File names are UTC dates; "today" is a LOCAL day computed from the record
//! timestamps inside the files. The two deliberately need not agree — the file
//! name is a shard key, not a claim about anyone's calendar.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};

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
/// How many un-written ledger lines may be in flight before [`UsageTracker::record`]
/// starts dropping them. A few thousand lines is seconds of this fleet's peak
/// traffic — enough to ride out a slow `write`, small enough that a writer stuck
/// for good costs bounded memory.
const LEDGER_QUEUE_DEPTH: usize = 4096;

/// Latched the first time a caller's `quota_input` came in below its own cache
/// components, so the warning is one line per boot rather than one per request.
static QUOTA_INPUT_UNDERFLOWED: AtomicBool = AtomicBool::new(false);

/// One served request's usage, as recorded and as persisted.
///
/// `input` is BASE input only — cache creation and cache reads are separate
/// billing dimensions and are carried separately, because they bill at
/// different rates.
///
/// `quota_input` is the caller's own quota figure, held VERBATIM. The quota
/// counter the rest of the manager keeps (`AccountRuntime::input_tokens`) is
/// exactly this number added up, and [`Self::input_total`] hands it back
/// untouched — so routing a request through here cannot change what
/// `inputTokens` has meant for months, whatever relationship the caller's own
/// components happen to have to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageRecord {
    pub ts_ms: i64,
    /// `None` when the request body carried no `model` — buckets file it under
    /// `"unknown"` and it prices as unpriced, never as free.
    pub model: Option<String>,
    pub session: Option<u64>,
    /// The caller's quota input figure, verbatim: what `sum_input_tokens`
    /// produced in `proxy.rs`, or what an embedder passed to
    /// `Manager::update_usage`. NOT re-derived from the fields below.
    pub quota_input: u64,
    /// BASE input, for pricing only: `quota_input` less the cache components,
    /// floored at 0. Build it with [`Self::from_quota_input`] rather than by
    /// hand, so the floor and its warning live in one place.
    pub input: u64,
    pub cache_5m: u64,
    pub cache_1h: u64,
    pub cache_read: u64,
    pub output: u64,
}

impl UsageRecord {
    /// Build a record from the caller's verbatim quota figure and the cache
    /// components that are a SUBSET of it.
    ///
    /// The base input priced at the input rate is what is left after the cache
    /// dimensions are taken out. A caller whose components exceed its own
    /// `quota_input` is inconsistent — a `pub` entry point on a library crate
    /// can be handed anything — so the base is floored at 0 and said out loud
    /// once per boot. What must NOT happen is the old behaviour: silently
    /// re-summing the floored parts and handing the quota counter a bigger
    /// number than the caller passed.
    #[allow(clippy::too_many_arguments)]
    pub fn from_quota_input(
        ts_ms: i64,
        model: Option<String>,
        session: Option<u64>,
        quota_input: u64,
        cache_5m: u64,
        cache_1h: u64,
        cache_read: u64,
        output: u64,
    ) -> Self {
        let components = cache_5m.saturating_add(cache_1h).saturating_add(cache_read);
        let input = quota_input.checked_sub(components).unwrap_or_else(|| {
            if !QUOTA_INPUT_UNDERFLOWED.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    quota_input,
                    cache_creation = cache_5m + cache_1h,
                    cache_read,
                    "a usage record's cache components exceed its own input total; \
                     pricing its base input as 0 (reported once per boot)"
                );
            }
            0
        });
        Self {
            ts_ms,
            model,
            session,
            quota_input,
            input,
            cache_5m,
            cache_1h,
            cache_read,
            output,
        }
    }

    /// The quota counter's input figure — the caller's number, unchanged.
    pub fn input_total(&self) -> u64 {
        self.quota_input
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
    /// `unpricedRequests` is how a reader knows it is partial. A bucket with NO
    /// requests reports `0.0`: nothing served is a measured zero, and it is the
    /// same zero `lastHour` has always reported for the same idle account.
    ///
    /// `cacheCreationTokens` is the FULL cache-creation count, both TTLs — the
    /// same quantity the row-level field of that name carries, so one key never
    /// means two things in one row. `cacheCreation1hTokens` is the 1-hour
    /// SUBSET of it, and the 5-minute part is the difference.
    fn to_wire(self) -> UsageTotals {
        let cost_usd = if self.requests > 0 && self.unpriced_requests >= self.requests {
            None
        } else {
            Some(self.cost_nanos as f64 / 1e9)
        };
        UsageTotals {
            requests: self.requests,
            input_tokens: self.input,
            cache_creation_tokens: self.cache_5m + self.cache_1h,
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

/// The account at `idx`, growing the vector to reach it.
///
/// The rotation GROWS at runtime (`tcr login`, TcrBar's add-account, the
/// `POST /_tcr/accounts` path), so a vector sized once at boot silently discards
/// a live-added account's whole day — a zero that reads as measured, which is
/// the one failure this module exists to prevent. Grow on demand instead: an
/// index is a position in the live rotation, and this structure follows it.
///
/// One function rather than one copy per caller, so recording and reading a row
/// cannot drift apart on what an out-of-range index means.
fn ensure_account(accounts: &mut Vec<AccountUsage>, idx: usize) -> Option<&mut AccountUsage> {
    if idx >= accounts.len() {
        accounts.resize_with(idx + 1, AccountUsage::default);
    }
    accounts.get_mut(idx)
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
    /// The serving account's org discriminator ([`crate::identity::org_key`]),
    /// because a NAME is not an identity here: the same email legitimately
    /// appears twice in one fleet, once per org (`identity.rs`), and replaying
    /// both onto whichever index came first doubles one account's day and
    /// zeroes the other's. Absent when the account carries no org at all, which
    /// is every config written before org identity existed — such a line still
    /// resolves by name, but only while that name is unique.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    g: Option<String>,
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

/// What the writer thread receives. A `Flush` is a rendezvous: the writer
/// answers it only after every line queued before it has been written, which is
/// what lets a caller (a test, or shutdown) know the file is complete.
enum LedgerMsg {
    Line(Box<LedgerLine>),
    Flush(SyncSender<()>),
}

/// The write half of the ledger, as seen from the request path. Holds no file
/// and performs no I/O — it only queues.
///
/// `Clone`, so a caller that needs to SEND can lift the sender out from under
/// [`UsageTracker::ledger`]'s mutex and let go of it first: the request path
/// takes that same mutex in [`UsageTracker::record`], and a send that waits
/// while holding it is the fleet-wide stall this module exists to remove.
#[derive(Debug, Clone)]
struct LedgerHandle {
    tx: SyncSender<LedgerMsg>,
    /// Lines the queue had no room for. Shared with the writer, which reports
    /// the running count on every health transition.
    dropped: Arc<AtomicU64>,
    /// The same flag [`Ledger`] sets from write results and
    /// [`UsageTracker::is_persisting`] reads. Cleared here too: a line the queue
    /// had no room for is a line that will not survive a restart, exactly like
    /// one whose write failed, and the writer never sees it to say so.
    healthy: Arc<AtomicBool>,
}

impl LedgerHandle {
    /// Queue one line, or count it as dropped. NEVER blocks: a writer stuck on
    /// a stalled volume must cost accounting, not latency.
    ///
    /// A drop is a health TRANSITION, not just a counter bump. `Full` means the
    /// writer is not keeping up and `Disconnected` means it is gone; in both,
    /// this line is lost and so is every one behind it until the queue moves
    /// again, so `is_persisting()` must stop saying today's totals will survive.
    fn queue(&self, line: LedgerLine) {
        let Err(err) = self.tx.try_send(LedgerMsg::Line(Box::new(line))) else {
            return;
        };
        let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
        let reason = match err {
            std::sync::mpsc::TrySendError::Full(_) => "the writer's queue is full",
            std::sync::mpsc::TrySendError::Disconnected(_) => "the writer thread has ended",
        };
        if self.healthy.swap(false, Ordering::Relaxed) {
            tracing::warn!(
                reason,
                dropped_lines = dropped,
                "the usage ledger is dropping lines; today's totals will NOT survive a \
                 restart until it recovers"
            );
        }
    }

    /// Block until the writer has drained everything queued before this call,
    /// or `deadline` passes. Bounded at BOTH steps — queueing the marker and
    /// waiting for its answer — because the point of the writer thread is that a
    /// stalled disk cannot hold anything else up. An unbounded `send` here would
    /// block forever on precisely the full queue a stall produces, which is the
    /// one case the bound exists for.
    fn flush_within(&self, deadline: std::time::Instant) -> bool {
        let (ack_tx, ack_rx) = sync_channel::<()>(0);
        let mut msg = LedgerMsg::Flush(ack_tx);
        loop {
            match self.tx.try_send(msg) {
                Ok(()) => break,
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => return false,
                Err(std::sync::mpsc::TrySendError::Full(back)) => {
                    if std::time::Instant::now() >= deadline {
                        return false;
                    }
                    msg = back;
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
        }
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        ack_rx.recv_timeout(left).is_ok()
    }
}

/// What [`UsageTracker::shutdown_ledger`] did — reported rather than logged
/// alone, so a caller (and a test) can tell "the day file is complete" apart
/// from "the writer was abandoned mid-stall".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerShutdown {
    /// No ledger was attached, so nothing was queued and nothing was lost.
    NotAttached,
    /// Every line queued before the shutdown is on disk, and the writer thread
    /// has ended with its file closed.
    Flushed,
    /// The writer did not drain inside the budget and was left behind. Whatever
    /// was still queued is gone, and this is the process saying so.
    Abandoned,
}

/// The append-only file half, owned by ONE dedicated thread. Holds the
/// currently-open day's file so the common case is one `write` syscall per
/// request, and rolls over when the UTC date changes.
#[derive(Debug)]
struct Ledger {
    dir: PathBuf,
    open: Option<(Date, std::fs::File)>,
    /// Cleared while writes are failing and set again when one succeeds. Read
    /// by [`UsageTracker::is_persisting`], so an operator asking "will today's
    /// totals survive a restart" gets the truth rather than the answer that was
    /// true at boot.
    healthy: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
}

impl Ledger {
    fn append(&mut self, line: &LedgerLine) {
        match self.try_append(line) {
            // Report every TRANSITION, in both directions. Warning once and
            // then falling silent forever is how a ledger that died at 10:00
            // goes on looking healthy at 18:00; warning per failed write is a
            // log flood. A transition is the one event that is both bounded and
            // never absent.
            Err(err) => {
                if self.healthy.swap(false, Ordering::Relaxed) {
                    tracing::warn!(
                        dir = %self.dir.display(),
                        error = %err,
                        dropped_lines = self.dropped.load(Ordering::Relaxed),
                        "the usage ledger stopped writing; today's totals will NOT survive a \
                         restart until it recovers"
                    );
                }
            }
            Ok(()) => {
                if !self.healthy.swap(true, Ordering::Relaxed) {
                    tracing::info!(
                        dir = %self.dir.display(),
                        dropped_lines = self.dropped.load(Ordering::Relaxed),
                        "the usage ledger is writing again; the lines it missed are gone"
                    );
                }
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

/// The writer thread's whole life: take lines off the queue, write them, answer
/// flushes. Ends when the last [`LedgerHandle`] is dropped and the channel
/// closes. On a clean shutdown that is [`UsageTracker::shutdown_ledger`]
/// dropping the handle after a flush and then joining this thread, so the last
/// line served is on disk; a tracker dropped in a test ends it the same way,
/// with nobody waiting.
fn ledger_writer(rx: Receiver<LedgerMsg>, mut ledger: Ledger) {
    while let Ok(msg) = rx.recv() {
        match msg {
            LedgerMsg::Line(line) => ledger.append(&line),
            LedgerMsg::Flush(ack) => {
                // The only error here is a flusher that timed out and dropped
                // its receiver, and it has already stopped waiting.
                if ack.send(()).is_err() {
                    tracing::debug!(
                        "a usage-ledger flush was abandoned before the writer answered"
                    );
                }
            }
        }
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

/// A live account as the ledger knows it: the display name it is recorded
/// under, plus the org discriminator that makes that name an identity. Built
/// from the rotation by `Manager::attach_usage_ledger`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerAccount {
    pub name: String,
    /// [`crate::identity::org_key`] for this account: org UUID, else org name,
    /// else `None`.
    pub org: Option<String>,
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
    ledger: Mutex<Option<LedgerHandle>>,
    /// The writer thread, kept so shutdown can JOIN it. Dropping this handle
    /// instead detaches the thread, and a detached writer holding an open file
    /// is exactly how a queued line goes missing at exit.
    writer: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Whether a ledger is attached at all, readable without taking the lock.
    attached: AtomicBool,
    /// Whether the writer's last write SUCCEEDED. Owned here rather than by the
    /// writer thread so `is_persisting` can read it without a lock, and so it
    /// outlives a writer that ends.
    healthy: Arc<AtomicBool>,
    /// Lines the queue had no room for, over the life of the process.
    dropped: Arc<AtomicU64>,
}

impl UsageTracker {
    pub fn new(account_count: usize, pricing: PricingTable) -> Self {
        let mut accounts = Vec::with_capacity(account_count);
        accounts.resize_with(account_count, AccountUsage::default);
        Self {
            accounts: Mutex::new(accounts),
            pricing,
            ledger: Mutex::new(None),
            writer: Mutex::new(None),
            attached: AtomicBool::new(false),
            healthy: Arc::new(AtomicBool::new(true)),
            dropped: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Whether this tracker is writing a ledger — i.e. whether today's totals
    /// will survive a restart.
    ///
    /// FALSE while the ledger is not keeping up, not just when none was
    /// attached: a ledger that stopped writing at 10:00, one whose queue is
    /// full, and one whose writer thread has gone all answer this question
    /// exactly the same way as one that was never there, because the answer to
    /// "will today's totals survive" is no in every one of them.
    pub fn is_persisting(&self) -> bool {
        self.attached.load(Ordering::Relaxed) && self.healthy.load(Ordering::Relaxed)
    }

    /// Whether a ledger is attached at all — i.e. whether a caller's per-request
    /// work to build a line has a reader. Deliberately NOT
    /// [`Self::is_persisting`]: a ledger whose writes are currently failing is
    /// still attached, still queueing, and a line queued now is still written
    /// when it recovers, so it must arrive with everything it needs to resolve.
    pub fn is_attached(&self) -> bool {
        self.attached.load(Ordering::Relaxed)
    }

    /// Ledger lines dropped because the writer's queue was full — always 0 on a
    /// healthy fleet, and the number that says how much of the day a stalled
    /// volume cost.
    pub fn dropped_lines(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Block until every line queued so far has been written, or five seconds
    /// pass. For shutdown and for tests; the request path never calls it.
    /// Returns false if no ledger is attached or the writer did not answer
    /// within the bound.
    pub fn flush_ledger(&self) -> bool {
        self.flush_ledger_within(std::time::Duration::from_secs(5))
    }

    /// [`Self::flush_ledger`] with the bound spelled out.
    ///
    /// The sender is CLONED out from under the ledger mutex and the guard
    /// dropped before anything is sent. Holding it across the send would put
    /// every concurrent [`Self::record`] behind this call — and the case where
    /// this call waits is the stalled writer, which is exactly the case where
    /// the request path must not wait for anything.
    pub fn flush_ledger_within(&self, budget: std::time::Duration) -> bool {
        let handle = self.ledger.lock().expect("ledger lock poisoned").clone();
        let deadline = std::time::Instant::now() + budget;
        handle.is_some_and(|handle| handle.flush_within(deadline))
    }

    /// Flush the queue and JOIN the writer, both inside `budget`, so the day
    /// file holds every line this process queued and is closed before it exits.
    ///
    /// Detaches the ledger first, so a request that lands mid-shutdown is
    /// counted in memory and neither queued nor waited on, and
    /// [`Self::is_persisting`] answers false from that instant.
    ///
    /// A writer that has not drained when the budget runs out is ABANDONED, not
    /// waited on: `tcr` quits into a stalled volume every bit as often as into a
    /// healthy one, and a shutdown that hangs is worse than a ledger that loses
    /// its tail — which the returned [`LedgerShutdown`] says out loud.
    pub fn shutdown_ledger(&self, budget: std::time::Duration) -> LedgerShutdown {
        let deadline = std::time::Instant::now() + budget;
        self.attached.store(false, Ordering::Relaxed);
        let Some(handle) = self.ledger.lock().expect("ledger lock poisoned").take() else {
            return LedgerShutdown::NotAttached;
        };
        let flushed = handle.flush_within(deadline);
        // The writer ends when the last sender goes, so this drop is what asks
        // it to stop — after the flush, never before.
        drop(handle);
        let Some(writer) = self.writer.lock().expect("writer lock poisoned").take() else {
            return if flushed {
                LedgerShutdown::Flushed
            } else {
                LedgerShutdown::Abandoned
            };
        };
        while !writer.is_finished() {
            if std::time::Instant::now() >= deadline {
                return LedgerShutdown::Abandoned;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        if writer.join().is_err() {
            tracing::warn!("the usage-ledger writer thread panicked; its last lines are gone");
            return LedgerShutdown::Abandoned;
        }
        if flushed {
            LedgerShutdown::Flushed
        } else {
            LedgerShutdown::Abandoned
        }
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
        let Some(account) = ensure_account(&mut accounts, idx) else {
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

    /// Record one served request: queue the ledger line, update the buckets.
    ///
    /// Called from the proxy's request path, so it does NO file I/O — see the
    /// module docs. The line goes on a bounded queue the writer thread owns; a
    /// full queue drops it and counts the drop rather than making a client wait
    /// on a disk.
    ///
    /// `account_org` is the serving account's org discriminator
    /// ([`crate::identity::org_key`]) and is what makes the line resolvable
    /// back to THIS account on replay when the fleet holds the same email in
    /// two orgs.
    pub fn record(
        &self,
        idx: usize,
        record: &UsageRecord,
        account_name: &str,
        account_org: Option<&str>,
    ) {
        if self.attached.load(Ordering::Relaxed) {
            let line = LedgerLine {
                t: record.ts_ms,
                a: account_name.to_string(),
                g: account_org.map(str::to_string),
                m: record.model.clone(),
                s: record.session,
                i: record.input,
                c5: record.cache_5m,
                c1: record.cache_1h,
                r: record.cache_read,
                o: record.output,
            };
            if let Some(ledger) = self.ledger.lock().expect("ledger lock poisoned").as_ref() {
                ledger.queue(line);
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
        accounts: &[LedgerAccount],
        now_ms: i64,
    ) -> ReplayReport {
        let mut report = ReplayReport::default();
        let today = ledger_date(now_ms);
        let yesterday = ledger_date(now_ms - 86_400_000);
        for date in [yesterday, today] {
            self.replay_file(
                &dir.join(format!("{}.jsonl", date_string(date))),
                accounts,
                &mut report,
            );
        }
        report.pruned = prune(&dir, today, retention_days);
        // Replay ran synchronously, above and before this point, so a replayed
        // line can never be written back into the file it came from — and it
        // runs before the listener binds, which is why the one blocking read
        // here is not on anybody's request path.
        let (tx, rx) = sync_channel::<LedgerMsg>(LEDGER_QUEUE_DEPTH);
        let ledger = Ledger {
            dir,
            open: None,
            healthy: Arc::clone(&self.healthy),
            dropped: Arc::clone(&self.dropped),
        };
        self.healthy.store(true, Ordering::Relaxed);
        match std::thread::Builder::new()
            .name("tcr-usage-ledger".to_string())
            .spawn(move || ledger_writer(rx, ledger))
        {
            // The thread handle is KEPT, not dropped: `shutdown_ledger` joins it
            // so the file is closed before the process exits.
            Ok(handle) => {
                *self.writer.lock().expect("writer lock poisoned") = Some(handle);
                *self.ledger.lock().expect("ledger lock poisoned") = Some(LedgerHandle {
                    tx,
                    dropped: Arc::clone(&self.dropped),
                    healthy: Arc::clone(&self.healthy),
                });
                self.attached.store(true, Ordering::Relaxed);
            }
            // A process that cannot spawn a thread keeps serving with no
            // ledger, and says so rather than pretending: `is_persisting` stays
            // false, which is the truth.
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "could not start the usage-ledger writer thread; today's totals will not \
                     survive a restart"
                );
            }
        }
        report
    }

    fn replay_file(&self, path: &Path, accounts: &[LedgerAccount], report: &mut ReplayReport) {
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
            let Some(idx) = resolve_account(accounts, &line) else {
                report.unresolved += 1;
                continue;
            };
            self.record_in_memory(
                idx,
                &UsageRecord {
                    ts_ms: line.t,
                    model: line.m,
                    session: line.s,
                    // Replay feeds the BUCKETS only — it never touches the
                    // cumulative quota counters — so this reconstructs the
                    // caller's original total from the dimensions the line
                    // carries rather than inventing one.
                    quota_input: line.i + line.c5 + line.c1 + line.r,
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
        let mut accounts = self.accounts.lock().expect("usage lock poisoned");
        let Some(account) = ensure_account(&mut accounts, idx) else {
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
        // An empty day goes through `to_wire` like every other bucket, so it
        // reports `costUsd: 0.0` — the same measured zero `lastHour` reports
        // for the same idle account. `UsageTotals::default()` would say `null`,
        // and `null` means "could not be priced", not "served nothing".
        let today = if today_current {
            sum(&account.today).to_wire()
        } else {
            Totals::default().to_wire()
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

/// Resolve one ledger line to a live account index.
///
/// A line that CARRIES an org resolves by (name, org) or not at all — that pair
/// is the account's identity (`identity.rs`), and the whole reason the line
/// carries an org. Falling back to the name when the org matched nothing undoes
/// the fix: with `alice@example.com` in org-a and org-b, removing the org-b
/// account leaves its whole day matching org-a's name uniquely, and it replays
/// onto org-a — inflating an account's `costUsd` with traffic it never served,
/// under a boot log reporting a clean `unresolved=0`. Unresolved and counted is
/// the honest answer; the org this line names is not in this fleet.
///
/// A line with NO org — every line written before org identity existed —
/// resolves by name alone, and ONLY when exactly one live account wears that
/// name: with two, a name match is a coin flip, and a coin flip that lands wrong
/// doubles one account's day and zeroes the other's.
fn resolve_account(accounts: &[LedgerAccount], line: &LedgerLine) -> Option<usize> {
    if let Some(org) = line.g.as_deref() {
        return accounts
            .iter()
            .position(|a| a.name == line.a && a.org.as_deref() == Some(org));
    }
    let mut by_name = accounts
        .iter()
        .enumerate()
        .filter(|(_, a)| a.name == line.a)
        .map(|(idx, _)| idx);
    match (by_name.next(), by_name.next()) {
        (Some(only), None) => Some(only),
        _ => None,
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
///
/// Today and YESTERDAY are never deleted, whatever the setting says. Those two
/// files are exactly what the boot replay reads, and `retention_days: 0` is
/// documented as the floor rather than as "delete the history you just
/// replayed": on a UTC+3 machine the first three hours of the local day live in
/// yesterday's UTC-named file, so pruning it costs the second restart of the
/// day those hours — silently, and still presented as measured.
fn prune(dir: &Path, today: Date, retention_days: u32) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let cutoff = (today - time::Duration::days(i64::from(retention_days)))
        .min(today - time::Duration::days(1));
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
        UsageRecord::from_quota_input(ts_ms, Some(model.to_string()), None, input, 0, 0, 0, 0)
    }

    /// One live account with no org — the single-org shape every pre-identity
    /// config has.
    fn named(name: &str) -> LedgerAccount {
        LedgerAccount {
            name: name.to_string(),
            org: None,
        }
    }

    /// A scratch directory named after this process AND thread, so concurrent
    /// tests never share one.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tcr-usage-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
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
        let dir = scratch("ledger");
        let names = vec![named("alice@example.com"), named("bob@example.com")];
        let now = crate::now_ms();

        let writer = tracker();
        let report = writer.attach_ledger(dir.clone(), 90, &names, now);
        assert_eq!(report.replayed, 0, "an empty directory replays nothing");
        assert!(writer.is_persisting());
        for i in 0..5 {
            writer.record(
                0,
                &UsageRecord::from_quota_input(
                    now - i * 60_000,
                    Some("claude-opus-5".to_string()),
                    Some(7),
                    5_500,
                    200,
                    300,
                    4_000,
                    50,
                ),
                &names[0].name,
                None,
            );
        }
        writer.record(
            1,
            &record(now, "claude-haiku-4-5-20251001", 10),
            &names[1].name,
            None,
        );
        // The write is off the request path now, so the file is complete only
        // after the writer has drained — see the module docs.
        assert!(writer.flush_ledger(), "the writer answered the flush");
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
            after.today.cache_creation_tokens, 2_500,
            "cacheCreationTokens is BOTH TTLs, the same quantity the row-level field carries"
        );
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
        let dir = scratch("unknown");
        let now = crate::now_ms();
        let writer = tracker();
        writer.attach_ledger(dir.clone(), 90, &[named("gone@example.com")], now);
        writer.record(
            0,
            &record(now, "claude-opus-5", 5),
            "gone@example.com",
            None,
        );
        assert!(writer.flush_ledger());

        let replayed = tracker();
        let report = replayed.attach_ledger(dir.clone(), 90, &[named("alice@example.com")], now);
        assert_eq!(report.replayed, 0);
        assert_eq!(report.unresolved, 1);
        assert_eq!(replayed.row(0, now, None).today.requests, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FINDING 4. The same email in two orgs is TWO accounts (`identity.rs`),
    /// and each one's day must replay onto its own index. Resolving by name
    /// alone put both onto whichever came first: one account's day doubled, the
    /// other's zeroed, and the boot log still said `unresolved=0`.
    #[test]
    fn two_accounts_sharing_a_name_replay_to_their_own_orgs() {
        let dir = scratch("same-name");
        let now = crate::now_ms();
        let accounts = vec![
            LedgerAccount {
                name: "alice@example.com".to_string(),
                org: Some("org-a".to_string()),
            },
            LedgerAccount {
                name: "alice@example.com".to_string(),
                org: Some("org-b".to_string()),
            },
        ];

        let writer = tracker();
        writer.attach_ledger(dir.clone(), 90, &accounts, now);
        writer.record(
            0,
            &record(now, "claude-opus-5", 1_000),
            "alice@example.com",
            Some("org-a"),
        );
        for _ in 0..3 {
            writer.record(
                1,
                &record(now, "claude-opus-5", 7),
                "alice@example.com",
                Some("org-b"),
            );
        }
        assert!(writer.flush_ledger());

        let replayed = tracker();
        let report = replayed.attach_ledger(dir.clone(), 90, &accounts, now);
        assert_eq!(report.replayed, 4);
        assert_eq!(report.unresolved, 0);
        assert_eq!(
            replayed.row(0, now, None).today.requests,
            1,
            "org A replays onto org A's index only"
        );
        assert_eq!(
            replayed.row(1, now, None).today.requests,
            3,
            "org B keeps the day it actually served"
        );
        assert_eq!(
            replayed.row(0, now, None).today.input_tokens,
            1_000,
            "and the tokens went with it"
        );

        // A line whose org matches NOTHING, against an ambiguous name, is
        // unresolved rather than guessed onto the first match.
        let orphan = tracker();
        let report = orphan.attach_ledger(dir.clone(), 90, &accounts, now);
        assert_eq!(report.unresolved, 0, "control: these four all resolve");
        let ambiguous = vec![named("alice@example.com"), named("alice@example.com")];
        let guessing = tracker();
        let report = guessing.attach_ledger(dir.clone(), 90, &ambiguous, now);
        assert_eq!(
            report.replayed, 0,
            "two live accounts wear this name and neither carries the org: a name match \
             would be a coin flip"
        );
        assert_eq!(report.unresolved, 4);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FINDING 6. A ledger whose writes fail must say so on every transition
    /// AND stop claiming today's totals will survive a restart. The old code
    /// warned once, left `is_persisting()` true forever, and a disk that filled
    /// at 10:00 produced a boot at 18:00 reporting a fraction of the day with a
    /// complete day's confidence.
    #[test]
    fn a_failing_ledger_stops_claiming_it_is_persisting() {
        // A regular FILE where the ledger wants its directory: every
        // `open_day_file` fails, for the whole life of the writer.
        let blocker = scratch("failing");
        std::fs::write(&blocker, "not a directory").expect("write blocker");
        let dir = blocker.join("usage");
        let now = crate::now_ms();

        let tracker = tracker();
        tracker.attach_ledger(dir, 90, &[named("alice@example.com")], now);
        assert!(
            tracker.is_persisting(),
            "nothing has failed yet, so the attach is honest"
        );
        tracker.record(
            0,
            &record(now, "claude-opus-5", 5),
            "alice@example.com",
            None,
        );
        assert!(tracker.flush_ledger(), "the writer is alive, just failing");
        assert!(
            !tracker.is_persisting(),
            "a ledger that cannot write must not report that today's totals will survive"
        );
        assert_eq!(
            tracker.dropped_lines(),
            0,
            "the line was queued and attempted, not dropped for a full queue"
        );
        assert_eq!(
            tracker.row(0, now, None).today.requests,
            1,
            "the in-memory day is unaffected by the file failing"
        );

        let _ = std::fs::remove_file(&blocker);
    }

    /// FINDING 1. The ledger write is not on the request path: with the writer
    /// thread wedged, `record()` still returns — it queues, drops what will not
    /// fit, and keeps the in-memory day complete. Before this, `record()` held
    /// a mutex across `write_all`, so a stalled volume queued every request in
    /// the fleet behind one disk.
    #[test]
    fn record_does_not_block_behind_a_stalled_ledger_writer() {
        let dir = scratch("stalled");
        let now = crate::now_ms();
        let tracker = Arc::new(tracker());
        tracker.attach_ledger(dir.clone(), 90, &[named("alice@example.com")], now);

        // Wedge the writer: a flush it can never complete, because this test
        // holds the receiver and never reads it.
        let (ack_tx, _ack_rx) = sync_channel::<()>(0);
        tracker
            .ledger
            .lock()
            .expect("ledger lock poisoned")
            .as_ref()
            .expect("a ledger is attached")
            .tx
            .send(LedgerMsg::Flush(ack_tx))
            .expect("the writer is alive");

        // Twice the queue depth, so the queue is provably full partway through.
        let calls = LEDGER_QUEUE_DEPTH * 2;
        let finished = Arc::new(AtomicBool::new(false));
        let worker = {
            let tracker = Arc::clone(&tracker);
            let finished = Arc::clone(&finished);
            std::thread::spawn(move || {
                for _ in 0..calls {
                    tracker.record(
                        0,
                        &record(now, "claude-opus-5", 1),
                        "alice@example.com",
                        None,
                    );
                }
                finished.store(true, Ordering::SeqCst);
            })
        };

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !finished.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            finished.load(Ordering::SeqCst),
            "record() blocked behind a stalled ledger writer — the request path is not free"
        );
        worker.join().expect("the recording thread finished");

        assert!(
            tracker.dropped_lines() > 0,
            "a full queue drops lines and counts them; it must not wait for the disk"
        );
        assert_eq!(
            tracker.row(0, now, None).today.requests,
            calls as u64,
            "every request is still counted in memory, whatever the file did"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Wedge the writer thread: send it a flush it cannot answer, and hand back
    /// the receiver that holds it there. Dropping the returned receiver frees
    /// it — `SyncSender::send` on a rendezvous channel fails the moment its
    /// receiver goes.
    fn wedge(tracker: &UsageTracker) -> Receiver<()> {
        let (ack_tx, ack_rx) = sync_channel::<()>(0);
        tracker
            .ledger
            .lock()
            .expect("ledger lock poisoned")
            .as_ref()
            .expect("a ledger is attached")
            .tx
            .send(LedgerMsg::Flush(ack_tx))
            .expect("the writer is alive");
        // No wait is needed for the wedge to bite: the channel is FIFO, so
        // everything queued after this marker stays queued until the writer
        // gets past it, whether or not it has reached it yet.
        ack_rx
    }

    fn lines_in(dir: &Path, now: i64) -> usize {
        let path = dir.join(format!("{}.jsonl", date_string(ledger_date(now))));
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count()
    }

    /// FINDING 0 (round 2). Shutdown DRAINS the queue and joins the writer.
    /// Nothing did, so every restart — and a TcrBar update forces one — threw
    /// away whatever was still queued, and the boot after it reported a day
    /// missing its final minutes as a measured number.
    ///
    /// The writer is wedged while the lines are recorded, so they are provably
    /// still in the queue and not on disk when the shutdown starts: the file
    /// being complete afterwards is the shutdown's doing and nothing else's.
    #[test]
    fn shutdown_drains_every_queued_line_to_disk() {
        let dir = scratch("shutdown");
        let now = crate::now_ms();
        let tracker = tracker();
        tracker.attach_ledger(dir.clone(), 90, &[named("alice@example.com")], now);

        let ack_rx = wedge(&tracker);
        for _ in 0..8 {
            tracker.record(
                0,
                &record(now, "claude-opus-5", 5),
                "alice@example.com",
                None,
            );
        }
        assert_eq!(
            lines_in(&dir, now),
            0,
            "control: with the writer wedged, not one line has reached the file yet"
        );

        // Freed a beat AFTER the shutdown begins, so the shutdown genuinely
        // waits for the drain rather than finding it already done.
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(250));
            drop(ack_rx);
        });

        assert_eq!(
            tracker.shutdown_ledger(std::time::Duration::from_secs(10)),
            LedgerShutdown::Flushed,
            "the writer drained and the file was closed inside the budget"
        );
        assert_eq!(
            lines_in(&dir, now),
            8,
            "every line queued before the shutdown is on disk"
        );
        assert!(
            !tracker.is_persisting() && !tracker.is_attached(),
            "and the tracker stops claiming a ledger it has just closed"
        );
        releaser.join().expect("the releasing thread finished");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FINDING 1 (round 2). A FULL queue and a DEAD writer both lose lines
    /// without any write ever failing, so `healthy` — set only from write
    /// results — kept `is_persisting()` true through both. A stalled volume at
    /// 10:00 then produced a `tcr status` at 18:00 showing a fraction of the
    /// day, presented as measured.
    #[test]
    fn a_full_queue_stops_claiming_it_is_persisting() {
        let dir = scratch("full-queue");
        let now = crate::now_ms();
        let tracker = tracker();
        tracker.attach_ledger(dir.clone(), 90, &[named("alice@example.com")], now);
        assert!(tracker.is_persisting(), "control: nothing has failed yet");

        let ack_rx = wedge(&tracker);
        for _ in 0..LEDGER_QUEUE_DEPTH + 16 {
            tracker.record(
                0,
                &record(now, "claude-opus-5", 1),
                "alice@example.com",
                None,
            );
        }
        assert!(
            tracker.dropped_lines() > 0,
            "control: the queue filled and lines were dropped"
        );
        assert!(
            !tracker.is_persisting(),
            "a ledger dropping lines must not report that today's totals will survive"
        );

        drop(ack_rx);
        assert!(tracker.flush_ledger(), "the writer drains once it is freed");
        tracker.record(
            0,
            &record(now, "claude-opus-5", 1),
            "alice@example.com",
            None,
        );
        assert!(tracker.flush_ledger());
        assert!(
            tracker.is_persisting(),
            "and a successful write says so again"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FINDING 3 (round 2). `flush_ledger` held the ledger mutex across a
    /// blocking send on the bounded queue — so calling it with the queue full
    /// (the stalled-writer case it exists for) blocked forever AND put every
    /// concurrent `record()` behind that same mutex, reinstating the fleet-wide
    /// stall this module exists to remove.
    #[test]
    fn a_flush_against_a_stalled_writer_is_bounded_and_frees_the_request_path() {
        let dir = scratch("flush-bound");
        let now = crate::now_ms();
        let tracker = Arc::new(tracker());
        tracker.attach_ledger(dir.clone(), 90, &[named("alice@example.com")], now);

        let ack_rx = wedge(&tracker);
        for _ in 0..LEDGER_QUEUE_DEPTH + 16 {
            tracker.record(
                0,
                &record(now, "claude-opus-5", 1),
                "alice@example.com",
                None,
            );
        }

        // A request landing while the flush is in progress, on its own thread.
        let recorded = Arc::new(AtomicBool::new(false));
        let worker = {
            let tracker = Arc::clone(&tracker);
            let recorded = Arc::clone(&recorded);
            std::thread::spawn(move || {
                for _ in 0..64 {
                    tracker.record(
                        0,
                        &record(now, "claude-opus-5", 1),
                        "alice@example.com",
                        None,
                    );
                }
                recorded.store(true, Ordering::SeqCst);
            })
        };

        let started = std::time::Instant::now();
        assert!(
            !tracker.flush_ledger_within(std::time::Duration::from_millis(200)),
            "a flush that cannot be queued reports failure rather than joining the stall"
        );
        let waited = started.elapsed();
        assert!(
            waited < std::time::Duration::from_secs(3),
            "the flush must return inside its own bound, not the writer's: waited {waited:?}"
        );

        worker.join().expect("the recording thread finished");
        assert!(
            recorded.load(Ordering::SeqCst),
            "record() completed while the flush was waiting — the request path never waits \
             on the writer"
        );
        drop(ack_rx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FINDING 2 (round 2). A line that CARRIES an org and matches no live
    /// (name, org) pair is unresolved, never handed to a same-named survivor.
    /// Falling through to the name-only path replayed a removed sibling's whole
    /// day onto the account that remained — the misattribution the `g` field
    /// exists to prevent — while the boot log reported `unresolved=0`.
    #[test]
    fn a_line_whose_org_is_gone_is_unresolved_not_given_to_the_same_name() {
        let dir = scratch("org-gone");
        let now = crate::now_ms();
        let both = vec![
            LedgerAccount {
                name: "alice@example.com".to_string(),
                org: Some("org-a".to_string()),
            },
            LedgerAccount {
                name: "alice@example.com".to_string(),
                org: Some("org-b".to_string()),
            },
        ];

        let writer = tracker();
        writer.attach_ledger(dir.clone(), 90, &both, now);
        writer.record(
            0,
            &record(now, "claude-opus-5", 11),
            "alice@example.com",
            Some("org-a"),
        );
        for _ in 0..3 {
            writer.record(
                1,
                &record(now, "claude-opus-5", 22),
                "alice@example.com",
                Some("org-b"),
            );
        }
        assert!(writer.flush_ledger());

        // org-b is removed; one live `alice@example.com` remains.
        let survivor = vec![LedgerAccount {
            name: "alice@example.com".to_string(),
            org: Some("org-a".to_string()),
        }];
        let replayed = tracker();
        let report = replayed.attach_ledger(dir.clone(), 90, &survivor, now);
        assert_eq!(report.replayed, 1, "only org A's own line replays");
        assert_eq!(
            report.unresolved, 3,
            "org B's day is counted as unresolved and said out loud, not given away"
        );
        assert_eq!(
            replayed.row(0, now, None).today.requests,
            1,
            "org A's day is its own traffic and nothing else's"
        );
        assert_eq!(replayed.row(0, now, None).today.input_tokens, 11);

        // The pre-identity shape still resolves: a line with NO org against a
        // uniquely-named account is not ambiguous and must keep replaying.
        let dir2 = scratch("org-none");
        let old = tracker();
        old.attach_ledger(dir2.clone(), 90, &[named("alice@example.com")], now);
        old.record(
            0,
            &record(now, "claude-opus-5", 7),
            "alice@example.com",
            None,
        );
        assert!(old.flush_ledger());
        let replayed = tracker();
        let report = replayed.attach_ledger(dir2.clone(), 90, &survivor, now);
        assert_eq!(
            (report.replayed, report.unresolved),
            (1, 0),
            "control: an org-less line still resolves by a unique name"
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    /// FINDING 3. The rotation grows at runtime (`tcr login`, TcrBar's
    /// add-account), and an account appended after boot serves real traffic. A
    /// vector sized once at `Manager::new` dropped its every record and read
    /// back `requests: 0` — a zero indistinguishable from a measurement.
    #[test]
    fn an_account_added_after_boot_is_recorded_and_read_back() {
        let tracker = tracker(); // sized for 2 accounts
        let now = 1_800_000_000_000;
        tracker.record_in_memory(4, &record(now, "claude-opus-5", 1_000_000));
        let row = tracker.row(4, now, None);
        assert_eq!(row.today.requests, 1, "an index past the boot size records");
        assert_eq!(row.today.input_tokens, 1_000_000);
        assert_eq!(row.today.cost_usd, Some(5.0), "and it is priced");
        assert_eq!(
            tracker.row(1, now, None).today.requests,
            0,
            "growing the vector did not disturb the accounts that were there"
        );
    }

    /// FINDING 8. Zero traffic is a MEASURED zero. An idle account reported
    /// `today.costUsd: null` next to `lastHour.costUsd: 0` for the same absence
    /// of traffic, and `null` is the wire's word for "could not be priced".
    #[test]
    fn an_idle_day_reports_a_zero_cost_not_null() {
        let tracker = tracker();
        let now = 1_800_000_000_000;

        // Never served at all.
        let row = tracker.row(0, now, None);
        assert_eq!(row.today.requests, 0);
        assert_eq!(
            row.today.cost_usd,
            Some(0.0),
            "an idle day is a measured zero, the same one lastHour reports"
        );
        assert_eq!(
            row.last_hour.cost_usd,
            Some(0.0),
            "control: lastHour agrees"
        );

        // Served yesterday, idle today — the shape after every local midnight.
        tracker.record_in_memory(1, &record(now, "claude-opus-5", 1_000_000));
        let tomorrow = now + 86_400_000;
        let row = tracker.row(1, tomorrow, None);
        assert_eq!(row.today.requests, 0);
        assert_eq!(
            row.today.cost_usd,
            Some(0.0),
            "a stale day accumulator reads as an empty day, not an unpriced one"
        );
    }

    /// FINDING 9. `cacheCreationTokens` carries the SAME quantity inside
    /// `usage` as it does at row level: both TTLs together, with the 1-hour
    /// part also broken out beside it.
    #[test]
    fn cache_creation_tokens_is_both_ttls() {
        let tracker = tracker();
        let now = 1_800_000_000_000;
        tracker.record_in_memory(
            0,
            &UsageRecord::from_quota_input(
                now,
                Some("claude-opus-5".to_string()),
                None,
                1_600_000,
                1_200_000,
                400_000,
                0,
                0,
            ),
        );
        let row = tracker.row(0, now, None);
        assert_eq!(
            row.today.cache_creation_tokens, 1_600_000,
            "5m + 1h, matching the row-level counter fed by `UsageRecord::cache_creation`"
        );
        assert_eq!(row.today.cache_creation_1h_tokens, 400_000, "the 1h subset");
        assert_eq!(
            row.today.cache_creation_tokens - row.today.cache_creation_1h_tokens,
            1_200_000,
            "and the 5-minute part is the difference"
        );
    }

    /// FINDING 7. `UsageRecord` holds the caller's quota figure verbatim, so
    /// the cumulative counter grows by exactly what was passed even when the
    /// caller's own components exceed it. The old code re-summed floored parts
    /// and grew the counter by 800 for an `input_tokens` of 100.
    #[test]
    fn a_records_quota_input_is_the_callers_number_verbatim() {
        // The consistent shape: components are a subset of the total.
        let ok = UsageRecord::from_quota_input(0, None, None, 100, 20, 30, 40, 50);
        assert_eq!(ok.input_total(), 100, "the caller's number, unchanged");
        assert_eq!(
            ok.input, 10,
            "base input is what the cache dimensions leave"
        );
        assert_eq!(ok.cache_creation(), 50);

        // The inconsistent shape a `pub` entry point can be handed: 100 total,
        // 800 of components.
        let bad = UsageRecord::from_quota_input(0, None, None, 100, 50, 0, 750, 0);
        assert_eq!(
            bad.input_total(),
            100,
            "the quota counter still grows by exactly what the caller passed"
        );
        assert_eq!(bad.input, 0, "the base priced at the input rate is floored");
    }

    /// FINDING 5. `usageRetentionDays: 0` is the documented FLOOR — today and
    /// yesterday, the two files the boot replay reads — not an instruction to
    /// delete the day just replayed. On a UTC+3 machine yesterday's UTC file
    /// holds the first three hours of the local day.
    #[test]
    fn retention_never_prunes_today_or_yesterday() {
        let dir = scratch("prune-floor");
        ensure_dir(&dir).expect("temp dir");
        let today = ledger_date(crate::now_ms());
        let yesterday = today - time::Duration::days(1);
        let older = today - time::Duration::days(2);
        let write_days = || {
            for date in [today, yesterday, older] {
                std::fs::write(dir.join(format!("{}.jsonl", date_string(date))), "")
                    .expect("write");
            }
        };
        let exists = |date: Date| dir.join(format!("{}.jsonl", date_string(date))).exists();

        write_days();
        assert_eq!(prune(&dir, today, 0), 1, "0 deletes only what is older");
        assert!(exists(today), "0 keeps today");
        assert!(
            exists(yesterday),
            "0 keeps yesterday — the boot replay reads it, and config.rs calls this the floor"
        );
        assert!(!exists(older));

        write_days();
        assert_eq!(prune(&dir, today, 1), 1, "1 is the same floor");
        assert!(exists(today) && exists(yesterday));
        assert!(!exists(older));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Retention deletes old day files and leaves everything else in the
    /// directory alone.
    #[test]
    fn prune_deletes_only_old_dated_files() {
        let dir = scratch("prune");
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
        let rec = UsageRecord::from_quota_input(0, None, None, 100, 20, 30, 40, 50);
        assert_eq!(rec.input, 10, "base input is the remainder");
        assert_eq!(rec.input_total(), 100);
        assert_eq!(rec.cache_creation(), 50);
    }
}
