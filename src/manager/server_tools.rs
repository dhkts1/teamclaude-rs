//! `Manager` glue for the `server_tool_use` id → minting account map — the live half of
//! [`crate::server_tool_pins`], mirroring `pins.rs`'s split for session affinity.
//!
//! The rule this map exists to serve: **a request whose messages carry a `server_tool_use`
//! block is served by the account that minted that block's id, or by nobody.** The result block
//! that travels with the id (`advisor_tool_result`'s `encrypted_content`) decrypts only on the
//! organization that produced it, so any other account answers `400 invalid_request_error` —
//! which makes a 429 the honest answer when the minting account is hard-ineligible, and makes
//! an eligible sibling the wrong answer always.
//!
//! Two responsibilities here that the store next door does not have, same as `pins.rs`:
//!
//! - **Lock discipline.** The map's mutex is never held while the accounts lock is taken. Every
//!   method below takes one lock, copies what it needs, drops it, and only then takes the other.
//! - **Translating index ↔ identity.** The live map is positional, because the response path
//!   learns an index and must not pay the accounts lock to record one id; the file is not, for
//!   the reason [`crate::server_tool_pins`]'s module doc gives.

use std::path::Path;
use std::sync::atomic::Ordering;

use crate::config::ConfigError;
use crate::identity;
use crate::server_tool_pins::{
    self, LoadReport, StoredServerToolPin, SERVER_TOOL_PIN_CAP, SERVER_TOOL_PIN_TTL_MS,
};

use super::Manager;

impl Manager {
    /// Flag the server-tool pin map as changed since the last flush — same relaxed-atomic
    /// debounce contract as [`Self::mark_affinity_dirty`].
    pub fn mark_server_tool_pins_dirty(&self) {
        self.server_tool_pins_dirty.store(true, Ordering::Relaxed);
    }

    /// Consume the dirty flag: `true` when something changed since the last call.
    pub fn take_server_tool_pins_dirty(&self) -> bool {
        self.server_tool_pins_dirty.swap(false, Ordering::Relaxed)
    }

    /// Remember that the account at `account_idx` minted these `srvtoolu_…` ids.
    ///
    /// Called from both response paths in `proxy.rs`, beside
    /// [`Self::record_wire_session_tool_uses`] — the response is the only place an id can be
    /// learned, and the serving account is only known there.
    ///
    /// Bounding is done HERE rather than at save time, because the map has to stay bounded in
    /// memory too: anything past the TTL goes first (it can never be honoured again), and if
    /// that is not enough the OLDEST mints are evicted until the map is back under
    /// [`SERVER_TOOL_PIN_CAP`]. Evicting the oldest, not the newest, is the whole point: an
    /// evicted id stops pinning its conversation, and the conversations most likely to still be
    /// live are the ones minted most recently.
    pub fn record_minted_server_tools(&self, ids: &[String], account_idx: usize, now_ms: i64) {
        if ids.is_empty() {
            return;
        }
        let mut map = self
            .server_tool_pins
            .lock()
            .expect("server tool pins lock poisoned");
        for id in ids {
            map.insert(id.clone(), (account_idx, now_ms));
        }
        map.retain(|_, &mut (_, minted)| now_ms.saturating_sub(minted) <= SERVER_TOOL_PIN_TTL_MS);
        if map.len() > SERVER_TOOL_PIN_CAP {
            let mut by_age: Vec<(String, i64)> = map
                .iter()
                .map(|(id, &(_, minted))| (id.clone(), minted))
                .collect();
            by_age.sort_by_key(|&(_, minted)| minted);
            let excess = map.len() - SERVER_TOOL_PIN_CAP;
            for (id, _) in by_age.into_iter().take(excess) {
                map.remove(&id);
            }
        }
        drop(map);
        self.mark_server_tool_pins_dirty();
    }

