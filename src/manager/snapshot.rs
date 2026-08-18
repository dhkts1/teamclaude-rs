//! `Manager` snapshot methods, split verbatim from `mod.rs`.

use super::*;

impl Manager {
    /// Count exactly one served client request against account `idx` (the true
    /// serving account) and stamp its last-used time. Called once per client
    /// request at the terminal outcome — never per upstream response — so retries
    /// that rotate across accounts do not inflate the counter (bug #4).
    pub fn record_served(
        &self,
        idx: usize,
        now: OffsetDateTime,
        session_key: Option<u64>,
        kind: SessionKind,
    ) {
        // Short display id for the log line, computed before the accounts lock so
        // the borrow ordering in `tracing::info!` stays simple.
        let sid = session_key.map(short_session_id);
        {
            let mut accounts = self.accounts.write().expect("accounts lock poisoned");
            if let Some(account) = accounts.get_mut(idx) {
                account.requests += 1;
                account.last_used_ms = Some(odt_to_ms(now));
                // One line per forwarded request naming the account that served it,
                // so the load spread is observable in the headless log (and auditable
                // against the reported "everything piles onto one account" symptom).
                tracing::info!(account = %account.name, index = idx, session = sid, "serving request");
            }
        }
        // Upsert per-session stats (only when a session key exists) so a session
        // pinned by affinity is observable live. Independent of the `affinity`
        // pin map, so routing is unaffected.
        if let Some(key) = session_key {
            let now_ms = odt_to_ms(now);
            let mut sessions = self.sessions.lock().expect("sessions lock poisoned");
            let stat = sessions.entry(key).or_insert(SessionStat {
                account_idx: idx,
                requests: 0,
                last_seen_ms: now_ms,
                kind,
            });
            stat.account_idx = idx;
            stat.requests += 1;
            stat.last_seen_ms = now_ms;
            stat.kind = kind;
            // Bound the map so a long-lived proxy can't grow it without limit: once over
            // SESSION_CAP, evict the single oldest-last-seen entry (not the one we just
            // touched). Personal use has a handful of sessions; this is a backstop.
            const SESSION_CAP: usize = 128;
            if sessions.len() > SESSION_CAP {
                if let Some((&oldest, _)) = sessions.iter().min_by_key(|(_, s)| s.last_seen_ms) {
                    if oldest != key {
                        sessions.remove(&oldest);
                    }
                }
            }
        }
        self.set_current(idx);
    }

    /// Append a served-request entry to the ring buffer (most-recent-last).
    pub fn push_log(&self, entry: RequestLogEntry) {
        let mut log = self.log.lock().expect("log lock poisoned");
        if log.len() >= REQUEST_LOG_CAPACITY {
            log.pop_front();
        }
        log.push_back(entry);
    }

    /// Each account's effective gating threshold — its own `switchThreshold`, else
    /// the global one — in account order, resolved exactly as [`Self::snapshot`]
    /// resolves it internally.
    ///
    /// Exposed for the status endpoint (`crate::proxy`), which ships the SERVER's
    /// thresholds alongside its snapshot. A client must not re-derive them from
    /// `~/.config/teamclaude.json`: that file may have been edited since the server
    /// booted, and a client-ordered threshold list zipped against a server-ordered
    /// account list would mislabel which windows are holding which account.
    pub fn thresholds(&self) -> Vec<f64> {
        self.accounts
            .read()
            .expect("accounts lock poisoned")
            .iter()
            .map(|a| a.switch_threshold.unwrap_or(self.global_threshold))
            .collect()
    }

    /// Whether this server's accounts are forced onto HTTP/1.1 — see
    /// [`config::Config::http1_only`]. Server-wide, not per-account, and
    /// exposed for the same reason `thresholds` is: a client must not
    /// re-derive this from `~/.config/teamclaude.json`, which may have been
    /// edited (or the process not yet restarted to pick it up) since the
    /// server actually booted its clients.
    pub fn http1_only(&self) -> bool {
        self.config.lock().expect("config lock poisoned").http1_only
    }

