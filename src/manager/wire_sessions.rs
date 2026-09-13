//! `Manager` glue for [`crate::session_wire::WireSessionTracker`] — split verbatim from
//! `mod.rs`, mirroring `usage.rs`/`snapshot.rs`'s split.

use std::path::Path;
use std::sync::atomic::Ordering;

use super::*;
use crate::session_wire::{ToolResultEvent, ToolUseEvent};
use crate::session_wire_persist::{self, LoadReport};

impl Manager {
    /// Fold one request's session identity, model and parsed tool events into the wire-session
    /// table. A no-op when `session_id` is `None` — a request with no recognizable Claude Code
    /// identity has nothing for this table to key on.
    ///
    /// Called once per client request at the terminal outcome, alongside [`Self::record_served`]
    /// (same call site in `proxy.rs`) — not per upstream retry, for the same reason
    /// `record_served` isn't: a request rotated across accounts must count once, not once per
    /// account tried.
    pub fn record_wire_session(
        &self,
        session_id: Option<&str>,
        account: Option<String>,
        model: Option<String>,
        now: OffsetDateTime,
        tool_uses: &[ToolUseEvent],
        tool_results: &[ToolResultEvent],
    ) {
        let Some(session_id) = session_id else {
            return;
        };
        let now_ms = odt_to_ms(now);
        let mut tracker = self
            .wire_sessions
            .lock()
            .expect("wire sessions lock poisoned");
        tracker.record_request(session_id, account, model, now_ms, tool_uses, tool_results);
        drop(tracker);
        self.mark_wire_sessions_dirty();
    }

    /// Fold the `tool_use` blocks parsed out of a RESPONSE into the wire-session table, as
    /// running from `now` (the instant the response finished — stream end, or the JSON body
    /// being read). A no-op when `session_id` is `None` or the tool list is empty.
    ///
    /// Called from both response paths in `proxy.rs`, beside
    /// [`Self::record_wire_session_usage`], and NOT from the request call site: this is the
    /// only source that can ever put a tool in `running`, because a Claude Code request body
    /// carries each `tool_use` together with its `tool_result` — see
    /// [`crate::session_wire::tool_use_event_from_block`].
    pub fn record_wire_session_tool_uses(
        &self,
        session_id: Option<&str>,
        now: OffsetDateTime,
        tool_uses: &[ToolUseEvent],
    ) {
        let Some(session_id) = session_id else {
            return;
        };
        if tool_uses.is_empty() {
            return;
        }
        let now_ms = odt_to_ms(now);
        let mut tracker = self
            .wire_sessions
            .lock()
            .expect("wire sessions lock poisoned");
        tracker.record_response_tool_uses(session_id, now_ms, tool_uses);
        drop(tracker);
        self.mark_wire_sessions_dirty();
    }

    /// Add token counts learned from a response's usage to the wire-session table. Called
    /// beside [`Self::record_usage`] (the per-account ledger) at both of its call sites in
    /// `proxy.rs` — streamed and non-streamed — with the SAME [`crate::usage::UsageRecord`]
    /// fields, so the account ledger, the per-model tally and the wire session's quota totals
    /// never disagree about what a response carried.
    ///
    /// `quota_input` is the QUOTA figure (`UsageRecord::input_total()`, verbatim) and folds
    /// into the session's existing `input_tokens` total, unchanged in meaning. `base_input`,
    /// `cache_5m` and `cache_1h` (`UsageRecord::input`/`cache_5m`/`cache_1h`) are the pricing
    /// dimensions, kept in [`crate::session_wire::WireSession::by_model`] — wire 2.
    #[allow(clippy::too_many_arguments)]
    pub fn record_wire_session_usage(
        &self,
        session_id: Option<&str>,
        model: Option<&str>,
        quota_input: u64,
        base_input: u64,
        cache_5m: u64,
        cache_1h: u64,
        cache_read_tokens: u64,
        output_tokens: u64,
    ) {
        let Some(session_id) = session_id else {
            return;
        };
        let mut tracker = self
            .wire_sessions
            .lock()
            .expect("wire sessions lock poisoned");
        tracker.record_usage(
            session_id,
            model,
            quota_input,
            base_input,
            cache_5m,
            cache_1h,
            cache_read_tokens,
            output_tokens,
        );
        drop(tracker);
        self.mark_wire_sessions_dirty();
    }

