//! Durable `server_tool_use` id → minting account pins: the on-disk half of
//! `Manager::server_tool_pins`.
//!
//! A SERVER-side tool (today: `--advisor`) is run by Anthropic, not by the client. The API
//! answers with two content blocks — a `server_tool_use` naming an `srvtoolu_…` id, and a
//! result block whose `encrypted_content` only that organization can decrypt — and Claude Code
//! echoes both back on every later turn of the same conversation. Serve one of those later
//! turns on a DIFFERENT account and the API answers `400 invalid_request_error: "Advisor tool
//! result content could not be processed."` — a wasted turn, every time, for the life of the
//! conversation.
//!
//! So the id has to outlive the response that minted it, and a restart is exactly when a
//! conversation is most likely to be re-keyed onto another account. This module is the file
//! that carries the map across a bounce; [`crate::manager::Manager`]'s side owns the live map.
//!
//! It is deliberately the same design as [`crate::affinity`], down to the failure modes, and
//! for the same reasons:
//!
//! 1. **It stores an account IDENTITY, never the index.** The live map is keyed on positions
//!    into the account list; a position restored against a reordered list names a DIFFERENT
//!    account. Here that is worse than for a cache pin: an affinity pin that mis-resolves costs
//!    a cold prompt prefix, while a server-tool pin that mis-resolves pins the conversation to
//!    the one account guaranteed to 400 it. Anything that does not resolve to exactly one
//!    account is DROPPED.
//! 2. **Every pin carries a mint timestamp and expires at load** ([`SERVER_TOOL_PIN_TTL_MS`]).
//! 3. **It is written atomically and can never take the proxy down.** Every read failure —
//!    missing, truncated, corrupt, wrong version — degrades to "no pins" plus a log line. A
//!    forgotten id is today's behaviour (no bench, the request routes normally); a panic is not.
//!
//! The file lives beside the affinity pins under the cache dir ([`path_beside`]) and is written
//! `0600` because it names accounts. It is a separate file, not a second array in
//! `session-affinity.json`: the two have different keys, different lifetimes and different TTLs,
//! and widening that file's schema would make a format bump there discard these too.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{Account, ConfigError};
use crate::identity::{self, Resolved};

/// How old a server-tool pin may be and still be honoured: **24 hours** since the response that
/// minted it.
///
/// Unlike an affinity pin this is not a bet about a cache being warm — the encrypted result is
/// undecryptable off its minting org forever, so the "correct" TTL is the life of the
/// conversation. The bound exists only to keep the map from growing without end. A Claude Code
/// conversation that has been idle for a day is over in every practical sense, and the cost of
/// being wrong is one 400 on a resumed conversation: exactly today's behaviour.
pub const SERVER_TOOL_PIN_TTL_MS: i64 = 24 * 60 * 60 * 1000;

/// Maximum ids held in memory and written to disk. One entry per server-side tool call, which
/// is at most one per turn per conversation; a few thousand covers a very busy day's fleet, and
/// the oldest are evicted first (see `Manager::record_minted_server_tools`).
pub const SERVER_TOOL_PIN_CAP: usize = 4096;

/// File name, written beside the affinity pin file — see [`path_beside`].
pub const FILE_NAME: &str = "server-tool-pins.json";

/// Bumped whenever the meaning of a field changes. A file written by a different version is
/// ignored wholesale rather than half-read.
const FORMAT_VERSION: u32 = 1;

/// One persisted pin: the server tool id, the identity of the account that minted it, and when.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StoredServerToolPin {
    /// The `srvtoolu_…` id the API put in the `server_tool_use` block. Opaque here; it is only
    /// ever compared for equality against an id echoed back in a request body.
    pub id: String,
    /// Display name of the minting account — also the identity fallback for records with no
    /// `account_uuid`, exactly as [`identity::same_identity`] falls back.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_name: Option<String>,
    /// Epoch ms of the response that minted the id. Compared against
    /// [`SERVER_TOOL_PIN_TTL_MS`] at load.
    pub minted_at_ms: i64,
}

