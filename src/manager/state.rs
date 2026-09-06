//! `Manager` state methods, split verbatim from `mod.rs`.

use super::*;

impl Manager {
    /// Configured probe cadence in seconds, read from the config's unmodelled
    /// `quotaProbeSeconds` (default [`crate::probe::DEFAULT_PROBE_SECONDS`]). A
    /// value `<= 0` disables probing.
    pub fn probe_interval_seconds(&self) -> u64 {
        self.config
            .lock()
            .expect("config lock poisoned")
            .extra
            .get("quotaProbeSeconds")
            .and_then(|v| v.as_i64())
            .map_or(crate::probe::DEFAULT_PROBE_SECONDS, |v| v.max(0) as u64)
    }

    /// Configured keep-warm cadence in seconds, read from the config's unmodelled
    /// `warmupSeconds`. **Default 0 = OFF** (unlike the probe's 75) — keep-warm
    /// spends real quota, so it ships dark and is only ever running when explicitly
    /// enabled. A value `<= 0` disables it (no warm task is spawned).
    pub fn warmup_interval_seconds(&self) -> u64 {
        self.config
            .lock()
            .expect("config lock poisoned")
            .extra
            .get("warmupSeconds")
            .and_then(|v| v.as_i64())
            .map_or(0, |v| v.max(0) as u64)
    }

    /// Whether session affinity is enabled, read from the config's unmodelled
    /// top-level `sessionAffinity` (default `true` — ON unless explicitly
    /// disabled with `"sessionAffinity": false`). An absent key fails safe
    /// toward keeping caches warm: measured 2026-08-14, a config that lost its
    /// `sessionAffinity` key silently went cold when the default was `false`,
    /// and the client-side prompt-cache hit ratio dropped 0.952 → 0.498 with
    /// cache_creation rising 6.5M/hour → 122M/hour, sustained seven hours — a
    /// single missing key costing roughly 100M re-created tokens per hour.
    /// Same read pattern as [`Self::probe_interval_seconds`]. When explicitly
    /// disabled, the hybrid server injects no `SessionKey` extension, so
    /// `select` always receives `affinity = None` and the disabled path is
    /// inert.
    pub fn session_affinity_enabled(&self) -> bool {
        self.config
            .lock()
            .expect("config lock poisoned")
            .extra
            .get("sessionAffinity")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    }

    /// Over-threshold revalidation-serve is ON by default; set top-level
    /// `"revalidationServe": false` in the config to disable it (pure fall-through
    /// to a synthesized 429 when the whole fleet reads over the soft threshold).
    /// Same read pattern as [`Self::session_affinity_enabled`]. See
    /// [`Manager::select_revalidation`].
    pub fn revalidation_serve_enabled(&self) -> bool {
        self.config
            .lock()
            .expect("config lock poisoned")
            .extra
            .get("revalidationServe")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    }

