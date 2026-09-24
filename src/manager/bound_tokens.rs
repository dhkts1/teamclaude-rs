//! `Manager` glue for the account-bound-token → minting account map — the live half of
//! [`crate::bound_tokens`], mirroring `pins.rs`'s split for session affinity.
//!
//! The rule this map serves: **a request whose history carries an account-bound token is served
//! by the account that minted that token while that account can hold it**, and — when the
//! history is bound but no token is known to this process — by the account the session is
//! PINNED to. A thinking signature, an advisor result or server-side thread state is unusable
//! off its minting account, so a sibling answers the first request with a 400 or a 404.
//!
//! "Can hold it" is [`Manager::bound_account_holds`], and it is deliberately narrower than "is
//! bound". Claude Code recovers from all three rejections on its own, in one extra round trip
//! (read in the 2.1.277..2.1.280 binaries: `retry:thinking-signature-strip`,
//! `retry:advisor-strip`, and `retry:tether-replay` on a 404 `thread_not_found`). So holding a
//! conversation on its account is worth a short wait, and never worth one that outlasts the
//! cache: an account that is gone, or out for longer than [`super::CACHE_WARM_HOLD_SECS`],
//! releases the conversation to the fleet. The cost of the other choice, measured 2026-09-22: a
//! conversation bound to a `rejected` account got a 429 on every turn, 6-11 a minute, until the
//! operator abandoned the session, because `/compact` keeps the thread id.
//!
//! Two responsibilities the store next door does not have, same as `pins.rs`:
//!
//! - **Lock discipline.** The map's mutex is never held while the accounts lock is taken. Every
//!   method takes one lock, copies what it needs, drops it, and only then takes the other.
//! - **Translating index ↔ identity.** The live map is positional, because the response path
//!   learns an index and must not pay the accounts lock per token; the file is not.

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::Ordering;

use time::OffsetDateTime;

use crate::bound_tokens::{
    self, BoundTokenKind, LoadReport, StoredAccount, BOUND_TOKEN_CAP, BOUND_TOKEN_TTL_MS,
};
use crate::config::ConfigError;
use crate::identity;

use super::Manager;

/// How long a session stays told that its held conversation is moving off one account (see
/// [`Manager::first_switch_warning`]). The client's own retry comes within seconds; this only
/// has to outlast a user who cancels the retry and sends the next turn by hand, without
/// silencing a warning that is due again much later.
pub const SWITCH_WARNING_TTL_MS: i64 = 10 * 60 * 1000;

/// One token as the response path hands it over: its kind and the token itself, unhashed. The
/// hashing happens here, once, so no caller can forget it.
pub type MintedToken = (BoundTokenKind, String);

impl Manager {
    /// Flag the bound-token map as changed since the last flush — same relaxed-atomic debounce
    /// contract as [`Self::mark_affinity_dirty`].
    pub fn mark_bound_tokens_dirty(&self) {
        self.bound_tokens_dirty.store(true, Ordering::Relaxed);
    }

    /// Consume the dirty flag: `true` when something changed since the last call.
    pub fn take_bound_tokens_dirty(&self) -> bool {
        self.bound_tokens_dirty.swap(false, Ordering::Relaxed)
    }

    /// Remember that the account at `account_idx` minted these tokens.
    ///
    /// Called from both response paths in `proxy.rs`: the response is the only place a token
    /// can be learned, and the serving account is only known there.
    ///
    /// Bounding is done HERE rather than at save time, because the map has to stay bounded in
    /// memory too: anything past the TTL goes first (it can never be honoured again), and if
    /// that is not enough the OLDEST mints are evicted until the map is back under
    /// [`BOUND_TOKEN_CAP`]. Oldest, not newest: an evicted token stops pinning its conversation,
    /// and the conversations most likely to still be live are the ones minted most recently.
    pub fn record_bound_tokens(&self, tokens: &[MintedToken], account_idx: usize, now_ms: i64) {
        if tokens.is_empty() {
            return;
        }
        let mut map = self
            .bound_tokens
            .lock()
            .expect("bound tokens lock poisoned");
        for (kind, token) in tokens {
            map.insert(
                bound_tokens::hash_token(token),
                (account_idx, *kind, now_ms),
            );
        }
        map.retain(|_, &mut (_, _, minted)| now_ms.saturating_sub(minted) <= BOUND_TOKEN_TTL_MS);
        if map.len() > BOUND_TOKEN_CAP {
            let mut by_age: Vec<(String, i64)> = map
                .iter()
                .map(|(hash, &(_, _, minted))| (hash.clone(), minted))
                .collect();
            by_age.sort_by_key(|&(_, minted)| minted);
            let excess = map.len() - BOUND_TOKEN_CAP;
            for (hash, _) in by_age.into_iter().take(excess) {
                map.remove(&hash);
            }
        }
        drop(map);
        self.mark_bound_tokens_dirty();
    }

