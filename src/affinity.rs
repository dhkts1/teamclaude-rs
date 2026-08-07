//! Durable session→account pins: the on-disk half of `Manager::affinity`.
//!
//! The pin map is what keeps one client session on one account so Anthropic's
//! prompt cache stays warm for it. It has always been memory-only, so every
//! restart cold-started every live session's prefix — the most expensive event
//! in this system, and it happened 50 times in 79.6 hours. This module makes the
//! map survive a bounce.
//!
//! Three properties the file has to have, each of which is a way the naive
//! version silently does the wrong thing:
//!
//! 1. **It stores an account IDENTITY, never the index.** The in-memory map is
//!    keyed on `usize` positions into the account list. A position is only
//!    meaningful against the exact list it was taken from: add, remove or reorder
//!    an account between boots and a restored index points at a DIFFERENT
//!    account, which is strictly worse than having no pin — the session is routed
//!    somewhere cold while the proxy believes it is warm, and the migration logic
//!    sees a settled pin and leaves it alone. So each pin stores the identity
//!    fields [`crate::identity`] already treats as an account's identity
//!    (`account_uuid` + org, falling back to `name`) and is resolved back to an
//!    index at load through [`crate::identity::resolve`]. Anything that does not
//!    resolve to EXACTLY one account is DROPPED — a [`Resolved::Many`] tie is
//!    refused here for the same reason `save_disabled` refuses one: a guess that
//!    lands on the wrong record is the failure being prevented, not a lesser
//!    version of success.
//! 2. **Every pin carries a timestamp and expires at load.** A restored pin is
//!    worth something only while that account's prompt cache is still warm; see
//!    [`PIN_TTL_MS`] for the number and the reasoning behind it.
//! 3. **It is written incrementally, atomically, and can never take the proxy
//!    down.** Shutdown-only persistence would miss exactly the case this exists
//!    to survive: `--replace` follows SIGTERM with SIGKILL, and no shutdown hook
//!    runs for a SIGKILL. The write goes through [`crate::config::write_atomic`]
//!    (same-dir temp + `rename`), so a crash mid-write leaves the previous file
//!    intact rather than a truncated one. And every read failure — missing,
//!    truncated, corrupt, wrong version — degrades to "no pins" plus a log line.
//!    This is a cache; a cache that can panic the process is a downgrade on the
//!    memory-only version it replaces.
//!
//! The file is NOT the credential config, deliberately: `~/.config/teamclaude.json`
//! holds live OAuth tokens behind a delicate read-modify-write merge, and a
//! high-frequency cache write has no business anywhere near it. Pins live under
//! the cache dir ([`default_path`]) and are written `0600` because they name
//! accounts.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{Account, ConfigError};
use crate::identity::{self, Resolved};

/// How stale a pin may be at load and still be restored: **15 minutes** since
/// its last touch (a pin's stamp is the time of the last request served on it).
///
/// Anthropic's default `cache_control` TTL is 5 minutes, refreshed on every
/// cache read — so a session that served a request within the last 5 minutes has
/// a warm prefix, and one idle far longer does not under any setting except the
/// 1-hour extended TTL. The window has to cover last-request → first-request-
/// after-the-restart, which includes the restart itself: a `--replace` upgrade is
/// a SIGTERM, a SIGKILL, a boot and often a `cargo build --release` in front of
/// it, so a 5-minute window would expire pins during the very event it exists to
/// survive. 15 minutes is that 5-minute warm window plus room for the bounce,
/// and still well under the 1-hour ceiling past which no cache can be warm.
///
/// Restoring a pin whose cache HAS gone cold is not free but it is cheap: the
/// session simply prefers an account it would otherwise have been assigned by
/// rotation, and the normal eligibility, migration and re-pin paths all still
/// apply on the next request. The expensive error is the mis-resolution in
/// property (1) above, not a slightly stale preference.
pub const PIN_TTL_MS: i64 = 15 * 60 * 1000;

/// Maximum pins written to disk, mirroring the in-memory `AFFINITY_CAP`. On
/// overflow the freshest are kept, matching the in-memory LRU-by-last-touch
/// eviction, so a restore can never produce a map larger than the process would
/// have held anyway.
pub const PIN_CAP: usize = 1024;

/// Bumped whenever the meaning of a field changes. A file written by a different
/// version is ignored wholesale rather than half-read.
const FORMAT_VERSION: u32 = 1;