    /// The live wire-session table, projected onto [`tcr_status_wire::SessionRow`] — what
    /// [`Self::snapshot`] hangs onto [`crate::stats::StatsSnapshot::wire_sessions`].
    pub(super) fn wire_sessions_snapshot(
        &self,
        now: OffsetDateTime,
    ) -> Vec<tcr_status_wire::SessionRow> {
        let now_ms = odt_to_ms(now);
        let tracker = self
            .wire_sessions
            .lock()
            .expect("wire sessions lock poisoned");
        tracker
            .snapshot(now_ms)
            .into_iter()
            .map(|(session_id, s)| tcr_status_wire::SessionRow {
                session_id,
                account: s.account,
                model: s.model,
                first_seen_ms: s.first_seen_ms,
                last_seen_ms: s.last_seen_ms,
                requests: s.requests,
                input_tokens: s.input_tokens,
                output_tokens: s.output_tokens,
                cache_read_tokens: s.cache_read_tokens,
                tools: tcr_status_wire::SessionToolsRow {
                    calls: s.tools.calls,
                    errors: s.tools.errors,
                    timeouts: s.tools.timeouts,
                    timeouts_by_class: s.tools.timeouts_by_class.clone(),
                    timed_out: s
                        .tools
                        .timed_out
                        .iter()
                        .map(|t| tcr_status_wire::SlowToolRow {
                            tool: t.tool.clone(),
                            seconds: t.seconds,
                            command_head: t.command_head.clone(),
                            command_class: t.command_class.map(|c| c.as_str().to_string()),
                            ended_ms: t.ended_ms,
                        })
                        .collect(),
                    over_one_minute: s.tools.over_one_minute,
                    subagents_running: s
                        .tools
                        .running
                        .values()
                        .filter(|r| {
                            (r.tool == "Agent" || r.tool == "Task")
                                && !crate::session_wire::running_tool_is_lost(
                                    &r.tool,
                                    r.started_ms,
                                    now_ms,
                                )
                        })
                        .count() as u64,
                    // Belt and braces beside `WireSessionTracker::restore`'s own drop: an
                    // entry too old to still be running is a LOST result, whatever put it
                    // there — see `crate::session_wire::running_tool_is_lost`.
                    running: s
                        .tools
                        .running
                        .into_values()
                        .filter(|r| {
                            !crate::session_wire::running_tool_is_lost(
                                &r.tool,
                                r.started_ms,
                                now_ms,
                            )
                        })
                        .map(|r| tcr_status_wire::RunningToolRow {
                            tool: r.tool,
                            started_ms: r.started_ms,
                            command_head: r.command_head,
                            command_class: r.command_class.map(|c| c.as_str().to_string()),
                        })
                        .collect(),
                    slowest: s
                        .tools
                        .slowest
                        .into_iter()
                        .map(|t| tcr_status_wire::SlowToolRow {
                            tool: t.tool,
                            seconds: t.seconds,
                            command_head: t.command_head,
                            command_class: t.command_class.map(|c| c.as_str().to_string()),
                            ended_ms: t.ended_ms,
                        })
                        .collect(),
                    by_tool: s
                        .tools
                        .by_tool
                        .iter()
                        .map(|(name, bucket)| tcr_status_wire::ToolBucketRow {
                            tool: name.clone(),
                            calls: bucket.calls,
                            errors: bucket.errors,
                            seconds_p50: bucket.seconds_p50(),
                            over_one_minute: bucket.over_one_minute,
                        })
                        .collect(),
                },
                req_per_minute: s.req_per_minute.projected(now_ms),
                cost_usd: Self::price_session(&self.usage, &s.by_model),
            })
            .collect()
    }

    /// Price one session's per-model token tallies against the account ledger's OWN pricing
    /// table (`crate::usage::UsageTracker::price_for`) and sum — one model's price at a time,
    /// so a session that spans two models is priced correctly rather than averaged. A model
    /// this table has no entry for contributes `0.0`, the same "unpriced" outcome
    /// `crate::pricing` documents for the account-level ledger, just not surfaced as a
    /// separate count here — `SessionRow::cost_usd` is a plain total, not an
    /// `Option`/`unpriced_requests` pair.
    fn price_session(
        usage: &crate::usage::UsageTracker,
        by_model: &std::collections::HashMap<String, crate::session_wire::ModelTokenTally>,
    ) -> f64 {
        let nanos: u64 = by_model
            .iter()
            .filter_map(|(model, tally)| {
                usage.price_for(model).map(|price| {
                    crate::pricing::cost_nanos(
                        &price,
                        tally.input,
                        tally.cache_5m,
                        tally.cache_1h,
                        tally.cache_read,
                        tally.output,
                    )
                })
            })
            .sum();
        nanos as f64 / 1_000_000_000.0
    }