    /// The account that minted any of `ids`, if this process knows one.
    ///
    /// Returns the account index and the id that named it. An id this map has never seen
    /// (minted before this shipped, or expired, or evicted) contributes nothing, so a
    /// conversation the proxy knows nothing about is routed exactly as it was before this
    /// feature existed.
    ///
    /// Several ids from one conversation always name the same account in practice — they were
    /// all minted on whichever account served their turns, and after this ships every turn
    /// after the first is pinned. When they disagree anyway (an id minted before this shipped
    /// and one minted after), the NEWEST mint wins: it is the one the client is most likely to
    /// still be echoing, and nothing else here can break the tie.
    pub fn server_tool_pin_account(&self, ids: &[String]) -> Option<(usize, String)> {
        if ids.is_empty() {
            return None;
        }
        let map = self
            .server_tool_pins
            .lock()
            .expect("server tool pins lock poisoned");
        ids.iter()
            .filter_map(|id| map.get(id).map(|&(index, minted)| (index, minted, id)))
            .max_by_key(|&(_, minted, _)| minted)
            .map(|(index, _, id)| (index, id.clone()))
    }

    /// Whether `ids` name more than one distinct account — the disagreement
    /// [`Self::server_tool_pin_account`] resolves by taking the newest. Split out so the caller
    /// can log it once, on the request path, without holding the lock twice for the common case.
    pub fn server_tool_pins_disagree(&self, ids: &[String]) -> bool {
        let map = self
            .server_tool_pins
            .lock()
            .expect("server tool pins lock poisoned");
        let mut seen: Option<usize> = None;
        for id in ids {
            let Some(&(index, _)) = map.get(id) else {
                continue;
            };
            match seen {
                None => seen = Some(index),
                Some(first) if first != index => return true,
                Some(_) => {}
            }
        }
        false
    }

    /// The map as persistable records — each live entry's index replaced by the identity of the
    /// account at that index.
    ///
    /// An entry whose index names no account is skipped rather than written: writing a
    /// placeholder identity is precisely the mis-resolution this design exists to prevent.
    pub fn server_tool_pin_snapshot(&self) -> Vec<StoredServerToolPin> {
        let entries: Vec<(String, usize, i64)> = {
            let map = self
                .server_tool_pins
                .lock()
                .expect("server tool pins lock poisoned");
            map.iter()
                .map(|(id, &(index, minted))| (id.clone(), index, minted))
                .collect()
        };
        if entries.is_empty() {
            return Vec::new();
        }
        let accounts = self.accounts.read().expect("accounts lock poisoned");
        entries
            .into_iter()
            .filter_map(|(id, index, minted_at_ms)| {
                let account = accounts.get(index)?;
                Some(StoredServerToolPin {
                    id,
                    name: account.name.clone(),
                    account_uuid: account.account_uuid.clone(),
                    org_uuid: account.org_uuid.clone(),
                    org_name: account.org_name.clone(),
                    minted_at_ms,
                })
            })
            .collect()
    }

    /// Write the map to `path`, atomically. Returns how many entries landed.
    ///
    /// The caller logs the failure and carries on: a proxy that cannot write this cache serves
    /// traffic exactly as it did before the file existed, and pays the 400s it was buying back.
    pub fn flush_server_tool_pins(&self, path: &Path) -> Result<usize, ConfigError> {
        server_tool_pins::save(path, &self.server_tool_pin_snapshot(), crate::now_ms())
    }