    /// The account that minted any of `tokens`, if this process knows one: the account index,
    /// the kind that named it, and the newest mint's timestamp.
    ///
    /// A token this map has never seen (minted before this shipped, expired, or evicted)
    /// contributes nothing — which is what the caller's bound-history fallback is for.
    ///
    /// Tokens from one conversation name the same account in practice; when they disagree (one
    /// minted before this shipped and one after), the NEWEST mint wins and the caller says so
    /// once.
    pub fn bound_token_account(&self, tokens: &[String]) -> Option<(usize, BoundTokenKind)> {
        if tokens.is_empty() {
            return None;
        }
        let map = self
            .bound_tokens
            .lock()
            .expect("bound tokens lock poisoned");
        tokens
            .iter()
            .filter_map(|token| map.get(&bound_tokens::hash_token(token)).copied())
            .max_by_key(|&(_, _, minted)| minted)
            .map(|(index, kind, _)| (index, kind))
    }

    /// Whether the account at `idx` can still hold a conversation whose history is bound to it:
    /// the one question that decides between holding the request there and releasing it to the
    /// fleet.
    ///
    /// Yes while the account is alive for every model class ([`Self::account_hard_ok`], which
    /// passes a hold that clears while the cache is warm) and for THIS request's class
    /// ([`Self::model_blocked`]). A short hold is waited out, or answered with that hold's own
    /// `retry-after`. The soft quota threshold is not consulted: an account over it still serves
    /// until the upstream says otherwise.
    ///
    /// No once the account is terminally gated (`disabled`, a dead login, a `rejected` unified
    /// status), parked, reserved away from this request, held for longer than the cache lives,
    /// or out of this model class's weekly bucket. None of those clears soon, and the client
    /// recovers from the one rejection a sibling gives (see this module's doc). An index that
    /// names no account is a no too: there is nothing to hold the request to.
    pub fn bound_account_holds(
        &self,
        idx: usize,
        now: OffsetDateTime,
        is_fable: bool,
        group: Option<&str>,
    ) -> bool {
        let now_ms = super::odt_to_ms(now);
        let reserved = self.reserved_groups();
        let parked = self.parked_groups();
        let accounts = self.accounts.read().expect("accounts lock poisoned");
        accounts.get(idx).is_some_and(|account| {
            Self::account_hard_ok(account, now_ms, group, &reserved, &parked)
                && !Self::model_blocked(
                    account,
                    self.global_threshold,
                    self.fable_weekly_threshold,
                    now,
                    is_fable,
                )
        })
    }