    /// Compute the live snapshot the TUI renders. Every quota figure is evaluated
    /// at `now` so the display can never show a past-reset window as still full.
    pub fn snapshot(&self, now: OffsetDateTime) -> StatsSnapshot {
        let now_ms = odt_to_ms(now);
        // (1) UNDER THE AFFINITY LOCK ONLY: copy every session's PIN out into a
        // local, then DROP the lock before the accounts lock below is taken — the
        // two are NEVER held simultaneously (the documented deadlock), the same
        // three-section shape [`Manager::select`] uses. The affinity map is the sole
        // authority on where a session is pinned; `SessionStat.account_idx` records
        // only who SERVED last, which a divert deliberately moves while the pin
        // stays put.
        let pins: HashMap<u64, usize> = {
            let affinity = self.affinity.lock().expect("affinity lock poisoned");
            affinity
                .iter()
                .map(|(&key, &(idx, _))| (key, idx))
                .collect()
        };
        // (2) UNDER THE ACCOUNTS LOCK: everything below resolves indices to names.
        let accounts = self.accounts.read().expect("accounts lock poisoned");
        let account_snaps = accounts
            .iter()
            .map(|a| {
                let five_hour = a.quota.five_hour.map(|w| w.effective(now));
                let seven_day = a.quota.seven_day.map(|w| w.effective(now));
                // Honest quota state vs this account's OWN threshold (same gating
                // dims `eligible` uses): the most-spent of the 5-hour and weekly
                // windows decides near-limit vs exhausted. Status stays "active" —
                // being quota-parked is not a dead credential.
                let threshold = a.switch_threshold.unwrap_or(self.global_threshold);
                let gating = [five_hour, seven_day]
                    .into_iter()
                    .flatten()
                    .reduce(f64::max);
                let quota_state = crate::stats::QuotaState::from_utilization(gating, threshold);
                // Why this account is out and when it clears — the GENERAL
                // (non-Fable) view: `is_fable = false`, so the model-scoped weekly
                // never gates a general fleet row (an account spent only on its
                // Fable bucket still serves every other model, so it reads `Ok`).
                let (gate, free_at) = Self::account_gate(a, threshold, now, now_ms, false);
                AccountSnapshot {
                    name: a.name.clone(),
                    priority: a.priority,
                    // `Throttled` is cleared from the enum only when the account
                    // next serves a non-429 (proxy.rs), so a naturally-expired hold
                    // would linger as a stale "throttled" label. Derive the DISPLAYED
                    // status from the live deadline — exactly as `eligible`,
                    // `rate_limited_until` and the quota bars already do — so the
                    // snapshot never shows a status the routing no longer honours.
                    status: {
                        let displayed = match a.status {
                            AccountStatus::Throttled
                                if a.rate_limited_until_ms
                                    .is_none_or(|until| until <= odt_to_ms(now)) =>
                            {
                                AccountStatus::Active
                            }
                            other => other,
                        };
                        displayed.as_str().to_string()
                    },
                    disabled: a.disabled,
                    five_hour,
                    five_hour_reset: a.quota.five_hour.and_then(|w| w.live_reset(now)),
                    seven_day,
                    seven_day_reset: a.quota.seven_day.and_then(|w| w.live_reset(now)),
                    seven_day_oi: a.quota.seven_day_oi.map(|w| w.effective(now)),
                    requests: a.requests,
                    input_tokens: a.input_tokens,
                    output_tokens: a.output_tokens,
                    cache_read_tokens: a.cache_read_tokens,
                    cache_creation_tokens: a.cache_creation_tokens,
                    last_used: a.last_used_ms.and_then(ms_to_odt),
                    rate_limited_until: a
                        .rate_limited_until_ms
                        .filter(|&until| until > odt_to_ms(now))
                        .and_then(ms_to_odt),
                    probe_status: a.probe_status,
                    last_probe: a.last_probe_ms.and_then(ms_to_odt),
                    probe_error: a.probe_error.clone(),
                    quota_state,
                    gate,
                    free_at,
                    stream_error_count: super::usage::stream_error_count(
                        &a.stream_error_times_ms,
                        now_ms,
                    ),
                    last_stream_error: a.last_stream_error.clone(),
                }
            })
            .collect();

        let recent = self
            .log
            .lock()
            .expect("log lock poisoned")
            .iter()
            .rev()
            .cloned()
            .collect();

        // Resolve each session's PIN (from `pins`, read above) and its LAST SERVER
        // (`SessionStat.account_idx`) to names from the accounts guard we already
        // hold. A session with no pin has no home, so it shows where it last served.
        let sessions = {
            let map = self.sessions.lock().expect("sessions lock poisoned");
            let name_of = |idx: usize| {
                accounts
                    .get(idx)
                    .map(|a| a.name.clone())
                    .unwrap_or_default()
            };
            let mut v: Vec<SessionSnapshot> = map
                .iter()
                .map(|(k, s)| {
                    let last_served_account = name_of(s.account_idx);
                    let account = pins
                        .get(k)
                        .map_or_else(|| last_served_account.clone(), |&idx| name_of(idx));
                    SessionSnapshot {
                        id: short_session_id(*k),
                        account,
                        last_served_account,
                        requests: s.requests,
                        last_seen: ms_to_odt(s.last_seen_ms),
                        kind: s.kind,
                    }
                })
                .collect();
            // STABLE order — (pinned account, session id) — not recency. Sorting
            // most-recent-first re-ordered the pane on every single request, so rows
            // churned under the operator's eyes and a session appeared to move even
            // when its pin never did. Both keys change only when the pin actually
            // moves or a session appears/disappears; the age stays visible as data in
            // the `Last` column rather than being encoded in the row order.
            v.sort_by(|a, b| a.account.cmp(&b.account).then_with(|| a.id.cmp(&b.id)));
            v
        };

        StatsSnapshot {
            accounts: account_snaps,
            current: *self.current.lock().expect("current lock poisoned"),
            recent,
            sessions,
        }
    }
}