    /// Fleet-wide tool-call totals, summed across every row `Self::wire_sessions_snapshot`
    /// just built — the ONE place this sum happens server-side, so a panel's headline and
    /// "BY TOOL" bars read the same numbers as the per-session rows (see the bridge, F3).
    pub(super) fn wire_sessions_summary(
        rows: &[tcr_status_wire::SessionRow],
    ) -> tcr_status_wire::SessionsSummary {
        let mut by_tool: std::collections::HashMap<String, tcr_status_wire::ToolBucketRow> =
            std::collections::HashMap::new();
        let mut summary = tcr_status_wire::SessionsSummary::default();
        for row in rows {
            summary.calls += row.tools.calls;
            summary.over_one_minute += row.tools.over_one_minute;
            summary.timeouts += row.tools.timeouts;
            for (class, count) in &row.tools.timeouts_by_class {
                *summary.timeouts_by_class.entry(class.clone()).or_insert(0) += count;
            }
            summary.cost_usd += row.cost_usd;
            for bucket in &row.tools.by_tool {
                let entry = by_tool.entry(bucket.tool.clone()).or_insert_with(|| {
                    tcr_status_wire::ToolBucketRow {
                        tool: bucket.tool.clone(),
                        ..Default::default()
                    }
                });
                entry.calls += bucket.calls;
                entry.errors += bucket.errors;
                entry.over_one_minute += bucket.over_one_minute;
            }
        }
        // `seconds_p50` cannot be summed across sessions — it is a median, not a total — so
        // the fleet-wide row leaves it at its default (0.0) rather than fabricating one from
        // an average of medians.
        summary.by_tool = by_tool.into_values().collect();
        summary
    }

    /// Flag the wire-session table as changed since the last flush. Mirrors
    /// [`Self::mark_affinity_dirty`]: a single relaxed store on the request path, with the
    /// actual write done off it by a debounced flusher.
    pub fn mark_wire_sessions_dirty(&self) {
        self.wire_sessions_dirty.store(true, Ordering::Relaxed);
    }

    /// Consume the dirty flag: `true` when something changed since the last call.
    pub fn take_wire_sessions_dirty(&self) -> bool {
        self.wire_sessions_dirty.swap(false, Ordering::Relaxed)
    }

    /// Write the wire-session table to `path`, atomically. Returns how many sessions
    /// landed. Never called on the request hot path — see `server.rs`'s debounced
    /// flusher, spawned only when a path was configured — and the caller logs the
    /// failure and carries on, same as [`Self::flush_affinity`]: a proxy that cannot
    /// write this cache still serves traffic exactly as it did before this file existed.
    pub fn flush_wire_sessions(&self, path: &Path) -> Result<usize, crate::config::ConfigError> {
        let now_ms = crate::now_ms();
        let sessions: std::collections::HashMap<String, crate::session_wire::WireSession> = {
            let tracker = self
                .wire_sessions
                .lock()
                .expect("wire sessions lock poisoned");
            tracker.snapshot(now_ms).into_iter().collect()
        };
        session_wire_persist::save(path, &sessions, now_ms)
    }