    /// Serve a held request on the account its history is bound to, over the soft threshold if
    /// need be: `Some(idx)` when that account can answer this request now.
    ///
    /// [`Self::bound_account_holds`] keeps a conversation on its account without consulting the
    /// soft threshold, so the serve side has to agree with it. It did not: a held request on an
    /// account over the threshold fell through to [`Self::select_revalidation`], whose fallback
    /// lets one request through per 2 s across the whole fleet, and the turn that lost that race
    /// got an exhausted 429 naming the account's 5-hour window. Claude Code renders that as
    /// "You've hit your session limit" (2026-09-24: 3 of 3 such 429s came 0.05-1.4 s after
    /// another turn on the same account, while four other accounts sat at 0%).
    ///
    /// Served the way `select_revalidation` serves a session's pin: the hard gates for THIS
    /// request ([`Self::hard_ok`] with the request's own group, so a live hold of any length
    /// still refuses), no revalidation window, and the session's pin left alone. `None` when the
    /// account already failed this request (`tried`), cannot serve it now, or sits outside the
    /// group this request asked for strictly: a strict group never serves from outside itself,
    /// held history included.
    pub fn select_bound(
        &self,
        idx: usize,
        tried: &HashSet<usize>,
        now: OffsetDateTime,
        is_fable: bool,
        group: Option<&str>,
        strict_group: Option<&str>,
    ) -> Option<usize> {
        if tried.contains(&idx) {
            return None;
        }
        let now_ms = super::odt_to_ms(now);
        let reserved = self.reserved_groups();
        let parked = self.parked_groups();
        let mut accounts = self.accounts.write().expect("accounts lock poisoned");
        let account = accounts.get_mut(idx)?;
        let outside_strict_group =
            strict_group.is_some_and(|g| !account.groups.iter().any(|carried| carried == g));
        if outside_strict_group
            || !Self::hard_ok(
                account,
                self.global_threshold,
                self.fable_weekly_threshold,
                now,
                now_ms,
                is_fable,
                group,
                &reserved,
                &parked,
            )
        {
            return None;
        }
        let utilization = account.quota.max_utilization(now, is_fable);
        account.last_selected_seq = self.select_seq.fetch_add(1, Ordering::Relaxed);
        tracing::info!(
            account = %account.name,
            utilization,
            is_fable,
            "revalidation-serve (bound-honor): serving the account this conversation's history is bound to"
        );
        Some(idx)
    }