/// One persisted pin: the session key, the identity of the account it points at,
/// and when it was last touched.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StoredPin {
    /// The stable session key — `stable_hash` over the client's durable identity
    /// (see [`crate::proxy`]). Deterministic across processes for a given build,
    /// which is what makes any of this work; a `std` hash change between Rust
    /// releases would simply make the keys stop matching, and unmatched keys are
    /// inert (they pin sessions that will never ask again, and age out).
    pub key: u64,
    /// Display name of the pinned account — also the identity fallback for
    /// records with no `account_uuid`, exactly as [`identity::same_identity`]
    /// falls back.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_name: Option<String>,
    /// Epoch ms of the last request served on this pin. Compared against
    /// [`PIN_TTL_MS`] at load.
    pub touched_at_ms: i64,
}

impl StoredPin {
    /// The identity probe this pin resolves through, in the shape
    /// [`identity::resolve`] compares against stored records.
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
pub struct PinFile {
    pub version: u32,
    pub saved_at_ms: i64,
    pub pins: Vec<StoredPin>,
}

/// What a [`load`] made of the file. Every field except `pins` exists so the
/// caller can say out loud what it threw away — a silent drop here is
/// indistinguishable from the feature not working.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LoadReport {
    /// Restored pins, in the in-memory map's own shape: key → (account index,
    /// last-touch ms).
    pub pins: HashMap<u64, (usize, i64)>,
    /// Dropped because their last touch was older than the TTL.
    pub expired: usize,
    /// Dropped because no live account carries that identity (removed, renamed).
    pub unresolved: usize,
    /// Dropped because two or more live accounts carry that identity and the tie
    /// could not be broken. Refused, never guessed.
    pub ambiguous: usize,
    /// Set when the file was ignored ENTIRELY — unreadable, corrupt, truncated,
    /// or a version this build does not understand. `None` on a clean read and on
    /// a simple "no file yet".
    pub degraded: Option<String>,
}

/// Default pin-file path: `$XDG_CACHE_HOME/teamclaude/session-affinity.json`,
/// else `$HOME/.cache/teamclaude/session-affinity.json`.
///
/// A cache dir, not `~/.config`: this is regenerable state written on a timer,
/// and the config file next door holds live OAuth credentials behind a merge
/// path that must not acquire a second high-frequency writer.
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
    base.join("teamclaude").join("session-affinity.json")
}

/// Write `pins` to `path` atomically at `0600`, keeping at most [`PIN_CAP`] of
/// them (freshest first).
///
/// Returns how many were written. The caller decides what a failure means; it is
/// never fatal here.
pub fn save(path: &Path, pins: &[StoredPin], now_ms: i64) -> Result<usize, ConfigError> {
    let mut pins = pins.to_vec();
    if pins.len() > PIN_CAP {
        pins.sort_by_key(|p| std::cmp::Reverse(p.touched_at_ms));
        pins.truncate(PIN_CAP);
    }
    // Stable order keeps the file diff-friendly and the write byte-identical when
    // nothing changed.
    pins.sort_by_key(|p| p.key);
    let count = pins.len();
    let file = PinFile {
        version: FORMAT_VERSION,
        saved_at_ms: now_ms,
        pins,
    };
    crate::config::write_atomic(path, &serde_json::to_string_pretty(&file)?)?;
    Ok(count)
}