impl StoredServerToolPin {
    /// The identity probe this pin resolves through, in the shape [`identity::resolve`]
    /// compares against stored records.
    fn probe(&self) -> Account {
        identity::probe(
            &self.name,
            self.account_uuid.clone(),
            self.org_uuid.clone(),
            self.org_name.clone(),
        )
    }
}

/// The file itself.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ServerToolPinFile {
    pub version: u32,
    pub saved_at_ms: i64,
    pub pins: Vec<StoredServerToolPin>,
}

/// What a [`load`] made of the file — same reporting contract as
/// [`crate::affinity::LoadReport`]: every field except `pins` exists so the caller can say out
/// loud what it threw away.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LoadReport {
    /// Restored pins, in the in-memory map's own shape: id → (account index, mint ms).
    pub pins: HashMap<String, (usize, i64)>,
    /// Dropped because they were minted longer ago than the TTL.
    pub expired: usize,
    /// Dropped because no live account carries that identity (removed, renamed).
    pub unresolved: usize,
    /// Dropped because two or more live accounts carry that identity. Refused, never guessed.
    pub ambiguous: usize,
    /// Set when the file was ignored ENTIRELY — unreadable, corrupt, truncated, or a version
    /// this build does not understand. `None` on a clean read and on "no file yet".
    pub degraded: Option<String>,
}

/// This file's path, given the session-affinity pin file's path: the same directory, a
/// different name.
///
/// Derived rather than resolved independently so the two caches can never land in different
/// places — and so an embedder that points `ServeOptions::affinity_path` at a disposable
/// directory (which the field's own doc tells it to do) gets this one redirected with it,
/// instead of quietly writing into the live proxy's cache dir.
pub fn path_beside(affinity_path: &Path) -> PathBuf {
    affinity_path.with_file_name(FILE_NAME)
}

/// Write `pins` to `path` atomically at `0600`, keeping at most [`SERVER_TOOL_PIN_CAP`] of them
/// (newest first).
///
/// Returns how many were written. The caller decides what a failure means; it is never fatal
/// here.
pub fn save(path: &Path, pins: &[StoredServerToolPin], now_ms: i64) -> Result<usize, ConfigError> {
    let mut pins = pins.to_vec();
    if pins.len() > SERVER_TOOL_PIN_CAP {
        pins.sort_by_key(|p| std::cmp::Reverse(p.minted_at_ms));
        pins.truncate(SERVER_TOOL_PIN_CAP);
    }
    // Stable order keeps the file diff-friendly and the write byte-identical when nothing
    // changed.
    pins.sort_by(|a, b| a.id.cmp(&b.id));
    let count = pins.len();
    let file = ServerToolPinFile {
        version: FORMAT_VERSION,
        saved_at_ms: now_ms,
        pins,
    };
    crate::config::write_atomic(path, &serde_json::to_string_pretty(&file)?)?;
    Ok(count)
}

