//! Durable wire sessions: the on-disk half of `Manager`'s
//! [`crate::session_wire::WireSessionTracker`].
//!
//! Today the Sessions and Tools tabs (`tcr status --json`'s `sessions` array,
//! `docs/design/panel-tabs.md`) live only in memory, so every restart — an app
//! update, a `--replace`, a SIGKILL — empties both lists. This mirrors what
//! [`crate::affinity`] already does for pins: a small cache file beside it, read
//! once at boot and written incrementally off the request path, never load-
//! bearing for correctness (a session simply starts its counters over if the
//! file is missing, stale, or corrupt).
//!
//! Unlike a pin, a [`crate::session_wire::WireSession`] needs no identity
//! resolution — its `account`/`model` fields are plain display strings, not an
//! index into a list that can be reordered between boots — so this file
//! round-trips the struct verbatim: [`save`] serializes the SAME
//! `WireSession` the live tracker holds, keyed by its session id, and [`load`]
//! deserializes it back into the SAME shape, which is what makes a restored row
//! land in exactly the projection code every live row already goes through
//! (`Manager::wire_sessions_snapshot`) rather than a second, parallel one that
//! could drift from it.
//!
//! Two things this file does NOT do, both deliberate:
//!
//! - **No body content.** Every field here already excludes it — `WireSession`
//!   never held request/response bodies in the first place (see
//!   `session_wire.rs`'s own module doc), so there is nothing to redact.
//! - **No rebuild from the usage ledger.** `~/.cache/teamclaude/usage/*.jsonl`
//!   persists every request already, but keyed on an internal `u64` session
//!   NUMBER (`UsageRecord::session`), not the wire's session id string — it
//!   cannot name a session after a restart, and reconstructing tool counts from
//!   it would mean re-deriving everything this module exists to avoid
//!   re-deriving.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::ConfigError;
use crate::session_wire::WireSession;

/// How stale a restored session may be — measured from its OWN `last_seen_ms`,
/// same field the live tracker's `SESSION_TTL_MS` already prunes on — and still
/// be worth restoring: **6 hours**.
///
/// Two things bound this number from opposite directions:
///
/// - The live tracker (`session_wire::SESSION_TTL_MS`, 1 hour) already drops a
///   session from memory — and therefore from what a flush can ever write — the
///   moment it has been idle that long, live or not. So every row this file can
///   possibly contain was, at write time, already within an hour of its own
///   last request; this TTL is not re-implementing that rule, it is bounding
///   how much ADDITIONAL time — genuine proxy downtime between the last flush
///   and this boot — is still worth trusting.
/// - Gil runs 11+ concurrent Claude Code sessions and updates the app several
///   times a day (`teamclaude-rs/CLAUDE.md`). With that many sessions open at
///   once, most of them sit idle for stretches while he works in one or two —
///   and an update can land in the middle of any of those stretches. A TTL of
///   "a few hours" easily covers a same-day gap between one update and the
///   next; going much past that starts covering an OVERNIGHT gap, where
///   yesterday's Claude Code sessions are almost certainly gone or replaced by
///   new ones this morning, and restoring their row would just be showing a
///   ghost.
///
/// 6 hours sits comfortably inside "same work day, several updates" while
/// staying well short of "yesterday".
pub const RESTORE_TTL_MS: i64 = 6 * 60 * 60 * 1000;

/// Maximum sessions kept in the file (freshest by `last_seen_ms` survive a
/// trim) and restored from it.
///
/// Gil's own floor: "a cap under about 30 rows would hide sessions he actually
/// has open" against his 11+ concurrent Claude Code sessions. 64 gives just
/// under 3x that floor — enough headroom for a session that fans out several
/// `Agent`/`Task` subagents (each shares its parent's `session_id`, so this is
/// margin for OTHER concurrent activity, not a per-subagent multiplier) or a
/// short second-terminal burst, without the file — or the boot-time resolve
/// work — growing unbounded. It also matches the shape of every other bound in
/// this system (`session_wire::PENDING_TOOL_CAP` is the same number for the
/// same reason: a small, round, already-precedented cap beats inventing a new
/// one).
pub const PERSIST_CAP: usize = 64;

/// Bumped whenever the meaning of a field changes. A file written by a
/// different version is ignored wholesale rather than half-read — see
/// [`crate::affinity::FORMAT_VERSION`] for the same rule and the same reason.
const FORMAT_VERSION: u32 = 1;

/// The file itself: one row per wire session, each the exact struct the live
/// tracker holds.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct WireSessionsFile {
    version: u32,
    saved_at_ms: i64,
    sessions: HashMap<String, WireSession>,
}

