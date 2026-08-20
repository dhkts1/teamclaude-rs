//! Durability for the session-affinity pin map: snapshot it to disk, and restore
//! it at boot.
//!
//! The file format, the identity resolution and the expiry rule live in
//! [`crate::affinity`]; this is only the bridge between that file and the live
//! `Manager::affinity` map. Two things it is responsible for and the store is
//! not:
//!
//! - **Lock discipline.** The affinity mutex is never held while the accounts
//!   lock is taken (see the field's doc-comment on `Manager::affinity`, and the
//!   ordering `select` follows). Both methods here take one lock, copy what they
//!   need, drop it, and only then take the other. Nothing below awaits, and no
//!   I/O happens under either lock.
//! - **Translating index ↔ identity.** The live map is positional; the file is
//!   not. Saving maps each index to that account's identity fields; restoring
//!   resolves each identity back to whatever index it occupies THIS boot.

use std::path::Path;
use std::sync::atomic::Ordering;

use crate::affinity::{self, LoadReport, StoredPin};
use crate::config::ConfigError;
use crate::identity;

use super::Manager;

impl Manager {
    /// Flag the pin map as changed since the last flush. Called from the
    /// selection path, so it is a single relaxed store and nothing more — the
    /// actual write is done off the request path by the flusher task.
    pub fn mark_affinity_dirty(&self) {
        self.affinity_dirty.store(true, Ordering::Relaxed);
    }

    /// Consume the dirty flag: `true` when something changed since the last call.
    pub fn take_affinity_dirty(&self) -> bool {
        self.affinity_dirty.swap(false, Ordering::Relaxed)
    }

    /// The pin map as persistable records — each live pin's index replaced by the
    /// identity of the account at that index.
    ///
    /// A pin whose index names no account is skipped rather than written: it
    /// could only come from a map mutated against a shorter account list, and
    /// writing a placeholder identity is precisely the mis-resolution this whole
    /// design exists to prevent.
    pub fn affinity_pin_snapshot(&self) -> Vec<StoredPin> {
        let pins: Vec<(u64, usize, i64)> = {
            let map = self.affinity.lock().expect("affinity lock poisoned");
            map.iter()
                .map(|(&key, &(index, touched))| (key, index, touched))
                .collect()
        };
        if pins.is_empty() {
            return Vec::new();
        }
        let accounts = self.accounts.read().expect("accounts lock poisoned");
        pins.into_iter()
            .filter_map(|(key, index, touched_at_ms)| {
                let account = accounts.get(index)?;
                Some(StoredPin {
                    key,
                    name: account.name.clone(),
                    account_uuid: account.account_uuid.clone(),
                    org_uuid: account.org_uuid.clone(),
                    org_name: account.org_name.clone(),
                    touched_at_ms,
                })
            })
            .collect()
    }

    /// Write the pin map to `path`, atomically. Returns how many pins landed.
    ///
    /// The caller logs the failure and carries on; a proxy that cannot write its
    /// pin cache still serves traffic exactly as it did before this file existed.
    pub fn flush_affinity(&self, path: &Path) -> Result<usize, ConfigError> {
        affinity::save(path, &self.affinity_pin_snapshot(), crate::now_ms())
    }

