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
    /// `proxy.rs` — streamed and non-streamed — with the SAME parsed totals, so the two never
    /// disagree about how much a response cost.
    pub fn record_wire_session_usage(
        &self,
        session_id: Option<&str>,
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
    ) {
        let Some(session_id) = session_id else {
            return;
        };
        let mut tracker = self
            .wire_sessions
            .lock()
            .expect("wire sessions lock poisoned");
        tracker.record_usage(session_id, input_tokens, output_tokens, cache_read_tokens);
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
                },
            })
            .collect()
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
        manager.record_wire_session_usage(Some("sess-glue"), 10, 20, 5);

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
        manager.record_wire_session_usage(None, 10, 20, 5);
        assert!(manager.snapshot(now).wire_sessions.is_empty());
    }
}