/// What a [`load`] made of the file — mirrors [`crate::affinity::LoadReport`]'s
/// shape and reasoning: every field except `sessions` exists so the caller can
/// say out loud what it threw away.
#[derive(Debug, Default, Clone)]
pub struct LoadReport {
    /// Restored sessions, keyed by session id — ready to fold straight into
    /// [`crate::session_wire::WireSessionTracker`]'s own map.
    pub sessions: HashMap<String, WireSession>,
    /// Dropped because their `last_seen_ms` was older than [`RESTORE_TTL_MS`]
    /// at load time.
    pub expired: usize,
    /// Set when the file was ignored ENTIRELY — unreadable, corrupt, truncated,
    /// or a version this build does not understand. `None` on a clean read and
    /// on a simple "no file yet".
    pub degraded: Option<String>,
}

/// Default cache-file path: `$XDG_CACHE_HOME/teamclaude/session-wire.json`,
/// else `$HOME/.cache/teamclaude/session-wire.json` — same base directory as
/// [`crate::affinity::default_path`], a sibling file rather than a subdirectory
/// so both are equally easy to find and equally easy to delete.
pub fn default_path() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".cache"))
        })
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("teamclaude").join("session-wire.json")
}

/// Write `sessions` to `path` atomically at `0600` (`crate::config::write_atomic`
/// sets the mode — see its own doc for why: a symlink-safe, fsynced temp file
/// plus rename, so a crash mid-write leaves the previous file intact), keeping
/// at most [`PERSIST_CAP`] of them (freshest by `last_seen_ms` first).
///
/// Returns how many were written. Never called on the request hot path — see
/// the debounced flusher in `manager/wire_sessions.rs` — and the caller decides
/// what a failure means; it is never fatal here, same as
/// [`crate::affinity::save`].
pub fn save(
    path: &Path,
    sessions: &HashMap<String, WireSession>,
    now_ms: i64,
) -> Result<usize, ConfigError> {
    let mut rows: Vec<(&String, &WireSession)> = sessions.iter().collect();
    if rows.len() > PERSIST_CAP {
        rows.sort_by_key(|(_, s)| std::cmp::Reverse(s.last_seen_ms));
        rows.truncate(PERSIST_CAP);
    }
    let sessions: HashMap<String, WireSession> = rows
        .into_iter()
        .map(|(k, s)| (k.clone(), s.clone()))
        .collect();
    let count = sessions.len();
    let file = WireSessionsFile {
        version: FORMAT_VERSION,
        saved_at_ms: now_ms,
        sessions,
    };
    crate::config::write_atomic(path, &serde_json::to_string_pretty(&file)?)?;
    Ok(count)
}