/// Read `path` and resolve each pin against `accounts`, dropping anything stale
/// or not resolvable to exactly one account.
///
/// Infallible by construction — every failure mode returns an empty map with
/// `degraded` set. See the module docs for why this must never propagate an
/// error, let alone panic.
pub fn load(path: &Path, accounts: &[Account], now_ms: i64, ttl_ms: i64) -> LoadReport {
    let data = match std::fs::read_to_string(path) {
        Ok(data) => data,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            // The ordinary first-boot case, and the case right after this feature
            // ships. Not a degradation.
            return LoadReport::default();
        }
        Err(err) => {
            return LoadReport {
                degraded: Some(format!("unreadable: {err}")),
                ..LoadReport::default()
            };
        }
    };

    let file: PinFile = match serde_json::from_str(&data) {
        Ok(file) => file,
        Err(err) => {
            // Truncated (a SIGKILL between `write` and `rename` cannot produce
            // this, but a full disk or a hand-edit can) or otherwise corrupt.
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
        if now_ms.saturating_sub(pin.touched_at_ms) > ttl_ms {
            report.expired += 1;
            continue;
        }
        match identity::resolve(accounts.iter().enumerate(), &pin.probe()) {
            Resolved::One(index) => {
                report.pins.insert(pin.key, (index, pin.touched_at_ms));
            }
            Resolved::None => report.unresolved += 1,
            Resolved::Many => report.ambiguous += 1,
        }
        if report.pins.len() >= PIN_CAP {
            break;
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acct(name: &str, uuid: Option<&str>, org: Option<&str>) -> Account {
        identity::probe(
            name,
            uuid.map(str::to_string),
            org.map(str::to_string),
            None,
        )
    }

    fn pin(key: u64, name: &str, uuid: Option<&str>, org: Option<&str>, touched: i64) -> StoredPin {
        StoredPin {
            key,
            name: name.to_string(),
            account_uuid: uuid.map(str::to_string),
            org_uuid: org.map(str::to_string),
            org_name: None,
            touched_at_ms: touched,
        }
    }

    fn tmp(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tcr-affinity-test-{}-{label}-{}",
            std::process::id(),
            crate::now_ms()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join("session-affinity.json")
    }

    /// The whole point: pins written by one process are read back by the next and
    /// land on the SAME accounts, resolved by identity rather than by position.
    #[test]
    fn round_trip_restores_the_same_accounts() {
        let path = tmp("round-trip");
        let accounts = [
            acct("a@example.com", Some("uuid-a"), Some("org-1")),
            acct("b@example.com", Some("uuid-b"), Some("org-1")),
        ];
        let now = 1_000_000;
        let written = save(
            &path,
            &[
                pin(11, "a@example.com", Some("uuid-a"), Some("org-1"), now),
                pin(22, "b@example.com", Some("uuid-b"), Some("org-1"), now),
            ],
            now,
        )
        .expect("save");
        assert_eq!(written, 2);

        let report = load(&path, &accounts, now, PIN_TTL_MS);
        assert_eq!(report.degraded, None);
        assert_eq!(report.pins.get(&11), Some(&(0, now)));
        assert_eq!(report.pins.get(&22), Some(&(1, now)));
        assert_eq!(
            (report.expired, report.unresolved, report.ambiguous),
            (0, 0, 0)
        );
    }

    /// The index trap. The saved pins are positions 0 and 1 in a list that is
    /// reordered, and has an account inserted in front of it, before the next
    /// boot. A position-based restore would silently point session 11 at the
    /// wrong account; identity resolution follows the account.
    #[test]
    fn reordered_accounts_resolve_by_identity_not_position() {
        let path = tmp("reorder");
        let now = 2_000_000;
        save(
            &path,
            &[
                pin(11, "a@example.com", Some("uuid-a"), Some("org-1"), now),
                pin(22, "b@example.com", Some("uuid-b"), Some("org-1"), now),
            ],
            now,
        )
        .expect("save");

        // Next boot: a new account first, and a and b swapped.
        let accounts = [
            acct("c@example.com", Some("uuid-c"), Some("org-1")),
            acct("b@example.com", Some("uuid-b"), Some("org-1")),
            acct("a@example.com", Some("uuid-a"), Some("org-1")),
        ];
        let report = load(&path, &accounts, now, PIN_TTL_MS);
        assert_eq!(
            report.pins.get(&11),
            Some(&(2, now)),
            "session 11 must follow account a to its new position, not stay at 0"
        );
        assert_eq!(report.pins.get(&22), Some(&(1, now)));
    }

    /// A removed account drops its pins rather than mis-resolving them.
    #[test]
    fn removed_account_drops_its_pin() {
        let path = tmp("removed");
        let now = 3_000_000;
        save(
            &path,
            &[
                pin(11, "a@example.com", Some("uuid-a"), Some("org-1"), now),
                pin(22, "b@example.com", Some("uuid-b"), Some("org-1"), now),
            ],
            now,
        )
        .expect("save");

        let accounts = [acct("b@example.com", Some("uuid-b"), Some("org-1"))];
        let report = load(&path, &accounts, now, PIN_TTL_MS);
        assert_eq!(report.unresolved, 1);
        assert!(
            !report.pins.contains_key(&11),
            "a is gone; its pin must not survive"
        );
        assert_eq!(report.pins.get(&22), Some(&(0, now)));
    }

    /// An unbreakable identity tie is refused, not guessed. Two entries sharing a
    /// name with no UUID are genuinely indistinguishable (see `identity::resolve`).
    #[test]
    fn ambiguous_identity_is_dropped_never_guessed() {
        let path = tmp("ambiguous");
        let now = 4_000_000;
        save(&path, &[pin(11, "twin@example.com", None, None, now)], now).expect("save");

        let accounts = [
            acct("twin@example.com", None, None),
            acct("twin@example.com", None, None),
        ];
        let report = load(&path, &accounts, now, PIN_TTL_MS);
        assert_eq!(report.ambiguous, 1);
        assert!(
            report.pins.is_empty(),
            "a tie routes nothing rather than the wrong thing"
        );
    }

    /// Stale pins are dropped at load: a warm-cache bet is only worth making
    /// while the cache can still be warm.
    #[test]
    fn expiry_drops_stale_pins_and_keeps_fresh_ones() {
        let path = tmp("expiry");
        let now = 10 * PIN_TTL_MS;
        save(
            &path,
            &[
                pin(
                    11,
                    "a@example.com",
                    Some("uuid-a"),
                    Some("org-1"),
                    now - 60_000,
                ),
                pin(
                    22,
                    "b@example.com",
                    Some("uuid-b"),
                    Some("org-1"),
                    now - PIN_TTL_MS - 1,
                ),
            ],
            now,
        )
        .expect("save");

        let accounts = [
            acct("a@example.com", Some("uuid-a"), Some("org-1")),
            acct("b@example.com", Some("uuid-b"), Some("org-1")),
        ];
        let report = load(&path, &accounts, now, PIN_TTL_MS);
        assert_eq!(report.expired, 1);
        assert!(report.pins.contains_key(&11), "a minute old is warm");
        assert!(!report.pins.contains_key(&22), "past the TTL is cold");
    }

    /// Every read failure degrades to "no pins" plus a stated reason. None of
    /// these may panic or propagate: this is a cache.
    #[test]
    fn unreadable_file_shapes_degrade_to_empty() {
        let accounts = [acct("a@example.com", Some("uuid-a"), Some("org-1"))];
        let now = 5_000_000;

        // Missing — the ordinary first boot, not a degradation.
        let missing = tmp("missing").with_file_name("does-not-exist.json");
        let report = load(&missing, &accounts, now, PIN_TTL_MS);
        assert!(report.pins.is_empty());
        assert_eq!(report.degraded, None);

        // Truncated mid-object.
        let path = tmp("truncated");
        save(
            &path,
            &[pin(11, "a@example.com", Some("uuid-a"), Some("org-1"), now)],
            now,
        )
        .expect("save");
        let full = std::fs::read_to_string(&path).expect("read back");
        std::fs::write(&path, &full[..full.len() / 2]).expect("truncate");
        let report = load(&path, &accounts, now, PIN_TTL_MS);
        assert!(report.pins.is_empty());
        assert!(
            report
                .degraded
                .as_deref()
                .unwrap_or_default()
                .contains("corrupt"),
            "a truncated file must be reported, not silently empty: {:?}",
            report.degraded
        );

        // Not JSON at all.
        std::fs::write(&path, "\0\0\0 not json").expect("write garbage");
        let report = load(&path, &accounts, now, PIN_TTL_MS);
        assert!(report.pins.is_empty());
        assert!(report.degraded.is_some());

        // Valid JSON, wrong shape.
        std::fs::write(&path, "[1, 2, 3]").expect("write array");
        let report = load(&path, &accounts, now, PIN_TTL_MS);
        assert!(report.pins.is_empty());
        assert!(report.degraded.is_some());

        // A future format version is ignored wholesale rather than half-read.
        std::fs::write(&path, r#"{"version":99,"savedAtMs":0,"pins":[]}"#).expect("write v99");
        let report = load(&path, &accounts, now, PIN_TTL_MS);
        assert!(report.pins.is_empty());
        assert!(
            report
                .degraded
                .as_deref()
                .unwrap_or_default()
                .contains("version"),
            "{:?}",
            report.degraded
        );
    }

    /// The file is written `0600`: it names accounts, and it sits in a shared
    /// cache dir.
    #[test]
    fn save_writes_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let path = tmp("perms");
        save(&path, &[pin(11, "a@example.com", None, None, 1)], 1).expect("save");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
    }

    /// Overflow keeps the freshest pins, mirroring the in-memory LRU eviction.
    #[test]
    fn save_caps_the_file_at_pin_cap_keeping_the_freshest() {
        let path = tmp("cap");
        let pins: Vec<StoredPin> = (0..(PIN_CAP as u64 + 10))
            .map(|i| pin(i, "a@example.com", Some("uuid-a"), Some("org-1"), i as i64))
            .collect();
        let written = save(&path, &pins, PIN_CAP as i64 + 100).expect("save");
        assert_eq!(written, PIN_CAP);

        let accounts = [acct("a@example.com", Some("uuid-a"), Some("org-1"))];
        // A TTL wide enough that nothing expires, so this measures the cap alone.
        let report = load(&path, &accounts, 0, i64::MAX);
        assert_eq!(report.pins.len(), PIN_CAP);
        assert!(
            report.pins.contains_key(&(PIN_CAP as u64 + 9)),
            "the freshest pin must survive the cap"
        );
        assert!(
            !report.pins.contains_key(&0),
            "the oldest pin is the one dropped"
        );
    }
}