/// Read `path` and resolve each pin against `accounts`, dropping anything stale or not
/// resolvable to exactly one account.
///
/// Infallible by construction — every failure mode returns an empty map with `degraded` set.
pub fn load(path: &Path, accounts: &[Account], now_ms: i64, ttl_ms: i64) -> LoadReport {
    let data = match std::fs::read_to_string(path) {
        Ok(data) => data,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            // First boot, and the boot right after this feature ships. Not a degradation.
            return LoadReport::default();
        }
        Err(err) => {
            return LoadReport {
                degraded: Some(format!("unreadable: {err}")),
                ..LoadReport::default()
            };
        }
    };

    let file: ServerToolPinFile = match serde_json::from_str(&data) {
        Ok(file) => file,
        Err(err) => {
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
    for pin in file.pins {
        if now_ms.saturating_sub(pin.minted_at_ms) > ttl_ms {
            report.expired += 1;
            continue;
        }
        match identity::resolve(accounts.iter().enumerate(), &pin.probe()) {
            Resolved::One(index) => {
                report.pins.insert(pin.id, (index, pin.minted_at_ms));
            }
            Resolved::None => report.unresolved += 1,
            Resolved::Many => report.ambiguous += 1,
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(name: &str, uuid: &str) -> Account {
        crate::identity::probe(
            name,
            Some(uuid.to_string()),
            Some("org-1".to_string()),
            None,
        )
    }

    fn pin(id: &str, name: &str, uuid: &str, minted_at_ms: i64) -> StoredServerToolPin {
        StoredServerToolPin {
            id: id.to_string(),
            name: name.to_string(),
            account_uuid: Some(uuid.to_string()),
            org_uuid: Some("org-1".to_string()),
            org_name: None,
            minted_at_ms,
        }
    }

    /// A unique path per test: the suite runs tests in parallel threads of ONE process, so a
    /// pid-only name collides between them.
    fn tmp(label: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "tcr-server-tool-pins-{label}-{}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join(FILE_NAME)
    }

    #[test]
    fn a_saved_pin_resolves_back_to_its_account_by_identity() {
        let path = tmp("round-trip");
        let now = 1_000_000;
        save(
            &path,
            &[pin("srvtoolu_fake_1", "b@example.com", "uuid-b", now)],
            now,
        )
        .expect("save");

        // Next boot lists b FIRST, so a restore by position would answer 1, not 0.
        let accounts = [
            account("b@example.com", "uuid-b"),
            account("a@example.com", "uuid-a"),
        ];
        let report = load(&path, &accounts, now, SERVER_TOOL_PIN_TTL_MS);
        assert_eq!(report.degraded, None);
        assert_eq!(
            report.pins.get("srvtoolu_fake_1").map(|&(i, _)| i),
            Some(0),
            "the id must follow b's identity, not its old position"
        );
    }

    #[test]
    fn a_pin_past_the_ttl_is_dropped() {
        let path = tmp("expiry");
        let now = SERVER_TOOL_PIN_TTL_MS * 3;
        save(
            &path,
            &[
                pin(
                    "srvtoolu_fake_fresh",
                    "a@example.com",
                    "uuid-a",
                    now - 60_000,
                ),
                pin(
                    "srvtoolu_fake_stale",
                    "a@example.com",
                    "uuid-a",
                    now - SERVER_TOOL_PIN_TTL_MS - 60_000,
                ),
            ],
            now,
        )
        .expect("save");

        let accounts = [account("a@example.com", "uuid-a")];
        let report = load(&path, &accounts, now, SERVER_TOOL_PIN_TTL_MS);
        assert_eq!(report.expired, 1);
        assert!(report.pins.contains_key("srvtoolu_fake_fresh"));
        assert!(!report.pins.contains_key("srvtoolu_fake_stale"));
    }

    /// An account that is gone takes its ids with it. Restoring them against whoever now
    /// occupies that identity's old slot would pin those conversations to an account
    /// guaranteed to 400 them.
    #[test]
    fn a_removed_account_drops_its_ids_instead_of_mis_resolving() {
        let path = tmp("removed");
        let now = 1_000_000;
        save(
            &path,
            &[pin("srvtoolu_fake_1", "gone@example.com", "uuid-gone", now)],
            now,
        )
        .expect("save");

        let report = load(
            &path,
            &[account("a@example.com", "uuid-a")],
            now,
            SERVER_TOOL_PIN_TTL_MS,
        );
        assert_eq!(report.unresolved, 1);
        assert!(report.pins.is_empty());
    }

    #[test]
    fn a_corrupt_file_degrades_to_no_pins_without_panicking() {
        let path = tmp("corrupt");
        std::fs::write(&path, "{\"version\":1,\"pins\":[{\"id\":").expect("write truncated");
        let report = load(
            &path,
            &[account("a@example.com", "uuid-a")],
            1_000_000,
            SERVER_TOOL_PIN_TTL_MS,
        );
        assert!(report.degraded.is_some());
        assert!(report.pins.is_empty());
    }

    #[test]
    fn a_missing_file_is_not_a_degradation() {
        let path = tmp("missing").with_file_name("no-such-file.json");
        let report = load(&path, &[], 1_000_000, SERVER_TOOL_PIN_TTL_MS);
        assert_eq!(report, LoadReport::default());
    }

    #[test]
    fn the_file_sits_beside_the_affinity_pins() {
        let affinity = Path::new("/tmp/example/session-affinity.json");
        assert_eq!(
            path_beside(affinity),
            PathBuf::from("/tmp/example/server-tool-pins.json")
        );
    }
}
