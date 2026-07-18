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

    /// Compute the live snapshot the TUI renders. Every quota figure is evaluated
    /// at `now` so the display can never show a past-reset window as still full.
    pub fn snapshot(&self, now: OffsetDateTime) -> StatsSnapshot {
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
                let quota_state = match gating {
                    Some(u) if u >= 1.0 => crate::stats::QuotaState::Exhausted,
                    Some(u) if u >= threshold => crate::stats::QuotaState::NearLimit,
                    _ => crate::stats::QuotaState::Normal,
                };
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
                    last_used: a.last_used_ms.and_then(ms_to_odt),
                    rate_limited_until: a
                        .rate_limited_until_ms
                        .filter(|&until| until > odt_to_ms(now))
                        .and_then(ms_to_odt),
                    probe_status: a.probe_status,
                    last_probe: a.last_probe_ms.and_then(ms_to_odt),
                    probe_error: a.probe_error.clone(),
                    quota_state,
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

        // Resolve each session's account_idx → name from the accounts guard we
        // already hold, sorted most-recent-first for the TUI sessions pane.
        let sessions = {
            let map = self.sessions.lock().expect("sessions lock poisoned");
            let mut v: Vec<SessionSnapshot> = map
                .iter()
                .map(|(k, s)| SessionSnapshot {
                    id: short_session_id(*k),
                    account: accounts
                        .get(s.account_idx)
                        .map(|a| a.name.clone())
                        .unwrap_or_default(),
                    requests: s.requests,
                    last_seen: ms_to_odt(s.last_seen_ms),
                    kind: s.kind,
                })
                .collect();
            v.sort_by_key(|s| std::cmp::Reverse(s.last_seen));
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