    /// Whether load-balancing migration is enabled, read from the config's
    /// unmodelled top-level `loadBalanceMigration` (**default `false` — OFF**).
    /// Same read pattern as [`Self::session_affinity_enabled`].
    ///
    /// It ships OFF because that migration moves an ALREADY-WARM session to a
    /// cooler account purely to even out pinned-session counts, and Anthropic's
    /// prompt cache is per-account: every such move costs a full prompt-cache
    /// re-creation of the whole conversation prefix on the target. A session's
    /// account is chosen at START, or when its pin fails an ACCOUNT-level HARD gate
    /// ([`Manager::account_hard_ok`]) — never merely to balance load. Set
    /// `"loadBalanceMigration": true` to restore the balancing behaviour.
    pub fn load_balance_migration_enabled(&self) -> bool {
        self.config
            .lock()
            .expect("config lock poisoned")
            .extra
            .get("loadBalanceMigration")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    /// Max DISTINCT destination accounts one session may be diverted to inside a
    /// single hold episode, read from the config's unmodelled top-level
    /// `divertBudget`. Same read pattern as [`Self::session_affinity_enabled`].
    ///
    /// **Default `0` = unlimited = today's behaviour byte-for-byte.** This is
    /// the kill switch: an unbounded budget never blocks a divert, so shipping
    /// it means every existing divert path is untouched until someone opts in
    /// with `"divertBudget": N`. It stays `0` rather than a small nonzero
    /// default because flipping it is a production-traffic decision (a nonzero
    /// budget can turn a would-have-served divert into a `None`, which is new
    /// client-visible behaviour) that must follow the live measurement, not
    /// accompany it — see the divert-budget design notes §4.6.
    pub fn divert_budget(&self) -> u32 {
        self.config
            .lock()
            .expect("config lock poisoned")
            .extra
            .get("divertBudget")
            .and_then(|v| v.as_u64())
            .map_or(0, |v| v.min(u64::from(u32::MAX)) as u32)
    }

    /// The keep-warm wake signal. The warm loop `select!`s over its own ticker and
    /// `warm_wake().notified()`, so the first sweep after boot happens as soon as
    /// the probe has read some quota rather than a full `warmupSeconds` later. See
    /// the field docs on [`Manager`] for why a lost wake is impossible here.
    pub fn warm_wake(&self) -> &Notify {
        &self.warm_wake
    }

    /// Mint the next session key: a strictly-increasing, unique `u64` starting at
    /// 1. Called once per connection by the hybrid server when affinity is on.
    pub fn next_session_key(&self) -> u64 {
        self.session_seq.fetch_add(1, Ordering::Relaxed)
    }

    pub fn account_count(&self) -> usize {
        self.accounts.read().expect("accounts lock poisoned").len()
    }

    pub fn upstream(&self) -> &str {
        &self.upstream
    }

    pub fn proxy_api_key(&self) -> Option<&str> {
        self.proxy_api_key.as_deref()
    }

    /// Account `idx`'s OWN upstream-forwarding client — see
    /// [`AccountRuntime::http`] for why every account carries its own. `None`
    /// only if `idx` is stale (accounts are appended, never removed, so this is
    /// never observed on the live serving path — it exists for the same reason
    /// [`Self::access_token`] and [`Self::account_name`] return `Option`).
    pub fn http_client(&self, idx: usize) -> Option<Arc<reqwest::Client>> {
        self.accounts
            .read()
            .expect("accounts lock poisoned")
            .get(idx)
            .map(|a| a.http.clone())
    }

    /// The access token to inject for account `idx` (a clone — the request
    /// outlives the lock).
    pub fn access_token(&self, idx: usize) -> Option<String> {
        self.accounts
            .read()
            .expect("accounts lock poisoned")
            .get(idx)
            .map(|a| a.access_token.clone())
    }

    /// Whether account `idx` carries a refresh token. `false` for a
    /// `claude setup-token` credential (`tcr login --token`), whose access token
    /// can never be renewed — so for such a row an upstream 401 is proof the
    /// credential is dead, not rotation churn (see the proxy's 401 arm).
    pub fn has_refresh_token(&self, idx: usize) -> bool {
        self.accounts
            .read()
            .expect("accounts lock poisoned")
            .get(idx)
            .is_some_and(|a| a.refresh_token.is_some())
    }

    /// Display name of account `idx`, for the request log.
    pub fn account_name(&self, idx: usize) -> Option<String> {
        self.accounts
            .read()
            .expect("accounts lock poisoned")
            .get(idx)
            .map(|a| a.name.clone())
    }

    /// The pooled `account_uuid` to inject for account `idx` (a clone — the
    /// request outlives the lock). `None` when the account has no configured
    /// UUID, in which case the proxy leaves the body unchanged.
    pub fn account_uuid(&self, idx: usize) -> Option<String> {
        self.accounts
            .read()
            .expect("accounts lock poisoned")
            .get(idx)
            .and_then(|a| a.account_uuid.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager_with(config: &str) -> Arc<Manager> {
        let config: Config = serde_json::from_str(config).expect("the inline test config parses");
        Manager::with_live_refresher(config, None)
    }

    /// The regression guard for the 2026-08-14 incident: a config that lost its
    /// `sessionAffinity` key silently went cold when the default was `false`
    /// (0.952 → 0.498 prompt-cache hit ratio, sustained seven hours). An absent
    /// key must now read as enabled.
    #[test]
    fn session_affinity_defaults_to_enabled_when_key_is_absent() {
        let manager = manager_with(r#"{"accounts": []}"#);
        assert!(
            manager.session_affinity_enabled(),
            "a config with no sessionAffinity key must default to affinity ON"
        );
    }

    /// The explicit opt-out must still work: `"sessionAffinity": false` disables
    /// it.
    #[test]
    fn session_affinity_explicit_false_stays_disabled() {
        let manager = manager_with(r#"{"sessionAffinity": false, "accounts": []}"#);
        assert!(
            !manager.session_affinity_enabled(),
            "an explicit `sessionAffinity: false` must disable affinity"
        );
    }
}
