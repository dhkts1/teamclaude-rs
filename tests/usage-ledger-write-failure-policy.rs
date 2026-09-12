//! Pins the write-failure policy `Ledger::append` implements
//! (`src/usage.rs:602-630`): a failed write is reported once per TRANSITION,
//! not once per line, a recovery is reported once, and the shared
//! `dropped_lines` count is never perturbed by a write failure or its
//! recovery — `append` only ever READS that counter to put it in the log
//! line, it does not own it.
//!
//! The failure is a real one: a ledger directory with its write bit removed,
//! so `open_day_file`'s `OpenOptions::open(..).create(true)` gets a real
//! `EACCES` without needing root. No mocking of the filesystem.
//!
//! Capturing `tracing::warn!`/`tracing::info!` output needs a subscriber, not
//! a channel `UsageTracker` exposes — there is none, deliberately: the whole
//! point of the policy under test is that it talks to logs, not to a caller.
//! A minimal `tracing_subscriber::Layer` records each event's message and its
//! `dropped_lines` field. The event of interest fires on the ledger's
//! dedicated writer thread, not the test thread, so this installs the
//! capturing subscriber as the PROCESS-WIDE default
//! (`tracing::subscriber::set_global_default`) rather than scoping it with
//! `with_default` — a thread-local default is invisible to a thread spawned
//! after it is set. This file has exactly one test, so "global for the whole
//! binary" and "global for this test" are the same thing.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};

use teamclaude_rs::pricing::PricingTable;
use teamclaude_rs::usage::{LedgerAccount, UsageRecord, UsageTracker};
use tracing_subscriber::layer::SubscriberExt;

/// One captured event: its formatted message and every field tracing handed
/// us, so a test can assert on `dropped_lines` without re-deriving tracing's
/// own formatting.
#[derive(Debug, Clone)]
struct CapturedEvent {
    message: String,
    fields: BTreeMap<String, String>,
}

#[derive(Clone, Default)]
struct CaptureLayer {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

struct FieldVisitor(BTreeMap<String, String>);

impl tracing::field::Visit for FieldVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_string(), format!("{value:?}"));
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CaptureLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = FieldVisitor(BTreeMap::new());
        event.record(&mut visitor);
        let message = visitor
            .0
            .get("message")
            .cloned()
            .unwrap_or_else(|| "<no message field>".to_string());
        self.events
            .lock()
            .expect("capture lock")
            .push(CapturedEvent {
                message,
                fields: visitor.0,
            });
    }
}

impl CaptureLayer {
    fn events(&self) -> Vec<CapturedEvent> {
        self.events.lock().expect("capture lock").clone()
    }

    fn matching(&self, needle: &str) -> Vec<CapturedEvent> {
        self.events()
            .into_iter()
            .filter(|e| e.message.contains(needle))
            .collect()
    }
}

/// A scratch directory named after this process and thread, so concurrent
/// test runs never collide on it — same shape as `usage.rs`'s own `scratch`
/// helper, duplicated here because that one is `#[cfg(test)]`-private to the
/// crate and this file is a separate, external test crate.
fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tcr-usage-write-failure-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&dir);
    dir
}

fn one_record(ts_ms: i64) -> UsageRecord {
    UsageRecord::from_quota_input(
        ts_ms,
        Some("claude-opus-5".to_string()),
        None,
        10,
        0,
        0,
        0,
        0,
    )
}

/// The whole policy, end to end: make the ledger directory unwritable, record
/// through two failed writes, restore it, record through two successful
/// ones, and check what got logged and what the drop counter says at every
/// step.
#[test]
fn write_failure_warns_once_per_transition_and_never_touches_the_drop_count() {
    let dir = scratch("policy");
    fs::create_dir_all(&dir).expect("create the ledger dir");
    // Remove the write bit. The directory stays traversable (so
    // `ensure_dir`'s `dir.is_dir()` short-circuits true and takes no second
    // path), but creating a new file inside it fails with EACCES, without
    // needing root.
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).expect("chmod read-only");

    let capture = CaptureLayer::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    tracing::subscriber::set_global_default(subscriber)
        .expect("no other default is set in this test binary");

    {
        let tracker = UsageTracker::new(1, PricingTable::default());
        let now = teamclaude_rs::now_ms();
        let names = vec![LedgerAccount {
            name: "alice@example.com".to_string(),
            ..LedgerAccount::default()
        }];
        tracker.attach_ledger(dir.clone(), 90, &names, now);
        assert!(
            tracker.is_persisting(),
            "control: nothing has been written yet, so nothing has failed yet"
        );
        assert_eq!(
            tracker.dropped_lines(),
            0,
            "control: a write failure has not happened yet"
        );

        // First failed write: the unhealthy transition, warned once.
        tracker.record(0, &one_record(now), "alice@example.com", None, None);
        assert!(
            tracker.flush_ledger(),
            "the writer answers the flush even though its write failed"
        );
        assert!(
            !tracker.is_persisting(),
            "a failed write must say today's totals will not survive"
        );
        let stopped = capture.matching("stopped writing");
        assert_eq!(
            stopped.len(),
            1,
            "exactly one warning on the healthy-to-unhealthy transition, got {stopped:?}"
        );
        assert_eq!(
            stopped[0].fields.get("dropped_lines").map(String::as_str),
            Some("0"),
            "no line has been dropped by a full queue, so the count the warning \
             carries must be 0, not fabricated"
        );
        assert_eq!(
            tracker.dropped_lines(),
            0,
            "a write failure is not a dropped line and must not be counted as one"
        );

        // Second failed write, same episode: must NOT warn again.
        tracker.record(0, &one_record(now), "alice@example.com", None, None);
        assert!(tracker.flush_ledger());
        let stopped = capture.matching("stopped writing");
        assert_eq!(
            stopped.len(),
            1,
            "a second failed write inside the SAME episode must not warn again \
             (warn-once-per-transition, not per line), got {stopped:?}"
        );
        assert_eq!(
            tracker.dropped_lines(),
            0,
            "still not a dropped line on the second failure either"
        );

        // Recover: put the write bit back, then a write must succeed.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("chmod writable");
        tracker.record(0, &one_record(now), "alice@example.com", None, None);
        assert!(tracker.flush_ledger());
        assert!(
            tracker.is_persisting(),
            "a write that succeeds again must say so"
        );
        let recovered = capture.matching("writing again");
        assert_eq!(
            recovered.len(),
            1,
            "exactly one recovery report on the unhealthy-to-healthy transition, \
             got {recovered:?}"
        );
        assert_eq!(
            recovered[0].fields.get("dropped_lines").map(String::as_str),
            Some("0"),
            "recovery must report the SAME drop count the failure did — still 0, \
             never reset to it, never fabricated"
        );
        assert_eq!(
            tracker.dropped_lines(),
            0,
            "recovery must not zero or otherwise touch the counter it only reads"
        );

        // One more successful write: recovery must not be reported twice.
        tracker.record(0, &one_record(now), "alice@example.com", None, None);
        assert!(tracker.flush_ledger());
        let recovered = capture.matching("writing again");
        assert_eq!(
            recovered.len(),
            1,
            "a second successful write inside the SAME healthy episode must not \
             report recovery again, got {recovered:?}"
        );

        tracker.shutdown_ledger(std::time::Duration::from_secs(2));
    }

    let _ = fs::remove_dir_all(&dir);
}