    /// Whether `tokens` name more than one distinct account — the disagreement
    /// [`Self::bound_token_account`] resolves by taking the newest.
    pub fn bound_tokens_disagree(&self, tokens: &[String]) -> bool {
        let map = self
            .bound_tokens
            .lock()
            .expect("bound tokens lock poisoned");
        let mut seen: Option<usize> = None;
        for token in tokens {
            let Some(&(index, _, _)) = map.get(&bound_tokens::hash_token(token)) else {
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

    /// First time this session key is held to its pin by bound history, `true`; every later
    /// time, `false`.
    ///
    /// The hold itself is unconditional — this only decides whether it is worth a log line, so
    /// a long conversation does not write one per turn. Bounded by the same cap the token map
    /// uses, cleared wholesale when it fills: the worst case of forgetting is one extra line.
    pub fn first_bound_history_hold(&self, session_key: u64) -> bool {
        let mut seen = self
            .bound_history_logged
            .lock()
            .expect("bound history log set poisoned");
        if seen.len() >= BOUND_TOKEN_CAP {
            seen.clear();
        }
        seen.insert(session_key)
    }

    /// Whether to warn this session before its held conversation moves off account `idx`:
    /// `true` the first time within [`SWITCH_WARNING_TTL_MS`], `false` after that.
    ///
    /// The move itself is [`Self::bound_account_holds`] answering no. Before this existed the
    /// move was silent. Now the first request that would move gets one plain 429 that says so,
    /// and Claude Code retries a plain 429 on its own (measured 2026-09-24 against 2.1.281: two
    /// retries allowed, three requests seen), so the retry is the request that moves. Keyed by
    /// account as well as session, so a conversation that moves again later, off the account it
    /// moved to, is told again.
    ///
    /// Bounded like [`Self::first_bound_history_hold`]: stale entries go on every call, and the
    /// map is cleared wholesale if it still fills. The worst case of forgetting is one extra
    /// warning.
    pub fn first_switch_warning(&self, session_key: u64, idx: usize, now_ms: i64) -> bool {
        let mut warned = self
            .bound_switch_warned
            .lock()
            .expect("bound switch warned map poisoned");
        warned.retain(|_, &mut at| now_ms.saturating_sub(at) < SWITCH_WARNING_TTL_MS);
        if warned.len() >= BOUND_TOKEN_CAP {
            warned.clear();
        }
        match warned.entry((session_key, idx)) {
            std::collections::hash_map::Entry::Occupied(_) => false,
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(now_ms);
                true
            }
        }
    }

    /// Whether this session was told, within [`SWITCH_WARNING_TTL_MS`], that its held
    /// conversation is moving off account `idx`: the read-only half of
    /// [`Self::first_switch_warning`].
    ///
    /// The request path reads it before [`Self::bound_account_holds`], so a warning is a
    /// commitment. The warning says the retry moves the conversation; an account whose hold
    /// crept back under the cache's life in the 3 s before the retry would otherwise hold it
    /// again and answer the retry with a 429 of its own (found in review, 2026-09-24).
    pub fn switch_warned(&self, session_key: u64, idx: usize, now_ms: i64) -> bool {
        self.bound_switch_warned
            .lock()
            .expect("bound switch warned map poisoned")
            .get(&(session_key, idx))
            .is_some_and(|&at| now_ms.saturating_sub(at) < SWITCH_WARNING_TTL_MS)
    }

    /// The map as persistable records — each live entry's index replaced by the identity of the
    /// account at that index.
    ///
    /// An entry whose index names no account is skipped rather than written: writing a
    /// placeholder identity is precisely the mis-resolution this design exists to prevent.
    pub fn bound_token_snapshot(&self) -> Vec<(String, BoundTokenKind, StoredAccount, i64)> {
        let entries: Vec<(String, usize, BoundTokenKind, i64)> = {
            let map = self
                .bound_tokens
                .lock()
                .expect("bound tokens lock poisoned");
            map.iter()
                .map(|(hash, &(index, kind, minted))| (hash.clone(), index, kind, minted))
                .collect()
        };
        if entries.is_empty() {
            return Vec::new();
        }
        let accounts = self.accounts.read().expect("accounts lock poisoned");
        entries
            .into_iter()
            .filter_map(|(hash, index, kind, minted_at_ms)| {
                let account = accounts.get(index)?;
                Some((
                    hash,
                    kind,
                    StoredAccount {
                        name: account.name.clone(),
                        account_uuid: account.account_uuid.clone(),
                        org_uuid: account.org_uuid.clone(),
                        org_name: account.org_name.clone(),
                    },
                    minted_at_ms,
                ))
            })
            .collect()
    }

    /// Write the map to `path`, atomically. Returns how many tokens landed.
    ///
    /// The caller logs the failure and carries on: a proxy that cannot write this cache serves
    /// traffic exactly as it did before the file existed, and pays the rejected turns it was
    /// buying back.
    pub fn flush_bound_tokens(&self, path: &Path) -> Result<usize, ConfigError> {
        bound_tokens::save(path, &self.bound_token_snapshot(), crate::now_ms())
    }

    /// Restore the map from `path`, resolving each stored identity against the accounts loaded
    /// THIS boot and dropping everything stale or not resolvable to exactly one account.
    ///
    /// Existing in-memory entries win, same as [`Self::restore_affinity`]: a token learned from
    /// a response served between boot and this call is fresher than anything on disk.
    pub fn restore_bound_tokens(&self, path: &Path, ttl_ms: i64) -> LoadReport {
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
        let report = bound_tokens::load(path, &candidates, crate::now_ms(), ttl_ms);
        let mut map = self
            .bound_tokens
            .lock()
            .expect("bound tokens lock poisoned");
        for (hash, &value) in &report.tokens {
            map.entry(hash.clone()).or_insert(value);
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
            "tcr-bound-tokens-manager-{label}-{}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join(bound_tokens::FILE_NAME)
    }

    fn sig(token: &str) -> Vec<MintedToken> {
        vec![(BoundTokenKind::ThinkingSignature, token.to_string())]
    }

    fn tokens(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// The whole claim of the map, at its own layer, for every kind.
    #[test]
    fn a_recorded_token_names_its_minting_account_whatever_its_kind() {
        let manager = manager_over(&[
            account("a@example.com", "uuid-a"),
            account("b@example.com", "uuid-b"),
        ]);
        let now = crate::now_ms();
        manager.record_bound_tokens(
            &[
                (BoundTokenKind::ServerToolUse, "srvtoolu_fake_1".into()),
                (BoundTokenKind::ThinkingSignature, "sig_fake_1".into()),
                (BoundTokenKind::RedactedThinking, "data_fake_1".into()),
                (BoundTokenKind::MessageId, "msg_fake_1".into()),
            ],
            1,
            now,
        );

        for (token, kind) in [
            ("srvtoolu_fake_1", BoundTokenKind::ServerToolUse),
            ("sig_fake_1", BoundTokenKind::ThinkingSignature),
            ("data_fake_1", BoundTokenKind::RedactedThinking),
            ("msg_fake_1", BoundTokenKind::MessageId),
        ] {
            assert_eq!(
                manager.bound_token_account(&tokens(&[token])),
                Some((1, kind)),
                "{token} must name the account that minted it"
            );
        }
        assert_eq!(
            manager.bound_token_account(&tokens(&["sig_fake_never_minted"])),
            None,
            "a token this process never minted must not pin anything"
        );
    }

    #[test]
    fn disagreeing_tokens_resolve_to_the_newest_mint() {
        let manager = manager_over(&[
            account("a@example.com", "uuid-a"),
            account("b@example.com", "uuid-b"),
        ]);
        let now = crate::now_ms();
        manager.record_bound_tokens(&sig("sig_fake_old"), 0, now - 60_000);
        manager.record_bound_tokens(&sig("sig_fake_new"), 1, now);

        let both = tokens(&["sig_fake_old", "sig_fake_new"]);
        assert!(manager.bound_tokens_disagree(&both));
        assert_eq!(
            manager.bound_token_account(&both),
            Some((1, BoundTokenKind::ThinkingSignature))
        );
        assert!(!manager.bound_tokens_disagree(&tokens(&["sig_fake_new"])));
    }

    /// The map is bounded in memory at the raised cap, and it is the OLDEST mints that go.
    #[test]
    fn the_map_caps_by_evicting_the_oldest_mints() {
        let manager = manager_over(&[account("a@example.com", "uuid-a")]);
        let now = crate::now_ms();
        for n in 0..(BOUND_TOKEN_CAP + 10) {
            manager.record_bound_tokens(
                &sig(&format!("sig_fake_{n}")),
                0,
                now - (BOUND_TOKEN_CAP + 10 - n) as i64,
            );
        }
        let held = manager
            .bound_tokens
            .lock()
            .expect("bound tokens lock poisoned")
            .len();
        assert_eq!(held, BOUND_TOKEN_CAP);
        assert_eq!(
            manager.bound_token_account(&tokens(&["sig_fake_0"])),
            None,
            "the oldest mint is the one evicted"
        );
        let newest = format!("sig_fake_{}", BOUND_TOKEN_CAP + 9);
        assert_eq!(
            manager.bound_token_account(std::slice::from_ref(&newest)),
            Some((0, BoundTokenKind::ThinkingSignature)),
            "the newest mint must survive its own insertion"
        );
    }

    #[test]
    fn an_entry_past_the_ttl_is_pruned_in_memory() {
        let manager = manager_over(&[account("a@example.com", "uuid-a")]);
        let now = crate::now_ms();
        manager.record_bound_tokens(
            &sig("sig_fake_ancient"),
            0,
            now - BOUND_TOKEN_TTL_MS - 1_000,
        );
        manager.record_bound_tokens(&sig("sig_fake_fresh"), 0, now);

        assert_eq!(
            manager.bound_token_account(&tokens(&["sig_fake_ancient"])),
            None
        );
        assert!(manager
            .bound_token_account(&tokens(&["sig_fake_fresh"]))
            .is_some());
    }

    /// The persistence bridge's own half: indices out to identities on save, identities back to
    /// (possibly different) indices on load, kinds intact.
    #[test]
    fn tokens_round_trip_across_a_reordered_account_list() {
        let path = tmp("round-trip");
        let a = account("a@example.com", "uuid-a");
        let b = account("b@example.com", "uuid-b");

        let first = manager_over(&[a.clone(), b.clone()]);
        first.record_bound_tokens(&sig("sig_fake_1"), 1, crate::now_ms());
        assert_eq!(first.flush_bound_tokens(&path).expect("flush"), 1);

        let raw = std::fs::read_to_string(&path).expect("read the token file");
        assert!(raw.contains("uuid-b"), "the entry must name b: {raw}");
        assert!(!raw.contains("uuid-a"), "and nobody else: {raw}");
        assert!(
            !raw.contains("sig_fake_1"),
            "and never the signature itself: {raw}"
        );

        // Next boot: b first, a second.
        let second = manager_over(&[b, a]);
        let report = second.restore_bound_tokens(&path, BOUND_TOKEN_TTL_MS);
        assert_eq!(report.degraded, None);
        assert_eq!(
            second.bound_token_account(&tokens(&["sig_fake_1"])),
            Some((0, BoundTokenKind::ThinkingSignature)),
            "the token must follow b to position 0"
        );
    }

    #[test]
    fn a_stale_entry_is_dropped_at_restore() {
        let path = tmp("expiry");
        let accounts = [account("a@example.com", "uuid-a")];
        let now = crate::now_ms();

        let first = manager_over(&accounts);
        first.record_bound_tokens(&sig("sig_fake_fresh"), 0, now - 60_000);
        // Seeded straight into the map: `record_bound_tokens` prunes anything already past the
        // TTL, and this test is about what LOAD does with a file that holds one.
        first
            .bound_tokens
            .lock()
            .expect("bound tokens lock poisoned")
            .insert(
                bound_tokens::hash_token("sig_fake_stale"),
                (
                    0,
                    BoundTokenKind::ThinkingSignature,
                    now - BOUND_TOKEN_TTL_MS - 60_000,
                ),
            );
        assert_eq!(first.flush_bound_tokens(&path).expect("flush"), 2);

        let second = manager_over(&accounts);
        let report = second.restore_bound_tokens(&path, BOUND_TOKEN_TTL_MS);
        assert_eq!(report.expired, 1);
        assert!(second
            .bound_token_account(&tokens(&["sig_fake_fresh"]))
            .is_some());
        assert_eq!(
            second.bound_token_account(&tokens(&["sig_fake_stale"])),
            None
        );
    }

    #[test]
    fn the_dirty_flag_is_raised_by_a_record_and_consumed_by_one_take() {
        let manager = manager_over(&[account("a@example.com", "uuid-a")]);
        assert!(!manager.take_bound_tokens_dirty());
        manager.record_bound_tokens(&sig("sig_fake_1"), 0, crate::now_ms());
        assert!(manager.take_bound_tokens_dirty());
        assert!(!manager.take_bound_tokens_dirty());
    }

    /// The log gate says yes once per session and no afterwards — a conversation held for a
    /// hundred turns is one line, not a hundred.
    #[test]
    fn a_bound_history_hold_is_logged_once_per_session() {
        let manager = manager_over(&[account("a@example.com", "uuid-a")]);
        assert!(manager.first_bound_history_hold(11));
        assert!(!manager.first_bound_history_hold(11));
        assert!(manager.first_bound_history_hold(22));
    }

    /// `select_bound` serves the held account only where this request may go. Its strict-group
    /// check is the only thing keeping a group-scoped request inside its group on this path,
    /// because `bound_account_holds` passes an account outside a strict group (found in review,
    /// 2026-09-24).
    #[test]
    fn select_bound_serves_the_held_account_only_where_this_request_may_go() {
        let mut member = account("a@example.com", "uuid-a");
        member.groups = Some(vec!["g".to_string()]);
        let manager = manager_over(&[member]);
        let now = OffsetDateTime::now_utc();
        let untried = HashSet::new();
        assert_eq!(
            manager.select_bound(0, &untried, now, false, Some("g"), Some("g")),
            Some(0),
            "a member of the strict group it asked for"
        );
        assert_eq!(
            manager.select_bound(0, &untried, now, false, Some("other"), Some("other")),
            None,
            "outside the strict group it asked for"
        );
        assert_eq!(
            manager.select_bound(0, &HashSet::from([0]), now, false, None, None),
            None,
            "already failed this request"
        );
        assert_eq!(
            manager.select_bound(1, &untried, now, false, None, None),
            None,
            "no such account"
        );
        manager.mark_rate_limited(0, 60);
        assert_eq!(
            manager.select_bound(0, &untried, now, false, None, None),
            None,
            "on a live hold"
        );
    }

    /// One warning per session per account, so the client's retry of the warning moves; told
    /// again when the conversation later moves off a different account, or once the warning's
    /// life has run out.
    #[test]
    fn a_switch_is_warned_once_per_session_and_account() {
        let manager = manager_over(&[account("a@example.com", "uuid-a")]);
        let t0 = 1_000_000;
        assert!(
            manager.first_switch_warning(11, 0, t0),
            "first request: warn"
        );
        assert!(
            !manager.first_switch_warning(11, 0, t0 + 3_000),
            "its retry: move"
        );
        assert!(
            manager.first_switch_warning(11, 1, t0 + 3_000),
            "another account: warn"
        );
        assert!(
            manager.first_switch_warning(22, 0, t0 + 3_000),
            "another session: warn"
        );
        assert!(
            manager.first_switch_warning(11, 0, t0 + SWITCH_WARNING_TTL_MS),
            "past the warning's life: warn again"
        );
    }
}
