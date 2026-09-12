//! `Manager` glue for [`crate::session_wire::WireSessionTracker`] — split verbatim from
//! `mod.rs`, mirroring `usage.rs`/`snapshot.rs`'s split.

use super::*;
use crate::session_wire::{ToolResultEvent, ToolUseEvent};

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
    /// dimensions, kept in [`crate::session_wire::WireSession::by_model`] — wire 2
    /// (`data/plans/wire-2-bridge.md`).
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
                    over_one_minute: s.tools.over_one_minute,
                    subagents_running: s
                        .tools
                        .running
                        .values()
                        .filter(|r| r.tool == "Agent" || r.tool == "Task")
                        .count() as u64,
                    running: s
                        .tools
                        .running
                        .into_values()
                        .map(|r| tcr_status_wire::RunningToolRow {
                            tool: r.tool,
                            started_ms: r.started_ms,
                            command_head: r.command_head,
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
}

#[cfg(test)]
mod tests {
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
            },
            ToolUseEvent {
                id: "tu_agent".to_string(),
                name: Some("Agent".to_string()),
                command_head: Some("henry:coder: F4 subagents on the wire".to_string()),
            },
            ToolUseEvent {
                id: "tu_task".to_string(),
                name: Some("Task".to_string()),
                command_head: Some("review the diff".to_string()),
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
}