    /// Restore wire sessions from `path` into the live table, dropping anything stale
    /// (older than `ttl_ms`) or past [`session_wire_persist::PERSIST_CAP`].
    ///
    /// Existing in-memory sessions win — a session already tracked by a request served
    /// between boot and this call is fresher than anything on disk, same rule
    /// [`Self::restore_affinity`] follows for pins. In practice the table is empty here:
    /// this runs before the listener binds. Returns the store's report so the caller can
    /// state what was dropped.
    pub fn restore_wire_sessions(&self, path: &Path, ttl_ms: i64) -> LoadReport {
        let now_ms = crate::now_ms();
        let report = session_wire_persist::load(path, now_ms, ttl_ms);
        if !report.sessions.is_empty() {
            let mut tracker = self
                .wire_sessions
                .lock()
                .expect("wire sessions lock poisoned");
            tracker.restore(report.sessions.clone());
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use time::Duration;

    use super::*;

    /// `WireSessionTracker` itself is unit-tested in `session_wire.rs` with canned bodies;
    /// these tests prove the `Manager` GLUE — the lock, the projection to `SessionRow`, and
    /// that an absent session id is a clean no-op rather than a panic. [`Manager::from_runtimes`]
    /// (also used by `pins.rs`'s tests) never touches the network.
    #[test]
    fn record_wire_session_round_trips_through_the_manager_snapshot() {
        let manager = Manager::from_runtimes(vec![]);
        let now = OffsetDateTime::now_utc();
        manager.record_wire_session(
            Some("sess-glue"),
            Some("alice@example.com".to_string()),
            Some("claude-fable-5".to_string()),
            now,
            &[],
            &[],
        );
        manager.record_wire_session_usage(
            Some("sess-glue"),
            Some("claude-fable-5"),
            10,
            10,
            0,
            0,
            5,
            20,
        );

        let snap = manager.snapshot(now);
        assert_eq!(snap.wire_sessions.len(), 1);
        let row = &snap.wire_sessions[0];
        assert_eq!(row.session_id, "sess-glue");
        assert_eq!(row.account.as_deref(), Some("alice@example.com"));
        assert_eq!(row.input_tokens, 10);
        assert_eq!(row.output_tokens, 20);
        assert_eq!(row.cache_read_tokens, 5);
    }

    /// `subagents_running` counts only `Agent`/`Task` entries among the running tools — a plain
    /// `Bash` call sitting alongside two subagents must not be counted, and a caller must be
    /// able to print "2 subagents" without scanning `running` itself.
    #[test]
    fn subagents_running_counts_agent_and_task_but_not_bash() {
        let manager = Manager::from_runtimes(vec![]);
        let now = OffsetDateTime::now_utc();
        let uses = vec![
            ToolUseEvent {
                id: "tu_bash".to_string(),
                name: Some("Bash".to_string()),
                command_head: Some("ls".to_string()),
                command_class: None,
            },
            ToolUseEvent {
                id: "tu_agent".to_string(),
                name: Some("Agent".to_string()),
                command_head: Some("reviewer: check the wire fixtures".to_string()),
                command_class: None,
            },
            ToolUseEvent {
                id: "tu_task".to_string(),
                name: Some("Task".to_string()),
                command_head: Some("review the diff".to_string()),
                command_class: None,
            },
        ];
        manager.record_wire_session(Some("sess-sub"), None, None, now, &uses, &[]);

        let snap = manager.snapshot(now);
        assert_eq!(snap.wire_sessions.len(), 1);
        let row = &snap.wire_sessions[0];
        assert_eq!(row.tools.running.len(), 3, "all three tools stay pending");
        assert_eq!(
            row.tools.subagents_running, 2,
            "only the Agent and Task entries count, not the Bash call"
        );
    }

    /// Belt and braces beside `WireSessionTracker::restore`'s drop: a `Bash` entry older than
    /// the tool's own timeout plus its grace is not a running call, it is a lost result, and
    /// the projection drops it however it got there. `Agent`/`Task` have no such deadline and
    /// stay — they are the calls that legitimately run for an hour.
    #[test]
    fn a_bash_entry_past_its_timeout_is_not_projected_as_running() {
        let manager = Manager::from_runtimes(vec![]);
        let started = OffsetDateTime::now_utc();
        let uses = vec![
            ToolUseEvent {
                id: "tu_bash".to_string(),
                name: Some("Bash".to_string()),
                command_head: Some("cargo test --release".to_string()),
                command_class: None,
            },
            ToolUseEvent {
                id: "tu_agent".to_string(),
                name: Some("Agent".to_string()),
                command_head: Some("reviewer: check the wire fixtures".to_string()),
                command_class: None,
            },
        ];
        manager.record_wire_session(Some("sess-lost"), None, None, started, &uses, &[]);

        let grace_ms = crate::session_wire::RUNNING_BASH_LOST_MS;
        let inside = manager.snapshot(started + Duration::milliseconds(grace_ms));
        assert_eq!(
            inside.wire_sessions[0].tools.running.len(),
            2,
            "at the line both are still running"
        );

        let past = manager.snapshot(started + Duration::milliseconds(grace_ms + 1_000));
        let row = &past.wire_sessions[0];
        assert_eq!(
            row.tools.running.len(),
            1,
            "the Bash entry is dropped, the Agent stays"
        );
        assert_eq!(row.tools.running[0].tool, "Agent");
        assert_eq!(
            row.tools.subagents_running, 1,
            "the count and the list agree about what is running"
        );
    }

    #[test]
    fn record_wire_session_with_no_session_id_is_a_no_op() {
        let manager = Manager::from_runtimes(vec![]);
        let now = OffsetDateTime::now_utc();
        manager.record_wire_session(
            None,
            Some("alice@example.com".to_string()),
            None,
            now,
            &[],
            &[],
        );
        manager.record_wire_session_usage(None, Some("claude-fable-5"), 10, 10, 0, 0, 5, 20);
        assert!(manager.snapshot(now).wire_sessions.is_empty());
    }

    /// The scoped ask: two requests on two different models, each with a known price, sum to
    /// the expected total to the cent — a session that spans models is priced per-model and
    /// summed, never averaged or priced against whichever model happened to be current.
    #[test]
    fn cost_usd_sums_two_models_priced_independently() {
        let manager = Manager::from_runtimes(vec![]);
        let now = OffsetDateTime::now_utc();
        manager.record_wire_session(Some("sess-cost"), None, None, now, &[], &[]);

        // 1,000,000 base input tokens on Opus 5 ($5.00/MTok) = $5.00 exactly.
        manager.record_wire_session_usage(
            Some("sess-cost"),
            Some("claude-opus-5"),
            1_000_000,
            1_000_000,
            0,
            0,
            0,
            0,
        );
        // 1,000,000 output tokens on Sonnet 5 ($10.00/MTok output) = $10.00 exactly.
        manager.record_wire_session_usage(
            Some("sess-cost"),
            Some("claude-sonnet-5"),
            1_000_000,
            0,
            0,
            0,
            0,
            1_000_000,
        );

        let snap = manager.snapshot(now);
        let row = &snap.wire_sessions[0];
        assert_eq!(
            row.cost_usd, 15.0,
            "$5.00 (opus input) + $10.00 (sonnet output)"
        );
        assert_eq!(
            snap.wire_sessions_summary.cost_usd, 15.0,
            "the fleet-wide summary sums the same per-session totals"
        );
    }

    /// The wire row, and the fleet summary above it, carry the per-class timeout split and
    /// the timed-out commands themselves — not just the headline count a panel could not
    /// act on. Two sessions, so the summary is proved to SUM rather than to copy one row.
    #[test]
    fn timeouts_by_class_and_the_timed_out_commands_reach_the_wire() {
        let manager = Manager::from_runtimes(vec![]);
        let now = OffsetDateTime::now_utc();
        let timed_out = |id: &str| ToolResultEvent {
            id: id.to_string(),
            is_error: true,
            timed_out: true,
        };
        let bash = |id: &str, command: &str| {
            crate::session_wire::tool_use_event_from_block(
                id.to_string(),
                Some("Bash".to_string()),
                Some(&serde_json::json!({ "command": command })),
            )
        };

        manager.record_wire_session(
            Some("sess-one"),
            None,
            None,
            now,
            &[
                bash("tu_wait", "until grep -q ready build.log; do sleep 5; done"),
                bash("tu_push", "git push origin main"),
            ],
            &[],
        );
        let later = now + Duration::seconds(1);
        manager.record_wire_session(
            Some("sess-one"),
            None,
            None,
            later,
            &[],
            &[timed_out("tu_wait")],
        );
        let later2 = now + Duration::seconds(2);
        manager.record_wire_session(
            Some("sess-one"),
            None,
            None,
            later2,
            &[],
            &[timed_out("tu_push")],
        );

        // A second session times out on another `git push`, so the fleet's `git-net` count
        // must read 2 while each session's own reads 1.
        manager.record_wire_session(
            Some("sess-two"),
            None,
            None,
            now,
            &[bash("tu_push2", "git push origin feature")],
            &[],
        );
        manager.record_wire_session(
            Some("sess-two"),
            None,
            None,
            later,
            &[],
            &[timed_out("tu_push2")],
        );

        let snap = manager.snapshot(later2);
        let one = snap
            .wire_sessions
            .iter()
            .find(|r| r.session_id == "sess-one")
            .expect("sess-one");
        assert_eq!(one.tools.timeouts, 2);
        assert_eq!(one.tools.timeouts_by_class.get("wait"), Some(&1));
        assert_eq!(one.tools.timeouts_by_class.get("git-net"), Some(&1));
        assert_eq!(one.tools.timed_out.len(), 2);
        assert_eq!(
            one.tools.timed_out[0].command_head.as_deref(),
            Some("git push origin main"),
            "newest first, with the command, not a bare tool name"
        );
        assert_eq!(
            one.tools.timed_out[0].command_class.as_deref(),
            Some("git-net")
        );

        let summary = &snap.wire_sessions_summary;
        assert_eq!(summary.timeouts, 3);
        assert_eq!(
            summary.timeouts_by_class.get("git-net"),
            Some(&2),
            "the fleet sums both sessions' git-net timeouts"
        );
        assert_eq!(summary.timeouts_by_class.get("wait"), Some(&1));
        assert_eq!(
            summary.timeouts_by_class.values().sum::<u64>(),
            summary.timeouts,
            "the headline and the section read one number"
        );
    }

    fn tmp_wire_sessions_path(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tcr-wire-sessions-manager-test-{}-{label}-{}",
            std::process::id(),
            crate::now_ms()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join("session-wire.json")
    }

    /// The Manager-level round trip: a call tracked by one manager, flushed to disk,
    /// restored by a second (fresh) manager — the restart-survival contract, proven
    /// through the same `flush_wire_sessions`/`restore_wire_sessions` pair `server.rs`
    /// wires into boot and the debounced flusher, not by calling
    /// `session_wire_persist::save`/`load` directly.
    #[test]
    fn flush_and_restore_round_trip_through_two_managers() {
        let path = tmp_wire_sessions_path("round-trip");
        let now = OffsetDateTime::now_utc();

        let first = Manager::from_runtimes(vec![]);
        first.record_wire_session(
            Some("sess-restart"),
            Some("alice@example.com".to_string()),
            Some("claude-sonnet-5".to_string()),
            now,
            &[ToolUseEvent {
                id: "tu_1".to_string(),
                name: Some("Bash".to_string()),
                command_head: Some("ls".to_string()),
                command_class: None,
            }],
            &[ToolResultEvent {
                id: "tu_1".to_string(),
                is_error: false,
                timed_out: false,
            }],
        );
        let written = first
            .flush_wire_sessions(&path)
            .expect("flush to a writable temp path");
        assert_eq!(written, 1);

        let second = Manager::from_runtimes(vec![]);
        let report =
            second.restore_wire_sessions(&path, crate::session_wire_persist::RESTORE_TTL_MS);
        assert_eq!(report.degraded, None);
        assert_eq!(report.sessions.len(), 1);

        let snap = second.snapshot(now);
        assert_eq!(snap.wire_sessions.len(), 1);
        let row = &snap.wire_sessions[0];
        assert_eq!(row.session_id, "sess-restart");
        assert_eq!(row.account.as_deref(), Some("alice@example.com"));
        assert_eq!(
            row.tools.calls, 1,
            "the restored row is indistinguishable from a live one, tool counts included"
        );
    }

    /// Restoring must never clobber a session the SECOND manager already tracked
    /// between its own boot and this call — same "existing wins" rule
    /// `restore_affinity` follows for pins.
    #[test]
    fn restore_does_not_overwrite_an_existing_in_memory_session() {
        let path = tmp_wire_sessions_path("no-clobber");
        let now = OffsetDateTime::now_utc();

        let first = Manager::from_runtimes(vec![]);
        first.record_wire_session(
            Some("sess-shared"),
            Some("alice@example.com".to_string()),
            None,
            now,
            &[],
            &[],
        );
        first.flush_wire_sessions(&path).expect("flush");

        let second = Manager::from_runtimes(vec![]);
        // A request already served on the SECOND manager before restore runs.
        second.record_wire_session(
            Some("sess-shared"),
            Some("bob@example.com".to_string()),
            None,
            now,
            &[],
            &[],
        );
        second.restore_wire_sessions(&path, crate::session_wire_persist::RESTORE_TTL_MS);

        let snap = second.snapshot(now);
        assert_eq!(snap.wire_sessions.len(), 1);
        assert_eq!(
            snap.wire_sessions[0].account.as_deref(),
            Some("bob@example.com"),
            "the in-memory value, fresher than the file, must survive the restore"
        );
    }

    /// The dirty flag: unset on a fresh manager, set by a call that mutates the wire
    /// session table, and cleared by consuming it — the same debounce contract the
    /// flusher task in `server.rs` runs on.
    #[test]
    fn wire_sessions_dirty_flag_tracks_mutation_and_clears_on_take() {
        let manager = Manager::from_runtimes(vec![]);
        assert!(!manager.take_wire_sessions_dirty());

        manager.record_wire_session(
            Some("sess-dirty"),
            None,
            None,
            OffsetDateTime::now_utc(),
            &[],
            &[],
        );
        assert!(manager.take_wire_sessions_dirty());
        assert!(
            !manager.take_wire_sessions_dirty(),
            "a second read without an intervening write finds nothing new"
        );
    }
}
