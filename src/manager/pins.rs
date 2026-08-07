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