/// Read `path`, dropping any session whose `last_seen_ms` is older than
/// `ttl_ms`, capped at [`PERSIST_CAP`] (freshest first).
///
/// Infallible by construction — every failure mode returns an empty map with
/// `degraded` set, never a panic and never an error the caller must handle.
/// Same contract as [`crate::affinity::load`], for the same reason: this is a
/// cache, and a boot must never fail, or even pause, over a corrupt one.
pub fn load(path: &Path, now_ms: i64, ttl_ms: i64) -> LoadReport {
    let data = match std::fs::read_to_string(path) {
        Ok(data) => data,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            // First boot, or the first boot since this feature shipped. Not a
            // degradation.
            return LoadReport::default();
        }
        Err(err) => {
            return LoadReport {
                degraded: Some(format!("unreadable: {err}")),
                ..LoadReport::default()
            };
        }
    };

    let file: WireSessionsFile = match serde_json::from_str(&data) {
        Ok(file) => file,
        Err(err) => {
            // Truncated (a full disk or a SIGKILL between `write` and `rename`
            // cannot produce this — `write_atomic` fsyncs before the rename —
            // but a hand-edit or a future format change can) or otherwise
            // corrupt.
            return LoadReport {
                degraded: Some(format!("corrupt: {err}")),
                ..LoadReport::default()
            };
        }
    };
    if file.version != FORMAT_VERSION {
        return LoadReport {
            degraded: Some(format!(
                "format version {} is not {FORMAT_VERSION}",
                file.version
            )),
            ..LoadReport::default()
        };
    }

    let mut report = LoadReport::default();
    let mut rows: Vec<(String, WireSession)> = file.sessions.into_iter().collect();
    // Freshest first, so a file written before a `save` ever enforced the cap
    // (or hand-edited past it) still restores the newest sessions rather than
    // whatever HashMap iteration order happened to hand back first.
    rows.sort_by_key(|(_, s)| std::cmp::Reverse(s.last_seen_ms));
    for (session_id, session) in rows {
        if now_ms.saturating_sub(session.last_seen_ms) > ttl_ms {
            report.expired += 1;
            continue;
        }
        if report.sessions.len() >= PERSIST_CAP {
            break;
        }
        report.sessions.insert(session_id, session);
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_wire::{RunningTool, SlowTool, ToolStats};

    fn tmp(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tcr-wire-persist-test-{}-{label}-{}",
            std::process::id(),
            crate::now_ms()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join("session-wire.json")
    }

    fn session(account: &str, last_seen_ms: i64, calls: u64) -> WireSession {
        WireSession {
            account: Some(account.to_string()),
            model: Some("claude-sonnet-5".to_string()),
            first_seen_ms: last_seen_ms - 1_000,
            last_seen_ms,
            requests: 3,
            input_tokens: 100,
            output_tokens: 200,
            cache_read_tokens: 50,
            tools: ToolStats {
                calls,
                ..Default::default()
            },
            req_per_minute: Default::default(),
            by_model: Default::default(),
        }
    }

    /// The whole point: a session written by one process reads back with the
    /// same fields, tool counts included — no separate, hand-maintained
    /// wire shape for the persisted version to drift from the live one.
    #[test]
    fn round_trip_restores_the_same_sessions() {
        let path = tmp("round-trip");
        let now = 1_000_000;
        let mut sessions = HashMap::new();
        sessions.insert("sess-a".to_string(), session("alice@example.com", now, 7));
        sessions.insert("sess-b".to_string(), session("bob@example.com", now, 0));
        let written = save(&path, &sessions, now).expect("save");
        assert_eq!(written, 2);

        let report = load(&path, now, RESTORE_TTL_MS);
        assert_eq!(report.degraded, None);
        assert_eq!(report.expired, 0);
        assert_eq!(report.sessions.len(), 2);
        let a = &report.sessions["sess-a"];
        assert_eq!(a.account.as_deref(), Some("alice@example.com"));
        assert_eq!(a.tools.calls, 7);
        assert_eq!(a.input_tokens, 100);
        assert_eq!(a.output_tokens, 200);
        assert_eq!(a.cache_read_tokens, 50);
        assert_eq!(a.requests, 3);
    }

    /// `command_head` never reaches disk — it is a raw shell command / file path / grep
    /// pattern, exactly the body content this module's doc says stays out of the file — while
    /// `command_class` (a coarse category) and `seconds` (a plain duration) still round-trip.
    #[test]
    fn command_head_does_not_reach_disk() {
        let path = tmp("no-heads");
        let now = 1_000_000;
        let mut sess = session("alice@example.com", now, 1);
        sess.tools.running.insert(
            "tool-1".to_string(),
            RunningTool {
                tool: "Bash".to_string(),
                started_ms: now - 500,
                command_head: Some("rm -rf /secret/customer-data".to_string()),
                command_class: None,
            },
        );
        sess.tools.slowest.push(SlowTool {
            tool: "Bash".to_string(),
            seconds: 12.5,
            command_head: Some("curl https://internal.example.com/token".to_string()),
            command_class: None,
            ended_ms: now,
        });
        let mut sessions = HashMap::new();
        sessions.insert("sess-a".to_string(), sess);
        save(&path, &sessions, now).expect("save");

        let raw = std::fs::read_to_string(&path).expect("read back the file as a string");
        assert!(
            !raw.contains("command_head"),
            "command_head must not be serialized at all"
        );
        assert!(
            !raw.contains("rm -rf"),
            "the running tool's head leaked to disk"
        );
        assert!(
            !raw.contains("curl "),
            "the slowest tool's head leaked to disk"
        );

        let report = load(&path, now, RESTORE_TTL_MS);
        let restored = &report.sessions["sess-a"];
        assert_eq!(restored.tools.slowest.len(), 1);
        assert!(restored.tools.slowest[0].command_head.is_none());
        assert_eq!(restored.tools.slowest[0].seconds, 12.5);
    }

    /// A session whose last request was longer ago than the TTL is dropped at
    /// load, not restored as a ghost of a conversation that is almost
    /// certainly gone.
    #[test]
    fn a_session_older_than_the_ttl_is_dropped() {
        let path = tmp("expiry");
        let now = 10_000_000;
        let mut sessions = HashMap::new();
        sessions.insert(
            "sess-fresh".to_string(),
            session("alice@example.com", now - 1_000, 1),
        );
        sessions.insert(
            "sess-stale".to_string(),
            session("bob@example.com", now - RESTORE_TTL_MS - 1, 1),
        );
        save(&path, &sessions, now).expect("save");

        let report = load(&path, now, RESTORE_TTL_MS);
        assert_eq!(report.sessions.len(), 1);
        assert!(report.sessions.contains_key("sess-fresh"));
        assert!(!report.sessions.contains_key("sess-stale"));
        assert_eq!(report.expired, 1);
    }

    /// More sessions than the cap: the newest survive, the oldest are the ones
    /// dropped, on BOTH the write side (the file itself is capped) and the read
    /// side (a hand-edited or pre-cap file restores only the freshest).
    #[test]
    fn more_than_the_cap_keeps_the_newest() {
        let path = tmp("cap");
        let now = 1_000_000;
        let mut sessions = HashMap::new();
        for i in 0..(PERSIST_CAP + 5) {
            sessions.insert(
                format!("sess-{i}"),
                session("alice@example.com", now - i as i64, 1),
            );
        }
        let written = save(&path, &sessions, now).expect("save");
        assert_eq!(written, PERSIST_CAP, "the file itself is capped on write");

        let report = load(&path, now, RESTORE_TTL_MS);
        assert_eq!(report.sessions.len(), PERSIST_CAP);
        // sess-0 has the newest last_seen_ms (now - 0); the last few indices
        // are the oldest and must be the ones missing.
        assert!(report.sessions.contains_key("sess-0"));
        assert!(!report
            .sessions
            .contains_key(&format!("sess-{}", PERSIST_CAP + 4)));
    }

    /// Garbage on disk must not panic, and must not wipe whatever the caller
    /// already has live — `load` only ever returns what it read; it is the
    /// caller's job (and already `manager/wire_sessions.rs`'s contract, mirroring
    /// `restore_affinity`) to fold the result in with `or_insert`, never
    /// overwrite.
    #[test]
    fn a_corrupt_file_degrades_instead_of_panicking() {
        let path = tmp("corrupt");
        std::fs::write(&path, b"{ not json at all").expect("write garbage");

        let report = load(&path, 1_000_000, RESTORE_TTL_MS);
        assert!(report.sessions.is_empty());
        assert!(report.degraded.is_some());
        assert!(report.degraded.unwrap().starts_with("corrupt:"));
    }

    /// A missing file (the ordinary first-boot case) is NOT a degradation.
    #[test]
    fn a_missing_file_is_not_degraded() {
        let path = tmp("missing");
        let report = load(&path, 1_000_000, RESTORE_TTL_MS);
        assert!(report.sessions.is_empty());
        assert_eq!(report.degraded, None);
    }

    /// A file from a future/incompatible format version is ignored wholesale
    /// rather than half-read.
    #[test]
    fn a_wrong_format_version_is_ignored_wholesale() {
        let path = tmp("version");
        std::fs::write(
            &path,
            serde_json::to_string(&serde_json::json!({
                "version": FORMAT_VERSION + 1,
                "savedAtMs": 1_000_000,
                "sessions": {},
            }))
            .unwrap(),
        )
        .expect("write");

        let report = load(&path, 1_000_000, RESTORE_TTL_MS);
        assert!(report.sessions.is_empty());
        assert!(report.degraded.unwrap().contains("format version"));
    }

    /// `ToolBucket`'s median reservoir — a private field reached only through
    /// `#[derive(Serialize, Deserialize)]`, the one place a typo in a rename
    /// would silently start dropping data instead of failing to compile —
    /// round-trips too, not just the plain counters. Built through the
    /// tracker's own public API (`record_request`) rather than reaching into
    /// `ToolBucket` directly, since its `record` method is private to
    /// `session_wire`.
    #[test]
    fn a_tool_buckets_reservoir_survives_the_round_trip() {
        use crate::session_wire::{ToolResultEvent, ToolUseEvent, WireSessionTracker};

        let path = tmp("bucket");
        let mut tracker = WireSessionTracker::new();
        let mut clock_ms = 0i64;
        for (i, secs) in [1.0, 3.0, 5.0].into_iter().enumerate() {
            let id = format!("bash_{i}");
            tracker.record_request(
                "sess-bucket",
                None,
                None,
                clock_ms,
                &[ToolUseEvent {
                    id: id.clone(),
                    name: Some("Bash".into()),
                    command_head: None,
                    command_class: None,
                }],
                &[],
            );
            clock_ms += (secs * 1000.0) as i64;
            tracker.record_request(
                "sess-bucket",
                None,
                None,
                clock_ms,
                &[],
                &[ToolResultEvent {
                    id,
                    is_error: false,
                    timed_out: false,
                }],
            );
        }
        let sessions: HashMap<String, WireSession> =
            tracker.snapshot(clock_ms).into_iter().collect();
        save(&path, &sessions, clock_ms).expect("save");

        let report = load(&path, clock_ms, RESTORE_TTL_MS);
        let restored = &report.sessions["sess-bucket"].tools.by_tool["Bash"];
        assert_eq!(restored.calls, 3);
        assert_eq!(
            restored.seconds_p50(),
            3.0,
            "the reservoir itself round-tripped, not just the call count"
        );
    }
}
