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
    /// top-level `sessionAffinity` (default `false` — off unless explicitly
    /// enabled). Same read pattern as [`Self::probe_interval_seconds`]. When
    /// `false`, the hybrid server injects no `SessionKey` extension, so `select`
    /// always receives `affinity = None` and the disabled path is inert.
    pub fn session_affinity_enabled(&self) -> bool {
        self.config
            .lock()
            .expect("config lock poisoned")
            .extra
            .get("sessionAffinity")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
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

    pub fn http_client(&self) -> reqwest::Client {
        self.http.clone()
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