    /// Restore the map from `path`, resolving each stored identity against the accounts loaded
    /// THIS boot and dropping everything stale or not resolvable to exactly one account.
    ///
    /// Existing in-memory entries win, same as [`Self::restore_affinity`]: an id learned from a
    /// response served between boot and this call is fresher than anything on disk.
    pub fn restore_server_tool_pins(&self, path: &Path, ttl_ms: i64) -> LoadReport {
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
        let report = server_tool_pins::load(path, &candidates, crate::now_ms(), ttl_ms);
        let mut map = self
            .server_tool_pins
            .lock()
            .expect("server tool pins lock poisoned");
        for (id, &value) in &report.pins {
            map.entry(id.clone()).or_insert(value);
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use super::*;
    use crate::config::Account;
    use crate::manager::AccountRuntime;

    fn account(name: &str, uuid: &str) -> Account {
        let mut account = identity::probe(name, Some(uuid.to_string()), Some("org-1".into()), None);
        account.access_token = format!("at-{name}");
        account.refresh_token = Some(format!("rt-{name}"));
        account.expires_at = Some(crate::now_ms() + 3_600_000);
        account.priority = Some(0);
        account
    }

    fn manager_over(accounts: &[Account]) -> Arc<Manager> {
        Manager::from_runtimes(
            accounts
                .iter()
                .map(|a| AccountRuntime::from_config(a, false))
                .collect(),
        )
    }

    fn tmp(label: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "tcr-server-tool-manager-{label}-{}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join(server_tool_pins::FILE_NAME)
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// The whole claim of the map, at its own layer: an id recorded from a response names the
    /// account that served that response.
    #[test]
    fn a_recorded_id_names_its_minting_account() {
        let manager = manager_over(&[
            account("a@example.com", "uuid-a"),
            account("b@example.com", "uuid-b"),
        ]);
        manager.record_minted_server_tools(&ids(&["srvtoolu_fake_1"]), 1, crate::now_ms());

        assert_eq!(
            manager.server_tool_pin_account(&ids(&["srvtoolu_fake_1"])),
            Some((1, "srvtoolu_fake_1".to_string()))
        );
        assert_eq!(
            manager.server_tool_pin_account(&ids(&["srvtoolu_fake_unknown"])),
            None,
            "an id this process never minted must not pin anything"
        );
    }

    /// Two ids naming different accounts is not supposed to happen, and must still answer
    /// deterministically rather than by hash order.
    #[test]
    fn disagreeing_ids_resolve_to_the_newest_mint() {
        let manager = manager_over(&[
            account("a@example.com", "uuid-a"),
            account("b@example.com", "uuid-b"),
        ]);
        let now = crate::now_ms();
        manager.record_minted_server_tools(&ids(&["srvtoolu_fake_old"]), 0, now - 60_000);
        manager.record_minted_server_tools(&ids(&["srvtoolu_fake_new"]), 1, now);

        let both = ids(&["srvtoolu_fake_old", "srvtoolu_fake_new"]);
        assert!(manager.server_tool_pins_disagree(&both));
        assert_eq!(
            manager.server_tool_pin_account(&both),
            Some((1, "srvtoolu_fake_new".to_string()))
        );
        assert!(
            !manager.server_tool_pins_disagree(&ids(&["srvtoolu_fake_new"])),
            "one id can never disagree with itself"
        );
    }

    /// The map is bounded in memory, and it is the OLDEST mints that go.
    #[test]
    fn the_map_caps_by_evicting_the_oldest_mints() {
        let manager = manager_over(&[account("a@example.com", "uuid-a")]);
        let now = crate::now_ms();
        // Oldest first, so the ones recorded early are the ones that must fall out.
        for n in 0..(SERVER_TOOL_PIN_CAP + 10) {
            manager.record_minted_server_tools(
                &[format!("srvtoolu_fake_{n}")],
                0,
                now - (SERVER_TOOL_PIN_CAP + 10 - n) as i64,
            );
        }
        let held = manager
            .server_tool_pins
            .lock()
            .expect("server tool pins lock poisoned")
            .len();
        assert_eq!(held, SERVER_TOOL_PIN_CAP);
        assert_eq!(
            manager.server_tool_pin_account(&ids(&["srvtoolu_fake_0"])),
            None,
            "the oldest mint is the one evicted"
        );
        let newest = format!("srvtoolu_fake_{}", SERVER_TOOL_PIN_CAP + 9);
        assert_eq!(
            manager.server_tool_pin_account(std::slice::from_ref(&newest)),
            Some((0, newest)),
            "the newest mint must survive its own insertion"
        );
    }

    /// An entry past the TTL is dropped on the next record, without waiting for a restart.
    #[test]
    fn an_entry_past_the_ttl_is_pruned_in_memory() {
        let manager = manager_over(&[account("a@example.com", "uuid-a")]);
        let now = crate::now_ms();
        manager.record_minted_server_tools(
            &ids(&["srvtoolu_fake_ancient"]),
            0,
            now - SERVER_TOOL_PIN_TTL_MS - 1_000,
        );
        manager.record_minted_server_tools(&ids(&["srvtoolu_fake_fresh"]), 0, now);

        assert_eq!(
            manager.server_tool_pin_account(&ids(&["srvtoolu_fake_ancient"])),
            None
        );
        assert!(manager
            .server_tool_pin_account(&ids(&["srvtoolu_fake_fresh"]))
            .is_some());
    }

    /// The persistence bridge's own half: indices out to identities on save, identities back to
    /// (possibly different) indices on load. A snapshot that wrote the position would pin every
    /// restored conversation to the account guaranteed to 400 it.
    #[test]
    fn pins_round_trip_across_a_reordered_account_list() {
        let path = tmp("round-trip");
        let a = account("a@example.com", "uuid-a");
        let b = account("b@example.com", "uuid-b");

        let first = manager_over(&[a.clone(), b.clone()]);
        first.record_minted_server_tools(&ids(&["srvtoolu_fake_1"]), 1, crate::now_ms());
        assert_eq!(first.flush_server_tool_pins(&path).expect("flush"), 1);

        let raw = std::fs::read_to_string(&path).expect("read pin file");
        assert!(raw.contains("uuid-b"), "the pin must name b: {raw}");
        assert!(!raw.contains("uuid-a"), "and nobody else: {raw}");

        // Next boot: b first, a second.
        let second = manager_over(&[b, a]);
        let report = second.restore_server_tool_pins(&path, SERVER_TOOL_PIN_TTL_MS);
        assert_eq!(report.degraded, None);
        assert_eq!(
            second.server_tool_pin_account(&ids(&["srvtoolu_fake_1"])),
            Some((0, "srvtoolu_fake_1".to_string())),
            "the id must follow b to position 0"
        );
    }

    /// A stale entry does not come back across a restart.
    #[test]
    fn a_stale_entry_is_dropped_at_restore() {
        let path = tmp("expiry");
        let accounts = [account("a@example.com", "uuid-a")];
        let now = crate::now_ms();

        let first = manager_over(&accounts);
        first.record_minted_server_tools(&ids(&["srvtoolu_fake_fresh"]), 0, now - 60_000);
        // Recorded straight into the map: `record_minted_server_tools` prunes anything already
        // past the TTL, and this test is about what LOAD does with a file that holds one.
        first
            .server_tool_pins
            .lock()
            .expect("server tool pins lock poisoned")
            .insert(
                "srvtoolu_fake_stale".to_string(),
                (0, now - SERVER_TOOL_PIN_TTL_MS - 60_000),
            );
        assert_eq!(first.flush_server_tool_pins(&path).expect("flush"), 2);

        let second = manager_over(&accounts);
        let report = second.restore_server_tool_pins(&path, SERVER_TOOL_PIN_TTL_MS);
        assert_eq!(report.expired, 1);
        assert!(second
            .server_tool_pin_account(&ids(&["srvtoolu_fake_fresh"]))
            .is_some());
        assert_eq!(
            second.server_tool_pin_account(&ids(&["srvtoolu_fake_stale"])),
            None
        );
    }

    /// The debounce contract the flusher task runs on.
    #[test]
    fn the_dirty_flag_is_raised_by_a_record_and_consumed_by_one_take() {
        let manager = manager_over(&[account("a@example.com", "uuid-a")]);
        assert!(!manager.take_server_tool_pins_dirty());
        manager.record_minted_server_tools(&ids(&["srvtoolu_fake_1"]), 0, crate::now_ms());
        assert!(manager.take_server_tool_pins_dirty());
        assert!(!manager.take_server_tool_pins_dirty());
    }
}