    /// Restore pins from `path` into the live map, resolving each stored identity
    /// against the accounts loaded THIS boot and dropping everything that is
    /// stale or does not resolve to exactly one account.
    ///
    /// Existing in-memory pins win: a key already pinned by a request served
    /// between boot and this call is fresher than anything on disk. In practice
    /// the map is empty here — this runs before the listener binds.
    ///
    /// Returns the store's report so the caller can state what was dropped.
    /// Restoring nothing is a legitimate outcome (first boot, everything expired,
    /// accounts all replaced) and never an error.
    pub fn restore_affinity(&self, path: &Path, ttl_ms: i64) -> LoadReport {
        let candidates: Vec<crate::config::Account> = {
            let accounts = self.accounts.read().expect("accounts lock poisoned");
            accounts
                .iter()
                .map(|a| {
                    identity::probe(
                        &a.name,
                        a.account_uuid.clone(),
                        a.org_uuid.clone(),
                        a.org_name.clone(),
                    )
                })
                .collect()
        };
        let report = affinity::load(path, &candidates, crate::now_ms(), ttl_ms);
        {
            let mut map = self.affinity.lock().expect("affinity lock poisoned");
            for (&key, &value) in &report.pins {
                map.entry(key).or_insert(value);
            }
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use super::*;
    use crate::affinity::PIN_TTL_MS;
    use crate::config::Account;
    use crate::manager::AccountRuntime;

    /// The store's own tests in [`crate::affinity`] cover the FILE half — format,
    /// expiry, identity resolution. These cover the half only the manager has: the
    /// live map's `usize` indices being translated OUT to identities on save and
    /// back IN to (possibly different) indices on load. A snapshot that wrote the
    /// wrong account's identity would pass every test over there and still route
    /// every restored session to a cold account.
    fn account(name: &str, uuid: &str) -> Account {
        Account {
            name: name.to_string(),
            account_type: "oauth".to_string(),
            account_uuid: Some(uuid.to_string()),
            org_uuid: Some("org-1".to_string()),
            org_name: None,
            access_token: format!("at-{name}"),
            refresh_token: Some(format!("rt-{name}")),
            expires_at: Some(crate::now_ms() + 3_600_000),
            priority: Some(0),
            switch_threshold: None,
            disabled: None,
            groups: None,
            extra: serde_json::Map::new(),
        }
    }

    /// A manager holding exactly `accounts`, in that order. `from_runtimes` never
    /// invokes its refresher/prober/warmer, and nothing here reaches the network.
    fn manager_over(accounts: &[Account]) -> Arc<Manager> {
        Manager::from_runtimes(
            accounts
                .iter()
                .map(|a| AccountRuntime::from_config(a, false))
                .collect(),
        )
    }

    /// A unique path per test: the suite runs tests in parallel threads of ONE
    /// process, so a pid-only name collides between them.
    fn tmp(label: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("tcr-pins-{label}-{}-{seq}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join("session-affinity.json")
    }

    /// Seed the live pin map directly. The selection path is what does this in
    /// production; driving a full rotation here would test `select`, not the
    /// persistence bridge these tests are about.
    fn seed_pins(manager: &Manager, pins: &[(u64, usize, i64)]) {
        let mut map = manager.affinity.lock().expect("affinity lock poisoned");
        for &(key, index, touched) in pins {
            map.insert(key, (index, touched));
        }
    }

    fn pins_of(manager: &Manager) -> std::collections::HashMap<u64, (usize, i64)> {
        manager
            .affinity
            .lock()
            .expect("affinity lock poisoned")
            .clone()
    }

    /// The feature's whole claim: pins written by one process are read back by the
    /// next and point at the SAME accounts.
    #[test]
    fn pins_round_trip_through_two_managers() {
        let path = tmp("round-trip");
        let accounts = [
            account("a@example.com", "uuid-a"),
            account("b@example.com", "uuid-b"),
        ];
        let now = crate::now_ms();

        let first = manager_over(&accounts);
        seed_pins(&first, &[(11, 0, now), (22, 1, now)]);
        assert_eq!(first.flush_affinity(&path).expect("flush"), 2);

        let second = manager_over(&accounts);
        let report = second.restore_affinity(&path, PIN_TTL_MS);
        assert_eq!(report.degraded, None);
        let restored = pins_of(&second);
        assert_eq!(restored.get(&11).map(|&(i, _)| i), Some(0));
        assert_eq!(restored.get(&22).map(|&(i, _)| i), Some(1));
    }

    /// The index trap, at the layer that can actually commit it. Session 11 is
    /// pinned to position 1 (account b); the next boot lists the accounts in the
    /// opposite order, so b is position 0. A snapshot that persisted the POSITION
    /// would restore 11 onto account a — warm in the proxy's belief, cold in fact.
    #[test]
    fn a_reordered_account_list_restores_by_identity_not_position() {
        let path = tmp("reorder");
        let a = account("a@example.com", "uuid-a");
        let b = account("b@example.com", "uuid-b");
        let now = crate::now_ms();

        let first = manager_over(&[a.clone(), b.clone()]);
        seed_pins(&first, &[(11, 1, now)]);
        first.flush_affinity(&path).expect("flush");

        // The file itself must name b, not "index 1".
        let raw = std::fs::read_to_string(&path).expect("read pin file");
        assert!(
            raw.contains("uuid-b"),
            "the pin must store b's identity: {raw}"
        );
        assert!(
            !raw.contains("uuid-a"),
            "no other account belongs in it: {raw}"
        );

        // Next boot: b first, a second.
        let second = manager_over(&[b, a]);
        second.restore_affinity(&path, PIN_TTL_MS);
        assert_eq!(
            pins_of(&second).get(&11).map(|&(i, _)| i),
            Some(0),
            "session 11 must follow account b to position 0"
        );
    }

    /// An account removed between boots takes its pins with it, rather than
    /// leaving them to land on whoever now occupies that index.
    #[test]
    fn a_removed_account_drops_its_pin_instead_of_mis_resolving() {
        let path = tmp("removed");
        let a = account("a@example.com", "uuid-a");
        let b = account("b@example.com", "uuid-b");
        let now = crate::now_ms();

        let first = manager_over(&[a, b.clone()]);
        seed_pins(&first, &[(11, 0, now), (22, 1, now)]);
        first.flush_affinity(&path).expect("flush");

        // a is gone; b is now the only account, at position 0.
        let second = manager_over(&[b]);
        let report = second.restore_affinity(&path, PIN_TTL_MS);
        assert_eq!(report.unresolved, 1);
        let restored = pins_of(&second);
        assert!(
            !restored.contains_key(&11),
            "a's pin must not survive a's removal"
        );
        assert_eq!(restored.get(&22).map(|&(i, _)| i), Some(0));
    }

    /// A pin older than the TTL is dropped: the bet it encodes is that the
    /// account's prompt cache is still warm, and past the window it is not.
    #[test]
    fn a_stale_pin_is_dropped_at_restore() {
        let path = tmp("expiry");
        let accounts = [
            account("a@example.com", "uuid-a"),
            account("b@example.com", "uuid-b"),
        ];
        let now = crate::now_ms();

        let first = manager_over(&accounts);
        seed_pins(
            &first,
            &[(11, 0, now - 60_000), (22, 1, now - PIN_TTL_MS - 60_000)],
        );
        first.flush_affinity(&path).expect("flush");

        let second = manager_over(&accounts);
        let report = second.restore_affinity(&path, PIN_TTL_MS);
        assert_eq!(report.expired, 1);
        let restored = pins_of(&second);
        assert!(restored.contains_key(&11), "a minute old is still warm");
        assert!(!restored.contains_key(&22), "past the TTL is cold");
    }

    /// A corrupt file degrades to an empty map and SAYS so. This is a cache: it
    /// may cost the warm start it would have bought, and nothing else.
    #[test]
    fn a_corrupt_file_degrades_to_no_pins_without_panicking() {
        let path = tmp("corrupt");
        let accounts = [account("a@example.com", "uuid-a")];
        std::fs::write(&path, "{\"version\":1,\"pins\":[{\"key\":1,").expect("write truncated");

        let manager = manager_over(&accounts);
        let report = manager.restore_affinity(&path, PIN_TTL_MS);
        assert!(report.degraded.is_some(), "the reason must be reportable");
        assert!(pins_of(&manager).is_empty(), "a bad file restores nothing");
    }

    /// A missing file is the ordinary first boot, not a degradation — the flusher
    /// must not spend every interval logging a warning about it.
    #[test]
    fn a_missing_file_restores_nothing_and_is_not_a_degradation() {
        let path = tmp("missing").with_file_name("no-such-file.json");
        let manager = manager_over(&[account("a@example.com", "uuid-a")]);
        let report = manager.restore_affinity(&path, PIN_TTL_MS);
        assert_eq!(report.degraded, None);
        assert!(pins_of(&manager).is_empty());
    }

    /// The debounce contract the flusher task runs on: a change raises the flag,
    /// one take consumes it, and a second take reports nothing more to write. Were
    /// the take not consuming, an idle proxy would rewrite the file every interval
    /// forever.
    #[test]
    fn the_dirty_flag_is_raised_by_a_change_and_consumed_by_one_take() {
        let manager = manager_over(&[account("a@example.com", "uuid-a")]);
        assert!(
            !manager.take_affinity_dirty(),
            "a fresh map has nothing to flush"
        );
        manager.mark_affinity_dirty();
        assert!(manager.take_affinity_dirty(), "a change must be observed");
        assert!(
            !manager.take_affinity_dirty(),
            "and consumed by that observation"
        );
    }

    /// A pin whose index names no account is skipped rather than written against
    /// whichever account happens to sit at index 0.
    #[test]
    fn a_pin_pointing_past_the_account_list_is_not_written() {
        let path = tmp("out-of-range");
        let manager = manager_over(&[account("a@example.com", "uuid-a")]);
        seed_pins(
            &manager,
            &[(11, 0, crate::now_ms()), (22, 7, crate::now_ms())],
        );
        assert_eq!(
            manager.flush_affinity(&path).expect("flush"),
            1,
            "only the resolvable pin belongs in the file"
        );
        let raw = std::fs::read_to_string(&path).expect("read pin file");
        assert!(raw.contains("\"key\": 11"), "{raw}");
        assert!(!raw.contains("\"key\": 22"), "{raw}");
    }
}
